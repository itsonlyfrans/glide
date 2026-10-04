use crate::config::{merge_patch, Config};
use glide_net::{
    InMemoryLink, InMemoryPeerManager, Link, PairTarget, PeerManager, PeerManagerEvent,
};
use glide_platform::{
    CaptureMode, InputSink, MockPlatform, Os, PermissionStatus, Permissions, Platform,
};
use glide_proto::ipc::*;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

mod clipboard;
mod layout_replication;
use layout_replication::reconcile_layout;
#[cfg(test)]
mod input_regression_tests;
#[cfg(test)]
mod latency_tests;
#[cfg(test)]
mod native_tests;
mod permissions;
mod runtime;
mod startup;
#[cfg(test)]
mod tests;
use runtime::Hotkey;

fn error(code: ErrorCode, message: impl Into<String>) -> IpcError {
    IpcError::new(code, message)
}

fn params<T: DeserializeOwned>(value: Value) -> Result<T, IpcError> {
    if !value.is_object() {
        return Err(error(ErrorCode::InvalidParams, "params must be an object"));
    }
    serde_json::from_value(value)
        .map_err(|_| error(ErrorCode::InvalidParams, "invalid method parameters"))
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

/// Native or explicit mock orchestration with persistent public configuration.
pub struct Core {
    ui_path: Option<PathBuf>,
    engine_autostart: bool,
    state: State,
    data_dir: PathBuf,
    manager: Box<dyn PeerManager>,
    peer_events: tokio::sync::broadcast::Receiver<PeerManagerEvent>,
    link: Arc<dyn Link>,
    platform: Arc<dyn Platform>,
    mock_platform: Option<MockPlatform>,
    native_manager: Option<Arc<glide_net::NativePeerManager>>,
    accept_job: Option<tokio::task::JoinHandle<()>>,
    connected_tokens: HashMap<String, glide_net::PeerToken>,
    monitor_rx: crossbeam_channel::Receiver<Vec<glide_platform::Monitor>>,
    /// This computer's screens as the operating system arranges them (the arranged ones are in `state.self_info`).
    native_monitors: Vec<glide_platform::Monitor>,
    /// Network card addresses learned in the background: (device id, address).
    wake_found: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
    /// When each sleeping computer was last sent a wake-up, so pushing at an edge does not flood the network.
    wake_sent: HashMap<String, Instant>,
    /// This computer's model, read once in the background.
    model_found: std::sync::Arc<std::sync::Mutex<Option<DeviceModel>>>,
    model_started: bool,
    /// Timings behind the cursor report (Settings > Troubleshooting).
    diag: crate::diag::CursorDiag,
    clipboard: clipboard::ClipboardSync,
    purge_cancel: glide_xfer::Cancel,
    purge_job: Option<tokio::task::JoinHandle<glide_xfer::Result<()>>>,
    capture_sink: InputSink,
    capture_rx: crossbeam_channel::Receiver<glide_platform::InputEvent>,
    capture_status_rx: crossbeam_channel::Receiver<glide_platform::CaptureStatus>,
    permission_job: Option<(
        Option<u64>,
        tokio::task::JoinHandle<permissions::PermissionUpdate>,
    )>,
    permission_requests: VecDeque<u64>,
    next_permission_check: Instant,
    /// Set while the displays could not be read (all asleep, lid closed); retried until they come back.
    monitors_retry: Option<Instant>,
    capture_pending: bool,
    permission_recovery_notified: bool,
    engine: crate::layout::EdgeEngine,
    translator: Option<crate::keyboard::KeyTranslator>,
    receiving_from: Option<String>,
    receiving_token: Option<glide_net::PeerToken>,
    last_receive: Instant,
    move_seq: Option<u64>,
    input_seq: Option<u64>,
    receiving_modifiers: [bool; 8],
    injection_health: runtime::InjectionHealth,
    // Hidden receivers retain the source's liveness after Leave clears input admission.
    cursor_hidden_peer: Option<(String, Instant)>,
    cursor_restore_pending: bool,
    outgoing_seq: u64,
    outgoing_epoch: u64,
    pending_move: Option<(glide_net::PeerToken, glide_proto::wire::Move)>,
    pending_pairings: HashMap<String, Peer>,
    receiving_epoch: Option<u64>,
    received_epochs: HashMap<String, (glide_net::PeerToken, u64)>,
    lamport: u64,
    layout_version: (u64, String),
    last_heartbeat_sent: Instant,
    last_discovery_poll: Instant,
    last_stats: HashMap<String, Instant>,
    held_physical: [bool; 256],
    return_hotkey: Hotkey,
    toggle_hotkey: Hotkey,
    pending_join_id: Option<u64>,
    join_job: Option<tokio::task::JoinHandle<Result<glide_net::PairingSession, IpcError>>>,
    verification_deadline: Option<u64>,
    deferred_responses: Vec<Response>,
    last_progress: HashMap<String, Instant>,
    events: Vec<Event>,
    dirty: bool,
    shutdown: bool,
}

impl Core {
    /// Executable control belongs to the engine; explicit mocks never change host startup.
    pub fn configure_background(&mut self, ui: Option<PathBuf>, real: bool) -> anyhow::Result<()> {
        if let Some(ui) = ui {
            self.ui_path = Some(ui);
        }
        if let Some(ui) = &self.ui_path {
            crate::autostart::validate_ui(ui)?;
        }
        self.engine_autostart = real;
        self.persist(&self.state)
            .map_err(|e| anyhow::anyhow!(e.message))?;
        if real {
            self.set_autostart(self.state.settings.startup.launch_at_login)?;
        }
        Ok(())
    }

    pub fn ui_path(&self) -> Option<PathBuf> {
        self.ui_path.clone()
    }

    fn set_autostart(&self, enabled: bool) -> Result<(), glide_platform::BackendError> {
        if self.engine_autostart {
            let data_dir = self
                .data_dir
                .canonicalize()
                .map_err(|_| glide_platform::BackendError::Unavailable)?;
            crate::autostart::repair(enabled, &data_dir, self.ui_path.as_deref())
                .map_err(|_| glide_platform::BackendError::Unavailable)
        } else {
            self.platform.set_autostart_enabled(enabled)
        }
    }
    /// Create a local-only mock daemon. No certificates, native hooks or network listeners are used.
    pub async fn mock(data_dir: &Path, port: Option<u16>) -> anyhow::Result<Self> {
        let os = if cfg!(target_os = "macos") {
            Os::Macos
        } else {
            Os::Windows
        };
        let platform = MockPlatform::new(os, data_dir.to_path_buf());
        platform.input.set_permissions(Permissions {
            accessibility: PermissionStatus::Granted,
            input_monitoring: PermissionStatus::Granted,
            injection: PermissionStatus::Granted,
        })?;
        let config = Self::load_config(data_dir, port, None)?;
        Self::initialize(
            data_dir,
            config,
            Arc::new(platform.clone()),
            Some(platform),
            None,
        )
        .await
    }

    fn load_config(
        data_dir: &Path,
        port: Option<u16>,
        discovery: Option<bool>,
    ) -> anyhow::Result<Config> {
        let mut config = match Config::load(data_dir)? {
            Some(config) => config,
            None => Config {
                ui_path: None,
                settings: Settings::default(),
                layout: Layout::default(),
                layout_version: (0, String::new()),
                peers: Vec::new(),
                sharing_enabled: true,
                mock_device_id: hex::encode(rand::random::<[u8; 32]>()),
            },
        };
        if let Some(port) = port {
            config.settings.network.port = port;
        }
        if let Some(discovery) = discovery {
            config.settings.network.discovery = discovery;
        }
        validate_settings(&config.settings).map_err(|error| anyhow::anyhow!(error.message))?;
        Ok(config)
    }

    async fn initialize(
        data_dir: &Path,
        config: Config,
        platform: Arc<dyn Platform>,
        mock_platform: Option<MockPlatform>,
        fingerprint: Option<String>,
    ) -> anyhow::Result<Self> {
        let os = platform.os();
        // Also repairs an invisible cursor left by a previous process crash.
        let cursor_restore_pending = platform.input_backend().set_cursor_visible(true).is_err();
        let real_network = fingerprint.is_some();
        let id = config.mock_device_id.clone();
        anyhow::ensure!(
            id.len() == 64
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
            "invalid mock identity in configuration"
        );
        let settings = config.settings;
        validate_settings(&settings).map_err(|error| anyhow::anyhow!(error.message))?;
        let mut layout = config.layout;
        if !layout.devices.iter().any(|device| device.device_id == id) {
            layout.devices.push(LayoutDevice {
                device_id: id.clone(),
                x: 0.0,
                y: 0.0,
            });
        }
        let mut peers = config.peers;
        anyhow::ensure!(peers.len() <= 32, "too many configured peers");
        let mut peer_ids = HashSet::new();
        for peer in &peers {
            validate_peer(peer).map_err(|error| anyhow::anyhow!(error.message))?;
            anyhow::ensure!(
                peer.device_id != id && peer_ids.insert(peer.device_id.clone()),
                "duplicate configured peer"
            );
        }
        for peer in &mut peers {
            peer.online = false;
            peer.connection = Connection::Offline;
            peer.latency_ms = None;
        }
        let mut state = State {
            self_info: SelfInfo {
                device_id: id.clone(),
                name: settings.device_name.clone(),
                os,
                fingerprint: fingerprint.unwrap_or_else(|| id.clone()),
                listen_port: settings.network.port,
                version: env!("CARGO_PKG_VERSION").into(),
                model: None,
                monitors: crate::arrangement::arrange(
                    &platform.input_backend().monitors()?,
                    &settings.display.arrangement,
                ),
            },
            sharing_enabled: config.sharing_enabled,
            active_device_id: id,
            permissions: platform.input_backend().permissions().into(),
            peers,
            discovered: Vec::new(),
            layout,
            settings,
            transfers: Vec::new(),
            cursor: None,
        };
        validate_layout(&state.layout.devices, &state)
            .map_err(|error| anyhow::anyhow!(error.message))?;
        let manager = InMemoryPeerManager::new();
        manager.seed_paired_peers(state.peers.clone());
        let fixture = DiscoveredPeer {
            device_id: "e".repeat(64),
            name: "Mock Mac".into(),
            os: Os::Macos,
            address: "127.0.0.1:24801".into(),
        };
        if !real_network {
            manager.script_reachable_peer(fixture.clone());
            manager.script_discovered_peer(fixture.clone());
            manager.script_remote_pairing_code(
                &fixture.device_id,
                "123456",
                now_ms().saturating_add(120_000),
            );
            manager.script_remote_confirmation(&fixture.device_id, true);
        }
        let peer_events = manager.events();
        let link = Arc::new(InMemoryLink::new());
        link.script_reachable(fixture.address.clone(), fixture.device_id.clone());
        for peer in &state.peers {
            if let Some(address) = &peer.address {
                link.script_reachable(address.clone(), peer.device_id.clone());
            }
        }
        let (capture_sink, capture_rx) = InputSink::bounded(1024)
            .map_err(|_| anyhow::anyhow!("could not create input queue"))?;
        let capture_platform = platform.clone();
        let startup_sink = capture_sink.clone();
        let capture_pending = match tokio::task::spawn_blocking(move || {
            capture_platform.input_backend().start_capture(startup_sink)
        })
        .await?
        {
            Ok(()) => false,
            Err(
                glide_platform::BackendError::PermissionDenied
                | glide_platform::BackendError::Unavailable,
            ) => {
                state.permissions.restart_required = !permissions::missing(state.permissions)
                    && platform.input_backend().secure_input_enabled() != Some(true);
                true
            }
            Err(failure) => return Err(failure.into()),
        };
        let engine = build_engine(&state, platform.as_ref())?;
        let return_hotkey = Hotkey::parse(&state.settings.hotkeys.return_home)
            .map_err(|error| anyhow::anyhow!(error.message))?;
        let toggle_hotkey = Hotkey::parse(&state.settings.hotkeys.toggle_sharing)
            .map_err(|error| anyhow::anyhow!(error.message))?;
        let mut layout_version = config.layout_version;
        if layout_version.1.is_empty() {
            layout_version.1 = state.self_info.device_id.clone();
        }
        let mut core = Self {
            ui_path: config.ui_path,
            engine_autostart: false,
            state,
            data_dir: data_dir.to_owned(),
            manager: Box::new(manager),
            peer_events,
            link,
            capture_status_rx: platform.input_backend().capture_status_changes(),
            permission_job: None,
            permission_requests: VecDeque::new(),
            next_permission_check: Instant::now(),
            monitors_retry: None,
            capture_pending,
            permission_recovery_notified: false,
            monitor_rx: platform.input_backend().monitor_changes(),
            native_monitors: platform.input_backend().monitors().unwrap_or_default(),
            wake_found: Default::default(),
            wake_sent: HashMap::new(),
            model_found: Default::default(),
            model_started: false,
            diag: Default::default(),
            clipboard: clipboard::ClipboardSync::new(platform.clipboard_backend().subscribe()),
            purge_cancel: glide_xfer::Cancel::new(),
            purge_job: None,
            platform,
            mock_platform,
            native_manager: None,
            accept_job: None,
            connected_tokens: HashMap::new(),
            capture_sink,
            capture_rx,
            engine,
            translator: None,
            receiving_from: None,
            receiving_token: None,
            last_receive: Instant::now(),
            move_seq: None,
            input_seq: None,
            receiving_modifiers: [false; 8],
            injection_health: runtime::InjectionHealth::default(),
            cursor_hidden_peer: None,
            cursor_restore_pending,
            outgoing_seq: 0,
            outgoing_epoch: 0,
            pending_move: None,
            pending_pairings: HashMap::new(),
            receiving_epoch: None,
            received_epochs: HashMap::new(),
            lamport: layout_version.0,
            layout_version,
            last_heartbeat_sent: Instant::now(),
            last_discovery_poll: Instant::now(),
            last_stats: HashMap::new(),
            held_physical: [false; 256],
            return_hotkey,
            toggle_hotkey,
            pending_join_id: None,
            join_job: None,
            verification_deadline: None,
            deferred_responses: Vec::new(),
            last_progress: HashMap::new(),
            events: Vec::new(),
            dirty: false,
            shutdown: false,
        };
        if capture_pending {
            core.notify_failure(&error(ErrorCode::PermissionDenied,
                if core.state.permissions.restart_required {
                    "Input permissions are granted, but capture could not start. Restart Glide to try again."
                } else {
                    "Allow Glide in Accessibility and Input Monitoring to share your mouse and keyboard."
                }));
        }
        if !real_network && core.state.settings.network.discovery {
            core.state.discovered = core
                .manager
                .discover()
                .await
                .map_err(|error| anyhow::anyhow!(error.message))?;
        }
        core.persist(&core.state)
            .map_err(|error| anyhow::anyhow!(error.message))?;
        Ok(core)
    }

    pub fn snapshot(&self) -> State {
        self.state.clone()
    }
    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }
    pub fn take_responses(&mut self) -> Vec<Response> {
        std::mem::take(&mut self.deferred_responses)
    }
    pub fn shutdown_requested(&self) -> bool {
        self.shutdown
    }
    /// Script input, clipboard changes and backend errors in integration tests.
    pub fn mock_platform(&self) -> Option<&MockPlatform> {
        self.mock_platform.as_ref()
    }
    /// One consumer only: the stdio runtime bridges this bounded queue into its event loop.
    pub fn capture_receiver(&self) -> crossbeam_channel::Receiver<glide_platform::InputEvent> {
        self.capture_rx.clone()
    }
    pub fn link(&self) -> Arc<dyn Link> {
        self.link.clone()
    }

    fn persist(&self, state: &State) -> Result<(), IpcError> {
        Config {
            ui_path: self.ui_path.clone(),
            settings: state.settings.clone(),
            layout: state.layout.clone(),
            layout_version: self.layout_version.clone(),
            peers: state.peers.clone(),
            sharing_enabled: state.sharing_enabled,
            mock_device_id: state.self_info.device_id.clone(),
        }
        .save(&self.data_dir)
        .map_err(|_| {
            tracing::error!("configuration replacement failed");
            error(ErrorCode::Internal, "could not save configuration")
        })
    }

    fn apply(&mut self, mut state: State) -> Result<(), IpcError> {
        state.active_device_id = self.state.active_device_id.clone();
        let engine = build_engine(&state, self.platform.as_ref())
            .map_err(|_| error(ErrorCode::InvalidParams, "invalid virtual desktop"))?;
        let return_hotkey = Hotkey::parse(&state.settings.hotkeys.return_home)?;
        let toggle_hotkey = Hotkey::parse(&state.settings.hotkeys.toggle_sharing)?;
        let clipboard_changed = state.settings.clipboard != self.state.settings.clipboard
            || self.state.peers.iter().any(|old| {
                !state.peers.iter().any(|p| {
                    p.device_id == old.device_id && p.clipboard_enabled == old.clipboard_enabled
                })
            });
        if clipboard_changed {
            for transfer in &mut state.transfers {
                if matches!(
                    transfer.state,
                    TransferState::Queued | TransferState::AwaitingConfirm | TransferState::Active
                ) {
                    transfer.state = TransferState::Cancelled;
                }
            }
        }
        self.persist(&state)?;
        self.connected_tokens
            .retain(|id, _| state.peers.iter().any(|peer| &peer.device_id == id));
        if clipboard_changed {
            self.clipboard_policy_changed();
        }
        self.return_hotkey = return_hotkey;
        self.toggle_hotkey = toggle_hotkey;
        self.state = state;
        if clipboard_changed {
            self.clipboard.pending = true;
        }
        self.engine = engine;
        self.dirty = true;
        Ok(())
    }

    fn return_home(&mut self, reason: &str) -> Result<(), IpcError> {
        self.pending_move = None;
        let input = self.platform.input_backend();
        self.cursor_restore_pending = input.set_cursor_visible(true).is_err();
        self.cursor_hidden_peer = None;
        self.injection_health.reset_run();
        let mode = input.set_mode(CaptureMode::Local);
        let releases = input.release_all();
        if mode.is_err() || releases.is_err() {
            // Exiting destroys native hooks; never report home while a failed mode change still swallows.
            self.shutdown = true;
            return Err(error(
                ErrorCode::PermissionDenied,
                "could not release input; daemon must exit",
            ));
        }
        let changed = self.state.active_device_id != self.state.self_info.device_id;
        self.state.active_device_id = self.state.self_info.device_id.clone();
        self.receiving_from = None;
        self.receiving_epoch = None;
        self.receiving_token = None;
        self.receiving_modifiers = [false; 8];
        self.translator = None;
        if changed {
            self.events.push(Event::ActiveChanged(ActiveChanged {
                device_id: self.state.active_device_id.clone(),
                reason: reason.into(),
            }));
            self.dirty = true;
        }
        Ok(())
    }

    /// A pairing.join response is deferred until both humans confirm or pairing aborts.
    pub async fn handle(&mut self, mut request: Request) -> Option<Response> {
        if request.method == METHOD_PERMISSIONS_REQUEST {
            if let Err(failure) = params::<EmptyParams>(request.params.take()) {
                return Some(Response::failure(request.id, failure));
            }
            // Bounded, serialized workers; every accepted user action requests once.
            if self.permission_requests.len() >= 16 {
                return Some(Response::failure(
                    request.id,
                    error(
                        ErrorCode::InvalidParams,
                        "Permission requests are already pending.",
                    ),
                ));
            }
            self.permission_requests.push_back(request.id);
            self.poll_permissions(Instant::now()).await;
            return None;
        }
        if request.method == "pairing.join" && self.pending_join_id.is_some() {
            let failure = error(
                ErrorCode::InvalidParams,
                "Pairing is already pending. Confirm or cancel it before starting another pairing.",
            );
            self.notify_failure(&failure);
            return Some(Response::failure(request.id, failure));
        }
        let defer = request.method == "pairing.join";
        let result = self.dispatch(&request.method, request.params.take()).await;
        if let Err(failure) = &result {
            if request.method != "pairing.confirm" || failure.code != ErrorCode::PermissionDenied {
                self.notify_failure(failure);
            }
        }
        let deferred = defer && result.is_ok();
        if deferred {
            self.pending_join_id = Some(request.id);
        }
        self.pump_peer_events().await;
        if deferred {
            None
        } else {
            Some(match result {
                Ok(result) => Response::success(request.id, result),
                Err(error) => Response::failure(request.id, error),
            })
        }
    }

    async fn dispatch(&mut self, method: &str, value: Value) -> Result<Value, IpcError> {
        match method {
            "get_state" => {
                let _: EmptyParams = params(value)?;
                serde_json::to_value(&self.state)
                    .map_err(|_| error(ErrorCode::Internal, "could not encode state"))
            }
            "set_settings" => {
                let patch: SetSettingsParams = params(value)?;
                if !patch.patch.is_object() {
                    return Err(error(ErrorCode::InvalidParams, "patch must be an object"));
                }
                let mut next = self.state.clone();
                let mut settings = serde_json::to_value(&next.settings)
                    .map_err(|_| error(ErrorCode::Internal, "could not encode settings"))?;
                merge_patch(&mut settings, &patch.patch);
                next.settings = serde_json::from_value(settings)
                    .map_err(|_| error(ErrorCode::InvalidParams, "invalid settings"))?;
                validate_settings(&next.settings)?;
                let arrangement_changed = next.settings.display != self.state.settings.display;
                if arrangement_changed {
                    let (placements, offset) =
                        crate::arrangement::normalize(&next.settings.display.arrangement);
                    next.settings.display.arrangement = placements;
                    let native = self.platform.input_backend().monitors().map_err(|_| {
                        error(
                            ErrorCode::Internal,
                            "Could not read this computer's screens.",
                        )
                    })?;
                    next.self_info.monitors =
                        crate::arrangement::arrange(&native, &next.settings.display.arrangement);
                    self.native_monitors = native;
                    // Keep the screens where the person dropped them: the arrangement's top-left moved by `offset`.
                    let me = next.self_info.device_id.clone();
                    if let Some(device) = next.layout.devices.iter_mut().find(|d| d.device_id == me)
                    {
                        device.x += offset.x;
                        device.y += offset.y;
                    }
                    next.layout = reconcile_layout(&next.layout.devices, &next)?;
                }
                if let Some(manager) = &self.native_manager {
                    if next.settings.network.port != self.state.settings.network.port {
                        manager.update_listen_port(next.settings.network.port).map_err(|_| {
                            error(ErrorCode::InvalidParams, "Changing the listening port requires a restart. Launch Glide with --port and the same data directory to keep paired devices.")
                        })?;
                    }
                }
                self.end_forwarding("settings_changed").await?;
                if !next.settings.network.discovery {
                    next.discovered.clear();
                }
                let previous_autostart = self.state.settings.startup.launch_at_login;
                let previous_discovery = self.state.settings.network.discovery;
                let previous_name = self.state.self_info.name.clone();
                self.set_autostart(next.settings.startup.launch_at_login)
                    .map_err(|_| {
                        error(ErrorCode::PermissionDenied, "could not set launch at login")
                    })?;
                if let Some(manager) = &self.native_manager {
                    if let Err(failure) = manager.set_discovery(next.settings.network.discovery) {
                        let _ = self.set_autostart(previous_autostart);
                        return Err(startup::native_error(failure));
                    }
                    if let Err(failure) = manager.update_local_metadata(
                        next.settings.device_name.clone(),
                        next.self_info.monitors.clone(),
                    ) {
                        let _ = self.set_autostart(previous_autostart);
                        let _ = manager.set_discovery(previous_discovery);
                        return Err(startup::native_error(failure));
                    }
                }
                next.self_info.name = next.settings.device_name.clone();
                next.self_info.listen_port = next.settings.network.port;
                let layout_changed = next.layout != self.state.layout;
                if let Err(error) = self.apply(next) {
                    let _ = self.set_autostart(previous_autostart);
                    if let Some(manager) = &self.native_manager {
                        let _ = manager.set_discovery(previous_discovery);
                        if manager
                            .update_local_metadata(
                                previous_name,
                                self.state.self_info.monitors.clone(),
                            )
                            .is_err()
                        {
                            self.shutdown = true;
                        }
                    }
                    return Err(error);
                }
                if layout_changed {
                    self.broadcast_layout().await;
                }
                Ok(json!({}))
            }
            "set_sharing" => {
                let patch: SetSharingParams = params(value)?;
                self.end_forwarding(if patch.enabled {
                    "sharing_changed"
                } else {
                    "sharing_disabled"
                })
                .await?;
                let mut next = self.state.clone();
                next.sharing_enabled = patch.enabled;
                self.apply(next)?;
                Ok(json!({}))
            }
            "set_layout" => {
                let layout: SetLayoutParams = params(value)?;
                validate_layout(&layout.devices, &self.state)?;
                self.end_forwarding("layout_changed").await?;
                let mut next = self.state.clone();
                next.layout = Layout {
                    devices: layout.devices,
                };
                self.commit_layout(next, None)?;
                self.broadcast_layout().await;
                Ok(json!({}))
            }
            "pairing.start_host" => {
                let _: EmptyParams = params(value)?;
                let host = self.manager.pair_host(now_ms()).await?;
                Ok(json!({"code":host.code.as_str(),"expires_at_ms":host.expires_at_ms}))
            }
            "pairing.cancel_host" => {
                let _: EmptyParams = params(value)?;
                self.manager.cancel_pair_host().await?;
                self.abort_pairing(error(ErrorCode::PermissionDenied, "pairing cancelled"));
                Ok(json!({}))
            }
            "pairing.join" => {
                let join: PairingJoinParams = params(value)?;
                if !join.has_single_target()
                    || join.code.len() != 6
                    || !join.code.bytes().all(|byte| byte.is_ascii_digit())
                {
                    return Err(error(
                        ErrorCode::InvalidParams,
                        "provide one target and a six-digit code",
                    ));
                }
                let target = match (join.address, join.device_id) {
                    (Some(address), None) => PairTarget::Address(address),
                    (None, Some(id)) => PairTarget::DeviceId(id),
                    _ => {
                        return Err(error(
                            ErrorCode::InvalidParams,
                            "provide exactly one target",
                        ))
                    }
                };
                if let Some(manager) = self.native_manager.clone() {
                    // PAKE/dial timeouts must not pause capture, escape or established links.
                    self.join_job = Some(tokio::spawn(async move {
                        manager.pair_join(target, &join.code, now_ms()).await
                    }));
                    return Ok(json!({}));
                }
                match self.manager.pair_join(target, &join.code, now_ms()).await {
                    Ok(session) => {
                        validate_peer(&session.peer)?;
                        self.verification_deadline = Some(session.verification.expires_at_ms);
                        Ok(json!({}))
                    }
                    Err(error) => {
                        self.events.push(Event::PairingResult(PairingResult {
                            ok: false,
                            device_id: None,
                            error: Some(error.clone()),
                        }));
                        Err(error)
                    }
                }
            }
            "pairing.confirm" => {
                let confirm: PairingConfirmParams = params(value)?;
                match self
                    .manager
                    .confirm_pairing(confirm.accepted, now_ms())
                    .await
                {
                    Ok(Some(peer)) => {
                        self.complete_pairing(peer).await?;
                        Ok(json!({}))
                    }
                    Ok(None) => Ok(json!({})),
                    Err(error) => {
                        self.abort_pairing(error.clone());
                        Err(error)
                    }
                }
            }
            "peer.add_manual" => {
                let manual: PeerAddManualParams = params(value)?;
                let peer = self.manager.add_manual(&manual.address).await?;
                if self
                    .state
                    .peers
                    .iter()
                    .any(|p| p.device_id == peer.device_id)
                {
                    return Ok(json!({}));
                }
                if self.state.discovered.len() >= 32
                    && !self
                        .state
                        .discovered
                        .iter()
                        .any(|p| p.device_id == peer.device_id)
                {
                    return Err(error(ErrorCode::InvalidParams, "discovery limit reached"));
                }
                self.state
                    .discovered
                    .retain(|existing| existing.device_id != peer.device_id);
                self.state.discovered.push(peer);
                self.dirty = true;
                Ok(json!({}))
            }
            "peer.unpair" => {
                let unpair: PeerUnpairParams = params(value)?;
                if !self
                    .state
                    .peers
                    .iter()
                    .any(|peer| peer.device_id == unpair.device_id)
                {
                    return Err(error(ErrorCode::NotPaired, "peer is not paired"));
                }
                self.end_forwarding("unpaired").await?;
                let mut previous = self.state.clone();
                if let Some(peer) = previous
                    .peers
                    .iter_mut()
                    .find(|peer| peer.device_id == unpair.device_id)
                {
                    peer.online = false;
                    peer.connection = Connection::Offline;
                }
                let mut next = self.state.clone();
                next.peers.retain(|peer| peer.device_id != unpair.device_id);
                next.layout
                    .devices
                    .retain(|device| device.device_id != unpair.device_id);
                self.apply(next)?;
                let _ = self.link.close(&unpair.device_id).await;
                if let Err(failure) = self.manager.unpair(&unpair.device_id).await {
                    if self.native_manager.is_some() {
                        // Native revocation removes live trust before reporting durable-storage errors.
                        self.shutdown = true;
                        return Err(failure);
                    }
                    if self.apply(previous).is_err() {
                        self.shutdown = true;
                        return Err(error(
                            ErrorCode::Internal,
                            "could not restore peer after failed revocation; daemon must exit",
                        ));
                    }
                    return Err(failure);
                }
                if self.state.active_device_id == unpair.device_id {
                    self.end_forwarding("unpaired").await?;
                }
                Ok(json!({}))
            }
            "diag.cursor" => {
                let _: EmptyParams = params(value)?;
                if let Some(timings) = self.platform.input_backend().take_move_timings() {
                    self.diag.add_platform(timings);
                }
                Ok(json!({ "text": self.diag.report(&self.state) }))
            }
            "peer.arrange" => {
                let arrange: PeerArrangeParams = params(value)?;
                let peer = self
                    .state
                    .peers
                    .iter()
                    .find(|p| p.device_id == arrange.device_id)
                    .ok_or_else(|| error(ErrorCode::InvalidParams, "unknown device"))?;
                let name = peer.name.clone();
                if !peer.online {
                    return Err(error(
                        ErrorCode::Unreachable,
                        format!("{name} is not connected right now."),
                    ));
                }
                if !glide_proto::wire::understands_details(peer.app_version.as_deref()) {
                    return Err(error(
                        ErrorCode::Unreachable,
                        format!("Update Glide on {name} to arrange its screens from here."),
                    ));
                }
                if arrange.arrangement.len() > 32
                    || arrange.arrangement.iter().any(|p| {
                        p.monitor_id.is_empty()
                            || p.monitor_id.len() > 64
                            || !p.x.is_finite()
                            || !p.y.is_finite()
                            || p.x.abs() > 1.0e6
                            || p.y.abs() > 1.0e6
                    })
                {
                    return Err(error(
                        ErrorCode::InvalidParams,
                        "invalid screen arrangement",
                    ));
                }
                let screens = arrange
                    .arrangement
                    .into_iter()
                    .map(|p| glide_proto::wire::ArrangedScreen {
                        monitor_id: p.monitor_id,
                        x: p.x,
                        y: p.y,
                    })
                    .collect();
                self.send_reliable(
                    &arrange.device_id,
                    glide_proto::wire::WireMessage::Control(
                        glide_proto::wire::ControlMessage::Arrange(glide_proto::wire::Arrange {
                            screens,
                        }),
                    ),
                )
                .await
                .map_err(|_| {
                    error(
                        ErrorCode::Unreachable,
                        format!("Could not reach {name}. Try again in a moment."),
                    )
                })?;
                Ok(json!({}))
            }
            "peer.wake" => {
                let wake: PeerWakeParams = params(value)?;
                let peer = self
                    .state
                    .peers
                    .iter()
                    .find(|p| p.device_id == wake.device_id)
                    .ok_or_else(|| error(ErrorCode::InvalidParams, "unknown device"))?;
                if peer.online {
                    return Ok(json!({}));
                }
                let mac = peer.wake_mac.clone().ok_or_else(|| {
                    error(
                        ErrorCode::Unreachable,
                        "Glide learns how to wake this computer the next time both are connected.",
                    )
                })?;
                crate::wake::send(&mac, crate::wake::peer_ip(peer)).map_err(|_| {
                    error(
                        ErrorCode::Unreachable,
                        "The wake-up signal could not be sent on this network.",
                    )
                })?;
                self.wake_sent.insert(wake.device_id, Instant::now());
                Ok(json!({}))
            }
            "peer.configure" => {
                let configure: PeerConfigureParams = params(value)?;
                let mut next = self.state.clone();
                let peer = next
                    .peers
                    .iter_mut()
                    .find(|peer| peer.device_id == configure.device_id)
                    .ok_or_else(|| error(ErrorCode::NotPaired, "peer is not paired"))?;
                if let Some(enabled) = configure.clipboard_enabled {
                    peer.clipboard_enabled = enabled;
                }
                self.end_forwarding("peer_settings_changed").await?;
                self.apply(next)?;
                Ok(json!({}))
            }
            "return_home" => {
                let _: EmptyParams = params(value)?;
                self.end_forwarding("return_home").await?;
                Ok(json!({}))
            }
            "transfer.cancel" => {
                let cancel: TransferIdParams = params(value)?;
                let transfer = self
                    .state
                    .transfers
                    .iter_mut()
                    .find(|transfer| transfer.id == cancel.id)
                    .ok_or_else(|| error(ErrorCode::InvalidParams, "unknown transfer"))?;
                if matches!(
                    transfer.state,
                    TransferState::Done | TransferState::Failed | TransferState::Cancelled
                ) {
                    return Err(error(
                        ErrorCode::InvalidParams,
                        "transfer has already ended",
                    ));
                }
                transfer.state = TransferState::Cancelled;
                self.clipboard.cancel_confirmation(&cancel.id);
                self.dirty = true;
                Ok(json!({}))
            }
            "transfer.confirm" => {
                let confirm: TransferConfirmParams = params(value)?;
                let index = self
                    .state
                    .transfers
                    .iter()
                    .position(|transfer| transfer.id == confirm.id)
                    .ok_or_else(|| error(ErrorCode::InvalidParams, "unknown transfer"))?;
                if self.state.transfers[index].state != TransferState::AwaitingConfirm {
                    return Err(error(
                        ErrorCode::InvalidParams,
                        "transfer is not awaiting confirmation",
                    ));
                }
                self.confirm_clipboard(&confirm.id, confirm.accept)?;
                self.state.transfers[index].state = if confirm.accept {
                    TransferState::Queued
                } else {
                    TransferState::Cancelled
                };
                self.dirty = true;
                Ok(json!({}))
            }
            "permissions.open_settings" => {
                let open: OpenPermissionSettingsParams = params(value)?;
                self.platform
                    .open_permission_settings(open.kind)
                    .map_err(|_| {
                        error(
                            ErrorCode::PermissionDenied,
                            "could not open permission settings",
                        )
                    })?;
                Ok(json!({}))
            }
            "app.shutdown" => {
                let _: EmptyParams = params(value)?;
                self.cancel_permission_requests();
                // end_forwarding restores local input before awaiting remote delivery.
                // A stalled peer must not delay the shutdown acknowledgement.
                if let Ok(result) = tokio::time::timeout(
                    Duration::from_millis(100),
                    self.end_forwarding("shutdown"),
                )
                .await
                {
                    result?;
                }
                self.abort_pairing(error(ErrorCode::PermissionDenied, "daemon shutting down"));
                self.shutdown = true;
                Ok(json!({}))
            }
            _ => Err(error(ErrorCode::InvalidParams, "unknown method")),
        }
    }

    /// Emit at most one state snapshot per 100 ms control turn (10/s).
    pub async fn tick(&mut self) -> anyhow::Result<()> {
        self.flush_pending_move()
            .await
            .map_err(|failure| anyhow::anyhow!(failure.message))?;
        if self.join_job.as_ref().is_some_and(|job| job.is_finished()) {
            if let Some(job) = self.join_job.take() {
                match job.await {
                    Ok(Ok(session)) => {
                        if let Err(failure) = validate_peer(&session.peer) {
                            let _ = self.manager.cancel_pair_host().await;
                            self.abort_pairing(failure);
                        } else if self.pending_join_id.is_some() {
                            self.verification_deadline = Some(session.verification.expires_at_ms);
                        }
                    }
                    Ok(Err(failure)) => self.abort_pairing(failure),
                    Err(_) => {
                        self.abort_pairing(error(ErrorCode::Internal, "Pairing worker failed."))
                    }
                }
            }
        }
        self.poll_clipboard().await;
        let mut capture_lost = false;
        for status in self.capture_status_rx.try_iter() {
            if status == glide_platform::CaptureStatus::PermissionLost {
                self.capture_pending = true;
                self.next_permission_check = Instant::now();
            }
            if status == glide_platform::CaptureStatus::SecureInput(false) && self.capture_pending {
                self.next_permission_check = Instant::now();
            }
            capture_lost |= status != glide_platform::CaptureStatus::SecureInput(false);
        }
        if capture_lost || self.platform.input_backend().secure_input_enabled() == Some(true) {
            self.end_forwarding("capture_lost")
                .await
                .map_err(|error| anyhow::anyhow!(error.message))?;
        }
        self.poll_permissions(Instant::now()).await;
        self.learn_own_model().await;
        let found = std::mem::take(&mut *self.wake_found.lock().unwrap_or_else(|e| e.into_inner()));
        for (device_id, mac) in found {
            if let Some(peer) = self
                .state
                .peers
                .iter_mut()
                .find(|p| p.device_id == device_id)
            {
                if peer.wake_mac.as_deref() != Some(mac.as_str()) {
                    peer.wake_mac = Some(mac);
                    let _ = self.persist(&self.state.clone());
                    self.dirty = true;
                }
            }
        }
        let retry_due = self.monitors_retry.is_some_and(|due| Instant::now() >= due);
        if self.monitor_rx.try_recv().is_ok() || retry_due {
            self.end_forwarding("monitors_changed")
                .await
                .map_err(|error| anyhow::anyhow!(error.message))?;
            while self.monitor_rx.try_recv().is_ok() {}
            // Displays can vanish for a while (screens asleep, lid closed, locked). Keep the current layout and
            // look again shortly instead of stopping the engine, which then could not restart either.
            match self.platform.input_backend().monitors() {
                Ok(monitors) => {
                    self.monitors_retry = None;
                    self.native_monitors = monitors;
                }
                Err(error) => {
                    tracing::warn!(?error, "displays unavailable; retrying");
                    self.monitors_retry = Some(Instant::now() + Duration::from_secs(1));
                    return Ok(());
                }
            }
            self.state.self_info.monitors = crate::arrangement::arrange(
                &self.native_monitors,
                &self.state.settings.display.arrangement,
            );
            if let Some(manager) = &self.native_manager {
                if manager
                    .update_local_metadata(
                        self.state.self_info.name.clone(),
                        self.state.self_info.monitors.clone(),
                    )
                    .is_err()
                {
                    self.shutdown = true;
                    anyhow::bail!("Could not publish changed displays; restart Glide.");
                }
            }
            if let Err(failure) = self.rebuild_desktop() {
                tracing::warn!(message = %failure.message, "desktop not rebuilt after a display change; retrying");
                self.monitors_retry = Some(Instant::now() + Duration::from_secs(1));
                return Ok(());
            }
            self.dirty = true;
        }
        if self.capture_sink.take_overflow() {
            self.end_forwarding("capture_overflow")
                .await
                .map_err(|error| anyhow::anyhow!(error.message))?;
        }
        let now = Instant::now();
        self.cursor_watchdog(now);
        if self
            .verification_deadline
            .is_some_and(|deadline| now_ms() >= deadline)
        {
            let _ = self.manager.cancel_pair_host().await;
            self.abort_pairing(error(
                ErrorCode::CodeExpired,
                "pairing verification expired",
            ));
        }
        if self.receiving_from.is_some()
            && (now.duration_since(self.last_receive) >= Duration::from_millis(1500)
                || self.state.permissions.injection == PermissionStatus::Denied)
        {
            self.end_forwarding("receiver_timeout_or_permission")
                .await
                .map_err(|error| anyhow::anyhow!(error.message))?;
        }
        let events = self.engine.tick(now);
        self.process_engine_events(events)
            .await
            .map_err(|error| anyhow::anyhow!(error.message))?;
        if now.duration_since(self.last_heartbeat_sent) >= Duration::from_millis(500) {
            self.last_heartbeat_sent = now;
            self.outgoing_seq = self.outgoing_seq.wrapping_add(1);
            for peer in self.state.peers.iter().filter(|peer| peer.online) {
                let _ = self
                    .send_reliable(
                        &peer.device_id,
                        glide_proto::wire::WireMessage::Control(
                            glide_proto::wire::ControlMessage::Heartbeat(
                                glide_proto::wire::Heartbeat {
                                    seq: self.outgoing_seq,
                                    ts: now_ms(),
                                },
                            ),
                        ),
                    )
                    .await;
            }
        }
        self.pump_peer_events().await;
        if self.native_manager.is_some()
            && self.state.settings.network.discovery
            && self.last_discovery_poll.elapsed() >= Duration::from_secs(1)
        {
            self.last_discovery_poll = Instant::now();
            if let Ok(mut peers) = self.manager.discover().await {
                peers.retain(|p| {
                    p.device_id != self.state.self_info.device_id
                        && !self
                            .state
                            .peers
                            .iter()
                            .any(|paired| paired.device_id == p.device_id)
                });
                peers.truncate(32);
                peers.sort_by(|a, b| a.device_id.cmp(&b.device_id));
                if peers != self.state.discovered {
                    self.state.discovered = peers;
                    self.dirty = true;
                }
            }
        }
        if self.purge_job.as_ref().is_some_and(|job| job.is_finished()) {
            if let Some(job) = self.purge_job.take() {
                if !matches!(job.await, Ok(Ok(()))) {
                    self.events.push(Event::Notification(Notification {
                        level: "warning".into(), title: "Staging cleanup stopped".into(),
                        body: "Could not purge stale transfer data. Check data-directory permissions and restart Glide.".into(), action: None,
                    }));
                }
            }
        }
        if self.dirty {
            self.events.push(Event::State(Box::new(self.snapshot())));
            self.dirty = false;
        }
        Ok(())
    }

    pub async fn shutdown(&mut self) -> anyhow::Result<()> {
        // Restore local control synchronously before waiting for any network or transfer work.
        let input = self.platform.input_backend();
        let _ = input.set_cursor_visible(true);
        let local = input.set_mode(CaptureMode::Local);
        let released = input.release_all();
        let cleanup = tokio::time::timeout(Duration::from_secs(1), self.shutdown_cleanup()).await;
        local?;
        released?;
        match cleanup {
            Ok(result) => result,
            Err(_) => {
                tracing::debug!("shutdown cleanup deadline reached; drop guards retain safety");
                Ok(())
            }
        }
    }

    async fn shutdown_cleanup(&mut self) -> anyhow::Result<()> {
        self.cancel_permission_requests();
        self.discard_incoming_clipboard();
        self.clipboard.invalidate();
        for transfer in &mut self.state.transfers {
            if matches!(
                transfer.state,
                TransferState::Queued | TransferState::AwaitingConfirm | TransferState::Active
            ) {
                transfer.state = TransferState::Cancelled;
            }
        }
        self.purge_cancel.cancel();
        self.abort_pairing(error(ErrorCode::PermissionDenied, "daemon shutting down"));
        let home = self.end_forwarding("shutdown").await;
        self.clipboard_shutdown().await;
        if let Some(job) = self.accept_job.take() {
            job.abort();
            let _ = job.await;
        }
        let cancel = self.manager.cancel_pair_host().await;
        for peer in &self.state.peers {
            let _ = self.link.close(&peer.device_id).await;
        }
        if let Some(job) = self.purge_job.take() {
            let mut job = job;
            if tokio::time::timeout(Duration::from_secs(1), &mut job)
                .await
                .is_err()
            {
                job.abort();
            }
        }
        home.map_err(|error| anyhow::anyhow!(error.message))?;
        cancel.map_err(|error| anyhow::anyhow!(error.message))?;
        Ok(())
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        self.cancel_permission_requests();
        if let Some(job) = self.accept_job.take() {
            job.abort();
        }
        if let Some(job) = self.join_job.take() {
            job.abort();
        }
        self.clipboard.invalidate();
        self.purge_cancel.cancel();
        let input = self.platform.input_backend();
        let _ = input.set_cursor_visible(true);
        let _ = input.set_mode(CaptureMode::Local);
        let _ = input.release_all();
    }
}

fn validate_settings(settings: &Settings) -> Result<(), IpcError> {
    if settings.display.arrangement.len() > 32
        || settings.display.arrangement.iter().any(|p| {
            p.monitor_id.is_empty()
                || p.monitor_id.len() > 128
                || !p.x.is_finite()
                || !p.y.is_finite()
                || p.x.abs() > 1_000_000.0
                || p.y.abs() > 1_000_000.0
        })
    {
        return Err(error(
            ErrorCode::InvalidParams,
            "invalid screen arrangement",
        ));
    }
    Hotkey::parse(&settings.hotkeys.return_home)?;
    Hotkey::parse(&settings.hotkeys.toggle_sharing)?;
    if settings.device_name.trim().is_empty()
        || settings.device_name.len() > 128
        || settings.device_name.chars().any(char::is_control)
        || settings.network.port == 0
        || settings.switching.edge_delay_ms > 60_000
        || !settings.switching.corner_dead_zone_px.is_finite()
        || !(0.0..=1000.0).contains(&settings.switching.corner_dead_zone_px)
        || !settings.switching.pointer_speed.is_finite()
        || !(0.25..=4.0).contains(&settings.switching.pointer_speed)
        || !settings.switching.pointer_acceleration.is_finite()
        || !(0.0..=4.0).contains(&settings.switching.pointer_acceleration)
        || settings.clipboard.max_auto_mb > 1_048_576
        || settings.hotkeys.return_home.len() > 128
        || settings.hotkeys.toggle_sharing.len() > 128
        || settings.hotkeys.return_home.is_empty()
        || settings.hotkeys.toggle_sharing.is_empty()
    {
        return Err(error(
            ErrorCode::InvalidParams,
            "settings are outside supported limits",
        ));
    }
    Ok(())
}

fn build_engine(
    state: &State,
    platform: &dyn Platform,
) -> anyhow::Result<crate::layout::EdgeEngine> {
    let native = platform.input_backend().monitors()?;
    let arranged = crate::arrangement::arrange(&native, &state.settings.display.arrangement);
    let mut monitors = HashMap::new();
    monitors.insert(state.self_info.device_id.clone(), arranged.clone());
    for peer in state.peers.iter().filter(|peer| peer.online) {
        monitors.insert(peer.device_id.clone(), peer.monitors.clone());
    }
    let desktop = crate::layout::Desktop::from_layout(&state.layout, &monitors)?;
    let local = crate::arrangement::to_arranged(
        &native,
        &arranged,
        platform.input_backend().local_cursor_pos()?,
    );
    let cursor = desktop
        .device_to_global_logical(&state.self_info.device_id, local)
        .ok_or_else(|| anyhow::anyhow!("local device has no monitor"))?;
    let mut engine = crate::layout::EdgeEngine::new(
        desktop,
        state.self_info.device_id.clone(),
        cursor,
        crate::layout::EdgeSettings {
            edge_delay: Duration::from_millis(u64::from(state.settings.switching.edge_delay_ms)),
            corner_dead_zone_px: state.settings.switching.corner_dead_zone_px,
            double_tap: state.settings.switching.double_tap,
        },
        Instant::now(),
    )?;
    engine.set_forward_speed(state.settings.switching.pointer_speed);
    // Windows reports raw, unaccelerated mouse movement; a Mac already accelerates what it captures.
    if state.self_info.os == glide_platform::Os::Windows {
        engine.set_forward_acceleration(state.settings.switching.pointer_acceleration);
    }
    platform
        .input_backend()
        .set_move_smoothing(state.settings.switching.smooth_moves);
    Ok(engine)
}

fn validate_layout(devices: &[LayoutDevice], state: &State) -> Result<(), IpcError> {
    let mut ids = HashSet::new();
    if devices.is_empty()
        || devices.len() > 32
        || !devices
            .iter()
            .any(|device| device.device_id == state.self_info.device_id)
    {
        return Err(error(
            ErrorCode::InvalidParams,
            "layout must include this device",
        ));
    }
    for device in devices {
        if !ids.insert(&device.device_id)
            || !device.x.is_finite()
            || !device.y.is_finite()
            || device.x.abs() > 1_000_000.0
            || device.y.abs() > 1_000_000.0
            || (device.device_id != state.self_info.device_id
                && !state
                    .peers
                    .iter()
                    .any(|peer| peer.device_id == device.device_id))
        {
            return Err(error(
                ErrorCode::InvalidParams,
                "invalid or unknown layout device",
            ));
        }
    }
    let mut monitors = HashMap::from([(
        state.self_info.device_id.clone(),
        state.self_info.monitors.clone(),
    )]);
    for peer in state.peers.iter().filter(|p| !p.monitors.is_empty()) {
        monitors.insert(peer.device_id.clone(), peer.monitors.clone());
    }
    let known = Layout {
        devices: devices
            .iter()
            .filter(|d| monitors.contains_key(&d.device_id))
            .cloned()
            .collect(),
    };
    crate::layout::Desktop::from_layout(&known, &monitors).map_err(|_| {
        error(
            ErrorCode::InvalidParams,
            "layout contains overlapping or invalid displays",
        )
    })?;
    Ok(())
}

fn validate_peer(peer: &Peer) -> Result<(), IpcError> {
    if peer.device_id.len() != 64
        || !peer
            .device_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || peer.name.trim().is_empty()
        || peer.name.len() > 128
        || peer.monitors.len() > 64
        || peer.fingerprint.len() > 256
        || peer.monitors.iter().any(|monitor| {
            !monitor.x.is_finite()
                || !monitor.y.is_finite()
                || !monitor.w.is_finite()
                || !monitor.h.is_finite()
                || !monitor.scale.is_finite()
                || monitor.w <= 0.0
                || monitor.h <= 0.0
                || monitor.scale <= 0.0
        })
    {
        return Err(error(ErrorCode::InvalidParams, "invalid peer metadata"));
    }
    Ok(())
}
