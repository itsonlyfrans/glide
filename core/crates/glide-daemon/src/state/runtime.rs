use super::*;
use crate::{
    keyboard::KeyTranslator,
    layout::{EngineEvent, InputEvent as ForwardedInput},
};
use glide_net::LinkEvent;
use glide_platform::{BackendError, InjectionFailure, InputEvent, InputEventKind, Key, Point};
use glide_proto::wire::{self, ControlMessage, InputMessage, WireMessage};

#[derive(Default)]
pub(super) struct InjectionHealth {
    consecutive: u32,
    since: Option<Instant>,
    dropped: u64,
    last_elevated_notice: Option<Instant>,
}

#[derive(Debug, Eq, PartialEq)]
enum InjectionAction {
    Drop,
    NotifyElevated,
    EndPermission,
    EndTransient,
}

impl InjectionHealth {
    pub(super) fn reset_run(&mut self) {
        self.consecutive = 0;
        self.since = None;
    }
    fn failure(&mut self, class: InjectionFailure, now: Instant) -> InjectionAction {
        self.dropped = self.dropped.saturating_add(1);
        match class {
            InjectionFailure::PermissionDenied => InjectionAction::EndPermission,
            InjectionFailure::TargetElevated => {
                self.reset_run();
                if self.last_elevated_notice.is_none_or(|last| {
                    now.saturating_duration_since(last) >= Duration::from_secs(60)
                }) {
                    self.last_elevated_notice = Some(now);
                    InjectionAction::NotifyElevated
                } else {
                    InjectionAction::Drop
                }
            }
            InjectionFailure::InvalidPosition => {
                self.reset_run();
                InjectionAction::Drop
            }
            InjectionFailure::Transient => {
                self.consecutive = self.consecutive.saturating_add(1);
                let start = *self.since.get_or_insert(now);
                if self.consecutive >= 50
                    || now.saturating_duration_since(start) >= Duration::from_secs(3)
                {
                    InjectionAction::EndTransient
                } else {
                    InjectionAction::Drop
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct Hotkey {
    modifiers: u8,
    key: Key,
}

impl Hotkey {
    pub(super) fn parse(text: &str) -> Result<Self, IpcError> {
        let mut modifiers = 0;
        let mut key = None;
        for token in text.split('+') {
            let token = token.trim().to_ascii_lowercase();
            let bit = match token.as_str() {
                "ctrl" | "control" => 1,
                "shift" => 2,
                "alt" | "option" => 4,
                "cmd" | "command" | "meta" | "win" => 8,
                _ => 0,
            };
            if bit != 0 {
                modifiers |= bit;
                continue;
            }
            let usage = match token.as_str() {
                "home" => 0x4a,
                "end" => 0x4d,
                "escape" | "esc" => 0x29,
                "space" => 0x2c,
                "enter" => 0x28,
                "tab" => 0x2b,
                "left" => 0x50,
                "right" => 0x4f,
                "up" => 0x52,
                "down" => 0x51,
                _ if token.len() == 1 && token.as_bytes()[0].is_ascii_lowercase() => {
                    u16::from(token.as_bytes()[0] - b'a') + 4
                }
                _ if token.len() == 1 && token.as_bytes()[0].is_ascii_digit() => {
                    if token == "0" {
                        0x27
                    } else {
                        u16::from(token.as_bytes()[0] - b'1') + 0x1e
                    }
                }
                _ => return Err(error(ErrorCode::InvalidParams, "unsupported hotkey key")),
            };
            if key.replace(Key(usage)).is_some() {
                return Err(error(
                    ErrorCode::InvalidParams,
                    "hotkey must have one non-modifier key",
                ));
            }
        }
        if modifiers == 0 {
            return Err(error(
                ErrorCode::InvalidParams,
                "hotkey requires a modifier",
            ));
        }
        Ok(Self {
            modifiers,
            key: key.ok_or_else(|| error(ErrorCode::InvalidParams, "hotkey requires a key"))?,
        })
    }

    fn matches(self, key: Key, held: &[bool; 256]) -> bool {
        let modifiers = u8::from(held[0xe0] || held[0xe4])
            | (u8::from(held[0xe1] || held[0xe5]) << 1)
            | (u8::from(held[0xe2] || held[0xe6]) << 2)
            | (u8::from(held[0xe3] || held[0xe7]) << 3);
        key == self.key && modifiers == self.modifiers
    }
}

impl Core {
    pub(super) fn rebuild_desktop(&mut self) -> Result<(), IpcError> {
        self.engine = match build_engine(&self.state, self.platform.as_ref()) {
            Ok(engine) => engine,
            Err(_) => {
                tracing::debug!("changed displays require layout reconciliation");
                let mut local = self.state.clone();
                for peer in &mut local.peers {
                    peer.online = false;
                }
                build_engine(&local, self.platform.as_ref()).map_err(|_| {
                    error(
                        ErrorCode::PermissionDenied,
                        "The local displays are unavailable. Check display and input permissions.",
                    )
                })?
            }
        };
        Ok(())
    }
    pub(crate) fn notify_failure(&mut self, failure: &IpcError) {
        let title = match failure.code {
            ErrorCode::BadCode => "Pairing code did not match",
            ErrorCode::CodeExpired => "Pairing expired",
            ErrorCode::LockedOut => "Pairing is temporarily locked",
            ErrorCode::PermissionDenied
                if !matches!(
                    failure.message.as_str(),
                    "pairing cancelled" | "daemon shutting down" | "transport changed"
                ) =>
            {
                "Glide needs permission"
            }
            ErrorCode::Internal if failure.message == "could not save configuration" => {
                "Glide could not save settings"
            }
            _ => {
                tracing::debug!(code = ?failure.code, "non-actionable operation failed");
                return;
            }
        };
        self.events.push(Event::Notification(Notification {
            level: "error".into(),
            title: title.into(),
            body: failure.message.clone(),
            action: None,
        }));
    }
    /// A stalled writer must not suppress escape or keep remote holds alive with heartbeats.
    pub(super) async fn send_reliable(
        &self,
        peer: &str,
        message: WireMessage,
    ) -> Result<(), glide_net::LinkError> {
        #[cfg(test)]
        let started = Instant::now();
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            self.link.send_reliable(peer, message),
        )
        .await
        .unwrap_or(Err(glide_net::LinkError::Closed));
        if result.is_err() {
            #[cfg(test)]
            eprintln!(
                "reliable failed: result={result:?}, elapsed={:?}, current_token={:?}",
                started.elapsed(),
                self.link.peer_token(peer)
            );
            let _ = tokio::time::timeout(Duration::from_millis(100), self.link.close(peer)).await;
        }
        result
    }

    /// The steady receive/inject mouse path owns no peer strings or boxed futures.
    pub async fn receive_move(
        &mut self,
        received: glide_net::ReceivedMove,
    ) -> Result<(), IpcError> {
        if self.receiving_token != Some(received.peer) {
            return Ok(());
        }
        if self
            .receiving_from
            .as_ref()
            .is_none_or(|id| self.link.peer_token(id).ok() != Some(received.peer))
        {
            return Ok(());
        }
        self.inject_move(received.movement).await
    }

    async fn inject_move(&mut self, movement: wire::Move) -> Result<(), IpcError> {
        if !newer(movement.seq, self.move_seq) {
            return Ok(());
        }
        if !movement.x.is_finite() || !movement.y.is_finite() {
            return Err(error(ErrorCode::InvalidParams, "invalid move"));
        }
        self.move_seq = Some(movement.seq);
        self.diag.received(Instant::now());
        let position = Point {
            x: movement.x,
            y: movement.y,
        };
        // Another computer's mouse is moving the cursor on one of this computer's screens.
        let me = self.state.self_info.device_id.clone();
        self.set_cursor_place(&me, Some(position));
        self.inject(InputEventKind::PointerMoved {
            position,
            delta_x: 0.0,
            delta_y: 0.0,
        })
        .await
    }
    pub(super) fn abort_pairing(&mut self, failure: IpcError) {
        if let Some(job) = self.join_job.take() {
            job.abort();
        }
        if self.pending_join_id.is_none() && self.verification_deadline.is_none() {
            return;
        }
        self.verification_deadline = None;
        if matches!(
            failure.code,
            ErrorCode::BadCode | ErrorCode::CodeExpired | ErrorCode::LockedOut
        ) || failure.message == "could not save configuration"
        {
            self.notify_failure(&failure);
        }
        if let Some(id) = self.pending_join_id.take() {
            self.deferred_responses
                .push(Response::failure(id, failure.clone()));
        }
        self.events.push(Event::PairingResult(PairingResult {
            ok: false,
            device_id: None,
            error: Some(failure),
        }));
    }

    pub(super) async fn complete_pairing(&mut self, peer: Peer) -> Result<(), IpcError> {
        if self
            .state
            .peers
            .iter()
            .any(|existing| existing.device_id == peer.device_id)
            && self.pending_join_id.is_none()
            && self.verification_deadline.is_none()
        {
            return Ok(());
        }
        let id = peer.device_id.clone();
        let commit = async {
            validate_peer(&peer)?;
            if self.state.peers.len() >= 31 || id == self.state.self_info.device_id {
                return Err(error(
                    ErrorCode::InvalidParams,
                    "peer limit or identity conflict",
                ));
            }
            self.end_forwarding("pairing_changed").await?;
            let mut next = self.state.clone();
            next.peers.retain(|existing| existing.device_id != id);
            next.peers.push(peer.clone());
            next.discovered.retain(|existing| existing.device_id != id);
            next.layout = reconcile_layout(&next.layout.devices, &next)?;
            self.commit_layout(next, None)
        }
        .await;
        if let Err(error) = commit {
            if error.message == "could not save configuration" {
                // Trust is already durable in the network store. Keep it and retry Core's
                // projection; an unrelated configuration failure must not revoke the pin.
                if self.pending_pairings.insert(id, peer).is_none() {
                    self.notify_failure(&error);
                }
                return Err(error);
            }
            let _ = self.manager.unpair(&id).await;
            self.abort_pairing(error.clone());
            return Err(error);
        }
        self.pending_pairings.remove(&id);
        self.verification_deadline = None;
        if let Some(request_id) = self.pending_join_id.take() {
            self.deferred_responses
                .push(Response::success(request_id, json!({"device_id":id})));
        }
        self.events.push(Event::PairingResult(PairingResult {
            ok: true,
            device_id: Some(id.clone()),
            error: None,
        }));
        self.broadcast_layout().await;
        if self.native_manager.is_none() {
            if let Some(peer) = self
                .state
                .peers
                .iter_mut()
                .find(|peer| peer.device_id == id)
            {
                if let Some(address) = &peer.address {
                    if self.link.connect(address, Some(&id)).await.is_err() {
                        peer.online = false;
                        peer.connection = Connection::Offline;
                    }
                }
            }
        }
        Ok(())
    }

    /// Replace the peer boundary after the caller initializes its pinned-peer store.
    /// Existing forwarding and pairing are ended before the old transport is replaced.
    pub async fn set_peer_interfaces(
        &mut self,
        manager: Box<dyn PeerManager>,
        link: Arc<dyn Link>,
    ) -> Result<(), IpcError> {
        self.end_forwarding("transport_changed").await?;
        self.manager.cancel_pair_host().await?;
        self.abort_pairing(error(ErrorCode::PermissionDenied, "transport changed"));
        self.peer_events = manager.events();
        self.stop_native_clipboard_and_wait().await;
        if let Some(job) = self.accept_job.take() {
            job.abort();
            let _ = job.await;
        }
        self.clipboard.invalidate();
        self.native_manager = None;
        self.connected_tokens.clear();
        self.manager = manager;
        self.link = link;
        Ok(())
    }

    /// Apply a transfer-engine snapshot, with bounded UI progress and confirmation notification.
    pub fn update_transfer(&mut self, transfer: Transfer) -> Result<(), IpcError> {
        if transfer.id.is_empty()
            || transfer.id.len() > 128
            || transfer.name.len() > 1024
            || transfer.bytes_done > transfer.bytes_total
            || transfer.items > wire::MAX_TRANSFER_ITEMS
            || !self
                .state
                .peers
                .iter()
                .any(|peer| peer.device_id == transfer.peer_id)
        {
            return Err(error(ErrorCode::InvalidParams, "invalid transfer snapshot"));
        }
        if !self
            .state
            .transfers
            .iter()
            .any(|existing| existing.id == transfer.id)
            && self.state.transfers.len() >= 64
        {
            if let Some(index) = self.state.transfers.iter().position(|old| {
                matches!(
                    old.state,
                    TransferState::Done | TransferState::Failed | TransferState::Cancelled
                )
            }) {
                let id = self.state.transfers.remove(index).id;
                self.last_progress.remove(&id);
            }
        }
        let prior = self
            .state
            .transfers
            .iter()
            .position(|existing| existing.id == transfer.id);
        if prior.is_none() && self.state.transfers.len() >= 64 {
            return Err(error(ErrorCode::InvalidParams, "transfer limit reached"));
        }
        let needs_notice = transfer.state == TransferState::AwaitingConfirm
            && prior.is_none_or(|index| {
                self.state.transfers[index].state != TransferState::AwaitingConfirm
            });
        let now = Instant::now();
        if self
            .last_progress
            .get(&transfer.id)
            .is_none_or(|last| now.duration_since(*last) >= Duration::from_millis(100))
            || matches!(
                transfer.state,
                TransferState::Done | TransferState::Failed | TransferState::Cancelled
            )
        {
            self.events.push(Event::TransferProgress(TransferProgress {
                id: transfer.id.clone(),
                bytes_done: transfer.bytes_done,
                rate_bps: transfer.rate_bps,
            }));
            self.last_progress.insert(transfer.id.clone(), now);
        }
        if needs_notice {
            self.events.push(Event::Notification(Notification {
                level: "info".into(),
                title: "Confirm file transfer".into(),
                body: "A transfer is waiting for your approval.".into(),
                action: Some(json!({"method":"transfer.confirm","id":transfer.id})),
            }));
        }
        if let Some(index) = prior {
            self.state.transfers[index] = transfer;
        } else {
            self.state.transfers.push(transfer);
        }
        self.dirty = true;
        Ok(())
    }

    /// A key-up the system never delivered (the app switcher, Secure Input) leaves a modifier "held" here, and the next
    /// crossing would press it on the other computer for good. Drop the ones the system says are up.
    fn drop_stale_modifiers(&mut self) {
        if self.engine.forwarding_to().is_some()
            || !(0xe0..=0xe7).any(|usage| self.held_physical[usage])
        {
            return;
        }
        let Some(maybe_held) = self.platform.input_backend().modifiers_maybe_held() else {
            return;
        };
        for (index, maybe) in maybe_held.into_iter().enumerate() {
            let usage = 0xe0 + index;
            if !maybe && self.held_physical[usage] {
                self.held_physical[usage] = false;
                self.engine.observe_source_key(Key(usage as u16), false);
            }
        }
    }

    /// Consume a physical sample outside the hook callback. Injected samples never preempt control.
    pub async fn capture_input(&mut self, event: InputEvent) -> Result<(), IpcError> {
        if event.injected {
            return Ok(());
        }
        // Escape keys are handled before takeover and before any network operation.
        if let InputEventKind::Key { key, down } = event.kind {
            let Some(held) = self.held_physical.get_mut(usize::from(key.0)) else {
                return Err(error(ErrorCode::InvalidParams, "unsupported HID usage"));
            };
            let was_down = *held;
            *held = down;
            if down && !was_down && self.return_hotkey.matches(key, &self.held_physical) {
                return self.end_forwarding("return_home_hotkey").await;
            }
            if down && !was_down && self.toggle_hotkey.matches(key, &self.held_physical) {
                self.end_forwarding("toggle_sharing_hotkey").await?;
                let mut next = self.state.clone();
                next.sharing_enabled = !next.sharing_enabled;
                return self.apply(next);
            }
        }
        if self.capture_pending || super::permissions::missing(self.state.permissions) {
            return Ok(());
        }
        if self.cursor_hidden_peer.is_some() && self.engine.forwarding_to().is_none() {
            self.end_forwarding("local_input").await?;
        }
        if self.receiving_from.is_some()
            || self.engine.brain_device() != self.state.self_info.device_id
        {
            self.end_forwarding("local_input").await?;
        }
        match event.kind {
            InputEventKind::PointerMoved {
                position,
                delta_x,
                delta_y,
            } if self.state.sharing_enabled => {
                let handling = Instant::now();
                self.drop_stale_modifiers();
                let position = crate::arrangement::to_arranged(
                    &self.native_monitors,
                    &self.state.self_info.monitors,
                    position,
                );
                self.engine.sync_home_cursor(
                    position,
                    Point {
                        x: delta_x,
                        y: delta_y,
                    },
                );
                let before = self.engine.cursor();
                let events = self.engine.move_by(
                    Point {
                        x: delta_x,
                        y: delta_y,
                    },
                    Instant::now(),
                );
                self.process_engine_events(events).await?;
                self.note_cursor_from_engine();
                self.wake_if_reaching_for_a_sleeping_peer(
                    before,
                    Point {
                        x: delta_x,
                        y: delta_y,
                    },
                );
                if let Some((target, pos)) = self.engine.forwarded_position() {
                    self.outgoing_seq = self.outgoing_seq.wrapping_add(1);
                    match self.link.peer_token(target) {
                        Ok(token) => {
                            self.pending_move = Some((
                                token,
                                wire::Move {
                                    seq: self.outgoing_seq,
                                    x: pos.x,
                                    y: pos.y,
                                },
                            ));
                            self.flush_pending_move().await?;
                            self.diag.captured(handling);
                        }
                        Err(_) => self.end_forwarding("link_lost").await?,
                    }
                }
            }
            InputEventKind::Key { key, down } => {
                if let Some(event) = self.engine.key_event(key, down) {
                    self.process_engine_events(vec![event]).await?;
                }
            }
            InputEventKind::Button { button, down } => {
                if let Some(event) = self.engine.button_event(button, down) {
                    self.process_engine_events(vec![event]).await?;
                }
            }
            InputEventKind::Wheel { dx, dy, precise } => {
                if let Some(target) = self.engine.forwarding_to() {
                    let (dx, dy) = crate::wheel::to_wire(self.state.self_info.os, dx, dy, precise);
                    self.outgoing_seq = self.outgoing_seq.wrapping_add(1);
                    if self
                        .send_reliable(
                            target,
                            WireMessage::Input(InputMessage::Wheel(wire::Wheel {
                                epoch: self.outgoing_epoch,
                                seq: self.outgoing_seq,
                                dx,
                                dy,
                                precise: true,
                            })),
                        )
                        .await
                        .is_err()
                    {
                        self.end_forwarding("link_lost").await?;
                        return Err(error(ErrorCode::Unreachable, "wheel forwarding failed"));
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// The cursor was pushed against a screen edge where a sleeping, wakeable computer sits on the desk: wake it.
    /// At most one wake-up per computer per minute.
    fn wake_if_reaching_for_a_sleeping_peer(&mut self, before: Point, delta: Point) {
        if self.engine.forwarding_to().is_some()
            || !self
                .state
                .peers
                .iter()
                .any(|p| !p.online && p.wake_mac.is_some())
        {
            return;
        }
        let after = self.engine.cursor();
        let (blocked_x, blocked_y) = (before.x + delta.x - after.x, before.y + delta.y - after.y);
        let length = blocked_x.hypot(blocked_y);
        if length < 1.0 {
            return;
        }
        let probe = Point {
            x: after.x + blocked_x / length * 24.0,
            y: after.y + blocked_y / length * 24.0,
        };
        let Some(peer) =
            crate::wake::sleeping_peer_at(probe, &self.state.layout.devices, &self.state.peers)
        else {
            return;
        };
        let now = Instant::now();
        if self
            .wake_sent
            .get(&peer.device_id)
            .is_some_and(|sent| now.duration_since(*sent) < Duration::from_secs(60))
        {
            return;
        }
        let (Some(mac), ip, id, name) = (
            peer.wake_mac.clone(),
            crate::wake::peer_ip(peer),
            peer.device_id.clone(),
            peer.name.clone(),
        ) else {
            return;
        };
        self.wake_sent.insert(id, now);
        tokio::task::spawn_blocking(move || {
            let _ = crate::wake::send(&mac, ip);
        });
        self.events.push(Event::Notification(Notification {
            level: "info".into(),
            title: format!("Waking {name}"),
            body: "It can take a few seconds to wake up and reconnect.".into(),
            action: None,
        }));
    }

    /// Read this computer's model once, in the background; when it is known, tell the connected computers.
    pub(super) async fn learn_own_model(&mut self) {
        if !self.model_started && self.native_manager.is_some() && self.mock_platform.is_some() {
            // Simulated computers (tests) describe themselves without asking the system: on macOS that runs
            // system_profiler, which is far too heavy to start for every test engine.
            self.model_started = true;
            *self.model_found.lock().unwrap_or_else(|e| e.into_inner()) = Some(DeviceModel {
                name: "Simulated computer".into(),
                kind: "desktop".into(),
                builtin_monitor: None,
            });
        }
        if !self.model_started && self.native_manager.is_some() {
            self.model_started = true;
            let slot = self.model_found.clone();
            let monitors = self.native_monitors.clone();
            tokio::task::spawn_blocking(move || {
                if let Some(model) = crate::device::detect(&monitors) {
                    *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(model);
                }
            });
        }
        let found = self
            .model_found
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(model) = found {
            if self.state.self_info.model.as_ref() != Some(&model) {
                self.state.self_info.model = Some(model);
                self.dirty = true;
            }
            self.send_details(None).await;
        }
    }

    /// Tell connected computers (or one) what kind of computer this is. Only peers on 0.2.2 or newer understand it.
    pub(super) async fn send_details(&mut self, only: Option<&str>) {
        let Some(model) = self.state.self_info.model.clone() else {
            return;
        };
        let targets: Vec<String> = self
            .state
            .peers
            .iter()
            .filter(|p| {
                p.online
                    && only.is_none_or(|id| id == p.device_id)
                    && wire::understands_details(p.app_version.as_deref())
            })
            .map(|p| p.device_id.clone())
            .collect();
        for target in targets {
            let message = WireMessage::Control(ControlMessage::Details(wire::DeviceDetails {
                model: model.name.clone(),
                kind: model.kind.clone(),
                builtin_monitor: model.builtin_monitor.clone(),
            }));
            let _ = self.send_reliable(&target, message).await;
        }
    }

    /// Remember where the cursor is (computer and, when known, screen) so the Desk can show it. The state is only
    /// pushed to the window when this changes, which is rare compared with cursor movement.
    pub(super) fn set_cursor_place(&mut self, device_id: &str, local: Option<Point>) {
        let monitors = if device_id == self.state.self_info.device_id {
            &self.state.self_info.monitors
        } else if let Some(peer) = self.state.peers.iter().find(|p| p.device_id == device_id) {
            &peer.monitors
        } else {
            return;
        };
        let monitor_id = local.and_then(|p| monitor_at(monitors, p));
        let place = CursorPlace {
            device_id: device_id.to_owned(),
            monitor_id,
        };
        if self.state.cursor.as_ref() != Some(&place) {
            self.state.cursor = Some(place);
            self.dirty = true;
        }
    }

    /// The cursor moved on this computer's own mouse: the engine knows exactly where it is.
    pub(super) fn note_cursor_from_engine(&mut self) {
        if let Some((device, local)) = self.engine.cursor_place().map(|(d, p)| (d.to_owned(), p)) {
            self.set_cursor_place(&device, Some(local));
        }
    }

    /// One latest position survives short transport lock contention, even if capture stops.
    pub(super) async fn flush_pending_move(&mut self) -> Result<(), IpcError> {
        let Some((token, movement)) = self.pending_move else {
            return Ok(());
        };
        let Some(target) = self.engine.forwarding_to() else {
            self.pending_move = None;
            return Ok(());
        };
        if self.link.peer_token(target).ok() != Some(token) {
            self.pending_move = None;
            return self.end_forwarding("link_lost").await;
        }
        match self.link.send_datagram(target, movement) {
            Ok(()) => {
                self.pending_move = None;
                self.diag.sent();
            }
            Err(glide_net::LinkError::Busy) => self.diag.held_back(),
            Err(_) => {
                self.pending_move = None;
                self.end_forwarding("link_lost").await?;
            }
        }
        Ok(())
    }

    pub(super) async fn end_forwarding(&mut self, reason: &str) -> Result<(), IpcError> {
        let source = self.receiving_from.clone();
        let pos = self
            .platform
            .input_backend()
            .local_cursor_pos()
            .ok()
            .map(|p| {
                crate::arrangement::to_arranged(
                    &self.native_monitors,
                    &self.state.self_info.monitors,
                    p,
                )
            });
        let came_home = source.is_some() || self.engine.forwarding_to().is_some();
        let events = self.engine.return_home();
        // Restore the local OS state before waiting for a stalled remote writer.
        self.return_home(reason)?;
        self.rebuild_desktop()?;
        if came_home {
            self.note_cursor_from_engine();
        }
        for (usage, down) in self.held_physical.iter().enumerate() {
            if *down {
                self.engine.observe_source_key(Key(usage as u16), true);
            }
        }
        if let Some(source) = source {
            let message = if matches!(reason, "injection_denied" | "injection_unavailable") {
                ControlMessage::Bye(wire::Bye {
                    reason: reason.into(),
                })
            } else if let Some(pos) = pos {
                ControlMessage::TakeOver(wire::TakeOver { pos })
            } else {
                ControlMessage::Bye(wire::Bye {
                    reason: "receiver_left".into(),
                })
            };
            let _ = self
                .send_reliable(&source, WireMessage::Control(message))
                .await;
        }
        let sent = self.process_engine_events(events).await;
        if sent.is_err() {
            tracing::debug!("remote leave could not be delivered");
        }
        Ok(())
    }

    pub(super) async fn process_engine_events(
        &mut self,
        events: Vec<EngineEvent>,
    ) -> Result<(), IpcError> {
        for event in events {
            match event {
                EngineEvent::Enter {
                    device_id,
                    position,
                    modifiers_down,
                } => {
                    let target_os = self
                        .state
                        .peers
                        .iter()
                        .find(|peer| peer.device_id == device_id && peer.online)
                        .map(|peer| peer.os)
                        .ok_or_else(|| error(ErrorCode::Unreachable, "target is offline"))?;
                    let mut translator = KeyTranslator::new(
                        self.state.self_info.os,
                        target_os,
                        self.state.settings.keyboard.swap_ctrl_cmd,
                    );
                    let modifiers = translator
                        .sync_held(&modifiers_down)
                        .into_iter()
                        .filter(|event| event.down)
                        .map(|event| event.key)
                        .collect::<Vec<_>>();
                    self.outgoing_epoch = self
                        .outgoing_epoch
                        .checked_add(1)
                        .ok_or_else(|| error(ErrorCode::Internal, "input epoch exhausted"))?;
                    let message = WireMessage::Control(ControlMessage::Enter(wire::Enter {
                        epoch: self.outgoing_epoch,
                        pos: position,
                        modifiers_down: wire::ModifierKeys::try_from_vec(modifiers)
                            .map_err(|_| error(ErrorCode::Internal, "modifier limit exceeded"))?,
                    }));
                    if self.send_reliable(&device_id, message).await.is_err() {
                        self.engine.return_home();
                        self.return_home("link_lost")?;
                        return Err(error(ErrorCode::Unreachable, "could not enter target"));
                    }
                    if self
                        .platform
                        .input_backend()
                        .set_mode(CaptureMode::Swallow { lock_pos: true })
                        .is_err()
                    {
                        let _ = self
                            .send_reliable(
                                &device_id,
                                WireMessage::Control(ControlMessage::Leave(wire::Leave {
                                    epoch: self.outgoing_epoch,
                                })),
                            )
                            .await;
                        self.engine.return_home();
                        self.return_home("permission_denied")?;
                        return Err(error(
                            ErrorCode::PermissionDenied,
                            "capture mode unavailable",
                        ));
                    }
                    self.cursor_visibility(false);
                    self.cursor_hidden_peer = Some((device_id.clone(), Instant::now()));
                    self.translator = Some(translator);
                    self.state.active_device_id = device_id.clone();
                    self.events.push(Event::ActiveChanged(ActiveChanged {
                        device_id,
                        reason: "edge_crossing".into(),
                    }));
                    self.dirty = true;
                }
                EngineEvent::Leave { device_id, .. } => {
                    let translator = self.translator.take();
                    self.return_home("leave")?;
                    if let Some(mut translator) = translator {
                        for event in translator.release_all() {
                            self.outgoing_seq = self.outgoing_seq.wrapping_add(1);
                            let _ = self
                                .send_reliable(
                                    &device_id,
                                    WireMessage::Input(InputMessage::Key(wire::InputKey {
                                        epoch: self.outgoing_epoch,
                                        seq: self.outgoing_seq,
                                        hid_usage: event.key,
                                        down: false,
                                    })),
                                )
                                .await;
                        }
                    }
                    self.translator = None;
                    let _ = self
                        .send_reliable(
                            &device_id,
                            WireMessage::Control(ControlMessage::Leave(wire::Leave {
                                epoch: self.outgoing_epoch,
                            })),
                        )
                        .await;
                }
                EngineEvent::Input { device_id, input } => {
                    self.outgoing_seq = self.outgoing_seq.wrapping_add(1);
                    // One physical key can become several target keys (Alt+Tab becomes Cmd+Tab).
                    let mut messages = Vec::new();
                    match input {
                        ForwardedInput::Key { key, down } => {
                            let events = self
                                .translator
                                .as_mut()
                                .map(|translator| translator.process(key, down))
                                .unwrap_or_default();
                            for (index, event) in events.into_iter().enumerate() {
                                if index > 0 {
                                    self.outgoing_seq = self.outgoing_seq.wrapping_add(1);
                                }
                                messages.push(InputMessage::Key(wire::InputKey {
                                    epoch: self.outgoing_epoch,
                                    seq: self.outgoing_seq,
                                    hid_usage: event.key,
                                    down: event.down,
                                }));
                            }
                        }
                        ForwardedInput::Button { button, down } => {
                            messages.push(InputMessage::Button(wire::InputButton {
                                epoch: self.outgoing_epoch,
                                seq: self.outgoing_seq,
                                button,
                                down,
                            }));
                        }
                    }
                    for message in messages {
                        if self
                            .send_reliable(&device_id, WireMessage::Input(message))
                            .await
                            .is_err()
                        {
                            self.engine.link_lost();
                            self.return_home("link_lost")?;
                            return Err(error(ErrorCode::Unreachable, "input forwarding failed"));
                        }
                    }
                }
                EngineEvent::BrainChanged { device_id, .. } => {
                    self.state.active_device_id = device_id;
                    self.dirty = true;
                }
                EngineEvent::TakeOver { device_id, .. } => {
                    self.events.push(Event::ActiveChanged(ActiveChanged {
                        device_id,
                        reason: "take_over".into(),
                    }));
                }
            }
        }
        Ok(())
    }

    /// Handle authenticated, already decoded transport events. Unpaired senders are rejected.
    pub async fn receive_link(&mut self, event: LinkEvent) -> Result<(), IpcError> {
        // The transport and manager have independent bounded queues. Apply trust/metadata
        // before dispatch, including when the manager broadcast lagged past Paired.
        self.pump_peer_events().await;
        if let LinkEvent::Reliable { peer_id, .. } | LinkEvent::Move { peer_id, .. } = &event {
            if !self
                .state
                .peers
                .iter()
                .any(|peer| &peer.device_id == peer_id)
            {
                // Connectivity is not membership. A live pin snapshot is authoritative
                // even when Core has not received Paired or the session is being replaced.
                let paired = if let Some(manager) = &self.native_manager {
                    manager
                        .paired_peers()
                        .map_err(|_| {
                            tracing::error!("could not read live pairing state");
                            error(ErrorCode::Internal, "could not read pairing state")
                        })?
                        .into_iter()
                        .find(|peer| &peer.device_id == peer_id)
                } else {
                    None
                };
                if let Some(peer) = paired {
                    self.complete_pairing(peer).await?;
                } else {
                    let _ =
                        tokio::time::timeout(Duration::from_millis(100), self.link.close(peer_id))
                            .await;
                    tracing::debug!("dropping unpaired sender");
                    return Ok(());
                }
            }
        }
        let (id, message) = match event {
            LinkEvent::Disconnected { peer_id, .. } => {
                // Events can wait for Core after a new authenticated generation has replaced them.
                if self.native_manager.is_some() && self.link.peer_token(&peer_id).is_ok() {
                    return Ok(());
                }
                self.connected_tokens.remove(&peer_id);
                self.clipboard_peer_disconnected(&peer_id);
                if let Some(peer) = self
                    .state
                    .peers
                    .iter_mut()
                    .find(|peer| peer.device_id == peer_id)
                {
                    peer.online = false;
                    peer.connection = Connection::Offline;
                    self.dirty = true;
                }
                if self.state.active_device_id == peer_id
                    || self.receiving_from.as_deref() == Some(&peer_id)
                    || self
                        .cursor_hidden_peer
                        .as_ref()
                        .is_some_and(|(id, _)| id == &peer_id)
                {
                    self.end_forwarding("link_lost").await?;
                }
                return Ok(());
            }
            LinkEvent::Move { peer_id, movement } => {
                if self.receiving_from.as_deref() != Some(&peer_id)
                    || !self.state.peers.iter().any(|p| p.device_id == peer_id)
                    || self
                        .receiving_token
                        .is_some_and(|token| self.link.peer_token(&peer_id).ok() != Some(token))
                {
                    return Ok(());
                }
                return self.inject_move(movement).await;
            }
            LinkEvent::Reliable {
                peer_id,
                peer_token,
                message,
            } => {
                if peer_token
                    .is_some_and(|token| self.link.peer_token(&peer_id).ok() != Some(token))
                {
                    return Ok(());
                }
                (peer_id, message)
            }
        };
        if !self.state.peers.iter().any(|peer| peer.device_id == id) {
            tracing::debug!("dropping message without local paired state");
            return Ok(());
        }
        match message {
            WireMessage::Clipboard(message) => {
                if let Err(failure) = self.receive_clipboard(&id, message) {
                    let _ = self.link.close(&id).await;
                    if self.engine.forwarding_to() == Some(&id)
                        || self.receiving_from.as_deref() == Some(&id)
                    {
                        self.end_forwarding("invalid_clipboard_message").await?;
                    }
                    return Err(failure);
                }
            }
            WireMessage::Control(ControlMessage::Heartbeat(beat)) => {
                self.engine.heartbeat_received(&id, Instant::now());
                if let Some((peer, last)) = &mut self.cursor_hidden_peer {
                    if *peer == id {
                        *last = Instant::now();
                    }
                }
                if self.receiving_from.as_deref() == Some(&id) {
                    self.last_receive = Instant::now();
                }
                let _ = self
                    .send_reliable(
                        &id,
                        WireMessage::Control(ControlMessage::HeartbeatAck(wire::HeartbeatAck {
                            seq: beat.seq,
                            ts: beat.ts,
                        })),
                    )
                    .await;
            }
            WireMessage::Control(ControlMessage::HeartbeatAck(_)) => {
                self.engine.heartbeat_received(&id, Instant::now());
                if let Some((peer, last)) = &mut self.cursor_hidden_peer {
                    if *peer == id {
                        *last = Instant::now();
                    }
                }
            }
            WireMessage::Control(ControlMessage::Enter(enter)) => {
                let token = self.link.peer_token(&id).ok();
                let permission_missing = self.platform.input_backend().permissions().injection
                    == PermissionStatus::Denied;
                if !self.state.sharing_enabled
                    || self.link.peer_token(&id).is_err()
                    || permission_missing
                    || self
                        .receiving_from
                        .as_ref()
                        .is_some_and(|source| source != &id)
                    || self.engine.forwarding_to().is_some()
                {
                    let _ = self
                        .send_reliable(
                            &id,
                            WireMessage::Control(ControlMessage::Bye(wire::Bye {
                                reason: "receiver_unavailable".into(),
                            })),
                        )
                        .await;
                    if permission_missing {
                        self.notify_failure(&error(
                            ErrorCode::PermissionDenied,
                            "Input injection permission is missing. Enable it in OS settings.",
                        ));
                    }
                    return Err(error(
                        ErrorCode::PermissionDenied,
                        "receiver is unavailable",
                    ));
                }
                if enter.epoch == 0
                    || self
                        .received_epochs
                        .get(&id)
                        .is_some_and(|(previous_token, epoch)| {
                            Some(*previous_token) == token && enter.epoch <= *epoch
                        })
                {
                    return Ok(());
                }
                if self.receiving_from.as_deref() == Some(&id)
                    && self
                        .receiving_epoch
                        .is_some_and(|epoch| enter.epoch <= epoch)
                {
                    return Ok(());
                }
                self.end_forwarding("incoming_enter").await?;
                self.received_epochs.retain(|peer, _| {
                    self.state
                        .peers
                        .iter()
                        .any(|known| &known.device_id == peer)
                });
                if let Some(token) = token {
                    self.received_epochs
                        .insert(id.clone(), (token, enter.epoch));
                }
                self.receiving_from = Some(id.clone());
                // The cursor just arrived: light up a screen that went dark (cannot unlock a locked one).
                let _ = self.platform.input_backend().wake_display();
                self.receiving_epoch = Some(enter.epoch);
                self.receiving_token = self.link.peer_token(&id).ok();
                self.last_receive = Instant::now();
                self.move_seq = None;
                self.input_seq = None;
                self.state.active_device_id = self.state.self_info.device_id.clone();
                self.inject(InputEventKind::PointerMoved {
                    position: enter.pos,
                    delta_x: 0.0,
                    delta_y: 0.0,
                })
                .await?;
                for key in enter.modifiers_down.iter() {
                    self.inject(InputEventKind::Key {
                        key: *key,
                        down: true,
                    })
                    .await?;
                }
                if self.link.admit_input_epoch(&id, enter.epoch).is_err() {
                    self.end_forwarding("input_admission_failed").await?;
                    return Err(error(ErrorCode::Unreachable, "input admission failed"));
                }
                self.events.push(Event::ActiveChanged(ActiveChanged {
                    device_id: self.state.self_info.device_id.clone(),
                    reason: "incoming_enter".into(),
                }));
                self.dirty = true;
            }
            WireMessage::Control(ControlMessage::Leave(leave))
                if self.receiving_from.as_deref() == Some(&id)
                    && self.receiving_epoch == Some(leave.epoch) =>
            {
                self.end_forwarding("remote_leave").await?;
                // The cursor went back to the computer whose mouse is moving it.
                self.set_cursor_place(&id, None);
                self.cursor_visibility(false);
                self.cursor_hidden_peer = Some((id.clone(), Instant::now()));
            }
            WireMessage::Control(ControlMessage::TakeOver(takeover))
                if self.engine.forwarding_to() == Some(&id) =>
            {
                let events = self
                    .engine
                    .local_input(&id, false, takeover.pos, Instant::now());
                self.process_engine_events(events).await?;
                self.cursor_visibility(false);
                self.cursor_hidden_peer = Some((id.clone(), Instant::now()));
                self.platform
                    .input_backend()
                    .set_mode(CaptureMode::Local)
                    .map_err(|_| {
                        error(
                            ErrorCode::PermissionDenied,
                            "could not restore local capture",
                        )
                    })?;
            }
            WireMessage::Control(ControlMessage::Unpaired) => {
                self.end_forwarding("remote_unpaired").await?;
                let mut next = self.state.clone();
                next.peers.retain(|peer| peer.device_id != id);
                next.layout.devices.retain(|device| device.device_id != id);
                self.apply(next)?;
                let _ = self.manager.unpair(&id).await;
                let _ = self.link.close(&id).await;
            }
            WireMessage::Control(ControlMessage::Bye(_)) => {
                self.end_forwarding("peer_left").await?;
            }
            WireMessage::Control(ControlMessage::Details(details)) => {
                let model = DeviceModel {
                    name: details.model,
                    kind: details.kind,
                    builtin_monitor: details.builtin_monitor,
                };
                if self
                    .state
                    .peers
                    .iter()
                    .any(|p| p.device_id == id && p.model.as_ref() != Some(&model))
                {
                    let mut next = self.state.clone();
                    if let Some(peer) = next.peers.iter_mut().find(|p| p.device_id == id) {
                        peer.model = Some(model);
                    }
                    self.apply(next)?;
                    self.dirty = true;
                }
            }
            WireMessage::Control(ControlMessage::Arrange(arrange)) => {
                // A paired computer arranged this computer's screens on its Desk: apply it exactly as if it was done here.
                let arrangement: Vec<Value> = arrange
                    .screens
                    .iter()
                    .map(|s| json!({ "monitor_id": s.monitor_id, "x": s.x, "y": s.y }))
                    .collect();
                let patch = json!({ "patch": { "display": { "arrangement": arrangement } } });
                if let Err(failure) = Box::pin(self.dispatch("set_settings", patch)).await {
                    tracing::debug!(code = ?failure.code, "remote screen arrangement was not applied");
                }
                self.dirty = true;
            }
            WireMessage::Control(ControlMessage::LayoutUpdate(update)) => {
                if update.version.0 == u64::MAX {
                    tracing::debug!("dropping exhausted remote layout clock");
                    return Ok(());
                }
                if update.version > self.layout_version {
                    let layout = match reconcile_layout(&update.devices, &self.state) {
                        Ok(layout) => layout,
                        Err(_) => {
                            tracing::debug!("dropping malformed remote layout");
                            return Ok(());
                        }
                    };
                    if !layout_replication::same_placements(&layout, &self.state.layout) {
                        self.end_forwarding("layout_changed").await?;
                    }
                    let mut next = self.state.clone();
                    let reconciled = layout.devices != *update.devices;
                    next.layout = layout;
                    self.commit_layout(next, Some((update.version, reconciled)))?;
                    self.broadcast_layout().await;
                }
            }
            WireMessage::Input(input)
                if self.receiving_epoch == Some(input.epoch())
                    && self.receiving_from.as_deref() == Some(&id)
                    && self
                        .receiving_token
                        .is_some_and(|token| self.link.peer_token(&id).ok() == Some(token)) =>
            {
                let seq = match &input {
                    InputMessage::Key(event) => event.seq,
                    InputMessage::Button(event) => event.seq,
                    InputMessage::Wheel(event) => event.seq,
                    InputMessage::ModifierSync(event) => event.seq,
                };
                if !newer(seq, self.input_seq) {
                    return Ok(());
                }
                self.input_seq = Some(seq);
                match input {
                    InputMessage::Key(event) => {
                        self.inject(InputEventKind::Key {
                            key: event.hid_usage,
                            down: event.down,
                        })
                        .await?
                    }
                    InputMessage::Button(event) => {
                        self.inject(InputEventKind::Button {
                            button: event.button,
                            down: event.down,
                        })
                        .await?
                    }
                    InputMessage::Wheel(event) => {
                        let (dx, dy, precise) =
                            crate::wheel::from_wire(self.state.self_info.os, event.dx, event.dy);
                        self.inject(InputEventKind::Wheel { dx, dy, precise })
                            .await?
                    }
                    InputMessage::ModifierSync(event) => {
                        if event
                            .modifiers_down
                            .iter()
                            .any(|key| !(0xe0..=0xe7).contains(&key.0))
                        {
                            return Err(error(
                                ErrorCode::InvalidParams,
                                "invalid modifier snapshot",
                            ));
                        }
                        for index in 0..8 {
                            let key = Key(0xe0 + index as u16);
                            let down = event.modifiers_down.contains(&key);
                            if self.receiving_modifiers[index] != down {
                                self.inject(InputEventKind::Key { key, down }).await?;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn inject(&mut self, mut kind: InputEventKind) -> Result<(), IpcError> {
        if let InputEventKind::PointerMoved {
            ref mut position, ..
        } = kind
        {
            // Positions arrive in the arranged screens; the operating system needs its real ones.
            *position = crate::arrangement::to_native(
                &self.native_monitors,
                &self.state.self_info.monitors,
                *position,
            );
            if let Some(clamped) =
                glide_platform::clamp_cursor_to_monitors(&self.native_monitors, *position)
            {
                *position = clamped;
            }
        }
        let mut result = self.platform.input_backend().inject(InputEvent {
            kind,
            injected: true,
        });
        if matches!(result, Err(BackendError::InvalidPosition)) {
            // A hotplug may have raced the snapshot. Retry exactly once on fresh geometry.
            if let InputEventKind::PointerMoved {
                ref mut position, ..
            } = kind
            {
                if let Ok(monitors) = self.platform.input_backend().monitors() {
                    if let Some(clamped) =
                        glide_platform::clamp_cursor_to_monitors(&monitors, *position)
                    {
                        *position = clamped;
                        result = self.platform.input_backend().inject(InputEvent {
                            kind,
                            injected: true,
                        });
                    }
                }
            }
        }
        if let Err(failure) = result {
            let action = self
                .injection_health
                .failure(failure.injection_failure(), Instant::now());
            if self.injection_health.dropped.is_power_of_two() {
                tracing::debug!(dropped=self.injection_health.dropped, class=?failure.injection_failure(), "input events dropped");
            }
            match action {
                InjectionAction::NotifyElevated => self.events.push(Event::Notification(Notification {
                    level:"warning".into(), title:"Windows is blocking Glide".into(),
                    body:"Windows is blocking Glide from controlling an app that is running as administrator. Close that app or run Glide as administrator.".into(), action:None,
                })),
                InjectionAction::EndPermission | InjectionAction::EndTransient => {
                    let denied = action == InjectionAction::EndPermission;
                    let reason = if denied { "injection_denied" } else { "injection_unavailable" };
                    // Safety restoration and releases precede any network wait, even a stuck writer.
                    self.end_forwarding(reason).await?;
                    let failure = if denied {
                        error(ErrorCode::PermissionDenied,"OS input injection permission was revoked. Enable it in OS settings.")
                    } else { error(ErrorCode::Internal,"Glide stopped controlling this computer because the OS kept rejecting input. Return to the normal desktop and try again.") };
                    if denied { self.notify_failure(&failure); }
                    else { self.events.push(Event::Notification(Notification {level:"warning".into(),title:"Input is unavailable".into(),body:failure.message.clone(),action:None})); }
                    return Err(failure);
                }
                InjectionAction::Drop => {},
            }
            return Ok(());
        }
        self.injection_health.reset_run();
        if let InputEventKind::Key { key, down } = kind {
            if (0xe0..=0xe7).contains(&key.0) {
                self.receiving_modifiers[usize::from(key.0 - 0xe0)] = down;
            }
        }
        Ok(())
    }

    fn cursor_visibility(&mut self, visible: bool) {
        let failed = self
            .platform
            .input_backend()
            .set_cursor_visible(visible)
            .is_err();
        self.cursor_restore_pending = visible && failed;
        if failed {
            tracing::debug!(visible, "cursor visibility unavailable");
        }
    }

    pub(super) fn cursor_watchdog(&mut self, now: Instant) {
        if self.cursor_restore_pending
            || self.cursor_hidden_peer.as_ref().is_some_and(|(id, last)| {
                now.saturating_duration_since(*last) >= Duration::from_millis(1500)
                    || self.link.peer_token(id).is_err()
            })
        {
            self.cursor_visibility(true);
            if !self.cursor_restore_pending {
                self.cursor_hidden_peer = None;
            }
        }
    }

    pub(super) async fn broadcast_layout(&self) {
        let Ok(devices) = wire::LayoutDevices::try_from_vec(self.state.layout.devices.clone())
        else {
            return;
        };
        for peer in self.state.peers.iter().filter(|peer| peer.online) {
            let _ = self
                .send_reliable(
                    &peer.device_id,
                    WireMessage::Control(ControlMessage::LayoutUpdate(wire::LayoutUpdate {
                        version: self.layout_version.clone(),
                        devices: devices.clone(),
                    })),
                )
                .await;
        }
    }

    pub(super) async fn pump_peer_events(&mut self) {
        let mut pending = self.pending_pairings.clone();
        if let Some(manager) = &self.native_manager {
            match manager.paired_peers() {
                Ok(peers) => {
                    // Recover lost/lagged Paired events; never retry a revoked pin.
                    pending.retain(|id, _| peers.iter().any(|p| &p.device_id == id));
                    self.pending_pairings
                        .retain(|id, _| peers.iter().any(|p| &p.device_id == id));
                    for peer in peers {
                        if !self
                            .state
                            .peers
                            .iter()
                            .any(|p| p.device_id == peer.device_id)
                        {
                            pending.entry(peer.device_id.clone()).or_insert(peer);
                        }
                    }
                }
                Err(_) => {
                    tracing::error!("could not reconcile pairing state");
                    self.pending_pairings = pending;
                    return;
                }
            }
        }
        for peer in pending.into_values() {
            if let Err(failure) = self.complete_pairing(peer).await {
                tracing::error!(code = ?failure.code, "peer registration pending or rejected");
            }
        }
        for _ in 0..128 {
            let event = match self.peer_events.try_recv() {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                    let _ = self.end_forwarding("peer_event_overflow").await;
                    continue;
                }
                Err(_) => break,
            };
            match event {
                PeerManagerEvent::DiscoveryRemoved { device_id } => {
                    self.state
                        .discovered
                        .retain(|peer| peer.device_id != device_id);
                    self.dirty = true;
                }
                PeerManagerEvent::Discovered(peer) => {
                    if !self.state.settings.network.discovery
                        || self
                            .state
                            .peers
                            .iter()
                            .any(|existing| existing.device_id == peer.device_id)
                    {
                        continue;
                    }
                    self.state
                        .discovered
                        .retain(|existing| existing.device_id != peer.device_id);
                    if self.state.discovered.len() < 32 {
                        self.state.discovered.push(peer);
                        self.dirty = true;
                    }
                }
                PeerManagerEvent::PairingIncoming { name, os, address } => {
                    self.events.push(Event::PairingIncoming(PairingIncoming {
                        name,
                        os,
                        address,
                    }))
                }
                PeerManagerEvent::PairingVerify(verify) => {
                    self.verification_deadline = Some(verify.expires_at_ms);
                    self.events.push(Event::PairingVerify(verify));
                }
                PeerManagerEvent::Paired(peer) => {
                    if self.native_manager.as_ref().is_some_and(|manager| {
                        !manager
                            .paired_peers()
                            .is_ok_and(|peers| peers.iter().any(|p| p.device_id == peer.device_id))
                    }) {
                        continue;
                    }
                    if let Err(failure) = self.complete_pairing(peer).await {
                        tracing::error!(code = ?failure.code, "peer registration pending or rejected");
                    }
                }
                PeerManagerEvent::PairingResult {
                    ok: false,
                    error: Some(code),
                    ..
                } if self.pending_join_id.is_some() || self.verification_deadline.is_some() => {
                    self.abort_pairing(error(code, pairing_message(code)))
                }
                PeerManagerEvent::PairingResult {
                    ok: false,
                    error: Some(code),
                    ..
                } => {
                    tracing::debug!(?code, "unsolicited pairing attempt failed");
                }
                PeerManagerEvent::PairingResult { .. } => {}
                PeerManagerEvent::PeerUpdated(peer) => {
                    if self.native_manager.as_ref().is_some_and(|manager| {
                        !manager
                            .paired_peers()
                            .is_ok_and(|peers| peers.iter().any(|p| p.device_id == peer.device_id))
                    }) {
                        continue;
                    }
                    // Keep the actual metadata event when persistence is pending; the
                    // durable trust snapshot is offline and cannot replace this transition.
                    if !self
                        .state
                        .peers
                        .iter()
                        .any(|p| p.device_id == peer.device_id)
                        && self.native_manager.is_some()
                    {
                        if let Err(failure) = self.complete_pairing(peer.clone()).await {
                            tracing::error!(code = ?failure.code, "peer registration pending or rejected");
                        }
                    }
                    if self
                        .state
                        .peers
                        .iter()
                        .any(|existing| existing.device_id == peer.device_id)
                    {
                        let mut connected = peer.online
                            && self
                                .state
                                .peers
                                .iter()
                                .any(|old| old.device_id == peer.device_id && !old.online);
                        if self.native_manager.is_some() {
                            if let Ok(token) = self.link.peer_token(&peer.device_id) {
                                connected = peer.online
                                    && self.connected_tokens.insert(peer.device_id.clone(), token)
                                        != Some(token);
                                if connected {
                                    self.clipboard_peer_disconnected(&peer.device_id);
                                }
                            }
                        }
                        let desktop_changed = connected
                            || self.state.peers.iter().any(|old| {
                                old.device_id == peer.device_id
                                    && (old.monitors != peer.monitors || old.online != peer.online)
                            });
                        let peer_id = peer.device_id.clone();
                        if desktop_changed {
                            let _ = self.end_forwarding("peer_monitors_changed").await;
                        }
                        let mut next = self.state.clone();
                        if let Some(existing) = next
                            .peers
                            .iter_mut()
                            .find(|existing| existing.device_id == peer.device_id)
                        {
                            let clipboard_enabled = existing.clipboard_enabled;
                            let wake_mac = existing.wake_mac.take();
                            let model = existing.model.take();
                            let mut last_monitors = std::mem::take(&mut existing.last_monitors);
                            if !peer.monitors.is_empty() {
                                last_monitors = peer.monitors.clone();
                            }
                            *existing = peer;
                            existing.clipboard_enabled = clipboard_enabled;
                            existing.wake_mac = wake_mac;
                            existing.model = model;
                            existing.last_monitors = last_monitors;
                            // Just connected over the real network: note its network card address for waking it later.
                            if connected
                                && self.native_manager.is_some()
                                && self.mock_platform.is_none()
                            {
                                if let Some(ip) = crate::wake::peer_ip(existing) {
                                    let found = self.wake_found.clone();
                                    let device_id = existing.device_id.clone();
                                    tokio::task::spawn_blocking(move || {
                                        if let Some(mac) = crate::wake::lookup_mac(ip) {
                                            found
                                                .lock()
                                                .unwrap_or_else(|e| e.into_inner())
                                                .push((device_id, mac));
                                        }
                                    });
                                }
                            }
                        }
                        let layout_changed = match reconcile_layout(&next.layout.devices, &next) {
                            Ok(layout) if layout != next.layout => {
                                next.layout = layout;
                                true
                            }
                            _ => false,
                        };
                        let saved = if layout_changed {
                            self.commit_layout(next.clone(), None)
                        } else {
                            self.persist(&next)
                        };
                        if let Err(failure) = saved {
                            self.notify_failure(&failure);
                            self.shutdown = true;
                        }
                        self.state = next;
                        if desktop_changed {
                            if let Err(failure) = self.rebuild_desktop() {
                                self.notify_failure(&failure);
                                self.shutdown = true;
                            }
                        }
                        self.dirty = true;
                        if connected {
                            self.clipboard_peer_connected(&peer_id);
                            self.send_details(Some(&peer_id)).await;
                        }
                        if connected || layout_changed {
                            self.broadcast_layout().await;
                        }
                    }
                }
                PeerManagerEvent::Stats(stats) => {
                    let now = Instant::now();
                    if self
                        .last_stats
                        .get(&stats.device_id)
                        .is_none_or(|last| now.duration_since(*last) >= Duration::from_secs(1))
                    {
                        if let Some(peer) = self
                            .state
                            .peers
                            .iter_mut()
                            .find(|peer| peer.device_id == stats.device_id)
                        {
                            peer.latency_ms = Some(stats.latency_ms);
                            self.events.push(Event::PeerStats(stats.clone()));
                            self.last_stats.insert(stats.device_id, now);
                            self.dirty = true;
                        }
                    }
                }
                PeerManagerEvent::Heartbeat { device_id } => {
                    self.engine.heartbeat_received(&device_id, Instant::now())
                }
                PeerManagerEvent::TakeOver { device_id, pos } => {
                    if self.engine.forwarding_to() == Some(&device_id) {
                        let events =
                            self.engine
                                .local_input(&device_id, false, pos, Instant::now());
                        let _ = self.process_engine_events(events).await;
                    }
                }
                PeerManagerEvent::Unpaired(device_id) => {
                    self.pending_pairings.remove(&device_id);
                    if self.state.peers.iter().any(|p| p.device_id == device_id) {
                        let _ = self.end_forwarding("remote_unpaired").await;
                        if self.manager.unpair(&device_id).await.is_err() {
                            self.shutdown = true;
                        }
                        let mut next = self.state.clone();
                        next.peers.retain(|p| p.device_id != device_id);
                        next.layout.devices.retain(|d| d.device_id != device_id);
                        if self.apply(next).is_err() {
                            self.shutdown = true;
                        }
                    }
                }
                PeerManagerEvent::InjectionDenied { device_id } => {
                    if self.state.active_device_id == device_id
                        || self.receiving_from.as_deref() == Some(&device_id)
                    {
                        let _ = self.end_forwarding("peer_cannot_inject").await;
                    }
                    tracing::debug!("peer cannot receive input");
                }
                PeerManagerEvent::Disappeared { device_id } => {
                    if self.native_manager.is_some() && self.link.peer_token(&device_id).is_ok() {
                        continue;
                    }
                    self.connected_tokens.remove(&device_id);
                    self.clipboard_peer_disconnected(&device_id);
                    if self.state.active_device_id == device_id
                        || self.receiving_from.as_deref() == Some(&device_id)
                    {
                        let _ = self.end_forwarding("peer_unavailable").await;
                    }
                    if let Some(peer) = self
                        .state
                        .peers
                        .iter_mut()
                        .find(|peer| peer.device_id == device_id)
                    {
                        peer.online = false;
                        peer.connection = Connection::Offline;
                        self.dirty = true;
                    }
                }
            }
        }
    }
}

fn newer(seq: u64, previous: Option<u64>) -> bool {
    previous.is_none_or(|previous| seq != previous && seq.wrapping_sub(previous) < (1 << 63))
}

fn pairing_message(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::BadCode => "The pairing code did not match. Read the current code on the other device and try again.",
        ErrorCode::CodeExpired => "The pairing code or confirmation expired. Start pairing again on both devices.",
        ErrorCode::LockedOut => "Too many incorrect attempts. Wait for the rate limit, then start pairing with a new code.",
        ErrorCode::PermissionDenied => "Pairing was declined or the device rejected the request. Start again and confirm only if both phrases match.",
        ErrorCode::Unreachable => "The other device disconnected or could not be reached. Check that Glide is running on both devices.",
        _ => "Pairing could not be completed. Start pairing again on both devices.",
    }
}

/// The screen containing `p`, a point in the computer's own coordinates (its screens' top-left corner is 0,0).
fn monitor_at(monitors: &[glide_platform::Monitor], p: Point) -> Option<String> {
    let min_x = monitors.iter().map(|m| m.x).fold(f64::INFINITY, f64::min);
    let min_y = monitors.iter().map(|m| m.y).fold(f64::INFINITY, f64::min);
    let inside = |m: &&glide_platform::Monitor, slack: f64| {
        let (x, y) = (p.x + min_x, p.y + min_y);
        x >= m.x - slack && x <= m.x + m.w + slack && y >= m.y - slack && y <= m.y + m.h + slack
    };
    monitors
        .iter()
        .find(|m| inside(m, 0.0))
        .or_else(|| monitors.iter().find(|m| inside(m, 2.0)))
        .map(|m| m.id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn administrator_warning_is_once_per_minute_even_across_sessions() {
        // Prevents permission-toast spam while an administrator app remains foreground.
        let start = Instant::now();
        let mut health = InjectionHealth::default();
        assert_eq!(
            health.failure(InjectionFailure::TargetElevated, start),
            InjectionAction::NotifyElevated
        );
        for second in 1..60 {
            health.reset_run(); // returning home must not reset notification admission
            assert_eq!(
                health.failure(
                    InjectionFailure::TargetElevated,
                    start + Duration::from_secs(second)
                ),
                InjectionAction::Drop
            );
        }
        assert_eq!(
            health.failure(
                InjectionFailure::TargetElevated,
                start + Duration::from_secs(60)
            ),
            InjectionAction::NotifyElevated
        );
        assert_eq!(health.consecutive, 0);
    }

    #[test]
    fn transient_input_ends_only_after_fifty_failures_or_three_seconds() {
        // Prevents one transient injection rejection disconnecting a working KVM session.
        let start = Instant::now();
        let mut health = InjectionHealth::default();
        for _ in 0..49 {
            assert_eq!(
                health.failure(InjectionFailure::Transient, start),
                InjectionAction::Drop
            );
        }
        health.reset_run();
        assert_eq!(
            health.failure(InjectionFailure::Transient, start),
            InjectionAction::Drop
        );
        assert_eq!(
            health.failure(
                InjectionFailure::Transient,
                start + Duration::from_millis(2999)
            ),
            InjectionAction::Drop
        );
        assert_eq!(
            health.failure(InjectionFailure::Transient, start + Duration::from_secs(3)),
            InjectionAction::EndTransient
        );
        health.reset_run();
        for _ in 0..49 {
            health.failure(InjectionFailure::Transient, start);
        }
        assert_eq!(
            health.failure(InjectionFailure::Transient, start),
            InjectionAction::EndTransient
        );
        assert_eq!(
            health.failure(InjectionFailure::PermissionDenied, start),
            InjectionAction::EndPermission
        );
    }

    #[test]
    fn hotkeys_and_sequence_wrap() {
        assert!(Hotkey::parse("Home").is_err());
        assert!(Hotkey::parse("Ctrl+A+B").is_err());
        let key = Hotkey::parse("Ctrl+Alt+Shift+Home").expect("default hotkey");
        let mut held = [false; 256];
        held[0xe4] = true;
        held[0xe5] = true;
        held[0xe6] = true;
        assert!(key.matches(Key(0x4a), &held));
        held[0xe7] = true;
        assert!(!key.matches(Key(0x4a), &held));
        assert!(newer(0, None));
        assert!(!newer(1, Some(1)));
        assert!(!newer(1, Some(2)));
        assert!(newer(0, Some(u64::MAX)));
    }
}
