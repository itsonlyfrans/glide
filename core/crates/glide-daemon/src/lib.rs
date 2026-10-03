//! Native Glide orchestration with explicit development mocks and fail-closed trust.

pub mod arrangement;
pub mod autostart;
pub mod config;
pub mod control;
#[cfg(unix)]
pub mod control_unix;
#[cfg(windows)]
pub mod control_win;
pub mod device;
pub mod ipc;
pub mod keyboard;
pub mod layout;
pub mod logging;
mod state;
pub mod tray;
pub mod wake;
pub mod wheel;

pub use state::Core;
