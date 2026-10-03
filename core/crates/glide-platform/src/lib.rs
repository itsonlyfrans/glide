//! Shared platform contracts for native input and clipboard backends.
//!
//! Windows cannot inject into elevated, UAC, or secure-desktop windows unless the daemon runs
//! elevated. macOS input capture and injection require the relevant Accessibility and Input
//! Monitoring grants, and Secure Input can block key capture. Wayland and Linux are out of scope
//! for v1.

mod clipboard;
mod input;
mod mock;

pub use clipboard::{
    validate_clipboard_bundle, ClipboardAdmission, ClipboardBackend, ClipboardChangeToken,
    ClipboardContent, ClipboardData, ClipboardEvent, ClipboardFormat, ClipboardMarker,
    ClipboardPublish, ClipboardSensitivity, ClipboardSnapshot, FileEntry, FileList,
};
pub use input::{
    clamp_cursor_to_monitors, Button, CaptureMode, CaptureStatus, InputBackend, InputEvent,
    InputEventKind, InputSink, InputSinkError, Key, Monitor, MoveTimings, Os, Point,
};
pub use mock::{
    ClipboardOperation, ClipboardRace, InputOperation, MockClipboard, MockInput, MockPlatform,
    PlatformOperation,
};

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;

/// The permission state reported by the host operating system.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionStatus {
    /// The requested capability is available.
    Granted,
    /// The user or operating system explicitly denied the capability.
    Denied,
    /// The backend cannot currently determine whether the capability is available.
    Unknown,
    /// The permission does not apply to this operating system or backend.
    #[serde(rename = "n/a")]
    NotApplicable,
}

/// Input and accessibility permissions relevant to forwarding.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Permissions {
    /// Permission to observe keyboard and pointer input.
    pub accessibility: PermissionStatus,
    /// Permission to observe input through the platform's input-monitoring API.
    pub input_monitoring: PermissionStatus,
    /// Permission to synthesize keyboard and pointer input.
    pub injection: PermissionStatus,
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            accessibility: PermissionStatus::Unknown,
            input_monitoring: PermissionStatus::Unknown,
            injection: PermissionStatus::Unknown,
        }
    }
}

/// A host permission pane that a platform backend can open.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionKind {
    /// Accessibility or assistive-device permission.
    Accessibility,
    /// Keyboard and pointer monitoring permission.
    InputMonitoring,
    /// Permission to inject events.
    Injection,
}

/// A failure reported by a platform backend.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum BackendError {
    #[error("clipboard changed during snapshot")]
    ClipboardChanged,
    /// The operation requires an OS permission that is not granted.
    #[error("permission denied")]
    PermissionDenied,
    /// Windows UIPI blocks the higher-integrity foreground application.
    #[error("foreground application is running with higher integrity")]
    TargetElevated,
    /// A finite pointer position lies outside the current monitor union.
    #[error("cursor is outside active monitors")]
    InvalidPosition,
    /// One input operation failed temporarily (for example during a desktop switch).
    #[error("input is temporarily unavailable")]
    Transient,
    /// The host API does not support this operation.
    #[error("operation is unsupported")]
    Unsupported,
    /// The host resource is temporarily unavailable.
    #[error("resource is unavailable")]
    Unavailable,
    /// The caller supplied data that the backend cannot accept.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// A mutex became poisoned after a panic in the mock or backend.
    #[error("backend state is unavailable")]
    StateUnavailable,
    /// A backend-specific failure with a safe, non-sensitive explanation.
    #[error("backend failure: {0}")]
    Failed(String),
}

/// How orchestration handles a rejected injection, independent of backend diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InjectionFailure {
    /// Explicit loss of OS posting access, including a known Windows secure desktop.
    PermissionDenied,
    /// The foreground application exceeds this Windows process's integrity level.
    TargetElevated,
    /// Retry after clamping using fresh monitor geometry; never tear down for coordinates.
    InvalidPosition,
    /// Drop one event; only a sustained run justifies ending the session.
    Transient,
}

impl BackendError {
    /// Only an explicit OS denial is permission loss; unknown failures are transient.
    pub fn injection_failure(&self) -> InjectionFailure {
        match self {
            Self::PermissionDenied => InjectionFailure::PermissionDenied,
            Self::TargetElevated => InjectionFailure::TargetElevated,
            Self::InvalidPosition => InjectionFailure::InvalidPosition,
            _ => InjectionFailure::Transient,
        }
    }
}

/// The native clipboard, input, and host integration for one operating system.
pub trait Platform: Send + Sync {
    /// Returns the OS implemented by this platform instance.
    fn os(&self) -> Os;

    /// Borrows the input backend for the lifetime of this platform instance.
    fn input_backend(&self) -> &dyn InputBackend;

    /// Borrows the clipboard backend for the lifetime of this platform instance.
    fn clipboard_backend(&self) -> &dyn ClipboardBackend;

    /// Returns the daemon's platform-specific data directory.
    fn data_dir(&self) -> Result<PathBuf, BackendError>;

    /// Returns whether launch-at-login is currently enabled.
    fn autostart_enabled(&self) -> Result<bool, BackendError>;

    /// Enables or disables launch-at-login and reports OS registration failures.
    fn set_autostart_enabled(&self, enabled: bool) -> Result<(), BackendError>;

    /// Opens the host settings pane for the requested permission, if one exists.
    fn open_permission_settings(&self, kind: PermissionKind) -> Result<(), BackendError>;
}
