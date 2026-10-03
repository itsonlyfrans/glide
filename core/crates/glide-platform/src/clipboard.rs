use crate::BackendError;
use crossbeam_channel::Receiver;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClipboardChangeToken(pub u64);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClipboardSnapshot {
    pub contents: Vec<ClipboardContent>,
    pub sensitivity: ClipboardSensitivity,
    pub marker: Option<ClipboardMarker>,
    pub change_token: ClipboardChangeToken,
}

/// Nonblocking revocable generation. Recheck immediately before native ownership.
#[derive(Clone, Debug)]
pub struct ClipboardAdmission {
    generation: Arc<AtomicU64>,
    expected: u64,
}
impl Default for ClipboardAdmission {
    fn default() -> Self {
        Self::for_generation(Arc::new(AtomicU64::new(1)), 1)
    }
}
impl ClipboardAdmission {
    pub fn for_generation(generation: Arc<AtomicU64>, expected: u64) -> Self {
        Self {
            generation,
            expected,
        }
    }
    pub fn is_admitted(&self) -> bool {
        self.expected != 0 && self.generation.load(Ordering::Acquire) == self.expected
    }
    pub fn revoke(&self) {
        let _ =
            self.generation
                .compare_exchange(self.expected, 0, Ordering::AcqRel, Ordering::Acquire);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClipboardPublish {
    Published {
        change_token: ClipboardChangeToken,
    },
    /// Local clipboard replaced the caller's snapshot; nothing was overwritten.
    ReplacedLocalChange {
        actual_change_token: ClipboardChangeToken,
    },
    Revoked,
    /// Native ownership changed but publication failed. Clear partial data when possible.
    PartialFailure {
        formats_written: usize,
        cleared: bool,
        error: BackendError,
    },
}

/// Shared trust-boundary bounds, including native file reference metadata.
pub fn validate_clipboard_bundle(contents: &[ClipboardContent]) -> Result<(), BackendError> {
    if contents.len() > 32 {
        return Err(BackendError::InvalidInput(
            "too many clipboard formats".into(),
        ));
    }
    let mut total = 0usize;
    let mut formats = std::collections::HashSet::new();
    for content in contents {
        content.validate()?;
        if !formats.insert(&content.format) {
            return Err(BackendError::InvalidInput(
                "duplicate clipboard format".into(),
            ));
        }
        let size = match &content.data {
            ClipboardData::Bytes(bytes) => bytes.len(),
            ClipboardData::Files(files) => {
                if files.entries.len() > 4096 {
                    return Err(BackendError::InvalidInput(
                        "too many clipboard files".into(),
                    ));
                }
                files.entries.iter().try_fold(0usize, |sum, entry| {
                    let path = entry.path.to_string_lossy();
                    if !entry.path.is_absolute() || path.len() > 65536 || path.contains('\0') {
                        return Err(BackendError::InvalidInput(
                            "invalid clipboard file path".into(),
                        ));
                    }
                    sum.checked_add(path.len())
                        .ok_or_else(|| BackendError::InvalidInput("clipboard size overflow".into()))
                })?
            }
        };
        total = total
            .checked_add(size)
            .filter(|n| *n <= 64 * 1024 * 1024)
            .ok_or_else(|| {
                BackendError::InvalidInput("clipboard bundle exceeds size limit".into())
            })?;
    }
    Ok(())
}

/// A clipboard representation normalized across supported host operating systems.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardFormat {
    /// UTF-8 plain text.
    Text,
    /// UTF-8 HTML fragment or document.
    Html,
    /// Rich Text Format bytes.
    Rtf,
    /// PNG image bytes; native DIB/TIFF input should be converted by the backend.
    Png,
    /// A native file list, including files and directories.
    Files,
    /// A host format that can be represented as raw bytes, identified by its MIME type.
    Other(String),
}

/// A stable marker supplied by the daemon to identify its own clipboard writes.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ClipboardMarker(pub u64);

/// Platform sensitivity metadata that prevents concealed or transient content from syncing.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ClipboardSensitivity {
    /// The native clipboard marks this content as sensitive.
    pub sensitive: bool,
    /// The native clipboard marks it concealed (including NSPasteboard concealed type).
    pub concealed: bool,
    /// The native clipboard marks it transient.
    pub transient: bool,
    /// The native clipboard marks it auto-generated.
    pub auto_generated: bool,
}

impl ClipboardSensitivity {
    /// Returns true when any flag requires the content to be treated as sensitive.
    pub fn should_exclude(self) -> bool {
        self.sensitive || self.concealed || self.transient || self.auto_generated
    }
}

/// A local file or directory advertised by a native file-list clipboard item.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Absolute local path used by the native clipboard provider.
    pub path: PathBuf,
    /// User-visible basename, kept separately from the path for transfer sanitization.
    pub name: String,
    /// File size in bytes; directory sizes may be zero until enumerated by transfer code.
    pub size: u64,
    /// True for a directory entry.
    pub is_dir: bool,
}

/// A native clipboard file list.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileList {
    /// Files and directories in their clipboard order.
    pub entries: Vec<FileEntry>,
    /// Sensitivity and concealment flags supplied by the native clipboard.
    pub sensitivity: ClipboardSensitivity,
}

/// Payload for one clipboard representation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "value")]
pub enum ClipboardData {
    /// Raw representation bytes for text, rich text, images, or custom formats.
    Bytes(Vec<u8>),
    /// Native file references; transfer code is responsible for staging their contents.
    Files(FileList),
}

/// Clipboard data plus the exact native sensitivity flags observed with it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ClipboardContent {
    /// Format of this representation.
    pub format: ClipboardFormat,
    /// Format payload; `Files` must pair with `ClipboardData::Files`.
    pub data: ClipboardData,
    /// Sensitivity flags used by the sync policy.
    pub sensitivity: ClipboardSensitivity,
}

impl ClipboardContent {
    /// Builds a byte-backed clipboard representation.
    pub fn bytes(
        format: ClipboardFormat,
        bytes: Vec<u8>,
        sensitivity: ClipboardSensitivity,
    ) -> Result<Self, BackendError> {
        if format == ClipboardFormat::Files {
            return Err(BackendError::InvalidInput(
                "file format requires a file-list payload".into(),
            ));
        }
        Ok(Self {
            format,
            data: ClipboardData::Bytes(bytes),
            sensitivity,
        })
    }

    /// Builds a file-list representation and copies its sensitivity onto the content envelope.
    pub fn files(files: FileList) -> Self {
        Self {
            format: ClipboardFormat::Files,
            sensitivity: files.sensitivity,
            data: ClipboardData::Files(files),
        }
    }

    /// Checks that the selected format and payload variant agree.
    pub fn validate(&self) -> Result<(), BackendError> {
        match (&self.format, &self.data) {
            (ClipboardFormat::Files, ClipboardData::Files(files))
                if files.sensitivity == self.sensitivity =>
            {
                Ok(())
            }
            (ClipboardFormat::Files, ClipboardData::Files(_)) => Err(BackendError::InvalidInput(
                "file sensitivity metadata mismatch".into(),
            )),
            (ClipboardFormat::Files, ClipboardData::Bytes(_)) => Err(BackendError::InvalidInput(
                "file format requires a file-list payload".into(),
            )),
            (_, ClipboardData::Files(_)) => Err(BackendError::InvalidInput(
                "file-list payload requires the file format".into(),
            )),
            (_, ClipboardData::Bytes(_)) => Ok(()),
        }
    }
}

/// Asynchronous notification from a native clipboard owner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClipboardEvent {
    /// Clipboard formats changed; the marker is absent for an external owner.
    Changed {
        /// Marker supplied by the daemon's write, when recognized by the backend.
        marker: Option<ClipboardMarker>,
        /// Formats currently available from the native owner.
        formats: Vec<ClipboardFormat>,
        /// Aggregate sensitivity flags reported for the change.
        sensitivity: ClipboardSensitivity,
    },
    /// A consumer pasted a delayed representation and the daemon must provide its bytes.
    RenderRequested {
        /// Marker identifying the delayed clipboard owner.
        marker: ClipboardMarker,
        /// Representation requested by the host clipboard manager.
        format: ClipboardFormat,
    },
    /// The bounded notification queue overflowed; callers must reread current formats.
    ResyncRequired,
}

/// Native clipboard access, including file lists and delayed rendering.
///
/// Methods run on daemon worker threads. Implementations marshal native clipboard calls to
/// their owning thread, retaining all owned payloads until OS use ends. OS callbacks only enqueue
/// notifications; they never wait for daemon/network work. API failures return BackendError.
pub trait ClipboardBackend: Send + Sync {
    /// Bounded consistent eager contents and native change token; no delayed render.
    fn read_snapshot(&self) -> Result<ClipboardSnapshot, BackendError>;
    /// Prepare every native representation, compare the snapshot token and
    /// admission immediately before ownership, then publish in one transaction.
    fn publish_snapshot(
        &self,
        contents: Vec<ClipboardContent>,
        marker: ClipboardMarker,
        expected_change_token: ClipboardChangeToken,
        admission: ClipboardAdmission,
    ) -> Result<ClipboardPublish, BackendError>;
    /// Returns one representation, if present, with the sensitivity flags captured with it.
    fn read(&self, format: &ClipboardFormat) -> Result<Option<ClipboardContent>, BackendError>;

    /// Writes one representation under `marker` so self-originated changes can be ignored.
    ///
    /// The backend copies the data before returning or retains it safely for delayed rendering.
    /// It rejects mismatched format/payload variants and returns errors from native APIs.
    fn write(&self, content: ClipboardContent, marker: ClipboardMarker)
        -> Result<(), BackendError>;

    /// Lists every normalized format currently available on the native clipboard.
    fn formats(&self) -> Result<Vec<ClipboardFormat>, BackendError>;

    /// Returns the file-list item, including local paths and native sensitivity metadata.
    fn read_files(&self) -> Result<Option<FileList>, BackendError>;

    /// Places a real local file list on the clipboard under `marker`.
    ///
    /// Implementations expose native file URLs or equivalent shell file-drop data and must not
    /// turn the paths into executable actions.
    fn write_files(&self, files: FileList, marker: ClipboardMarker) -> Result<(), BackendError>;

    /// Returns the bounded change/render notification stream.
    ///
    /// When the queue reports `ResyncRequired`, reread `formats` and content because intermediate
    /// changes may have been coalesced. Repeated calls may clone a receiver sharing one stream.
    fn subscribe(&self) -> Receiver<ClipboardEvent>;

    /// Registers a placeholder so the OS requests this format only when a user pastes it.
    ///
    /// The backend must later send `RenderRequested` without blocking its OS callback. Register
    /// each format under the marker that owns the current delayed clipboard item. File lists use
    /// prefetch and must not be registered as delayed representations.
    fn set_delayed_render(
        &self,
        format: ClipboardFormat,
        marker: ClipboardMarker,
    ) -> Result<(), BackendError>;

    /// Supplies bytes requested for a previously registered delayed representation.
    /// The v1 daemon uses eager/prefetch publication and never calls delayed-render hooks;
    /// synchronous OS paste callbacks cannot wait for network completion.
    fn fulfill_delayed_render(
        &self,
        content: ClipboardContent,
        marker: ClipboardMarker,
    ) -> Result<(), BackendError>;

    /// Cancels every outstanding delayed representation owned by `marker`.
    fn cancel_delayed_render(&self, marker: ClipboardMarker) -> Result<(), BackendError>;
}
