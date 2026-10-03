use crate::{Cancel, Config, Error, Result};
use glide_proto::{
    codec::{self, FileChunkView},
    wire::*,
};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const CHUNK_HEADER_CAPACITY: usize = MAX_TRANSFER_ID_BYTES + 32;
const CHUNK_CONTROL_FRAME_CAPACITY: usize = MAX_TRANSFER_ID_BYTES + 16;
const CHUNK_FRAME_OVERHEAD: usize = MAX_FILE_CHUNK_FRAME_BYTES - MAX_FILE_CHUNK_BYTES;
const MANIFEST_HEADER_CAPACITY: usize = MAX_TRANSFER_ID_BYTES * 2 + 32;

/// A big-endian u32 length followed by the existing postcard TransferMessage.
/// The deadline covers the entire frame, so trickle bytes cannot reset it.
pub async fn read_message<R: AsyncRead + Unpin>(
    reader: &mut R,
    timeout: Duration,
    cancel: &Cancel,
) -> Result<TransferMessage> {
    cancel
        .run(
            timeout,
            read_frame(reader, None, MAX_FILE_CONTROL_FRAME_BYTES, None),
        )
        .await
}

// Control is allowed to stay idle while chunk lanes are active. Once its first
// byte arrives, the entire remaining frame has the same slow-peer deadline.
pub(crate) async fn read_control_message<R: AsyncRead + Unpin>(
    reader: &mut R,
    timeout: Duration,
    cancel: &Cancel,
) -> Result<TransferMessage> {
    let first = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(Error::Cancelled),
        first = reader.read_u8() => first?,
    };
    cancel
        .run(
            timeout,
            read_frame(reader, Some(first), MAX_FILE_CONTROL_FRAME_BYTES, None),
        )
        .await
}

pub(crate) async fn read_manifest_page<R: AsyncRead + Unpin>(
    reader: &mut R,
    max_frame_bytes: usize,
    max_entries: usize,
    config: &Config,
    timeout: Duration,
    cancel: &Cancel,
) -> Result<FileManifest> {
    if max_frame_bytes == 0 || max_frame_bytes > MAX_FILE_CONTROL_FRAME_BYTES || max_entries == 0 {
        return Err(Error::Limit("manifest page frame limit"));
    }
    config.validate()?;
    let config = config.clone();
    let max_entries = max_entries.min(MAX_MANIFEST_FILES);
    cancel
        .run(timeout, async {
            let bytes = async {
                let frame_len = reader.read_u32().await? as usize;
                if frame_len == 0 || frame_len > max_frame_bytes {
                    return Err(Error::Limit("manifest page frame size"));
                }
                let variant = reader.read_u8().await?;
                if variant != 0 {
                    return Err(Error::Invalid("expected manifest page"));
                }
                let mut prefix = [0u8; MANIFEST_HEADER_CAPACITY];
                prefix[0] = variant;
                let mut prefix_len = 1;

                for _ in 0..2 {
                    let id_len =
                        read_header_varint(reader, &mut prefix, &mut prefix_len, frame_len, 64)
                            .await?;
                    if id_len == 0 || id_len > MAX_TRANSFER_ID_BYTES as u64 {
                        return Err(Error::Limit("manifest id length"));
                    }
                    let id_len = id_len as usize;
                    read_header_bytes(reader, &mut prefix, &mut prefix_len, frame_len, id_len)
                        .await?;
                    let id_start = prefix_len - id_len;
                    validate_manifest_id(&prefix[id_start..prefix_len])?;
                }

                let chunk_size =
                    read_header_varint(reader, &mut prefix, &mut prefix_len, frame_len, 32).await?;
                let page =
                    read_header_varint(reader, &mut prefix, &mut prefix_len, frame_len, 32).await?;
                if !(1..=MAX_FILE_CHUNK_BYTES as u64).contains(&chunk_size)
                    || page >= u64::from(MAX_TRANSFER_ITEMS)
                {
                    return Err(Error::Invalid("manifest page geometry"));
                }
                if prefix_len >= frame_len || prefix_len == prefix.len() {
                    return Err(Error::Invalid("incomplete manifest page header"));
                }
                let final_page = reader.read_u8().await?;
                prefix[prefix_len] = final_page;
                prefix_len += 1;
                if final_page > 1 {
                    return Err(Error::Invalid("manifest final-page flag"));
                }

                let entries =
                    read_header_varint(reader, &mut prefix, &mut prefix_len, frame_len, 64).await?;
                if entries == 0 || entries > max_entries as u64 {
                    return Err(Error::Limit("manifest page entries"));
                }

                let mut bytes = Vec::new();
                bytes
                    .try_reserve_exact(frame_len)
                    .map_err(|_| Error::Limit("manifest page allocation"))?;
                bytes.extend_from_slice(&prefix[..prefix_len]);
                while bytes.len() < frame_len {
                    let remaining = frame_len - bytes.len();
                    let count = (&mut *reader)
                        .take(remaining as u64)
                        .read_buf(&mut bytes)
                        .await?;
                    if count == 0 {
                        return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
                    }
                }
                Ok(bytes)
            }
            .await?;
            match tokio::task::spawn_blocking(move || {
                preflight_manifest_frame(&bytes, max_entries, &config)?;
                codec::decode_transfer(&bytes).map_err(Error::from)
            })
            .await
            .map_err(|_| Error::Worker)??
            {
                TransferMessage::FileManifest(manifest) => Ok(manifest),
                _ => Err(Error::Invalid("expected manifest page")),
            }
        })
        .await
}

pub(crate) async fn read_resume_message<R: AsyncRead + Unpin>(
    reader: &mut R,
    timeout: Duration,
    cancel: &Cancel,
) -> Result<FileResume> {
    cancel
        .run(timeout, async {
            let length = reader.read_u32().await? as usize;
            const LIMIT: usize = MAX_RESUME_RANGES * 10 + MAX_TRANSFER_ID_BYTES + 64;
            if length == 0 || length > LIMIT {
                return Err(Error::Limit("resume frame size"));
            }
            if reader.read_u8().await? != 2 {
                return Err(Error::Invalid("expected resume"));
            }
            let mut bytes = vec![0; length];
            bytes[0] = 2;
            reader.read_exact(&mut bytes[1..]).await?;
            tokio::task::spawn_blocking(move || {
                let mut offset = 1;
                let id_len = read_manifest_varint(&bytes, &mut offset, 64)?;
                if id_len == 0 || id_len > MAX_TRANSFER_ID_BYTES as u64 {
                    return Err(Error::Limit("resume id length"));
                }
                validate_manifest_id(read_manifest_bytes(&bytes, &mut offset, id_len as usize)?)?;
                if read_manifest_varint(&bytes, &mut offset, 32)? >= u64::from(MAX_TRANSFER_ITEMS) {
                    return Err(Error::Invalid("resume file id"));
                }
                let count = read_manifest_varint(&bytes, &mut offset, 64)?;
                if count > MAX_RESUME_RANGES as u64 {
                    return Err(Error::Limit("resume ranges"));
                }
                for _ in 0..count {
                    read_manifest_varint(&bytes, &mut offset, 32)?;
                    read_manifest_varint(&bytes, &mut offset, 32)?;
                }
                if offset != bytes.len() {
                    return Err(Error::Invalid("resume trailing bytes"));
                }
                match codec::decode_transfer(&bytes)? {
                    TransferMessage::FileResume(resume) => Ok(resume),
                    _ => Err(Error::Invalid("expected resume")),
                }
            })
            .await
            .map_err(|_| Error::Worker)?
        })
        .await
}

async fn read_header_varint<R: AsyncRead + Unpin>(
    reader: &mut R,
    prefix: &mut [u8],
    prefix_len: &mut usize,
    frame_len: usize,
    bits: u32,
) -> Result<u64> {
    let mut value = 0u64;
    for shift in (0..bits).step_by(7) {
        if *prefix_len >= frame_len || *prefix_len == prefix.len() {
            return Err(Error::Invalid("incomplete manifest page header"));
        }
        reader
            .read_exact(&mut prefix[*prefix_len..*prefix_len + 1])
            .await?;
        let byte = prefix[*prefix_len];
        *prefix_len += 1;
        let part = u64::from(byte & 0x7f);
        if part > (u64::MAX >> (64 - bits + shift)) || (shift != 0 && byte == 0) {
            return Err(Error::Invalid("manifest header varint"));
        }
        value |= part << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(Error::Invalid("manifest header varint"))
}

async fn read_header_bytes<R: AsyncRead + Unpin>(
    reader: &mut R,
    prefix: &mut [u8],
    prefix_len: &mut usize,
    frame_len: usize,
    length: usize,
) -> Result<()> {
    let end = prefix_len
        .checked_add(length)
        .filter(|end| *end <= frame_len && *end <= prefix.len())
        .ok_or(Error::Invalid("manifest header length"))?;
    reader.read_exact(&mut prefix[*prefix_len..end]).await?;
    *prefix_len = end;
    Ok(())
}

fn validate_manifest_id(bytes: &[u8]) -> Result<()> {
    let id = std::str::from_utf8(bytes).map_err(|_| Error::Invalid("manifest id UTF-8"))?;
    if id.chars().any(char::is_control) {
        return Err(Error::Invalid("manifest id"));
    }
    Ok(())
}

fn preflight_manifest_frame(bytes: &[u8], max_entries: usize, config: &Config) -> Result<()> {
    let mut offset = 0;
    if read_manifest_varint(bytes, &mut offset, 32)? != 0 {
        return Err(Error::Invalid("expected manifest page"));
    }
    for _ in 0..2 {
        let id_len = read_manifest_varint(bytes, &mut offset, 64)?;
        if id_len == 0 || id_len > MAX_TRANSFER_ID_BYTES as u64 {
            return Err(Error::Limit("manifest id length"));
        }
        let id_len = id_len as usize;
        let id = read_manifest_bytes(bytes, &mut offset, id_len)?;
        validate_manifest_id(id)?;
    }
    let chunk_size = read_manifest_varint(bytes, &mut offset, 32)?;
    let page = read_manifest_varint(bytes, &mut offset, 32)?;
    if !(1..=MAX_FILE_CHUNK_BYTES as u64).contains(&chunk_size)
        || page >= u64::from(MAX_TRANSFER_ITEMS)
    {
        return Err(Error::Invalid("manifest page geometry"));
    }
    if read_manifest_bool(bytes, &mut offset)? > 1 {
        return Err(Error::Invalid("manifest final-page flag"));
    }
    let entries = read_manifest_varint(bytes, &mut offset, 64)?;
    if entries == 0 || entries > max_entries as u64 {
        return Err(Error::Limit("manifest page entries"));
    }

    let chunk_size = chunk_size as u64;
    let mut page_bytes = 0u64;
    let mut previous_file_id = None;
    for index in 0..entries as usize {
        let file_id = read_manifest_varint(bytes, &mut offset, 32)? as u32;
        if file_id >= MAX_TRANSFER_ITEMS
            || (index == 0 && page == 0 && file_id != 0)
            || previous_file_id
                .is_some_and(|previous: u32| previous.checked_add(1) != Some(file_id))
        {
            return Err(Error::Invalid("manifest file id sequence"));
        }
        previous_file_id = Some(file_id);

        let path_len = read_manifest_varint(bytes, &mut offset, 64)?;
        if path_len == 0
            || path_len > config.max_path_bytes as u64
            || path_len > MAX_MANIFEST_PATH_BYTES as u64
        {
            return Err(Error::Limit("manifest path length"));
        }
        let path_bytes = read_manifest_bytes(bytes, &mut offset, path_len as usize)?;
        let path =
            std::str::from_utf8(path_bytes).map_err(|_| Error::Invalid("manifest path UTF-8"))?;
        crate::filesystem::validate_relative(path, config.max_depth, config.max_path_bytes)?;

        let size = read_manifest_varint(bytes, &mut offset, 64)?;
        if size > config.max_file_bytes {
            return Err(Error::Limit("manifest file size"));
        }
        if size.div_ceil(chunk_size) > u64::from(config.max_chunks_per_file) {
            return Err(Error::Limit("manifest chunk count"));
        }
        let is_dir = read_manifest_bool(bytes, &mut offset)?;
        if is_dir > 1 {
            return Err(Error::Invalid("manifest directory flag"));
        }
        let has_hash = read_manifest_bool(bytes, &mut offset)?;
        if has_hash > 1 {
            return Err(Error::Invalid("manifest hash flag"));
        }
        if (is_dir == 1 && (size != 0 || has_hash != 0)) || (is_dir == 0 && has_hash != 1) {
            return Err(Error::Invalid("manifest entry fields"));
        }
        if has_hash == 1 {
            let _ = read_manifest_bytes(bytes, &mut offset, 32)?;
        }
        page_bytes = page_bytes
            .checked_add(size)
            .filter(|total| *total <= config.max_transfer_bytes && *total <= MAX_TRANSFER_BYTES)
            .ok_or(Error::Limit("manifest page bytes"))?;
    }
    if offset != bytes.len() {
        return Err(Error::Invalid("trailing manifest page bytes"));
    }
    Ok(())
}

fn read_manifest_varint(bytes: &[u8], offset: &mut usize, bits: u32) -> Result<u64> {
    let mut value = 0u64;
    for shift in (0..bits).step_by(7) {
        let byte = *bytes
            .get(*offset)
            .ok_or(Error::Invalid("truncated manifest page"))?;
        *offset += 1;
        let part = u64::from(byte & 0x7f);
        if part > (u64::MAX >> (64 - bits + shift)) || (shift != 0 && byte == 0) {
            return Err(Error::Invalid("manifest varint"));
        }
        value |= part << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(Error::Invalid("manifest varint"))
}

fn read_manifest_bool(bytes: &[u8], offset: &mut usize) -> Result<u8> {
    let value = *bytes
        .get(*offset)
        .ok_or(Error::Invalid("truncated manifest page"))?;
    *offset += 1;
    Ok(value)
}

fn read_manifest_bytes<'a>(bytes: &'a [u8], offset: &mut usize, length: usize) -> Result<&'a [u8]> {
    let end = (*offset)
        .checked_add(length)
        .ok_or(Error::Invalid("manifest field length"))?;
    let slice = bytes
        .get(*offset..end)
        .ok_or(Error::Invalid("truncated manifest page"))?;
    *offset = end;
    Ok(slice)
}

#[cfg(test)]
mod manifest_preflight_tests {
    use super::*;
    use proptest::prelude::*;

    fn page(path: &str, size: u64) -> FileManifest {
        FileManifest {
            transfer_id: "transfer".into(),
            clip_id: "clip".into(),
            chunk_size: 2,
            page: 0,
            final_page: true,
            files: ManifestFiles::try_from_vec(vec![FileManifestEntry {
                file_id: 0,
                relative_path: path.into(),
                size,
                is_dir: false,
                blake3_hash: Some([7; 32]),
            }])
            .expect("one manifest entry"),
        }
    }

    fn encoded(page: &FileManifest) -> Vec<u8> {
        codec::encode_transfer(&TransferMessage::FileManifest(page.clone())).expect("page encode")
    }

    #[test]
    fn manifest_preflight_accepts_canonical_pages_and_rejects_configured_limits() {
        let bytes = encoded(&page("assets/readme.txt", 3));
        let config = Config::default();
        preflight_manifest_frame(&bytes, 4096, &config).expect("valid page");

        let mut limited = config.clone();
        limited.max_path_bytes = 8;
        assert!(matches!(
            preflight_manifest_frame(&bytes, 4096, &limited),
            Err(Error::Limit("manifest path length"))
        ));

        let mut limited = config.clone();
        limited.max_depth = 1;
        assert!(matches!(
            preflight_manifest_frame(&bytes, 4096, &limited),
            Err(Error::Limit("path length/depth"))
        ));

        let mut limited = config.clone();
        limited.max_file_bytes = 2;
        assert!(matches!(
            preflight_manifest_frame(&bytes, 4096, &limited),
            Err(Error::Limit("manifest file size"))
        ));

        let mut limited = config.clone();
        limited.max_chunks_per_file = 1;
        assert!(matches!(
            preflight_manifest_frame(&bytes, 4096, &limited),
            Err(Error::Limit("manifest chunk count"))
        ));

        let reserved_name = encoded(&page("assets/CON", 0));
        assert!(matches!(
            preflight_manifest_frame(&reserved_name, 4096, &config),
            Err(Error::Invalid("reserved name"))
        ));
    }

    #[tokio::test]
    async fn resume_preflight_rejects_unbounded_fields_and_truncated_frames() {
        let resume = FileResume {
            transfer_id: "transfer".into(),
            file_id: 0,
            missing_chunks: ResumeRanges::try_from_vec(vec![ChunkRange {
                first_chunk: 3,
                count: 2,
            }])
            .expect("one range"),
        };
        let canonical = codec::encode_transfer(&TransferMessage::FileResume(resume.clone()))
            .expect("resume encode");
        let framed = |bytes: &[u8]| {
            let mut frame = (bytes.len() as u32).to_be_bytes().to_vec();
            frame.extend_from_slice(bytes);
            frame
        };
        let frame = framed(&canonical);
        let decoded = read_resume_message(
            &mut frame.as_slice(),
            Duration::from_secs(1),
            &Cancel::new(),
        )
        .await
        .expect("canonical resume");
        assert_eq!(decoded, resume);

        let too_many_ranges = vec![2, 1, b't', 0, 0x81, 0x20]; // 4097 ranges
                                                               // The owned decoder must never attempt to reserve the advertised count.
        for bytes in [vec![2, 0x81, 1], too_many_ranges] {
            let frame = framed(&bytes);
            assert!(matches!(
                read_resume_message(
                    &mut frame.as_slice(),
                    Duration::from_secs(1),
                    &Cancel::new()
                )
                .await,
                Err(Error::Limit(_))
            ));
        }
        let huge = u32::MAX.to_be_bytes();
        assert!(matches!(
            read_resume_message(&mut huge.as_slice(), Duration::from_secs(1), &Cancel::new()).await,
            Err(Error::Limit("resume frame size"))
        ));
        for length in 0..frame.len() {
            assert!(read_resume_message(
                &mut &frame[..length],
                Duration::from_secs(1),
                &Cancel::new()
            )
            .await
            .is_err());
        }
    }

    proptest! {
        #[test]
        fn manifest_preflight_never_panics_on_arbitrary_frame_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
            let config = Config::default();
            let result = std::panic::catch_unwind(|| preflight_manifest_frame(&bytes, MAX_MANIFEST_FILES, &config));
            prop_assert!(result.is_ok());
        }
    }
}

async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    first: Option<u8>,
    max_frame_bytes: usize,
    expected_variant: Option<u8>,
) -> Result<TransferMessage> {
    let length = if let Some(first) = first {
        let mut prefix = [first, 0, 0, 0];
        reader.read_exact(&mut prefix[1..]).await?;
        u32::from_be_bytes(prefix)
    } else {
        reader.read_u32().await?
    } as usize;
    if length == 0 || length > max_frame_bytes {
        return Err(Error::Limit("frame size"));
    }
    // The discriminant is one byte for all current TransferMessage variants.
    // Check the chunk-specific cap before reserving the rest of the frame.
    let variant = reader.read_u8().await?;
    if variant > 4
        || (variant == 1 && length > MAX_FILE_CHUNK_FRAME_BYTES)
        || expected_variant.is_some_and(|expected| variant != expected)
    {
        return Err(Error::Invalid("frame variant/length"));
    }
    let mut bytes = vec![0; length];
    bytes[0] = variant;
    reader.read_exact(&mut bytes[1..]).await?;
    // Large postcard decodes must not occupy the input event runtime.
    tokio::task::spawn_blocking(move || codec::decode_transfer(&bytes))
        .await
        .map_err(|_| Error::Worker)?
        .map_err(Error::from)
}

pub async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    message: &TransferMessage,
    timeout: Duration,
    cancel: &Cancel,
) -> Result<()> {
    let bytes = codec::encode_transfer(message)?;
    cancel
        .run(timeout, async {
            writer.write_u32(bytes.len() as u32).await?;
            writer.write_all(&bytes).await?;
            writer.flush().await?;
            Ok(())
        })
        .await
}

/// Write a borrowed chunk as its frame prefix, payload and trailer.
///
/// The payload and transfer ID are written from the supplied view without an
/// intermediate `FileChunk` or encoded payload buffer.
pub async fn write_file_chunk<W: AsyncWrite + Unpin>(
    writer: &mut W,
    chunk: &FileChunkView<'_>,
    timeout: Duration,
    cancel: &Cancel,
) -> Result<()> {
    let mut prefix = [0u8; CHUNK_HEADER_CAPACITY];
    let prefix = codec::encode_file_chunk_prefix_into(chunk, &mut prefix)?;
    let mut trailer = [0u8; 38];
    let trailer = codec::encode_file_chunk_trailer_into(chunk, &mut trailer)?;
    let frame_len = prefix
        .len()
        .checked_add(chunk.data.len())
        .and_then(|length| length.checked_add(trailer.len()))
        .ok_or(Error::Limit("chunk frame size"))?;
    if frame_len > MAX_FILE_CHUNK_FRAME_BYTES || frame_len > u32::MAX as usize {
        return Err(Error::Limit("chunk frame size"));
    }
    let length = (frame_len as u32).to_be_bytes();
    cancel
        .run(timeout, async {
            writer.write_all(&length).await?;
            writer.write_all(prefix).await?;
            writer.write_all(chunk.data).await?;
            writer.write_all(trailer).await?;
            writer.flush().await?;
            Ok(())
        })
        .await
}

/// The small control messages permitted on a chunk lane.
#[derive(Debug, PartialEq, Eq)]
pub enum ChunkMessage<'a> {
    Chunk(FileChunkView<'a>),
    Complete(FileComplete),
    Cancel(FileCancel),
}

enum ChunkControl {
    Complete(FileComplete),
    Cancel(FileCancel),
}

/// Reads one chunk-lane frame into a bounded, reusable buffer.
///
/// Chunk headers are accumulated in a fixed stack buffer. Only after the
/// advertised data and frame lengths pass the configured limits is the frame
/// buffer grown and the payload read into it.
pub struct ChunkReader {
    frame: Vec<u8>,
    max_chunk_bytes: usize,
    max_frame_bytes: usize,
    last_was_chunk: bool,
}

impl ChunkReader {
    /// Configure the data limit; the frame limit includes the protocol's
    /// maximum fixed header and trailer overhead.
    pub fn new(max_chunk_bytes: usize) -> Result<Self> {
        let max_frame_bytes = max_chunk_bytes
            .checked_add(CHUNK_FRAME_OVERHEAD)
            .ok_or(Error::Limit("chunk frame size"))?;
        Self::with_limits(max_chunk_bytes, max_frame_bytes)
    }

    /// Configure both chunk data and total frame limits.
    pub fn with_limits(max_chunk_bytes: usize, max_frame_bytes: usize) -> Result<Self> {
        if max_chunk_bytes == 0
            || max_chunk_bytes > MAX_FILE_CHUNK_BYTES
            || max_frame_bytes == 0
            || max_frame_bytes > MAX_FILE_CHUNK_FRAME_BYTES
        {
            return Err(Error::Limit("chunk reader limits"));
        }
        Ok(Self {
            frame: Vec::new(),
            max_chunk_bytes,
            max_frame_bytes,
            last_was_chunk: false,
        })
    }

    /// Read a chunk or one of the two bounded control messages accepted on a
    /// chunk lane. The returned chunk borrows this reader until its next read.
    pub async fn read<'a, R: AsyncRead + Unpin>(
        &'a mut self,
        reader: &mut R,
        timeout: Duration,
        cancel: &Cancel,
    ) -> Result<ChunkMessage<'a>> {
        let control = cancel.run(timeout, self.read_frame(reader)).await?;
        if let Some(control) = control {
            return Ok(match control {
                ChunkControl::Complete(done) => ChunkMessage::Complete(done),
                ChunkControl::Cancel(done) => ChunkMessage::Cancel(done),
            });
        }
        Ok(ChunkMessage::Chunk(self.chunk()?))
    }

    /// Reborrow the last successfully read chunk after the reader itself has
    /// crossed a task boundary. Returns an error when the last frame was a
    /// control message or no complete chunk has been read.
    pub fn chunk(&self) -> Result<FileChunkView<'_>> {
        if !self.last_was_chunk {
            return Err(Error::Invalid("no retained chunk frame"));
        }
        codec::decode_file_chunk_view(&self.frame).map_err(Error::from)
    }

    /// Current reusable frame capacity in bytes.
    pub fn buffer_capacity(&self) -> usize {
        self.frame.capacity()
    }

    async fn read_frame<R: AsyncRead + Unpin>(
        &mut self,
        reader: &mut R,
    ) -> Result<Option<ChunkControl>> {
        self.last_was_chunk = false;
        let frame_len = reader.read_u32().await? as usize;
        if frame_len == 0 || frame_len > self.max_frame_bytes {
            return Err(Error::Limit("chunk lane frame size"));
        }

        let variant = reader.read_u8().await?;
        match variant {
            1 => {
                self.read_chunk(reader, frame_len, variant).await?;
                Ok(None)
            }
            3 | 4 => {
                if frame_len > CHUNK_CONTROL_FRAME_CAPACITY {
                    return Err(Error::Limit("chunk lane control frame size"));
                }
                let mut frame = [0u8; CHUNK_CONTROL_FRAME_CAPACITY];
                frame[0] = variant;
                reader.read_exact(&mut frame[1..frame_len]).await?;
                match codec::decode_transfer(&frame[..frame_len])? {
                    TransferMessage::FileComplete(done) if variant == 3 => {
                        Ok(Some(ChunkControl::Complete(done)))
                    }
                    TransferMessage::FileCancel(cancel) if variant == 4 => {
                        Ok(Some(ChunkControl::Cancel(cancel)))
                    }
                    _ => Err(Error::Invalid("unexpected chunk-lane control message")),
                }
            }
            _ => Err(Error::Invalid("unexpected chunk-lane frame variant")),
        }
    }

    async fn read_chunk<R: AsyncRead + Unpin>(
        &mut self,
        reader: &mut R,
        frame_len: usize,
        variant: u8,
    ) -> Result<()> {
        if frame_len > self.max_frame_bytes {
            return Err(Error::Limit("chunk lane frame size"));
        }
        let mut header = [0u8; CHUNK_HEADER_CAPACITY];
        header[0] = variant;
        let mut header_len = 1;
        let (chunk_header, prefix_len) = loop {
            if let Some((header, prefix_len)) =
                codec::decode_file_chunk_header(&header[..header_len])?
            {
                break (header, prefix_len);
            }
            if header_len == header.len() {
                return Err(Error::Limit("chunk header size"));
            }
            if header_len >= frame_len {
                return Err(Error::Invalid("incomplete chunk header"));
            }
            reader
                .read_exact(&mut header[header_len..header_len + 1])
                .await?;
            header_len += 1;
        };
        if chunk_header.data_len > self.max_chunk_bytes {
            return Err(Error::Limit("chunk data size"));
        }
        let payload_end = prefix_len
            .checked_add(chunk_header.data_len)
            .ok_or(Error::Limit("chunk frame size"))?;
        let trailer_len = frame_len
            .checked_sub(payload_end)
            .ok_or(Error::Invalid("chunk frame length"))?;
        if !(34..=38).contains(&trailer_len) {
            return Err(Error::Invalid("chunk trailer length"));
        }

        self.frame.clear();
        self.frame
            .try_reserve_exact(frame_len)
            .map_err(|_| Error::Limit("chunk frame allocation"))?;
        self.frame.extend_from_slice(&header[..prefix_len]);
        while self.frame.len() < frame_len {
            let remaining = frame_len - self.frame.len();
            let count = (&mut *reader)
                .take(remaining as u64)
                .read_buf(&mut self.frame)
                .await?;
            if count == 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
            }
        }
        // Validate trailer fields and chunk invariants before exposing the view.
        codec::decode_file_chunk_view(&self.frame)?;
        self.last_was_chunk = true;
        Ok(())
    }
}

pub fn known_compressed(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|s| s.to_str())
        .is_some_and(|s| {
            [
                "mp4", "mov", "mkv", "webm", "avi", "mp3", "aac", "ogg", "flac", "jpg", "jpeg",
                "png", "gif", "webp", "heic", "avif", "zip", "7z", "rar", "gz", "bz2", "xz", "zst",
                "pdf", "woff", "woff2",
            ]
            .iter()
            .any(|extension| s.eq_ignore_ascii_case(extension))
        })
}

/// Reuses raw/compressed storage and a level-1 zstd context. Call recycle after send.
/// Hash/compression work should run on a blocking worker, outside input event tasks.
pub struct ChunkEncoder {
    raw: Vec<u8>,
    packed: Vec<u8>,
    compressor: zstd::bulk::Compressor<'static>,
    chunk_size: usize,
    generation: u64,
    generation_exhausted: bool,
}

/// Copyable metadata describing the most recently prepared encoder buffer.
/// Its fields are private so views can only refer to encoder-owned bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreparedChunk {
    generation: u64,
    data_len: usize,
    uncompressed_size: u32,
    compressed: bool,
    blake3_hash: [u8; 32],
}

impl ChunkEncoder {
    pub fn new(config: &Config) -> Result<Self> {
        config.validate()?;
        let mut compressor = zstd::bulk::Compressor::new(1)?;
        compressor.window_log(22)?;
        Ok(Self {
            raw: vec![0; config.chunk_size],
            packed: vec![0; config.chunk_size],
            compressor,
            chunk_size: config.chunk_size,
            generation: 0,
            generation_exhausted: false,
        })
    }

    pub fn buffer_mut(&mut self, size: usize) -> Result<&mut [u8]> {
        if size > self.chunk_size {
            return Err(Error::Limit("chunk size"));
        }
        let generation = self.next_generation()?;
        self.raw.resize(size, 0);
        self.generation = generation;
        Ok(&mut self.raw)
    }

    pub fn encode(
        &mut self,
        transfer_id: &str,
        file_id: u32,
        index: u32,
        name: &str,
    ) -> Result<FileChunk> {
        let prepared = self.prepare(name)?;
        let data = if prepared.compressed {
            self.packed.truncate(prepared.data_len);
            std::mem::take(&mut self.packed)
        } else {
            std::mem::take(&mut self.raw)
        };
        Ok(FileChunk {
            transfer_id: transfer_id.to_owned(),
            file_id,
            chunk_index: index,
            offset: u64::from(index) * self.chunk_size as u64,
            data: ChunkBytes::try_from_vec(data).map_err(|_| Error::Limit("chunk payload"))?,
            uncompressed_size: prepared.uncompressed_size,
            compressed: prepared.compressed,
            blake3_hash: prepared.blake3_hash,
        })
    }

    /// Hash and optionally compress the current raw buffer without moving or
    /// copying either encoder buffer.
    pub fn prepare(&mut self, name: &str) -> Result<PreparedChunk> {
        let generation = self.next_generation()?;
        let size = self.raw.len();
        let size_u32 = u32::try_from(size).map_err(|_| Error::Limit("chunk size"))?;
        let digest = *blake3::hash(&self.raw).as_bytes();
        let mut compressed = false;
        let mut length = 0;
        if size >= 1024 && !known_compressed(name) {
            let sample_size = size.min(64 * 1024);
            let sample = self
                .compressor
                .compress_to_buffer(&self.raw[..sample_size], self.packed.as_mut_slice());
            if sample.is_ok_and(|sample| sample * 100 <= sample_size * 90) {
                // A later chunk may be incompressible even when its sample was compressible.
                if let Ok(count) = self
                    .compressor
                    .compress_to_buffer(&self.raw, self.packed.as_mut_slice())
                {
                    if count * 100 <= size * 90 {
                        compressed = true;
                        length = count;
                    }
                }
            }
        }
        self.generation = generation;
        Ok(PreparedChunk {
            generation: self.generation,
            data_len: if compressed { length } else { size },
            uncompressed_size: size_u32,
            compressed,
            blake3_hash: digest,
        })
    }

    /// Borrow the prepared bytes with transfer metadata, without allocating a
    /// transfer-ID string or taking ownership of the payload.
    pub fn view<'a>(
        &'a self,
        transfer_id: &'a str,
        file_id: u32,
        index: u32,
        prepared: PreparedChunk,
    ) -> Result<FileChunkView<'a>> {
        if self.generation_exhausted
            || prepared.generation != self.generation
            || prepared.data_len > self.chunk_size
            || prepared.uncompressed_size as usize > self.chunk_size
        {
            return Err(Error::Invalid("stale or invalid prepared chunk"));
        }
        let data = if prepared.compressed {
            self.packed
                .get(..prepared.data_len)
                .ok_or(Error::Invalid("prepared payload length"))?
        } else {
            self.raw
                .get(..prepared.data_len)
                .ok_or(Error::Invalid("prepared payload length"))?
        };
        Ok(FileChunkView {
            transfer_id,
            file_id,
            chunk_index: index,
            offset: u64::from(index) * self.chunk_size as u64,
            data,
            uncompressed_size: prepared.uncompressed_size,
            compressed: prepared.compressed,
            blake3_hash: prepared.blake3_hash,
        })
    }

    pub fn recycle(&mut self, chunk: FileChunk) {
        if let Some(generation) = self.generation.checked_add(1) {
            self.generation = generation;
        } else {
            // The legacy recycle API cannot return an overflow error. Poison
            // future views/preparations rather than risk stale metadata aliasing.
            self.generation_exhausted = true;
        }
        if chunk.compressed {
            self.packed = chunk.data.into_vec();
            self.packed.resize(self.chunk_size, 0);
        } else {
            self.raw = chunk.data.into_vec();
        }
    }

    pub fn buffer_capacity(&self) -> usize {
        self.raw.capacity() + self.packed.capacity()
    }

    fn next_generation(&self) -> Result<u64> {
        if self.generation_exhausted {
            return Err(Error::Limit("chunk generation"));
        }
        self.generation
            .checked_add(1)
            .ok_or(Error::Limit("chunk generation"))
    }
}
