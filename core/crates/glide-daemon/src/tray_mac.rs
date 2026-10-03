//! Native tray support is not available on macOS yet.
//!
//! A real implementation needs an `NSStatusItem` owned by the main-thread
//! `NSApplication` run loop. The current daemon deliberately leaves the Mac
//! process structure alone; Electron continues to own the Mac tray.

use std::path::PathBuf;

use glide_proto::ipc::Event;
use tokio::sync::mpsc;

use super::{TrayAction, TrayError};

pub struct Tray;

impl Tray {
    pub fn start(
        _ui: Option<PathBuf>,
        _actions: mpsc::Sender<TrayAction>,
    ) -> Result<Self, TrayError> {
        Err(TrayError(
            "native tray is not available yet on macOS".to_owned(),
        ))
    }

    pub fn update(&self, _event: &Event, _clients: usize) {}
}
