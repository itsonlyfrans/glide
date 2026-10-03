use super::{check_size, decode_postcard, encode_postcard_into, CodecError};
use crate::wire::{
    MAX_FILE_CHUNK_BYTES, MAX_FILE_CHUNK_FRAME_BYTES, MAX_TRANSFER_ID_BYTES, MAX_TRANSFER_ITEMS,
};

/// Borrowed FileChunk fields, encoded exactly as TransferMessage::FileChunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileChunkView<'a> {
    pub transfer_id: &'a str,
    pub file_id: u32,
    pub chunk_index: u32,
    pub offset: u64,
    pub data: &'a [u8],
    pub uncompressed_size: u32,
    pub compressed: bool,
    pub blake3_hash: [u8; 32],
}

/// Validated prefix geometry. No payload storage is allocated by the decoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileChunkHeader<'a> {
    pub transfer_id: &'a str,
    pub file_id: u32,
    pub chunk_index: u32,
    pub offset: u64,
    pub data_len: usize,
}

pub(super) fn validate_id(id: &str) -> Result<(), CodecError> {
    if id.is_empty() || id.len() > MAX_TRANSFER_ID_BYTES || id.chars().any(char::is_control) {
        return Err(CodecError::InvalidValue("transfer/clipboard id"));
    }
    Ok(())
}

pub(super) fn validate(chunk: &FileChunkView<'_>) -> Result<(), CodecError> {
    validate_id(chunk.transfer_id)?;
    if chunk.data.len() > MAX_FILE_CHUNK_BYTES
        || chunk.file_id >= MAX_TRANSFER_ITEMS
        || chunk.uncompressed_size as usize > MAX_FILE_CHUNK_BYTES
        || (chunk.compressed && (chunk.uncompressed_size == 0 || chunk.data.is_empty()))
        || (!chunk.compressed && chunk.uncompressed_size as usize != chunk.data.len())
        || chunk
            .offset
            .checked_add(u64::from(chunk.uncompressed_size))
            .is_none()
    {
        return Err(CodecError::InvalidValue("file_chunk length or offset"));
    }
    Ok(())
}

pub fn encode_file_chunk_into<'a>(
    chunk: &FileChunkView<'_>,
    out: &'a mut [u8],
) -> Result<&'a [u8], CodecError> {
    validate(chunk)?;
    encode_postcard_into(
        &(
            1u32,
            chunk.transfer_id,
            chunk.file_id,
            chunk.chunk_index,
            chunk.offset,
            chunk.data,
            chunk.uncompressed_size,
            chunk.compressed,
            chunk.blake3_hash,
        ),
        out,
        MAX_FILE_CHUNK_FRAME_BYTES,
    )
}

/// Write prefix, original payload slice, then trailer to avoid copying payloads.
pub fn encode_file_chunk_prefix_into<'a>(
    chunk: &FileChunkView<'_>,
    out: &'a mut [u8],
) -> Result<&'a [u8], CodecError> {
    validate(chunk)?;
    encode_postcard_into(
        &(
            1u32,
            chunk.transfer_id,
            chunk.file_id,
            chunk.chunk_index,
            chunk.offset,
            chunk.data.len(),
        ),
        out,
        MAX_FILE_CHUNK_FRAME_BYTES,
    )
}

pub fn encode_file_chunk_trailer_into<'a>(
    chunk: &FileChunkView<'_>,
    out: &'a mut [u8],
) -> Result<&'a [u8], CodecError> {
    validate(chunk)?;
    encode_postcard_into(
        &(chunk.uncompressed_size, chunk.compressed, chunk.blake3_hash),
        out,
        38,
    )
}

/// Retry with the accumulated prefix when this returns None. The borrowed ID
/// and byte count are valid until that caller storage changes. A length/ID bound
/// is rejected as soon as its varint arrives, before allocating a payload buffer.
pub fn decode_file_chunk_header(
    bytes: &[u8],
) -> Result<Option<(FileChunkHeader<'_>, usize)>, CodecError> {
    let mut offset = 0;
    macro_rules! integer {
        ($bits:expr) => {
            match varint(bytes, &mut offset, $bits)? {
                Some(value) => value,
                None => return Ok(None),
            }
        };
    }
    if integer!(32) != 1 {
        return Err(CodecError::InvalidValue("not a FileChunk"));
    }
    let id_len = integer!(64);
    if id_len == 0 || id_len > MAX_TRANSFER_ID_BYTES as u64 {
        return Err(CodecError::InvalidValue("transfer id length"));
    }
    let Some(id) = bytes.get(offset..offset + id_len as usize) else {
        return Ok(None);
    };
    let transfer_id =
        std::str::from_utf8(id).map_err(|_| CodecError::InvalidValue("transfer id UTF-8"))?;
    validate_id(transfer_id)?;
    offset += id_len as usize;
    let file_id = integer!(32) as u32;
    if file_id >= MAX_TRANSFER_ITEMS {
        return Err(CodecError::InvalidValue("file_chunk file id"));
    }
    let chunk_index = integer!(32) as u32;
    let file_offset = integer!(64);
    let data_len = integer!(64);
    if data_len > MAX_FILE_CHUNK_BYTES as u64 {
        return Err(CodecError::InvalidValue("file_chunk data length"));
    }
    Ok(Some((
        FileChunkHeader {
            transfer_id,
            file_id,
            chunk_index,
            offset: file_offset,
            data_len: data_len as usize,
        },
        offset,
    )))
}

fn varint(bytes: &[u8], offset: &mut usize, bits: u32) -> Result<Option<u64>, CodecError> {
    let mut value = 0u64;
    for shift in (0..bits).step_by(7) {
        let Some(&byte) = bytes.get(*offset) else {
            return Ok(None);
        };
        *offset += 1;
        let part = u64::from(byte & 0x7f);
        if part > (u64::MAX >> (64 - bits + shift)) || (shift != 0 && byte == 0) {
            return Err(CodecError::InvalidValue("noncanonical/overflowing varint"));
        }
        value |= part << shift;
        if byte & 0x80 == 0 {
            return Ok(Some(value));
        }
    }
    Err(CodecError::InvalidValue("overflowing varint"))
}

pub fn decode_file_chunk_view(bytes: &[u8]) -> Result<FileChunkView<'_>, CodecError> {
    check_size(bytes, MAX_FILE_CHUNK_FRAME_BYTES)?;
    let (header, prefix_len) = decode_file_chunk_header(bytes)?
        .ok_or(CodecError::InvalidValue("incomplete file_chunk header"))?;
    let payload_end = prefix_len + header.data_len;
    let data = bytes
        .get(prefix_len..payload_end)
        .ok_or(CodecError::InvalidValue("incomplete file_chunk payload"))?;
    let trailer = bytes
        .get(payload_end..)
        .ok_or(CodecError::InvalidValue("incomplete file_chunk trailer"))?;
    let (uncompressed_size, compressed, blake3_hash) = decode_postcard(trailer, 38)?;
    let chunk = FileChunkView {
        transfer_id: header.transfer_id,
        file_id: header.file_id,
        chunk_index: header.chunk_index,
        offset: header.offset,
        data,
        uncompressed_size,
        compressed,
        blake3_hash,
    };
    validate(&chunk)?;
    Ok(chunk)
}

pub(super) fn into_owned(chunk: FileChunkView<'_>) -> Result<crate::wire::FileChunk, CodecError> {
    // Private callers have fully validated the borrowed frame before either copy.
    let data = crate::wire::ChunkBytes::try_from_vec(chunk.data.to_vec())
        .map_err(|_| CodecError::InvalidValue("file_chunk data length"))?;
    Ok(crate::wire::FileChunk {
        transfer_id: chunk.transfer_id.to_owned(),
        file_id: chunk.file_id,
        chunk_index: chunk.chunk_index,
        offset: chunk.offset,
        data,
        uncompressed_size: chunk.uncompressed_size,
        compressed: chunk.compressed,
        blake3_hash: chunk.blake3_hash,
    })
}
