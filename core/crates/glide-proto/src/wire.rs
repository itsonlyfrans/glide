//! Versioned postcard messages carried by Glide's separate QUIC streams.

use glide_platform::{Button, Key, Monitor, Os, Point};
use serde::de::{SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

use crate::ipc::LayoutDevice;

pub const PROTOCOL_VERSION: u16 = 3;
pub const ALPN_PROTOCOL: &str = "glide/3";
pub const ALPN: &[u8] = ALPN_PROTOCOL.as_bytes();
pub const PAIR_ALPN: &[u8] = b"glide/pair/3";

pub const MAX_MONITORS: usize = 64;
pub const MAX_LAYOUT_DEVICES: usize = 64;
pub const MAX_MODIFIERS: usize = 32;
pub const MAX_CLIP_FORMATS: usize = 32;
pub const MAX_ANNOUNCED_FILES: usize = 4096;
pub const MAX_MANIFEST_FILES: usize = 4096;
/// Total tree size across manifest pages and Core/IPC transfer snapshots.
pub const MAX_TRANSFER_ITEMS: u32 = 1_048_576;
pub const MAX_TRANSFER_BYTES: u64 = 64 << 30;
pub const MAX_TRANSFER_ID_BYTES: usize = 128;
pub const MAX_MANIFEST_PATH_BYTES: usize = 4096;
pub const MAX_MANIFEST_DEPTH: usize = 64;
pub const MAX_RESUME_RANGES: usize = 4096;
pub const MAX_CLIPBOARD_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_FILE_CHUNK_BYTES: usize = 4 * 1024 * 1024;

pub const MAX_CONTROL_FRAME_BYTES: usize = 512 * 1024;
pub const MAX_INPUT_FRAME_BYTES: usize = 8 * 1024;
pub const MAX_MOVE_FRAME_BYTES: usize = 128;
pub const MAX_CLIP_ANNOUNCE_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_CLIP_DATA_FRAME_BYTES: usize = MAX_CLIPBOARD_PAYLOAD_BYTES + 4096;
pub const MAX_FILE_CONTROL_FRAME_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_FILE_CHUNK_FRAME_BYTES: usize = MAX_FILE_CHUNK_BYTES + 4096;
pub const MAX_WIRE_FRAME_BYTES: usize = MAX_CLIP_DATA_FRAME_BYTES;

/// A serde sequence that checks the advertised item count before growing and
/// never reserves from an untrusted postcard length hint.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct BoundedVec<T, const LIMIT: usize>(Vec<T>);

impl<T, const LIMIT: usize> BoundedVec<T, LIMIT> {
    pub fn new() -> Self {
        Self(Vec::new())
    }

    pub fn try_from_vec(items: Vec<T>) -> Result<Self, Vec<T>> {
        if items.len() <= LIMIT {
            Ok(Self(items))
        } else {
            Err(items)
        }
    }

    pub fn push(&mut self, item: T) -> Result<(), T> {
        if self.0.len() == LIMIT {
            Err(item)
        } else {
            self.0.push(item);
            Ok(())
        }
    }

    pub fn into_vec(self) -> Vec<T> {
        self.0
    }
}

impl<T, const LIMIT: usize> std::ops::Deref for BoundedVec<T, LIMIT> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T, const LIMIT: usize> std::ops::DerefMut for BoundedVec<T, LIMIT> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<'de, T, const LIMIT: usize> Deserialize<'de> for BoundedVec<T, LIMIT>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BoundedVisitor<T, const LIMIT: usize>(std::marker::PhantomData<T>);

        impl<'de, T, const LIMIT: usize> Visitor<'de> for BoundedVisitor<T, LIMIT>
        where
            T: Deserialize<'de>,
        {
            type Value = BoundedVec<T, LIMIT>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "a sequence with at most {LIMIT} items")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                if seq.size_hint().is_some_and(|length| length > LIMIT) {
                    return Err(serde::de::Error::custom("sequence exceeds protocol limit"));
                }

                let mut items = Vec::new();
                while items.len() < LIMIT {
                    match seq.next_element()? {
                        Some(item) => items.push(item),
                        None => return Ok(BoundedVec(items)),
                    }
                }

                if seq.next_element::<T>()?.is_some() {
                    return Err(serde::de::Error::custom("sequence exceeds protocol limit"));
                }
                Ok(BoundedVec(items))
            }
        }

        deserializer.deserialize_seq(BoundedVisitor::<T, LIMIT>(std::marker::PhantomData))
    }
}

pub type Monitors = BoundedVec<Monitor, MAX_MONITORS>;
pub type LayoutDevices = BoundedVec<LayoutDevice, MAX_LAYOUT_DEVICES>;
pub type ModifierKeys = BoundedVec<Key, MAX_MODIFIERS>;
pub type ClipFormats = BoundedVec<ClipFormat, MAX_CLIP_FORMATS>;
pub type AnnouncedFiles = BoundedVec<AnnouncedFile, MAX_ANNOUNCED_FILES>;
pub type ManifestFiles = BoundedVec<FileManifestEntry, MAX_MANIFEST_FILES>;
pub type ResumeRanges = BoundedVec<ChunkRange, MAX_RESUME_RANGES>;
pub type ClipboardBytes = BoundedVec<u8, MAX_CLIPBOARD_PAYLOAD_BYTES>;
pub type ChunkBytes = BoundedVec<u8, MAX_FILE_CHUNK_BYTES>;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
/// Control messages sent on the peer's bidirectional control stream.
pub enum ControlMessage {
    Hello(Hello),
    LayoutUpdate(LayoutUpdate),
    Heartbeat(Heartbeat),
    HeartbeatAck(HeartbeatAck),
    Bye(Bye),
    Unpaired,
    TakeOver(TakeOver),
    Enter(Enter),
    Leave(Leave),
    EnterAck {
        epoch: u64,
    },
    MetadataUpdate(MetadataUpdate),
    /// Since 0.2.2: what kind of computer the sender is, for its picture on the Desk.
    /// Only sent to peers whose Hello reports 0.2.2 or newer ([`understands_details`]).
    Details(DeviceDetails),
    /// Since 0.2.2: arrange the receiver's own screens, as chosen on the sender's Desk.
    /// Only sent to peers whose Hello reports 0.2.2 or newer ([`understands_details`]).
    Arrange(Arrange),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeviceDetails {
    /// For people: "MacBook Pro 16-inch", "Mac Studio", "Windows laptop".
    pub model: String,
    /// For the picture: laptop, desktop, mini, studio, imac or tower.
    pub kind: String,
    /// The laptop's own screen, when known.
    pub builtin_monitor: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Arrange {
    pub screens: Vec<ArrangedScreen>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ArrangedScreen {
    pub monitor_id: String,
    pub x: f64,
    pub y: f64,
}

/// Whether a peer's app version (from its Hello) can decode [`ControlMessage::Details`] and
/// [`ControlMessage::Arrange`]. Older versions close the connection on an unknown message.
pub fn understands_details(app_version: Option<&str>) -> bool {
    let Some(version) = app_version else {
        return false;
    };
    let mut parts = version
        .split(['.', '-', '+'])
        .map(|part| part.parse::<u64>().ok());
    match (
        parts.next().flatten(),
        parts.next().flatten(),
        parts.next().flatten(),
    ) {
        (Some(major), Some(minor), Some(patch)) => (major, minor, patch) >= (0, 2, 2),
        _ => false,
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MetadataUpdate {
    pub version: u16,
    pub name: String,
    pub monitors: Monitors,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Leave {
    pub epoch: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub proto_version: u16,
    pub device_id: String,
    pub name: String,
    pub os: Os,
    pub monitors: Monitors,
    pub app_version: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayoutUpdate {
    pub version: (u64, String),
    pub devices: LayoutDevices,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Heartbeat {
    pub seq: u64,
    pub ts: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HeartbeatAck {
    pub seq: u64,
    pub ts: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Bye {
    pub reason: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct TakeOver {
    pub pos: Point,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Enter {
    pub epoch: u64,
    pub pos: Point,
    pub modifiers_down: ModifierKeys,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
/// Reliable keyboard, button, wheel, and modifier-state input.
pub enum InputMessage {
    Key(InputKey),
    Button(InputButton),
    Wheel(Wheel),
    ModifierSync(ModifierSync),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InputKey {
    pub epoch: u64,
    pub seq: u64,
    pub hid_usage: Key,
    pub down: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InputButton {
    pub epoch: u64,
    pub seq: u64,
    pub button: Button,
    pub down: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Wheel {
    pub epoch: u64,
    pub seq: u64,
    pub dx: f64,
    pub dy: f64,
    pub precise: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModifierSync {
    pub epoch: u64,
    pub seq: u64,
    pub modifiers_down: ModifierKeys,
}

impl InputMessage {
    pub fn epoch(&self) -> u64 {
        match self {
            Self::Key(value) => value.epoch,
            Self::Button(value) => value.epoch,
            Self::Wheel(value) => value.epoch,
            Self::ModifierSync(value) => value.epoch,
        }
    }
}

/// Latest-wins pointer position sent as a QUIC datagram.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
/// Latest-wins pointer position sent as a QUIC datagram in target logical pixels.
pub struct Move {
    pub seq: u64,
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ClipboardMessage {
    ClipAnnounce(ClipAnnounce),
    ClipFetch(ClipFetch),
    ClipData(ClipData),
    ClipFailure(ClipFailure),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClipAnnounce {
    pub clip_id: String,
    pub origin: String,
    /// Unix timestamp in milliseconds used for last-writer-wins ordering.
    pub timestamp_ms: u64,
    pub formats: ClipFormats,
    pub files: AnnouncedFiles,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ClipFormat {
    pub kind: String,
    pub mime: String,
    pub size: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AnnouncedFile {
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ClipFetch {
    pub clip_id: String,
    pub format: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClipData {
    pub clip_id: String,
    pub format: String,
    pub data: ClipboardBytes,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ClipFailure {
    pub clip_id: String,
    pub format: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TransferMessage {
    FileManifest(FileManifest),
    FileChunk(FileChunk),
    FileResume(FileResume),
    FileComplete(FileComplete),
    FileCancel(FileCancel),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileManifest {
    pub transfer_id: String,
    pub clip_id: String,
    pub chunk_size: u32,
    /// Zero-based page number, increasing by one for this transfer.
    pub page: u32,
    pub final_page: bool,
    pub files: ManifestFiles,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileManifestEntry {
    pub file_id: u32,
    /// Relative path within the transfer; receivers must validate every component.
    pub relative_path: String,
    pub size: u64,
    pub is_dir: bool,
    pub blake3_hash: Option<[u8; 32]>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileChunk {
    pub transfer_id: String,
    pub file_id: u32,
    pub chunk_index: u32,
    /// Offset in the uncompressed file contents.
    pub offset: u64,
    pub data: ChunkBytes,
    /// Uncompressed chunk length, checked before zstd decompression.
    pub uncompressed_size: u32,
    /// Whether the data is zstd-compressed; the digest always covers uncompressed bytes.
    pub compressed: bool,
    pub blake3_hash: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileResume {
    pub transfer_id: String,
    pub file_id: u32,
    pub missing_chunks: ResumeRanges,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChunkRange {
    pub first_chunk: u32,
    pub count: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileComplete {
    pub transfer_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileCancel {
    pub transfer_id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WireMessage {
    Control(ControlMessage),
    Input(InputMessage),
    Clipboard(ClipboardMessage),
    Transfer(TransferMessage),
}
