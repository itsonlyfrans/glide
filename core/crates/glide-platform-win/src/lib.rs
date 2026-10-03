//! Windows Raw Input, low-level hooks, SendInput, native clipboard and autostart backend.
//! UIPI limits injection into elevated applications. UAC and secure desktops are unsupported.

/// Whether this crate targets its native operating system.
pub const NATIVE_TARGET: bool = cfg!(windows);

#[cfg(windows)]
mod clipboard;
#[cfg(windows)]
mod cursor;
#[cfg(windows)]
mod formats;
#[cfg(windows)]
mod input;
#[cfg(windows)]
mod keyboard;
#[cfg(windows)]
mod native;
#[cfg(windows)]
mod platform;

#[cfg(windows)]
pub use clipboard::WindowsClipboard;
#[cfg(windows)]
pub use cursor::restore_system_cursors;
#[cfg(windows)]
pub use input::WindowsInput;
#[cfg(windows)]
pub use native::{enable_per_monitor_v2, monitor_geometry_snapshot};
#[cfg(windows)]
pub use platform::{set_autostart_command, WindowsPlatform};
