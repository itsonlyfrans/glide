//! Native macOS platform support. No native backends are exported on other hosts.
//! Pure conversions remain available to this crate's tests on every host.

#[cfg(target_os = "macos")]
mod clipboard;
#[cfg(any(target_os = "macos", test))]
mod clipboard_logic;
#[cfg(target_os = "macos")]
mod input;
#[cfg(any(target_os = "macos", test))]
mod input_logic;
#[cfg(target_os = "macos")]
mod platform;
#[cfg(any(target_os = "macos", test))]
mod platform_logic;

#[cfg(target_os = "macos")]
pub use clipboard::MacClipboard;
#[cfg(target_os = "macos")]
pub use input::{prioritize_current_thread, MacInput};
#[cfg(target_os = "macos")]
pub use platform::{repair_autostart, MacPlatform};

/// Whether this crate targets its native operating system.
pub const NATIVE_TARGET: bool = cfg!(target_os = "macos");
