use crate::{
    validate_clipboard_bundle, ClipboardAdmission, ClipboardChangeToken, ClipboardPublish,
    ClipboardSnapshot,
};
use crate::{
    BackendError, Button, CaptureMode, CaptureStatus, ClipboardBackend, ClipboardContent,
    ClipboardData, ClipboardEvent, ClipboardFormat, ClipboardMarker, ClipboardSensitivity,
    FileList, InputBackend, InputEvent, InputEventKind, InputSink, Key, Monitor, Os,
    PermissionKind, Permissions, Platform, Point,
};
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Clone, Debug)]
pub enum ClipboardRace {
    LocalCopy(Vec<ClipboardContent>),
    RevokeAdmission,
    NativeFailure { formats_written: usize },
}

/// Operations that can be configured to fail once in `MockInput`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum InputOperation {
    /// Starting or replacing the capture sink.
    StartCapture,
    /// Changing local/swallow mode.
    SetMode,
    /// Changing inactive cursor visibility.
    SetCursorVisible,
    /// Injecting one event.
    Inject,
    /// Releasing all injected keys and buttons.
    ReleaseAll,
    /// Reading the monitor snapshot.
    Monitors,
    /// Reading the local cursor position.
    LocalCursorPos,
}

/// Operations that can be configured to fail once in `MockClipboard`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ClipboardOperation {
    ReadSnapshot,
    PublishSnapshot,
    /// Reading one clipboard representation.
    Read,
    /// Writing one clipboard representation.
    Write,
    /// Listing available formats.
    Formats,
    /// Reading the native file list.
    ReadFiles,
    /// Writing the native file list.
    WriteFiles,
    /// Registering a delayed representation.
    SetDelayedRender,
    /// Fulfilling a delayed representation.
    FulfillDelayedRender,
    /// Cancelling delayed representations.
    CancelDelayedRender,
}

/// Operations that can be configured to fail once in `MockPlatform`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PlatformOperation {
    /// Reading the configured data directory.
    DataDir,
    /// Reading launch-at-login state.
    AutostartEnabled,
    /// Changing launch-at-login state.
    SetAutostartEnabled,
    /// Opening a permission pane.
    OpenPermissionSettings,
}

struct MockInputState {
    capture_sink: Option<InputSink>,
    mode: CaptureMode,
    cursor_visible: bool,
    display_wakes: u32,
    monitors: Vec<Monitor>,
    cursor: Point,
    permissions: Permissions,
    permission_script: VecDeque<Permissions>,
    permission_checks: usize,
    permission_requests: usize,
    capture_starts: usize,
    secure_input: Option<bool>,
    injected_events: Vec<InputEvent>,
    held_keys: HashSet<Key>,
    held_buttons: HashSet<Button>,
    release_all_count: usize,
    failures: HashMap<InputOperation, VecDeque<BackendError>>,
}

/// Scriptable input backend for tests and `--mock-backends` development.
#[derive(Clone)]
pub struct MockInput {
    state: Arc<Mutex<MockInputState>>,
    monitor_sender: Sender<Vec<Monitor>>,
    monitor_receiver: Receiver<Vec<Monitor>>,
    status_sender: Sender<CaptureStatus>,
    status_receiver: Receiver<CaptureStatus>,
}

impl Default for MockInput {
    fn default() -> Self {
        let (monitor_sender, monitor_receiver) = bounded(16);
        let (status_sender, status_receiver) = bounded(16);
        Self {
            state: Arc::new(Mutex::new(MockInputState {
                capture_sink: None,
                mode: CaptureMode::Local,
                cursor_visible: true,
                display_wakes: 0,
                monitors: vec![Monitor {
                    id: "mock-display".into(),
                    x: 0.0,
                    y: 0.0,
                    w: 1920.0,
                    h: 1080.0,
                    scale: 1.0,
                    primary: true,
                }],
                cursor: Point { x: 0.0, y: 0.0 },
                permissions: Permissions::default(),
                permission_script: VecDeque::new(),
                permission_checks: 0,
                permission_requests: 0,
                capture_starts: 0,
                secure_input: None,
                injected_events: Vec::new(),
                held_keys: HashSet::new(),
                held_buttons: HashSet::new(),
                release_all_count: 0,
                failures: HashMap::new(),
            })),
            monitor_sender,
            monitor_receiver,
            status_sender,
            status_receiver,
        }
    }
}

impl MockInput {
    /// Current requested OS cursor visibility, for transition tests.
    pub fn cursor_visible(&self) -> Result<bool, BackendError> {
        Ok(self.lock()?.cursor_visible)
    }
    /// How many times the screen was asked to wake.
    pub fn display_wakes(&self) -> Result<u32, BackendError> {
        Ok(self.lock()?.display_wakes)
    }
    /// Scripts a native transition, including immediate local fallback and an
    /// overflow latch that survives pressure on the bounded status stream.
    pub fn script_capture_status(&self, status: CaptureStatus) -> Result<(), BackendError> {
        let mut state = self.lock()?;
        if let CaptureStatus::SecureInput(enabled) = status {
            state.secure_input = Some(enabled);
        }
        if status != CaptureStatus::SecureInput(false) {
            state.mode = CaptureMode::Local;
            if let Some(sink) = &state.capture_sink {
                sink.mark_overflow();
            }
        }
        let _ = self.status_sender.try_send(status);
        Ok(())
    }
    fn lock(&self) -> Result<MutexGuard<'_, MockInputState>, BackendError> {
        self.state
            .lock()
            .map_err(|_| BackendError::StateUnavailable)
    }

    fn check_failure(
        state: &mut MockInputState,
        operation: InputOperation,
    ) -> Result<(), BackendError> {
        if let Some(queue) = state.failures.get_mut(&operation) {
            if let Some(error) = queue.pop_front() {
                return Err(error);
            }
        }
        Ok(())
    }

    /// Queues one failure for the next call to `operation`.
    pub fn fail_next(
        &self,
        operation: InputOperation,
        error: BackendError,
    ) -> Result<(), BackendError> {
        self.lock()?
            .failures
            .entry(operation)
            .or_default()
            .push_back(error);
        Ok(())
    }

    /// Supplies the monitor snapshot returned by future reads.
    pub fn set_monitors(&self, monitors: Vec<Monitor>) -> Result<(), BackendError> {
        {
            let mut state = self.lock()?;
            state.monitors = monitors.clone();
        }
        match self.monitor_sender.try_send(monitors.clone()) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                while self.monitor_receiver.try_recv().is_ok() {}
                let _ = self.monitor_sender.try_send(monitors);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
        Ok(())
    }

    /// Sets the local cursor position returned by future reads.
    pub fn set_cursor(&self, position: Point) -> Result<(), BackendError> {
        self.lock()?.cursor = position;
        Ok(())
    }

    /// Sets the permission snapshot returned by the mock.
    pub fn set_permissions(&self, permissions: Permissions) -> Result<(), BackendError> {
        self.lock()?.permissions = permissions;
        Ok(())
    }

    /// Each permission query consumes one snapshot, then retains the last one.
    pub fn script_permissions(&self, snapshots: Vec<Permissions>) -> Result<(), BackendError> {
        self.lock()?.permission_script = snapshots.into();
        Ok(())
    }

    /// Permission checks, explicit requests and capture attempts, including failures.
    pub fn permission_call_counts(&self) -> Result<(usize, usize, usize), BackendError> {
        let state = self.lock()?;
        Ok((
            state.permission_checks,
            state.permission_requests,
            state.capture_starts,
        ))
    }

    /// Sends a simulated native sample through the currently installed capture sink.
    pub fn emit(&self, event: InputEvent) -> Result<(), crate::InputSinkError> {
        let sink = match self.state.lock() {
            Ok(mut state) => {
                let cursor_update = match (event.kind, event.injected) {
                    (InputEventKind::PointerMoved { position, .. }, false) => Some(position),
                    _ => None,
                };
                if let Some(position) = cursor_update {
                    state.cursor = position;
                }
                state.capture_sink.clone()
            }
            Err(_) => return Err(crate::InputSinkError::Disconnected),
        };
        match sink {
            Some(sink) => match sink.try_push(event) {
                Ok(()) => Ok(()),
                Err(error) => {
                    if let Ok(mut state) = self.state.lock() {
                        state.mode = CaptureMode::Local;
                    }
                    Err(error)
                }
            },
            None => {
                if let Ok(mut state) = self.state.lock() {
                    state.mode = CaptureMode::Local;
                }
                Err(crate::InputSinkError::Disconnected)
            }
        }
    }

    /// Returns the most recently requested capture mode.
    pub fn mode(&self) -> Result<CaptureMode, BackendError> {
        Ok(self.lock()?.mode)
    }

    /// Returns and clears events accepted by `inject`.
    pub fn take_injected_events(&self) -> Result<Vec<InputEvent>, BackendError> {
        Ok(std::mem::take(&mut self.lock()?.injected_events))
    }

    /// Returns key usages currently held by injected events.
    pub fn held_keys(&self) -> Result<HashSet<Key>, BackendError> {
        Ok(self.lock()?.held_keys.clone())
    }

    /// Returns buttons currently held by injected events.
    pub fn held_buttons(&self) -> Result<HashSet<Button>, BackendError> {
        Ok(self.lock()?.held_buttons.clone())
    }

    /// Returns how often `release_all` completed successfully.
    pub fn release_all_count(&self) -> Result<usize, BackendError> {
        Ok(self.lock()?.release_all_count)
    }
}

impl InputBackend for MockInput {
    fn secure_input_enabled(&self) -> Option<bool> {
        self.state.lock().ok().and_then(|state| state.secure_input)
    }
    fn capture_status_changes(&self) -> Receiver<CaptureStatus> {
        self.status_receiver.clone()
    }
    fn start_capture(&self, sink: InputSink) -> Result<(), BackendError> {
        let mut state = self.lock()?;
        state.capture_starts += 1;
        Self::check_failure(&mut state, InputOperation::StartCapture)?;
        state.capture_sink = Some(sink);
        Ok(())
    }

    fn set_mode(&self, mode: CaptureMode) -> Result<(), BackendError> {
        let mut state = self.lock()?;
        Self::check_failure(&mut state, InputOperation::SetMode)?;
        state.mode = mode;
        Ok(())
    }

    fn set_cursor_visible(&self, visible: bool) -> Result<(), BackendError> {
        let mut state = self.lock()?;
        Self::check_failure(&mut state, InputOperation::SetCursorVisible)?;
        state.cursor_visible = visible;
        Ok(())
    }

    fn wake_display(&self) -> Result<(), BackendError> {
        self.lock()?.display_wakes += 1;
        Ok(())
    }

    fn inject(&self, mut event: InputEvent) -> Result<(), BackendError> {
        if let InputEventKind::PointerMoved { position, .. } = event.kind {
            if !position.x.is_finite() || !position.y.is_finite() {
                return Err(BackendError::InvalidInput(
                    "pointer position must be finite".into(),
                ));
            }
        }
        let mut state = self.lock()?;
        Self::check_failure(&mut state, InputOperation::Inject)?;
        if let InputEventKind::PointerMoved {
            ref mut position, ..
        } = event.kind
        {
            *position = crate::clamp_cursor_to_monitors(&state.monitors, *position)
                .ok_or(BackendError::Transient)?;
        }
        event.injected = true;
        match event.kind {
            InputEventKind::Key { key, down: true } => {
                state.held_keys.insert(key);
            }
            InputEventKind::Key { key, down: false } => {
                state.held_keys.remove(&key);
            }
            InputEventKind::Button { button, down: true } => {
                state.held_buttons.insert(button);
            }
            InputEventKind::Button {
                button,
                down: false,
            } => {
                state.held_buttons.remove(&button);
            }
            InputEventKind::PointerMoved { position, .. } => state.cursor = position,
            InputEventKind::Wheel { .. } => {}
        }
        state.injected_events.push(event);
        Ok(())
    }

    fn release_all(&self) -> Result<(), BackendError> {
        let mut state = self.lock()?;
        Self::check_failure(&mut state, InputOperation::ReleaseAll)?;
        state.held_keys.clear();
        state.held_buttons.clear();
        state.release_all_count += 1;
        Ok(())
    }

    fn monitors(&self) -> Result<Vec<Monitor>, BackendError> {
        let mut state = self.lock()?;
        Self::check_failure(&mut state, InputOperation::Monitors)?;
        Ok(state.monitors.clone())
    }

    fn monitor_changes(&self) -> Receiver<Vec<Monitor>> {
        self.monitor_receiver.clone()
    }

    fn local_cursor_pos(&self) -> Result<Point, BackendError> {
        let mut state = self.lock()?;
        Self::check_failure(&mut state, InputOperation::LocalCursorPos)?;
        Ok(state.cursor)
    }

    fn permissions(&self) -> Permissions {
        self.state
            .lock()
            .map(|mut state| {
                state.permission_checks += 1;
                if let Some(permissions) = state.permission_script.pop_front() {
                    state.permissions = permissions;
                }
                state.permissions
            })
            .unwrap_or_default()
    }

    fn request_permissions(&self) -> Permissions {
        if let Ok(mut state) = self.state.lock() {
            state.permission_requests += 1;
        }
        self.permissions()
    }
}

struct MockClipboardState {
    change_token: u64,
    marker: Option<ClipboardMarker>,
    races: VecDeque<ClipboardRace>,
    contents: HashMap<ClipboardFormat, ClipboardContent>,
    delayed: HashMap<ClipboardMarker, HashSet<ClipboardFormat>>,
    failures: HashMap<ClipboardOperation, VecDeque<BackendError>>,
}

/// Scriptable clipboard backend for tests and `--mock-backends` development.
#[derive(Clone)]
pub struct MockClipboard {
    state: Arc<Mutex<MockClipboardState>>,
    event_sender: Sender<ClipboardEvent>,
    event_receiver: Receiver<ClipboardEvent>,
}

impl Default for MockClipboard {
    fn default() -> Self {
        let (event_sender, event_receiver) = bounded(64);
        Self {
            state: Arc::new(Mutex::new(MockClipboardState {
                change_token: 0,
                marker: None,
                races: VecDeque::new(),
                contents: HashMap::new(),
                delayed: HashMap::new(),
                failures: HashMap::new(),
            })),
            event_sender,
            event_receiver,
        }
    }
}

impl MockClipboard {
    /// Execute a race after bundle preparation and before ownership admission.
    pub fn script_publish_race(&self, race: ClipboardRace) -> Result<(), BackendError> {
        self.lock()?.races.push_back(race);
        Ok(())
    }
    fn lock(&self) -> Result<MutexGuard<'_, MockClipboardState>, BackendError> {
        self.state
            .lock()
            .map_err(|_| BackendError::StateUnavailable)
    }

    fn check_failure(
        state: &mut MockClipboardState,
        operation: ClipboardOperation,
    ) -> Result<(), BackendError> {
        if let Some(queue) = state.failures.get_mut(&operation) {
            if let Some(error) = queue.pop_front() {
                return Err(error);
            }
        }
        Ok(())
    }

    fn emit(&self, event: ClipboardEvent) {
        match self.event_sender.try_send(event) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                while self.event_receiver.try_recv().is_ok() {}
                let _ = self.event_sender.try_send(ClipboardEvent::ResyncRequired);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    fn changed_event(
        state: &MockClipboardState,
        marker: Option<ClipboardMarker>,
    ) -> ClipboardEvent {
        let mut formats: Vec<_> = state.contents.keys().cloned().collect();
        for delayed_formats in state.delayed.values() {
            formats.extend(delayed_formats.iter().cloned());
        }
        formats.sort();
        formats.dedup();
        let sensitivity =
            state
                .contents
                .values()
                .fold(ClipboardSensitivity::default(), |mut merged, content| {
                    merged.sensitive |= content.sensitivity.sensitive;
                    merged.concealed |= content.sensitivity.concealed;
                    merged.transient |= content.sensitivity.transient;
                    merged.auto_generated |= content.sensitivity.auto_generated;
                    merged
                });
        ClipboardEvent::Changed {
            marker,
            formats,
            sensitivity,
        }
    }

    fn set_content(
        &self,
        content: ClipboardContent,
        marker: ClipboardMarker,
        operation: ClipboardOperation,
    ) -> Result<(), BackendError> {
        content.validate()?;
        let event = {
            let mut state = self.lock()?;
            Self::check_failure(&mut state, operation)?;
            state.delayed.clear();
            state.contents.insert(content.format.clone(), content);
            state.change_token = state
                .change_token
                .checked_add(1)
                .ok_or(BackendError::StateUnavailable)?;
            state.marker = Some(marker);
            Self::changed_event(&state, Some(marker))
        };
        self.emit(event);
        Ok(())
    }

    /// Queues one failure for the next call to `operation`.
    pub fn fail_next(
        &self,
        operation: ClipboardOperation,
        error: BackendError,
    ) -> Result<(), BackendError> {
        self.lock()?
            .failures
            .entry(operation)
            .or_default()
            .push_back(error);
        Ok(())
    }

    /// Sets or replaces one representation as if a local application had written it.
    pub fn set_external_content(&self, content: ClipboardContent) -> Result<(), BackendError> {
        content.validate()?;
        let event = {
            let mut state = self.lock()?;
            state.delayed.clear();
            state.contents.insert(content.format.clone(), content);
            state.change_token = state
                .change_token
                .checked_add(1)
                .ok_or(BackendError::StateUnavailable)?;
            state.marker = None;
            Self::changed_event(&state, None)
        };
        self.emit(event);
        Ok(())
    }

    /// Simulates a local application pasting a registered delayed representation.
    pub fn request_delayed_render(
        &self,
        format: ClipboardFormat,
        marker: ClipboardMarker,
    ) -> Result<(), BackendError> {
        {
            let state = self.lock()?;
            if !state
                .delayed
                .get(&marker)
                .is_some_and(|formats| formats.contains(&format))
            {
                return Err(BackendError::InvalidInput(
                    "no delayed representation is registered for that marker and format".into(),
                ));
            }
        }
        self.emit(ClipboardEvent::RenderRequested { marker, format });
        Ok(())
    }
}

impl ClipboardBackend for MockClipboard {
    fn read_snapshot(&self) -> Result<ClipboardSnapshot, BackendError> {
        let mut state = self.lock()?;
        Self::check_failure(&mut state, ClipboardOperation::ReadSnapshot)?;
        let contents: Vec<_> = state.contents.values().cloned().collect();
        validate_clipboard_bundle(&contents)?;
        let ClipboardEvent::Changed { sensitivity, .. } = Self::changed_event(&state, state.marker)
        else {
            return Err(BackendError::StateUnavailable);
        };
        Ok(ClipboardSnapshot {
            contents,
            sensitivity,
            marker: state.marker,
            change_token: ClipboardChangeToken(state.change_token),
        })
    }

    fn publish_snapshot(
        &self,
        contents: Vec<ClipboardContent>,
        marker: ClipboardMarker,
        expected: ClipboardChangeToken,
        admission: ClipboardAdmission,
    ) -> Result<ClipboardPublish, BackendError> {
        validate_clipboard_bundle(&contents)?;
        let mut state = self.lock()?;
        Self::check_failure(&mut state, ClipboardOperation::PublishSnapshot)?;
        let mut failure = None;
        match state.races.pop_front() {
            Some(ClipboardRace::LocalCopy(local)) => {
                validate_clipboard_bundle(&local)?;
                state.contents = local.into_iter().map(|c| (c.format.clone(), c)).collect();
                state.marker = None;
                state.change_token = state
                    .change_token
                    .checked_add(1)
                    .ok_or(BackendError::StateUnavailable)?;
            }
            Some(ClipboardRace::RevokeAdmission) => admission.revoke(),
            Some(ClipboardRace::NativeFailure { formats_written }) => {
                failure = Some(formats_written)
            }
            None => {}
        }
        if state.change_token != expected.0 {
            return Ok(ClipboardPublish::ReplacedLocalChange {
                actual_change_token: ClipboardChangeToken(state.change_token),
            });
        }
        if !admission.is_admitted() {
            return Ok(ClipboardPublish::Revoked);
        }
        let token = state
            .change_token
            .checked_add(1)
            .ok_or(BackendError::StateUnavailable)?;
        state.delayed.clear();
        state.change_token = token;
        if let Some(formats_written) = failure {
            state.contents.clear();
            state.marker = None;
            let event = Self::changed_event(&state, None);
            drop(state);
            self.emit(event);
            return Ok(ClipboardPublish::PartialFailure {
                formats_written,
                cleared: true,
                error: BackendError::Unavailable,
            });
        }
        state.contents = contents
            .into_iter()
            .map(|c| (c.format.clone(), c))
            .collect();
        state.marker = Some(marker);
        let event = Self::changed_event(&state, Some(marker));
        drop(state);
        self.emit(event);
        Ok(ClipboardPublish::Published {
            change_token: ClipboardChangeToken(token),
        })
    }
    fn read(&self, format: &ClipboardFormat) -> Result<Option<ClipboardContent>, BackendError> {
        let mut state = self.lock()?;
        Self::check_failure(&mut state, ClipboardOperation::Read)?;
        Ok(state.contents.get(format).cloned())
    }

    fn write(
        &self,
        content: ClipboardContent,
        marker: ClipboardMarker,
    ) -> Result<(), BackendError> {
        self.set_content(content, marker, ClipboardOperation::Write)
    }

    fn formats(&self) -> Result<Vec<ClipboardFormat>, BackendError> {
        let mut state = self.lock()?;
        Self::check_failure(&mut state, ClipboardOperation::Formats)?;
        let mut formats: Vec<_> = state.contents.keys().cloned().collect();
        for delayed_formats in state.delayed.values() {
            formats.extend(delayed_formats.iter().cloned());
        }
        formats.sort();
        formats.dedup();
        Ok(formats)
    }

    fn read_files(&self) -> Result<Option<FileList>, BackendError> {
        let mut state = self.lock()?;
        Self::check_failure(&mut state, ClipboardOperation::ReadFiles)?;
        match state.contents.get(&ClipboardFormat::Files) {
            Some(ClipboardContent {
                data: ClipboardData::Files(files),
                ..
            }) => Ok(Some(files.clone())),
            Some(_) => Err(BackendError::InvalidInput(
                "mock file-list content has an invalid payload".into(),
            )),
            None => Ok(None),
        }
    }

    fn write_files(&self, files: FileList, marker: ClipboardMarker) -> Result<(), BackendError> {
        self.set_content(
            ClipboardContent::files(files),
            marker,
            ClipboardOperation::WriteFiles,
        )
    }

    fn subscribe(&self) -> Receiver<ClipboardEvent> {
        self.event_receiver.clone()
    }

    fn set_delayed_render(
        &self,
        format: ClipboardFormat,
        marker: ClipboardMarker,
    ) -> Result<(), BackendError> {
        if format == ClipboardFormat::Files {
            return Err(BackendError::InvalidInput(
                "file lists use prefetch instead of delayed rendering".into(),
            ));
        }
        let event = {
            let mut state = self.lock()?;
            Self::check_failure(&mut state, ClipboardOperation::SetDelayedRender)?;
            state.delayed.retain(|existing, _| *existing == marker);
            state.delayed.entry(marker).or_default().insert(format);
            state.change_token = state
                .change_token
                .checked_add(1)
                .ok_or(BackendError::Unavailable)?;
            state.marker = Some(marker);
            Self::changed_event(&state, Some(marker))
        };
        self.emit(event);
        Ok(())
    }

    fn fulfill_delayed_render(
        &self,
        content: ClipboardContent,
        marker: ClipboardMarker,
    ) -> Result<(), BackendError> {
        content.validate()?;
        if content.format == ClipboardFormat::Files {
            return Err(BackendError::InvalidInput(
                "file lists use prefetch instead of delayed rendering".into(),
            ));
        }
        let mut state = self.lock()?;
        Self::check_failure(&mut state, ClipboardOperation::FulfillDelayedRender)?;
        let registered = state
            .delayed
            .get_mut(&marker)
            .is_some_and(|formats| formats.remove(&content.format));
        if !registered {
            return Err(BackendError::InvalidInput(
                "delayed representation was not registered".into(),
            ));
        }
        state.contents.insert(content.format.clone(), content);
        state.change_token = state
            .change_token
            .checked_add(1)
            .ok_or(BackendError::Unavailable)?;
        state.marker = Some(marker);
        if state.delayed.get(&marker).is_some_and(HashSet::is_empty) {
            state.delayed.remove(&marker);
        }
        Ok(())
    }

    fn cancel_delayed_render(&self, marker: ClipboardMarker) -> Result<(), BackendError> {
        let mut state = self.lock()?;
        Self::check_failure(&mut state, ClipboardOperation::CancelDelayedRender)?;
        if state.delayed.remove(&marker).is_some() {
            state.change_token = state
                .change_token
                .checked_add(1)
                .ok_or(BackendError::Unavailable)?;
        }
        Ok(())
    }
}

struct MockPlatformState {
    data_dir: PathBuf,
    autostart: bool,
    opened_permission_settings: Vec<PermissionKind>,
    failures: HashMap<PlatformOperation, VecDeque<BackendError>>,
}

/// A fully local platform bundle for UI development and deterministic daemon tests.
#[derive(Clone)]
pub struct MockPlatform {
    os: Os,
    /// Scriptable input backend owned by this platform.
    pub input: MockInput,
    /// Scriptable clipboard backend owned by this platform.
    pub clipboard: MockClipboard,
    state: Arc<Mutex<MockPlatformState>>,
}

impl Default for MockPlatform {
    fn default() -> Self {
        Self::new(Os::Windows, PathBuf::from("mock-data"))
    }
}

impl MockPlatform {
    /// Creates a mock platform with the supplied OS label and data directory.
    pub fn new(os: Os, data_dir: PathBuf) -> Self {
        Self {
            os,
            input: MockInput::default(),
            clipboard: MockClipboard::default(),
            state: Arc::new(Mutex::new(MockPlatformState {
                data_dir,
                autostart: false,
                opened_permission_settings: Vec::new(),
                failures: HashMap::new(),
            })),
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, MockPlatformState>, BackendError> {
        self.state
            .lock()
            .map_err(|_| BackendError::StateUnavailable)
    }

    fn check_failure(
        state: &mut MockPlatformState,
        operation: PlatformOperation,
    ) -> Result<(), BackendError> {
        if let Some(queue) = state.failures.get_mut(&operation) {
            if let Some(error) = queue.pop_front() {
                return Err(error);
            }
        }
        Ok(())
    }

    /// Queues one failure for the next call to `operation`.
    pub fn fail_next(
        &self,
        operation: PlatformOperation,
        error: BackendError,
    ) -> Result<(), BackendError> {
        self.lock()?
            .failures
            .entry(operation)
            .or_default()
            .push_back(error);
        Ok(())
    }

    /// Replaces the configured mock data directory.
    pub fn set_data_dir(&self, data_dir: PathBuf) -> Result<(), BackendError> {
        self.lock()?.data_dir = data_dir;
        Ok(())
    }

    /// Returns permission panes opened through this mock.
    pub fn opened_permission_settings(&self) -> Result<Vec<PermissionKind>, BackendError> {
        Ok(self.lock()?.opened_permission_settings.clone())
    }
}

impl Platform for MockPlatform {
    fn os(&self) -> Os {
        self.os
    }

    fn input_backend(&self) -> &dyn InputBackend {
        &self.input
    }

    fn clipboard_backend(&self) -> &dyn ClipboardBackend {
        &self.clipboard
    }

    fn data_dir(&self) -> Result<PathBuf, BackendError> {
        let mut state = self.lock()?;
        Self::check_failure(&mut state, PlatformOperation::DataDir)?;
        Ok(state.data_dir.clone())
    }

    fn autostart_enabled(&self) -> Result<bool, BackendError> {
        let mut state = self.lock()?;
        Self::check_failure(&mut state, PlatformOperation::AutostartEnabled)?;
        Ok(state.autostart)
    }

    fn set_autostart_enabled(&self, enabled: bool) -> Result<(), BackendError> {
        let mut state = self.lock()?;
        Self::check_failure(&mut state, PlatformOperation::SetAutostartEnabled)?;
        state.autostart = enabled;
        Ok(())
    }

    fn open_permission_settings(&self, kind: PermissionKind) -> Result<(), BackendError> {
        let mut state = self.lock()?;
        Self::check_failure(&mut state, PlatformOperation::OpenPermissionSettings)?;
        state.opened_permission_settings.push(kind);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn atomic_snapshots_refuse_local_copy_revocation_and_native_failure() {
        use crate::{
            ClipboardAdmission, ClipboardBackend, ClipboardContent, ClipboardData, ClipboardFormat,
            ClipboardMarker, ClipboardPublish, ClipboardRace, ClipboardSensitivity, MockClipboard,
        };
        let clipboard = MockClipboard::default();
        let bytes = |format, value: &[u8]| {
            ClipboardContent::bytes(format, value.to_vec(), ClipboardSensitivity::default())
                .expect("content")
        };
        let original = clipboard.read_snapshot().expect("snapshot");
        let bundle = vec![
            bytes(ClipboardFormat::Text, b"atomic"),
            bytes(ClipboardFormat::Html, b"<b>atomic</b>"),
        ];
        assert!(matches!(
            clipboard
                .publish_snapshot(
                    bundle.clone(),
                    ClipboardMarker(3),
                    original.change_token,
                    ClipboardAdmission::default()
                )
                .expect("publish"),
            ClipboardPublish::Published { .. }
        ));
        let snapshot = clipboard.read_snapshot().expect("bundle");
        assert_eq!(snapshot.contents.len(), 2);
        assert_eq!(snapshot.marker, Some(ClipboardMarker(3)));
        clipboard
            .script_publish_race(ClipboardRace::LocalCopy(vec![bytes(
                ClipboardFormat::Text,
                b"local",
            )]))
            .expect("race");
        assert!(matches!(
            clipboard
                .publish_snapshot(
                    bundle.clone(),
                    ClipboardMarker(4),
                    snapshot.change_token,
                    ClipboardAdmission::default()
                )
                .expect("local race"),
            ClipboardPublish::ReplacedLocalChange { .. }
        ));
        let snapshot = clipboard.read_snapshot().expect("local preserved");
        assert_eq!(
            snapshot.contents[0].data,
            ClipboardData::Bytes(b"local".to_vec())
        );
        clipboard
            .script_publish_race(ClipboardRace::RevokeAdmission)
            .expect("race");
        assert_eq!(
            clipboard
                .publish_snapshot(
                    bundle.clone(),
                    ClipboardMarker(5),
                    snapshot.change_token,
                    ClipboardAdmission::default()
                )
                .expect("revoke"),
            ClipboardPublish::Revoked
        );
        assert_eq!(clipboard.read_snapshot().expect("unchanged"), snapshot);
        clipboard
            .script_publish_race(ClipboardRace::NativeFailure { formats_written: 1 })
            .expect("race");
        assert!(matches!(
            clipboard
                .publish_snapshot(
                    bundle,
                    ClipboardMarker(6),
                    snapshot.change_token,
                    ClipboardAdmission::default()
                )
                .expect("partial"),
            ClipboardPublish::PartialFailure {
                cleared: true,
                formats_written: 1,
                ..
            }
        ));
        assert!(clipboard
            .read_snapshot()
            .expect("clear")
            .contents
            .is_empty());
    }
    use super::*;

    fn key_down() -> InputEvent {
        InputEvent {
            kind: InputEventKind::Key {
                key: Key(0x04),
                down: true,
            },
            injected: false,
        }
    }

    #[test]
    fn capture_queue_overflow_latches_and_falls_back_to_local() {
        let input = MockInput::default();
        let (sink, receiver) = InputSink::bounded(1).expect("positive queue capacity");
        input.start_capture(sink.clone()).expect("capture starts");
        input
            .set_mode(CaptureMode::Swallow { lock_pos: true })
            .expect("swallow mode applies");

        input.emit(key_down()).expect("first sample fits");
        assert_eq!(input.emit(key_down()), Err(crate::InputSinkError::Full));
        assert!(sink.take_overflow());
        assert!(!sink.take_overflow());
        assert_eq!(input.mode().expect("mode reads"), CaptureMode::Local);
        assert!(receiver.try_recv().is_ok());
    }

    #[test]
    fn injection_is_tagged_and_release_all_clears_held_state() {
        let input = MockInput::default();
        input.inject(key_down()).expect("key injects");
        assert!(input.take_injected_events().expect("events read")[0].injected);
        assert!(input
            .held_keys()
            .expect("held keys read")
            .contains(&Key(0x04)));

        input.release_all().expect("release guard runs");
        assert!(input.held_keys().expect("held keys read").is_empty());
        assert_eq!(input.release_all_count().expect("release count reads"), 1);
    }

    #[test]
    fn clipboard_markers_delayed_render_and_scripted_errors_work() {
        let clipboard = MockClipboard::default();
        let events = clipboard.subscribe();
        let marker = ClipboardMarker(9);
        let sensitive = ClipboardSensitivity {
            sensitive: true,
            ..ClipboardSensitivity::default()
        };
        clipboard
            .write(
                ClipboardContent::bytes(ClipboardFormat::Text, b"private".to_vec(), sensitive)
                    .expect("text format accepts bytes"),
                marker,
            )
            .expect("clipboard write succeeds");
        assert!(matches!(
            events.try_recv(),
            Ok(ClipboardEvent::Changed {
                marker: Some(found),
                sensitivity,
                ..
            }) if found == marker && sensitivity.should_exclude()
        ));

        clipboard
            .fail_next(ClipboardOperation::Read, BackendError::PermissionDenied)
            .expect("failure is scripted");
        assert_eq!(
            clipboard.read(&ClipboardFormat::Text),
            Err(BackendError::PermissionDenied)
        );

        clipboard
            .set_delayed_render(ClipboardFormat::Html, marker)
            .expect("delayed format registers");
        let _ = events.try_recv();
        clipboard
            .request_delayed_render(ClipboardFormat::Html, marker)
            .expect("paste requests delayed format");
        assert_eq!(
            events.try_recv(),
            Ok(ClipboardEvent::RenderRequested {
                marker,
                format: ClipboardFormat::Html,
            })
        );
        clipboard
            .fulfill_delayed_render(
                ClipboardContent::bytes(
                    ClipboardFormat::Html,
                    b"<b>ready</b>".to_vec(),
                    ClipboardSensitivity::default(),
                )
                .expect("html format accepts bytes"),
                marker,
            )
            .expect("delayed payload is fulfilled");
        assert_eq!(
            clipboard
                .read(&ClipboardFormat::Html)
                .expect("html reads")
                .map(|content| content.data),
            Some(ClipboardData::Bytes(b"<b>ready</b>".to_vec()))
        );
    }
}
