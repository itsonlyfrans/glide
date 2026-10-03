//! Shared tray state and small menu model. The Mac implementation remains a
//! documented stub until an `NSApplication` main-thread loop is available.
#![cfg_attr(not(windows), allow(dead_code))]

use std::time::{Duration, Instant};

use glide_proto::ipc::{Connection, Event};

#[cfg(windows)]
#[path = "tray_win.rs"]
mod platform;
#[cfg(target_os = "macos")]
#[path = "tray_mac.rs"]
mod platform;
#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
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
                "native tray is unavailable on this platform".to_owned(),
            ))
        }

        pub fn update(&self, _event: &Event, _clients: usize) {}
    }
}

pub use platform::Tray;

pub(super) fn icon_frame(bytes: &[u8], size: u8) -> Option<&[u8]> {
    if bytes.get(0..4)? != [0, 0, 1, 0] {
        return None;
    }
    let count = u16::from_le_bytes(bytes.get(4..6)?.try_into().ok()?) as usize;
    let table_end = 6usize.checked_add(count.checked_mul(16)?)?;
    for entry in bytes.get(6..table_end)?.as_chunks::<16>().0 {
        if entry[0] != size || entry[1] != size {
            continue;
        }
        let length = u32::from_le_bytes(entry[8..12].try_into().ok()?) as usize;
        let offset = u32::from_le_bytes(entry[12..16].try_into().ok()?) as usize;
        return bytes.get(offset..offset.checked_add(length)?);
    }
    None
}

/// A request from the native tray to the daemon's normal event loop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrayAction {
    Open,
    OpenLogs,
    ToggleSharing,
    ReturnHome,
    Quit,
}

/// Safe, user-facing startup failure. The engine can keep running without a tray.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct TrayError(pub String);

const BALLOON_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TrayView {
    pub sharing_enabled: bool,
    pub connected: usize,
}

impl TrayView {
    pub fn tooltip(&self) -> String {
        format!("Glide - {} connected", self.connected)
    }

    pub fn menu(&self, ui_available: bool) -> [MenuItem; 7] {
        [
            MenuItem::OpenGlide {
                enabled: ui_available,
            },
            MenuItem::ShareKeyboardMouse {
                checked: self.sharing_enabled,
            },
            MenuItem::ReturnHome,
            MenuItem::Status(self.tooltip()),
            MenuItem::OpenLogs,
            MenuItem::Separator,
            MenuItem::Quit,
        ]
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum MenuItem {
    OpenGlide { enabled: bool },
    ShareKeyboardMouse { checked: bool },
    ReturnHome,
    Status(String),
    OpenLogs,
    Separator,
    Quit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Balloon {
    pub title: String,
    pub body: String,
    pub level: BalloonLevel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BalloonLevel {
    Info,
    Warning,
    Error,
}

#[derive(Debug)]
pub(super) struct TrayModel {
    pub view: TrayView,
    pub pending_balloon: Option<Balloon>,
    last_balloon: Option<Instant>,
}

impl Default for TrayModel {
    fn default() -> Self {
        Self {
            view: TrayView {
                sharing_enabled: false,
                connected: 0,
            },
            pending_balloon: None,
            last_balloon: None,
        }
    }
}

impl TrayModel {
    /// Applies one protocol event and returns true when the native icon should refresh.
    pub fn update(&mut self, event: &Event, ui_clients: usize, now: Instant) -> bool {
        let old_view = self.view.clone();
        let balloon = match event {
            Event::State(state) => {
                self.view.sharing_enabled = state.sharing_enabled;
                self.view.connected = state
                    .peers
                    .iter()
                    .filter(|peer| peer.connection == Connection::Connected)
                    .count();
                None
            }
            Event::Notification(notification) if notification.level == "error" => Some(Balloon {
                title: notification.title.clone(),
                body: notification.body.clone(),
                level: BalloonLevel::Error,
            }),
            Event::Notification(notification)
                if notification.level == "warn" || notification.level == "warning" =>
            {
                Some(Balloon {
                    title: notification.title.clone(),
                    body: notification.body.clone(),
                    level: BalloonLevel::Warning,
                })
            }
            Event::PairingIncoming(incoming) if ui_clients == 0 => Some(Balloon {
                title: "Pairing request".to_owned(),
                body: format!("{} wants to pair", incoming.name),
                level: BalloonLevel::Info,
            }),
            _ => None,
        };

        let mut balloon_changed = false;
        if let Some(balloon) = balloon {
            if self
                .last_balloon
                .is_none_or(|last| now.duration_since(last) >= BALLOON_INTERVAL)
            {
                self.pending_balloon = Some(balloon);
                self.last_balloon = Some(now);
                balloon_changed = true;
            }
        }
        old_view != self.view || balloon_changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glide_platform::Os;
    use glide_proto::ipc::{Notification, PairingIncoming, State};

    #[test]
    fn tray_menu_and_tooltip_reflect_state_and_ui_availability() {
        let view = TrayView {
            sharing_enabled: true,
            connected: 2,
        };
        assert_eq!(view.tooltip(), "Glide - 2 connected");
        assert_eq!(
            view.menu(false),
            [
                MenuItem::OpenGlide { enabled: false },
                MenuItem::ShareKeyboardMouse { checked: true },
                MenuItem::ReturnHome,
                MenuItem::Status("Glide - 2 connected".to_owned()),
                MenuItem::OpenLogs,
                MenuItem::Separator,
                MenuItem::Quit,
            ]
        );
    }

    #[test]
    fn event_snapshots_and_balloon_throttle_are_bounded() {
        let now = Instant::now();
        let mut model = TrayModel::default();
        let state = Event::State(Box::new(State {
            self_info: glide_proto::ipc::SelfInfo {
                device_id: "self".to_owned(),
                name: "Glide".to_owned(),
                os: Os::Windows,
                fingerprint: "fp".to_owned(),
                listen_port: 0,
                version: "test".to_owned(),
                monitors: vec![],
            },
            sharing_enabled: true,
            active_device_id: "self".to_owned(),
            permissions: glide_proto::ipc::PermissionState {
                accessibility: glide_platform::PermissionStatus::NotApplicable,
                input_monitoring: glide_platform::PermissionStatus::NotApplicable,
                injection: glide_platform::PermissionStatus::NotApplicable,
                restart_required: false,
            },
            peers: vec![],
            discovered: vec![],
            layout: Default::default(),
            settings: Default::default(),
            transfers: vec![],
        }));
        assert!(model.update(&state, 0, now));
        assert!(model.view.sharing_enabled);

        let incoming = Event::PairingIncoming(PairingIncoming {
            name: "Nearby".to_owned(),
            os: Os::Windows,
            address: "127.0.0.1:0".to_owned(),
        });
        assert!(!model.update(&incoming, 1, now));
        assert!(model.update(&incoming, 0, now));
        assert_eq!(
            model.pending_balloon.as_ref().unwrap().title,
            "Pairing request"
        );

        let warning = Event::Notification(Notification {
            level: "warning".to_owned(),
            title: "Warning".to_owned(),
            body: "Limited".to_owned(),
            action: None,
        });
        assert!(!model.update(&warning, 0, now + Duration::from_secs(1)));
        assert!(model.update(&warning, 0, now + BALLOON_INTERVAL));
        assert_eq!(model.pending_balloon.as_ref().unwrap().title, "Warning");
    }

    #[test]
    fn embedded_ico_has_valid_16_and_32_pixel_png_frames() {
        let icon = include_bytes!("../assets/tray.ico");
        for (size, frame) in [
            (16u32, icon_frame(icon, 16).expect("16 px icon frame")),
            (32u32, icon_frame(icon, 32).expect("32 px icon frame")),
        ] {
            assert!(frame.starts_with(b"\x89PNG\r\n\x1a\n"));
            assert_eq!(u32::from_be_bytes(frame[16..20].try_into().unwrap()), size);
            assert_eq!(u32::from_be_bytes(frame[20..24].try_into().unwrap()), size);
        }
    }
}
