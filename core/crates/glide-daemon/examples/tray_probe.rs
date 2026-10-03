//! Run on Windows to inspect the native menu and manually exercise tray actions.

use std::error::Error;

use glide_daemon::tray::{Tray, TrayAction};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let (actions, mut receiver) = tokio::sync::mpsc::channel(8);
    let tray = Tray::start(None, actions)?;
    eprintln!("Glide tray probe running; Open Glide is disabled without a UI path.");
    while let Some(action) = receiver.recv().await {
        eprintln!("tray action: {action:?}");
        if action == TrayAction::Quit {
            break;
        }
    }
    #[cfg_attr(not(windows), allow(clippy::drop_non_drop))]
    // the tray only has a destructor on Windows
    drop(tray);
    Ok(())
}
