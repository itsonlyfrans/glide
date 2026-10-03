//! Size-bounded wire and JSONL codecs.

use crate::ipc::Request;
use crate::wire::{
    self, ClipboardMessage, ControlMessage, InputMessage, Move, TransferMessage, WireMessage,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::io::{self, Write};
use thiserror::Error;

mod file_chunk;
pub use file_chunk::{
    decode_file_chunk_header, decode_file_chunk_view, encode_file_chunk_into,
    encode_file_chunk_prefix_into, encode_file_chunk_trailer_into, FileChunkHeader, FileChunkView,
};

pub const MAX_IPC_LINE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum CodecError {
    #[error("frame exceeds the {limit} byte limit ({actual} bytes)")]
    FrameTooLarge { limit: usize, actual: usize },
    #[error("invalid JSON Lines frame")]
    InvalidJsonLine,
    #[error("invalid protocol value: {0}")]
    InvalidValue(&'static str),
    #[error("JSON codec failed: {0}")]
    Json(String),
    #[error("postcard codec failed: {0}")]
    Postcard(String),
}

pub fn decode_jsonl_request(bytes: &[u8]) -> Result<Request, CodecError> {
    check_size(bytes, MAX_IPC_LINE_BYTES)?;
    let line = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.iter().any(|byte| *byte == b'\n' || *byte == b'\r') || line.is_empty() {
        return Err(CodecError::InvalidJsonLine);
    }
    serde_json::from_slice(line).map_err(|error| CodecError::Json(error.to_string()))
}

pub fn encode_jsonl<T: Serialize>(value: &T) -> Result<Vec<u8>, CodecError> {
    let mut writer = BoundedWriter {
        bytes: Vec::new(),
        limit: MAX_IPC_LINE_BYTES - 1,
    };
    serde_json::to_writer(&mut writer, value)
        .map_err(|error| CodecError::Json(error.to_string()))?;
    writer.bytes.push(b'\n');
    Ok(writer.bytes)
}

pub fn encode_control(message: &ControlMessage) -> Result<Vec<u8>, CodecError> {
    validate_control(message)?;
    encode_postcard(message, wire::MAX_CONTROL_FRAME_BYTES)
}

/// Encodes validated control data into caller-owned storage without allocation.
pub fn encode_control_into<'a>(
    message: &ControlMessage,
    out: &'a mut [u8],
) -> Result<&'a [u8], CodecError> {
    validate_control(message)?;
    encode_postcard_into(message, out, wire::MAX_CONTROL_FRAME_BYTES)
}

pub fn decode_control(bytes: &[u8]) -> Result<ControlMessage, CodecError> {
    let message = decode_postcard(bytes, wire::MAX_CONTROL_FRAME_BYTES)?;
    validate_control(&message)?;
    Ok(message)
}

pub fn encode_input(message: &InputMessage) -> Result<Vec<u8>, CodecError> {
    validate_input(message)?;
    encode_postcard(message, wire::MAX_INPUT_FRAME_BYTES)
}

/// Encodes validated reliable input into caller-owned storage without allocation.
pub fn encode_input_into<'a>(
    message: &InputMessage,
    out: &'a mut [u8],
) -> Result<&'a [u8], CodecError> {
    validate_input(message)?;
    encode_postcard_into(message, out, wire::MAX_INPUT_FRAME_BYTES)
}

pub fn decode_input(bytes: &[u8]) -> Result<InputMessage, CodecError> {
    let message = decode_postcard(bytes, wire::MAX_INPUT_FRAME_BYTES)?;
    validate_input(&message)?;
    Ok(message)
}

pub fn encode_move(message: &Move) -> Result<Vec<u8>, CodecError> {
    validate_finite(message.x, "move.x")?;
    validate_finite(message.y, "move.y")?;
    encode_postcard(message, wire::MAX_MOVE_FRAME_BYTES)
}

/// Encodes a pointer datagram into caller-owned fixed storage without allocating.
///
/// The returned slice borrows the supplied maximum-sized buffer and is valid until
/// that buffer is next mutably borrowed.
pub fn encode_move_into<'a>(
    message: &Move,
    buffer: &'a mut [u8; wire::MAX_MOVE_FRAME_BYTES],
) -> Result<&'a [u8], CodecError> {
    validate_finite(message.x, "move.x")?;
    validate_finite(message.y, "move.y")?;
    let encoded = postcard::to_slice(message, buffer.as_mut_slice())
        .map_err(|error| CodecError::Postcard(error.to_string()))?;
    Ok(encoded)
}

pub fn decode_move(bytes: &[u8]) -> Result<Move, CodecError> {
    let message: Move = decode_postcard(bytes, wire::MAX_MOVE_FRAME_BYTES)?;
    validate_finite(message.x, "move.x")?;
    validate_finite(message.y, "move.y")?;
    Ok(message)
}

pub fn encode_clipboard(message: &ClipboardMessage) -> Result<Vec<u8>, CodecError> {
    let limit = match message {
        ClipboardMessage::ClipAnnounce(_) => wire::MAX_CLIP_ANNOUNCE_FRAME_BYTES,
        ClipboardMessage::ClipFetch(_) | ClipboardMessage::ClipFailure(_) => {
            wire::MAX_INPUT_FRAME_BYTES
        }
        ClipboardMessage::ClipData(_) => wire::MAX_CLIP_DATA_FRAME_BYTES,
    };
    encode_postcard(message, limit)
}

pub fn decode_clipboard(bytes: &[u8]) -> Result<ClipboardMessage, CodecError> {
    let mut offset = 0;
    let limit = match read_variant(bytes, &mut offset) {
        Some(0) => wire::MAX_CLIP_ANNOUNCE_FRAME_BYTES,
        Some(1 | 3) => wire::MAX_INPUT_FRAME_BYTES,
        Some(2) => wire::MAX_CLIP_DATA_FRAME_BYTES,
        _ => wire::MAX_CLIP_DATA_FRAME_BYTES,
    };
    decode_postcard(bytes, limit)
}

pub fn encode_transfer(message: &TransferMessage) -> Result<Vec<u8>, CodecError> {
    validate_transfer(message)?;
    let limit = match message {
        TransferMessage::FileChunk(_) => wire::MAX_FILE_CHUNK_FRAME_BYTES,
        _ => wire::MAX_FILE_CONTROL_FRAME_BYTES,
    };
    encode_postcard(message, limit)
}

pub fn decode_transfer(bytes: &[u8]) -> Result<TransferMessage, CodecError> {
    let mut offset = 0;
    if read_variant(bytes, &mut offset) == Some(1) {
        // The borrowed seam validates every canonical header/trailer field before
        // allocation. Payload bytes need no second serde/canonical traversal.
        return Ok(TransferMessage::FileChunk(file_chunk::into_owned(
            decode_file_chunk_view(bytes)?,
        )?));
    }
    let message = decode_postcard(bytes, wire::MAX_FILE_CONTROL_FRAME_BYTES)?;
    validate_transfer(&message)?;
    Ok(message)
}

pub fn encode_wire(message: &WireMessage) -> Result<Vec<u8>, CodecError> {
    match message {
        WireMessage::Control(message) => validate_control(message)?,
        WireMessage::Input(message) => validate_input(message)?,
        WireMessage::Transfer(message) => validate_transfer(message)?,
        WireMessage::Clipboard(_) => {}
    }
    encode_postcard(message, wire_limit(message))
}

pub fn decode_wire(bytes: &[u8]) -> Result<WireMessage, CodecError> {
    let limit = wire_limit_from_prefix(bytes).unwrap_or(wire::MAX_WIRE_FRAME_BYTES);
    let mut offset = 0;
    if read_variant(bytes, &mut offset) == Some(3) {
        let transfer_start = offset;
        if read_variant(bytes, &mut offset) == Some(1) {
            if transfer_start != 1 {
                return Err(CodecError::InvalidValue("noncanonical protocol bytes"));
            }
            return Ok(WireMessage::Transfer(TransferMessage::FileChunk(
                file_chunk::into_owned(decode_file_chunk_view(&bytes[transfer_start..])?)?,
            )));
        }
    }
    let message: WireMessage = decode_postcard(bytes, limit)?;
    match &message {
        WireMessage::Control(message) => validate_control(message)?,
        WireMessage::Input(message) => validate_input(message)?,
        WireMessage::Transfer(message) => validate_transfer(message)?,
        WireMessage::Clipboard(_) => {}
    }
    Ok(message)
}

fn wire_limit(message: &WireMessage) -> usize {
    match message {
        WireMessage::Control(_) => wire::MAX_CONTROL_FRAME_BYTES,
        WireMessage::Input(_) => wire::MAX_INPUT_FRAME_BYTES,
        WireMessage::Clipboard(message) => match message {
            ClipboardMessage::ClipAnnounce(_) => wire::MAX_CLIP_ANNOUNCE_FRAME_BYTES,
            ClipboardMessage::ClipFetch(_) | ClipboardMessage::ClipFailure(_) => {
                wire::MAX_INPUT_FRAME_BYTES
            }
            ClipboardMessage::ClipData(_) => wire::MAX_CLIP_DATA_FRAME_BYTES,
        },
        WireMessage::Transfer(message) => match message {
            TransferMessage::FileChunk(_) => wire::MAX_FILE_CHUNK_FRAME_BYTES,
            _ => wire::MAX_FILE_CONTROL_FRAME_BYTES,
        },
    }
}

fn wire_limit_from_prefix(bytes: &[u8]) -> Option<usize> {
    let mut offset = 0;
    let lane = read_variant(bytes, &mut offset)?;
    let message = read_variant(bytes, &mut offset)?;
    Some(match (lane, message) {
        (0, _) => wire::MAX_CONTROL_FRAME_BYTES,
        (1, _) => wire::MAX_INPUT_FRAME_BYTES,
        (2, 0) => wire::MAX_CLIP_ANNOUNCE_FRAME_BYTES,
        (2, 1 | 3) => wire::MAX_INPUT_FRAME_BYTES,
        (2, 2) => wire::MAX_CLIP_DATA_FRAME_BYTES,
        (3, 1) => wire::MAX_FILE_CHUNK_FRAME_BYTES,
        (3, _) => wire::MAX_FILE_CONTROL_FRAME_BYTES,
        _ => wire::MAX_WIRE_FRAME_BYTES,
    })
}

fn read_variant(bytes: &[u8], offset: &mut usize) -> Option<u32> {
    let mut value = 0u32;
    for shift in (0..35).step_by(7) {
        let byte = *bytes.get(*offset)?;
        *offset += 1;
        let bits = u32::from(byte & 0x7f);
        if shift == 28 && bits > 0x0f {
            return None;
        }
        value |= bits << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

fn validate_control(message: &ControlMessage) -> Result<(), CodecError> {
    match message {
        ControlMessage::Hello(hello) => {
            if hello.proto_version != wire::PROTOCOL_VERSION {
                return Err(CodecError::InvalidValue("hello.proto_version"));
            }
            validate_metadata(&hello.name, &hello.monitors)?;
            for monitor in hello.monitors.iter() {
                for (value, field) in [
                    (monitor.x, "hello.monitor.x"),
                    (monitor.y, "hello.monitor.y"),
                    (monitor.w, "hello.monitor.w"),
                    (monitor.h, "hello.monitor.h"),
                    (monitor.scale, "hello.monitor.scale"),
                ] {
                    validate_finite(value, field)?;
                }
                if monitor.w <= 0.0 || monitor.h <= 0.0 || monitor.scale <= 0.0 {
                    return Err(CodecError::InvalidValue("hello.monitor dimensions"));
                }
            }
        }
        ControlMessage::LayoutUpdate(update) => {
            for device in update.devices.iter() {
                validate_finite(device.x, "layout_update.device.x")?;
                validate_finite(device.y, "layout_update.device.y")?;
            }
        }
        ControlMessage::TakeOver(take_over) => validate_point(take_over.pos, "take_over.pos")?,
        ControlMessage::Enter(enter) => {
            validate_epoch(enter.epoch)?;
            validate_point(enter.pos, "enter.pos")?;
        }
        ControlMessage::Leave(leave) => validate_epoch(leave.epoch)?,
        ControlMessage::EnterAck { epoch } => validate_epoch(*epoch)?,
        ControlMessage::MetadataUpdate(update) => {
            if update.version != wire::PROTOCOL_VERSION {
                return Err(CodecError::InvalidValue("metadata.version"));
            }
            validate_metadata(&update.name, &update.monitors)?;
        }
        ControlMessage::Details(details) => {
            let text_ok = |text: &str, max: usize| {
                !text.trim().is_empty() && text.len() <= max && !text.chars().any(char::is_control)
            };
            if !text_ok(&details.model, 96)
                || !text_ok(&details.kind, 16)
                || details
                    .builtin_monitor
                    .as_deref()
                    .is_some_and(|id| !text_ok(id, 64))
            {
                return Err(CodecError::InvalidValue("details"));
            }
        }
        ControlMessage::Arrange(arrange) => {
            if arrange.screens.len() > 32 {
                return Err(CodecError::InvalidValue("arrange.screens"));
            }
            for screen in &arrange.screens {
                validate_finite(screen.x, "arrange.x")?;
                validate_finite(screen.y, "arrange.y")?;
                if screen.monitor_id.is_empty()
                    || screen.monitor_id.len() > 64
                    || screen.x.abs() > 1.0e6
                    || screen.y.abs() > 1.0e6
                {
                    return Err(CodecError::InvalidValue("arrange.screen"));
                }
            }
        }
        ControlMessage::Heartbeat(_)
        | ControlMessage::HeartbeatAck(_)
        | ControlMessage::Bye(_)
        | ControlMessage::Unpaired => {}
    }
    Ok(())
}

fn validate_input(message: &InputMessage) -> Result<(), CodecError> {
    validate_epoch(message.epoch())?;
    if let InputMessage::Wheel(wheel) = message {
        validate_finite(wheel.dx, "wheel.dx")?;
        validate_finite(wheel.dy, "wheel.dy")?;
    }
    Ok(())
}

fn validate_epoch(epoch: u64) -> Result<(), CodecError> {
    if epoch == 0 {
        Err(CodecError::InvalidValue("input.epoch"))
    } else {
        Ok(())
    }
}

fn validate_metadata(name: &str, monitors: &wire::Monitors) -> Result<(), CodecError> {
    if name.trim().is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
        return Err(CodecError::InvalidValue("metadata.name"));
    }
    for monitor in monitors.iter() {
        for value in [monitor.x, monitor.y, monitor.w, monitor.h, monitor.scale] {
            validate_finite(value, "metadata.monitor")?;
        }
        if monitor.w <= 0.0 || monitor.h <= 0.0 || monitor.scale <= 0.0 {
            return Err(CodecError::InvalidValue("metadata.monitor dimensions"));
        }
    }
    Ok(())
}

fn validate_transfer(message: &TransferMessage) -> Result<(), CodecError> {
    match message {
        TransferMessage::FileManifest(manifest) => {
            crate::manifest::validate_manifest_page(manifest)?;
        }
        TransferMessage::FileChunk(chunk) => {
            file_chunk::validate(&FileChunkView {
                transfer_id: &chunk.transfer_id,
                file_id: chunk.file_id,
                chunk_index: chunk.chunk_index,
                offset: chunk.offset,
                data: &chunk.data,
                uncompressed_size: chunk.uncompressed_size,
                compressed: chunk.compressed,
                blake3_hash: chunk.blake3_hash,
            })?;
        }
        TransferMessage::FileResume(resume) => {
            for range in resume.missing_chunks.iter() {
                if range.count == 0 || range.first_chunk.checked_add(range.count).is_none() {
                    return Err(CodecError::InvalidValue("file_resume.missing_chunks"));
                }
            }
        }
        TransferMessage::FileComplete(_) | TransferMessage::FileCancel(_) => {}
    }
    Ok(())
}

fn validate_point(point: glide_platform::Point, field: &'static str) -> Result<(), CodecError> {
    validate_finite(point.x, field)?;
    validate_finite(point.y, field)
}

fn validate_finite(value: f64, field: &'static str) -> Result<(), CodecError> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(CodecError::InvalidValue(field))
    }
}

fn check_size(bytes: &[u8], limit: usize) -> Result<(), CodecError> {
    if bytes.len() > limit {
        Err(CodecError::FrameTooLarge {
            limit,
            actual: bytes.len(),
        })
    } else {
        Ok(())
    }
}

fn encode_postcard<T: Serialize>(value: &T, limit: usize) -> Result<Vec<u8>, CodecError> {
    let bytes =
        postcard::to_allocvec(value).map_err(|error| CodecError::Postcard(error.to_string()))?;
    check_size(&bytes, limit)?;
    Ok(bytes)
}

fn encode_postcard_into<'a, T: Serialize>(
    value: &T,
    out: &'a mut [u8],
    limit: usize,
) -> Result<&'a [u8], CodecError> {
    let length = out.len().min(limit);
    postcard::to_slice(value, &mut out[..length])
        .map(|bytes| &*bytes)
        .map_err(|error| CodecError::Postcard(error.to_string()))
}

fn decode_postcard<T: DeserializeOwned + Serialize>(
    bytes: &[u8],
    limit: usize,
) -> Result<T, CodecError> {
    check_size(bytes, limit)?;
    let (value, remaining) = postcard::take_from_bytes::<T>(bytes)
        .map_err(|error| CodecError::Postcard(error.to_string()))?;
    if !remaining.is_empty() {
        return Err(CodecError::InvalidValue("trailing protocol bytes"));
    }
    // Postcard accepts overlong varints. Compare canonical serialization directly
    // against the input without allocating a second frame (including bulk data).
    postcard::serialize_with_flavor(&value, Canonical(bytes))
        .map_err(|_| CodecError::InvalidValue("noncanonical protocol bytes"))?;
    Ok(value)
}

struct Canonical<'a>(&'a [u8]);

impl postcard::ser_flavors::Flavor for Canonical<'_> {
    type Output = ();

    fn try_push(&mut self, byte: u8) -> postcard::Result<()> {
        let Some((&expected, remaining)) = self.0.split_first() else {
            return Err(postcard::Error::SerializeBufferFull);
        };
        if expected != byte {
            return Err(postcard::Error::SerializeBufferFull);
        }
        self.0 = remaining;
        Ok(())
    }

    fn try_extend(&mut self, bytes: &[u8]) -> postcard::Result<()> {
        if !self.0.starts_with(bytes) {
            return Err(postcard::Error::SerializeBufferFull);
        }
        self.0 = &self.0[bytes.len()..];
        Ok(())
    }

    fn finalize(self) -> postcard::Result<()> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(postcard::Error::SerializeBufferFull)
        }
    }
}

struct BoundedWriter {
    bytes: Vec<u8>,
    limit: usize,
}

impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(length) = self.bytes.len().checked_add(bytes.len()) else {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "JSONL output too large",
            ));
        };
        if length > self.limit {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "JSONL output too large",
            ));
        }
        self.bytes
            .try_reserve(bytes.len())
            .map_err(|error| io::Error::other(error.to_string()))?;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
