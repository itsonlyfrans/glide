use super::*;
use glide_platform::{
    ClipboardAdmission, ClipboardChangeToken, ClipboardContent, ClipboardData, ClipboardEvent,
    ClipboardFormat, ClipboardMarker, ClipboardPublish, ClipboardSensitivity, ClipboardSnapshot,
};
use glide_proto::wire::{self, ClipboardMessage, WireMessage};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use tokio::task::JoinHandle;

#[path = "transfer.rs"]
mod transfer;

pub(super) const EAGER_BYTES: usize = 8 * 1024 * 1024;
const LOCAL_INTERVAL: Duration = Duration::from_millis(250);

struct Incoming {
    peer: String,
    token: glide_net::PeerToken,
    announcement: wire::ClipAnnounce,
    payloads: Vec<ClipboardContent>,
    started: Instant,
    confirmation: Option<String>,
    baseline: Option<ClipboardChangeToken>,
    baseline_read: Option<JoinHandle<Result<ClipboardSnapshot, glide_platform::BackendError>>>,
    native_pending: bool,
    native_manifest: Option<glide_proto::wire::FileManifest>,
    native_consent: Option<glide_xfer::Consent>,
    native_fetch_requested: bool,
    native_fetch_at: Option<Instant>,
    received: Option<glide_xfer::Received>,
}

type ClipboardWrite = (
    ClipboardPublish,
    Option<glide_xfer::Received>,
    ClipboardMarker,
);

pub(super) struct ClipboardSync {
    changes: crossbeam_channel::Receiver<ClipboardEvent>,
    bridged: bool,
    ready: Arc<tokio::sync::Notify>,
    pub(super) pending: bool,
    pub(super) read: Option<JoinHandle<Result<ClipboardSnapshot, glide_platform::BackendError>>>,
    read_version: (u64, String, String),
    pub(super) send: Vec<JoinHandle<()>>,
    pub(super) write: Option<JoinHandle<Result<ClipboardWrite, glide_platform::BackendError>>>,
    gate: Arc<AtomicU64>,
    admission_generation: Arc<AtomicU64>,
    next_admission: u64,
    admission: Option<ClipboardAdmission>,
    markers: VecDeque<ClipboardMarker>,
    pub(super) version: (u64, String, String),
    snapshot_token: Option<ClipboardChangeToken>,
    outgoing: Option<transfer::Outgoing>,
    incoming: Option<Incoming>,
    writing_confirmation: Option<String>,
    last_local: Instant,
    native_link: Option<glide_net::NativeLink>,
    file_engine: Option<glide_xfer::FileEngine>,
    accept_task: Option<JoinHandle<()>>,
    accepted: Option<tokio::sync::mpsc::Receiver<glide_net::TransferStreams>>,
    native_jobs: HashMap<String, transfer::NativeJob>,
    pending_native_sends: VecDeque<transfer::PendingNativeSend>,
    native_send_limiter: HashMap<String, Instant>,
    pending_native_receives: VecDeque<glide_net::TransferStreams>,
    received_leases: VecDeque<glide_xfer::Received>,
    active_file_marker: Option<ClipboardMarker>,
    writing_peer: Option<String>,
}

impl ClipboardSync {
    pub(super) fn new(changes: crossbeam_channel::Receiver<ClipboardEvent>) -> Self {
        Self {
            changes,
            bridged: false,
            ready: Arc::new(tokio::sync::Notify::new()),
            pending: false,
            read: None,
            send: Vec::new(),
            write: None,
            read_version: (0, String::new(), String::new()),
            gate: Arc::new(AtomicU64::new(0)),
            admission_generation: Arc::new(AtomicU64::new(0)),
            next_admission: 1,
            admission: None,
            markers: VecDeque::new(),
            version: (0, String::new(), String::new()),
            snapshot_token: None,
            outgoing: None,
            incoming: None,
            writing_confirmation: None,
            last_local: Instant::now() - LOCAL_INTERVAL,
            native_link: None,
            file_engine: None,
            accept_task: None,
            accepted: None,
            native_jobs: HashMap::new(),
            pending_native_sends: VecDeque::new(),
            native_send_limiter: HashMap::new(),
            pending_native_receives: VecDeque::new(),
            received_leases: VecDeque::new(),
            active_file_marker: None,
            writing_peer: None,
        }
    }

    #[cfg(test)]
    pub(super) fn is_settled(&self) -> bool {
        self.read.is_none()
            && self.write.is_none()
            && self.send.is_empty()
            && self
                .incoming
                .as_ref()
                .is_none_or(|incoming| incoming.baseline_read.is_none())
    }

    pub(super) fn invalidate(&mut self) {
        self.gate.fetch_add(1, Ordering::AcqRel);
        self.revoke_admission();
        for send in self.send.drain(..) {
            send.abort();
        }
        for job in self.native_jobs.values() {
            job.cancel.cancel();
        }
        self.pending_native_sends.clear();
        for streams in self.pending_native_receives.drain(..) {
            streams.handle.cancel();
        }
        // Blocking OS work cannot be aborted; its publication gate rejects stale results.
        self.incoming = None;
        self.outgoing = None;
        self.pending = false;
        self.read_version.0 = self.read_version.0.wrapping_add(1);
    }

    fn revoke_admission(&mut self) {
        if let Some(admission) = self.admission.take() {
            admission.revoke();
        }
    }

    pub(super) fn cancel_confirmation(&mut self, id: &str) {
        if self
            .incoming
            .as_ref()
            .is_some_and(|incoming| incoming.confirmation.as_deref() == Some(id))
        {
            self.incoming = None;
        }
        if self.writing_confirmation.as_deref() == Some(id) {
            self.revoke_admission();
            self.gate.fetch_add(1, Ordering::AcqRel);
        }
        if let Some(job) = self.native_jobs.remove(id) {
            job.cancel.cancel();
        }
        self.pending_native_sends
            .retain(|pending| transfer::transfer_id(&pending.peer, &pending.clip_id) != id);
        self.pending_native_receives.retain(|streams| {
            let matches = transfer::transfer_id(&streams.peer_id, &streams.transfer_id) == id;
            if matches {
                streams.handle.cancel();
            }
            !matches
        });
        if self.incoming.as_ref().is_some_and(|incoming| {
            incoming.confirmation.as_deref() == Some(id)
                || transfer::transfer_id(&incoming.peer, &incoming.announcement.clip_id) == id
        }) {
            self.incoming = None;
        }
    }
}

impl Drop for ClipboardSync {
    fn drop(&mut self) {
        self.invalidate();
        if let Some(task) = self.accept_task.take() {
            task.abort();
        }
        if let Some(accepted) = self.accepted.as_mut() {
            while let Ok(streams) = accepted.try_recv() {
                streams.handle.cancel();
            }
        }
        for streams in self.pending_native_receives.drain(..) {
            streams.handle.cancel();
        }
        if self.active_file_marker.is_some() {
            std::mem::forget(std::mem::take(&mut self.received_leases));
        }
    }
}

fn kind(format: &ClipboardFormat) -> Option<(&'static str, &'static str)> {
    match format {
        ClipboardFormat::Text => Some(("text", "text/plain")),
        ClipboardFormat::Html => Some(("html", "text/html")),
        ClipboardFormat::Rtf => Some(("rtf", "text/rtf")),
        ClipboardFormat::Png => Some(("png", "image/png")),
        _ => None,
    }
}

fn format(kind: &str) -> Option<ClipboardFormat> {
    match kind {
        "text" => Some(ClipboardFormat::Text),
        "html" => Some(ClipboardFormat::Html),
        "rtf" => Some(ClipboardFormat::Rtf),
        "png" => Some(ClipboardFormat::Png),
        _ => None,
    }
}

fn enabled(settings: &ClipboardSettings, format: &ClipboardFormat) -> bool {
    settings.enabled
        && match format {
            ClipboardFormat::Text | ClipboardFormat::Html | ClipboardFormat::Rtf => {
                settings.sync_text
            }
            ClipboardFormat::Png => settings.sync_images,
            ClipboardFormat::Files => settings.sync_files,
            _ => false,
        }
}

impl Core {
    pub(crate) fn initialize_native_clipboard(
        &mut self,
        link: glide_net::NativeLink,
        engine: glide_xfer::FileEngine,
    ) {
        self.stop_native_clipboard();
        let _ = purge_outgoing_clipboard_staging(&self.data_dir);
        let (sender, receiver) = tokio::sync::mpsc::channel(4);
        let accept_link = link.clone();
        self.clipboard.accept_task = Some(tokio::spawn(async move {
            while let Ok(streams) = accept_link.accept_transfer().await {
                if sender.send(streams).await.is_err() {
                    break;
                }
            }
        }));
        self.clipboard.accepted = Some(receiver);
        self.clipboard.native_link = Some(link);
        self.clipboard.file_engine = Some(engine);
    }

    /// Stop the native transport worker without releasing files still referenced by the OS clipboard.
    pub(crate) fn stop_native_clipboard(&mut self) {
        if let Some(task) = self.clipboard.accept_task.take() {
            task.abort();
        }
        if let Some(accepted) = self.clipboard.accepted.as_mut() {
            while let Ok(streams) = accepted.try_recv() {
                streams.handle.cancel();
            }
        }
        self.clipboard.accepted = None;
        self.clipboard.native_link = None;
        self.clipboard.pending_native_sends.clear();
        for streams in self.clipboard.pending_native_receives.drain(..) {
            streams.handle.cancel();
        }
        for job in self.clipboard.native_jobs.values() {
            job.cancel.cancel();
        }
    }

    /// Await acceptance-loop cancellation before the owning transport is released.
    pub(crate) async fn stop_native_clipboard_and_wait(&mut self) {
        let accept = self.clipboard.accept_task.take();
        if let Some(task) = &accept {
            task.abort();
        }
        self.stop_native_clipboard();
        if let Some(task) = accept {
            let _ = task.await;
        }
    }

    /// Clear our own file clipboard before releasing its received-path leases during shutdown.
    pub(crate) async fn clipboard_shutdown(&mut self) {
        self.clipboard.invalidate();
        let accept = self.clipboard.accept_task.take();
        if let Some(task) = &accept {
            task.abort();
        }
        self.stop_native_clipboard();
        if let Some(task) = accept {
            let _ = task.await;
        }

        if let Some(job) = self.clipboard.write.take() {
            match job.await {
                Ok(Ok((ClipboardPublish::Published { .. }, received, marker))) => {
                    self.clipboard.active_file_marker = received.as_ref().map(|_| marker);
                    if let Some(received) = received {
                        if self.clipboard.received_leases.len() < 2 {
                            self.clipboard.received_leases.push_back(received);
                        }
                    }
                }
                Ok(Ok((ClipboardPublish::PartialFailure { cleared, .. }, received, marker))) => {
                    if cleared {
                        self.clipboard.received_leases.clear();
                        self.clipboard.active_file_marker = None;
                    } else if let Some(received) = received {
                        if self.clipboard.received_leases.len() < 2 {
                            self.clipboard.received_leases.push_back(received);
                        }
                        self.clipboard.active_file_marker = Some(marker);
                    }
                }
                Ok(Ok((ClipboardPublish::ReplacedLocalChange { .. }, _, _)))
                | Ok(Ok((ClipboardPublish::Revoked, _, _)))
                | Ok(Err(_))
                | Err(_) => {}
            }
        }

        let tasks = self
            .clipboard
            .native_jobs
            .drain()
            .map(|(_, job)| job.task)
            .collect::<Vec<_>>();
        for mut task in tasks {
            if tokio::time::timeout(Duration::from_secs(2), &mut task)
                .await
                .is_err()
            {
                task.abort();
            }
        }

        let marker = self.clipboard.active_file_marker;
        let mut safe_to_release = marker.is_none();
        if let Some(marker) = marker {
            let backend = self.platform.clone();
            let current =
                tokio::task::spawn_blocking(move || backend.clipboard_backend().read_snapshot())
                    .await;
            if let Ok(Ok(snapshot)) = current {
                if snapshot.marker == Some(marker) {
                    let backend = self.platform.clone();
                    let admission = ClipboardAdmission::default();
                    let clear_marker = ClipboardMarker(rand::random());
                    let clear = tokio::task::spawn_blocking(move || {
                        backend.clipboard_backend().publish_snapshot(
                            Vec::new(),
                            clear_marker,
                            snapshot.change_token,
                            admission,
                        )
                    })
                    .await;
                    safe_to_release = matches!(
                        clear,
                        Ok(Ok(ClipboardPublish::Published { .. }
                            | ClipboardPublish::ReplacedLocalChange { .. }))
                            | Ok(Ok(ClipboardPublish::PartialFailure { cleared: true, .. }))
                    );
                    if !safe_to_release {
                        self.clipboard_notice(
                            "Glide could not clear its received files from the system clipboard.",
                        );
                    }
                } else {
                    safe_to_release = true;
                }
            } else {
                self.clipboard_notice("Glide could not check its received files before shutdown.");
            }
        }

        if safe_to_release {
            self.clipboard.received_leases.clear();
            self.clipboard.active_file_marker = None;
        } else {
            // Keep the path leases alive until process exit rather than leave a live clipboard path unleased.
            std::mem::forget(std::mem::take(&mut self.clipboard.received_leases));
        }
        self.clipboard.incoming = None;
        self.clipboard.outgoing = None;
    }

    /// A transport connection is a reason to reread and reannounce the current local clipboard.
    pub(crate) fn clipboard_peer_connected(&mut self, peer_id: &str) {
        let enabled = self.clipboard_peer_enabled(peer_id);
        let mut reannounce = false;
        if enabled {
            if let Ok(token) = self.link.peer_token(peer_id) {
                if let Some(incoming) = self
                    .clipboard
                    .incoming
                    .as_mut()
                    .filter(|incoming| incoming.peer == peer_id)
                {
                    incoming.token = token;
                    incoming.native_fetch_requested = false;
                    incoming.native_fetch_at = None;
                    incoming.started = Instant::now();
                }
            }
            if let Some(outgoing) = self.clipboard.outgoing.clone() {
                reannounce = true;
                self.send_clipboard_offer(vec![peer_id.to_owned()], &outgoing);
            }
        }
        if !reannounce {
            self.clipboard.pending = true;
            self.clipboard.last_local = Instant::now() - LOCAL_INTERVAL;
        }
        if self.clipboard.incoming.as_ref().is_some_and(|incoming| {
            incoming.peer == peer_id && incoming.native_pending && incoming.confirmation.is_none()
        }) {
            let clip_id = self
                .clipboard
                .incoming
                .as_ref()
                .expect("checked above")
                .announcement
                .clip_id
                .clone();
            self.request_native_clipboard(peer_id, &clip_id);
        }
    }

    /// Revoke clipboard admissions synchronously when clipboard policy or peer trust changes.
    pub(crate) fn clipboard_policy_changed(&mut self) {
        self.discard_incoming_clipboard();
        self.clipboard.invalidate();
    }

    /// Retain the transfer identity and verified partial staging so reconnect can resume it.
    pub(crate) fn clipboard_peer_disconnected(&mut self, peer_id: &str) {
        if self.clipboard.writing_peer.as_deref() == Some(peer_id) {
            self.clipboard.revoke_admission();
            self.clipboard.gate.fetch_add(1, Ordering::AcqRel);
        }
        if self.clipboard.incoming.as_ref().is_some_and(|incoming| {
            incoming.peer == peer_id && !incoming.native_pending && incoming.received.is_none()
        }) {
            self.discard_incoming_clipboard();
        }
        if let Some(incoming) = self
            .clipboard
            .incoming
            .as_mut()
            .filter(|incoming| incoming.peer == peer_id && incoming.native_pending)
        {
            // Let QUIC closure surface as an I/O interruption so verified partial chunks survive.
            incoming.native_fetch_requested = false;
            incoming.native_fetch_at = None;
        }
        self.clipboard
            .pending_native_sends
            .retain(|pending| pending.peer != peer_id);
    }

    pub(super) fn discard_incoming_clipboard(&mut self) {
        if let Some(incoming) = self.clipboard.incoming.take() {
            let native_id = transfer::transfer_id(&incoming.peer, &incoming.announcement.clip_id);
            if let Some(job) = self.clipboard.native_jobs.remove(&native_id) {
                job.cancel.cancel();
            }
            self.clipboard.pending_native_sends.retain(|pending| {
                !(pending.peer == incoming.peer && pending.clip_id == incoming.announcement.clip_id)
            });
            if let Some(id) = incoming.confirmation {
                if let Some(mut transfer) =
                    self.state.transfers.iter().find(|t| t.id == id).cloned()
                {
                    transfer.state = TransferState::Cancelled;
                    let _ = self.update_transfer(transfer);
                }
            }
        }
    }

    pub(crate) fn clipboard_receiver(&mut self) -> crossbeam_channel::Receiver<ClipboardEvent> {
        self.clipboard.bridged = true;
        self.clipboard.changes.clone()
    }

    pub(crate) fn clipboard_ready(&self) -> Arc<tokio::sync::Notify> {
        self.clipboard.ready.clone()
    }

    fn clipboard_peer_enabled(&self, peer: &str) -> bool {
        self.state.settings.clipboard.enabled
            && self
                .state
                .peers
                .iter()
                .any(|p| p.device_id == peer && p.clipboard_enabled)
            && self.link.peer_token(peer).is_ok()
    }

    fn clipboard_notice(&mut self, body: &str) {
        self.events.push(Event::Notification(Notification {
            level: "warning".into(),
            title: "Clipboard sync unavailable".into(),
            body: body.into(),
            action: None,
        }));
    }

    pub(crate) fn clipboard_changed(&mut self, event: ClipboardEvent) {
        match event {
            ClipboardEvent::Changed {
                marker,
                sensitivity,
                ..
            } => {
                if marker.is_some_and(|m| self.clipboard.markers.contains(&m)) {
                    return;
                }
                self.clipboard.pending = !(self.state.settings.clipboard.exclude_sensitive
                    && sensitivity.should_exclude());
                self.discard_incoming_clipboard();
                self.clipboard.version = (
                    now_ms().max(self.clipboard.version.0.saturating_add(1)),
                    self.state.self_info.device_id.clone(),
                    String::new(),
                );
                self.clipboard.revoke_admission();
                self.clipboard.gate.fetch_add(1, Ordering::AcqRel);
                for send in self.clipboard.send.drain(..) {
                    send.abort();
                }
                self.clipboard.outgoing = None;
                self.clipboard.received_leases.clear();
                self.clipboard.active_file_marker = None;
            }
            ClipboardEvent::ResyncRequired => {
                self.clipboard.pending = true;
                self.discard_incoming_clipboard();
                self.clipboard.version.0 = now_ms().max(self.clipboard.version.0.saturating_add(1));
                self.clipboard.revoke_admission();
                self.clipboard.gate.fetch_add(1, Ordering::AcqRel);
            }
            ClipboardEvent::RenderRequested { .. } => {}
        }
    }

    /// Coalesce local changes before reading; native calls and bulk writes never run on the input task.
    pub(crate) async fn poll_clipboard(&mut self) {
        if !self.clipboard.bridged {
            for _ in 0..128 {
                let Ok(event) = self.clipboard.changes.try_recv() else {
                    break;
                };
                self.clipboard_changed(event);
            }
        }
        if self.clipboard.incoming.as_ref().is_some_and(|clip| {
            (clip.received.is_none()
                && !clip.native_pending
                && clip.confirmation.is_none()
                && clip.started.elapsed() > Duration::from_secs(5))
                || (clip.confirmation.is_some()
                    && clip.started.elapsed() > Duration::from_secs(120))
        }) {
            self.discard_incoming_clipboard();
        }
        self.poll_native_transfers().await;
        if self
            .clipboard
            .write
            .as_ref()
            .is_some_and(|job| job.is_finished())
        {
            if let Some(job) = self.clipboard.write.take() {
                let result = job.await;
                self.clipboard.writing_peer = None;
                if let Some(id) = self.clipboard.writing_confirmation.take() {
                    if let Some(mut transfer) =
                        self.state.transfers.iter().find(|t| t.id == id).cloned()
                    {
                        if !matches!(
                            transfer.state,
                            TransferState::Cancelled | TransferState::Failed
                        ) {
                            transfer.state = match &result {
                                Ok(Ok((ClipboardPublish::Published { .. }, _, _))) => {
                                    TransferState::Done
                                }
                                Ok(Ok((ClipboardPublish::ReplacedLocalChange { .. }, _, _))) => {
                                    TransferState::Cancelled
                                }
                                Ok(Ok((ClipboardPublish::Revoked, _, _))) => {
                                    TransferState::Cancelled
                                }
                                _ => TransferState::Failed,
                            };
                            if transfer.state == TransferState::Done {
                                transfer.bytes_done = transfer.bytes_total;
                            }
                            if transfer.state == TransferState::Failed {
                                transfer.error =
                                    Some("The OS rejected the clipboard update.".into());
                            }
                            let _ = self.update_transfer(transfer);
                        }
                    }
                }
                match result {
                    Ok(Ok((ClipboardPublish::Published { .. }, received, marker))) => {
                        self.clipboard.received_leases.clear();
                        self.clipboard.active_file_marker = received.as_ref().map(|_| marker);
                        if let Some(received) = received {
                            self.clipboard.received_leases.push_back(received);
                        }
                        self.clipboard.pending = true;
                        self.clipboard.last_local = Instant::now() - LOCAL_INTERVAL;
                    }
                    Ok(Ok((ClipboardPublish::ReplacedLocalChange { .. }, _, _))) => {
                        tracing::info!(
                            "received clipboard dropped: this computer's clipboard changed first"
                        );
                        self.clipboard.received_leases.clear();
                        self.clipboard.active_file_marker = None;
                    }
                    Ok(Ok((ClipboardPublish::Revoked, _, _))) => {
                        tracing::info!("received clipboard dropped: no longer allowed");
                    }
                    Ok(Ok((
                        ClipboardPublish::PartialFailure { cleared, .. },
                        received,
                        marker,
                    ))) => {
                        if cleared {
                            self.clipboard.received_leases.clear();
                            self.clipboard.active_file_marker = None;
                        } else if let Some(received) = received {
                            if self.clipboard.received_leases.len() < 2 {
                                self.clipboard.received_leases.push_back(received);
                            }
                            self.clipboard.active_file_marker = Some(marker);
                        }
                        tracing::info!("received clipboard could not be written by the system");
                        self.clipboard_notice(
                            "The operating system could not finish the clipboard update.",
                        );
                    }
                    _ => {
                        tracing::info!("received clipboard was rejected by the system");
                        self.clipboard_notice(
                            "The operating system rejected a completed clipboard update.",
                        );
                    }
                }
                self.clipboard.revoke_admission();
            }
        }
        self.clipboard.send.retain(|job| !job.is_finished());
        if self
            .clipboard
            .read
            .as_ref()
            .is_some_and(|job| job.is_finished())
        {
            if let Some(job) = self.clipboard.read.take() {
                match job.await {
                    Ok(Ok(snapshot))
                        if !self.clipboard.pending
                            && self.clipboard.read_version == self.clipboard.version =>
                    {
                        self.clipboard.snapshot_token = Some(snapshot.change_token);
                        self.announce_eager(snapshot)
                    }
                    _ => {}
                }
            }
        }
        if let Some(incoming) = self.clipboard.incoming.as_mut() {
            if incoming.baseline.is_none()
                && incoming
                    .baseline_read
                    .as_ref()
                    .is_some_and(|job| job.is_finished())
            {
                if let Some(job) = incoming.baseline_read.take() {
                    if let Ok(Ok(snapshot)) = job.await {
                        incoming.baseline = Some(snapshot.change_token);
                        self.clipboard.snapshot_token = Some(snapshot.change_token);
                    }
                }
            }
        }
        let eager_ready = self.clipboard.incoming.as_ref().is_some_and(|incoming| {
            incoming.confirmation.is_none()
                && (incoming.received.is_some()
                    || (!incoming.native_pending
                        && incoming.payloads.len() == incoming.announcement.formats.len()))
        });
        if eager_ready {
            let total = self.clipboard.incoming.as_ref().map_or(0, |incoming| {
                incoming
                    .received
                    .as_ref()
                    .map(|received| transfer::manifest_totals(&received.manifest).1)
                    .unwrap_or_else(|| transfer::announced_totals(&incoming.announcement).1)
            });
            let needs_confirmation = self
                .clipboard
                .incoming
                .as_ref()
                .is_some_and(|incoming| incoming.received.is_none())
                && total
                    > self
                        .state
                        .settings
                        .clipboard
                        .max_auto_mb
                        .saturating_mul(1024 * 1024);
            if needs_confirmation {
                let transfer = self.clipboard.incoming.as_mut().map(|incoming| {
                    let id = transfer::transfer_id(&incoming.peer, &incoming.announcement.clip_id);
                    incoming.confirmation = Some(id.clone());
                    Transfer {
                        id,
                        direction: TransferDirection::Receive,
                        peer_id: incoming.peer.clone(),
                        name: "Clipboard".into(),
                        items: transfer::announced_totals(&incoming.announcement).0,
                        bytes_total: total,
                        bytes_done: 0,
                        rate_bps: 0,
                        state: TransferState::AwaitingConfirm,
                        error: None,
                    }
                });
                if let Some(transfer) = transfer {
                    let _ = self.update_transfer(transfer);
                }
            } else if self.clipboard.write.is_none()
                && self
                    .clipboard
                    .incoming
                    .as_ref()
                    .is_some_and(|incoming| incoming.baseline.is_some())
            {
                let _ = self.publish_clipboard();
            }
        }
        if self.clipboard.pending
            && self.state.settings.clipboard.enabled
            && self.clipboard.read.is_none()
            && self.clipboard.last_local.elapsed() >= LOCAL_INTERVAL
        {
            self.clipboard.pending = false;
            self.clipboard.last_local = Instant::now();
            self.clipboard.read_version = self.clipboard.version.clone();
            let platform = self.platform.clone();
            let ready = self.clipboard.ready.clone();
            self.clipboard.read = Some(tokio::spawn(async move {
                let result = tokio::task::spawn_blocking(move || {
                    platform.clipboard_backend().read_snapshot()
                })
                .await
                .map_err(|_| glide_platform::BackendError::StateUnavailable)?;
                ready.notify_one();
                result
            }));
        }
    }

    async fn poll_native_transfers(&mut self) {
        let ids = self
            .clipboard
            .native_jobs
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for id in ids {
            let finished = self
                .clipboard
                .native_jobs
                .get(&id)
                .is_some_and(|job| job.task.is_finished());
            if finished {
                if let Some(job) = self.clipboard.native_jobs.remove(&id) {
                    let result = match job.task.await {
                        Ok(result) => result,
                        Err(_) => Err(glide_xfer::Error::Worker),
                    };
                    self.finish_native_job(id, job.transfer, job.peer_token, job.progress, result);
                }
                continue;
            }
            let update = self.clipboard.native_jobs.get_mut(&id).and_then(|job| {
                if job.last_progress_update.elapsed() < Duration::from_millis(100) {
                    return None;
                }
                job.last_progress_update = Instant::now();
                let totals = job
                    .manifest
                    .lock()
                    .ok()
                    .and_then(|manifest| manifest.as_ref().map(transfer::manifest_totals));
                if let Some((items, bytes_total)) = totals {
                    job.transfer.items = items;
                    job.transfer.bytes_total = bytes_total;
                }
                let progress = job.progress.snapshot();
                job.transfer.bytes_done = progress.bytes_done.min(job.transfer.bytes_total);
                job.transfer.rate_bps = progress.rate_bps;
                job.transfer.state = TransferState::Active;
                Some(job.transfer.clone())
            });
            if let Some(transfer) = update {
                let _ = self.update_transfer(transfer);
            }
        }

        for _ in 0..4 {
            let streams = self
                .clipboard
                .accepted
                .as_mut()
                .and_then(|accepted| accepted.try_recv().ok());
            let Some(streams) = streams else { break };
            self.queue_native_receive(streams);
        }
        while self.clipboard.native_jobs.len() < 2 {
            let Some(streams) = self.clipboard.pending_native_receives.pop_front() else {
                break;
            };
            self.start_native_receive(streams);
        }
        self.schedule_native_sends();

        let retry = self.clipboard.incoming.as_ref().and_then(|incoming| {
            (incoming.native_pending
                && incoming.confirmation.is_none()
                && !self
                    .clipboard
                    .native_jobs
                    .contains_key(&transfer::transfer_id(
                        &incoming.peer,
                        &incoming.announcement.clip_id,
                    ))
                && incoming
                    .native_fetch_at
                    .is_some_and(|at| at.elapsed() >= Duration::from_secs(5)))
            .then(|| (incoming.peer.clone(), incoming.announcement.clip_id.clone()))
        });
        if let Some((peer, clip_id)) = retry {
            self.request_native_clipboard(&peer, &clip_id);
        }
    }

    fn queue_native_receive(&mut self, streams: glide_net::TransferStreams) {
        let id = transfer::transfer_id(&streams.peer_id, &streams.transfer_id);
        let allowed = self.clipboard_peer_enabled(&streams.peer_id)
            && self.link.peer_token(&streams.peer_id).ok() == Some(streams.peer_token)
            && self.clipboard.incoming.as_ref().is_some_and(|incoming| {
                incoming.peer == streams.peer_id
                    && incoming.token == streams.peer_token
                    && incoming.announcement.clip_id == streams.transfer_id
                    && incoming.native_pending
                    && incoming.confirmation.is_none()
            });
        if !allowed || self.clipboard.native_jobs.contains_key(&id) {
            streams.handle.cancel();
            return;
        }
        if self.clipboard.native_jobs.len() < 2 {
            self.start_native_receive(streams);
        } else if self.clipboard.pending_native_receives.len() < 2 {
            self.clipboard.pending_native_receives.push_back(streams);
        } else {
            streams.handle.cancel();
        }
    }

    fn start_native_receive(&mut self, streams: glide_net::TransferStreams) {
        let peer = streams.peer_id.clone();
        let clip_id = streams.transfer_id.clone();
        let peer_token = streams.peer_token;
        let id = transfer::transfer_id(&peer, &clip_id);
        let Some((announcement, consent, manifest)) =
            self.clipboard.incoming.as_mut().and_then(|incoming| {
                if incoming.peer != peer
                    || incoming.token != streams.peer_token
                    || incoming.announcement.clip_id != clip_id
                    || incoming.confirmation.is_some()
                {
                    return None;
                }
                incoming.native_fetch_requested = false;
                incoming.native_fetch_at = None;
                Some((
                    incoming.announcement.clone(),
                    incoming
                        .native_consent
                        .unwrap_or(glide_xfer::Consent::Automatic),
                    incoming.native_manifest.clone(),
                ))
            })
        else {
            streams.handle.cancel();
            return;
        };
        let Some(engine) = self.clipboard.file_engine.clone() else {
            streams.handle.cancel();
            tracing::info!("native clipboard transfer unavailable on connection");
            self.send_clip_failure(&peer, &clip_id);
            return;
        };
        let (items, bytes_total) = transfer::announced_totals(&announcement);
        let transfer = self
            .state
            .transfers
            .iter()
            .find(|transfer| transfer.id == id)
            .cloned()
            .unwrap_or(Transfer {
                id: id.clone(),
                direction: TransferDirection::Receive,
                peer_id: peer.clone(),
                name: "Clipboard".into(),
                items,
                bytes_total,
                bytes_done: 0,
                rate_bps: 0,
                state: TransferState::Queued,
                error: None,
            });
        let mut transfer = transfer;
        transfer.state = TransferState::Active;
        transfer.error = None;
        let _ = self.update_transfer(transfer.clone());

        let progress = glide_xfer::Progress::new();
        let cancel = glide_xfer::Cancel::new();
        let manifest = Arc::new(std::sync::Mutex::new(manifest));
        let task_progress = progress.clone();
        let task_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            match engine
                .receive_native(streams, consent, &task_cancel, &task_progress)
                .await
            {
                Err(glide_xfer::Error::ConfirmationRequired { manifest, .. }) => {
                    transfer::validate_manifest_for_announcement(&peer, &announcement, &manifest)
                        .map_err(glide_xfer::Error::Invalid)?;
                    Ok(transfer::NativeCompletion::ConsentRequired(*manifest))
                }
                Err(error) => Err(error),
                Ok(received) => {
                    transfer::validate_manifest_for_announcement(
                        &peer,
                        &announcement,
                        &received.manifest,
                    )
                    .map_err(glide_xfer::Error::Invalid)?;
                    let validation_announcement = announcement.clone();
                    let (received, contents) = tokio::task::spawn_blocking(move || {
                        let contents = transfer::validate_received_for_announcement(
                            &validation_announcement,
                            &received,
                        )
                        .map_err(glide_xfer::Error::Invalid)?;
                        Ok::<_, glide_xfer::Error>((received, contents))
                    })
                    .await
                    .map_err(|_| glide_xfer::Error::Worker)??;
                    Ok(transfer::NativeCompletion::Received {
                        manifest: received.manifest.clone(),
                        received,
                        contents,
                    })
                }
            }
        });
        self.clipboard.native_jobs.insert(
            id,
            transfer::NativeJob {
                transfer,
                peer_token,
                probe: false,
                progress,
                cancel,
                task,
                last_progress_update: Instant::now(),
                manifest,
            },
        );
    }

    fn finish_native_job(
        &mut self,
        id: String,
        mut job_transfer: Transfer,
        starting_peer_token: glide_net::PeerToken,
        progress: glide_xfer::Progress,
        result: Result<transfer::NativeCompletion, glide_xfer::Error>,
    ) {
        match result {
            Ok(transfer::NativeCompletion::Sent(manifest)) => {
                let (items, bytes) = transfer::manifest_totals(&manifest);
                job_transfer.items = items;
                job_transfer.bytes_total = bytes;
                job_transfer.bytes_done = bytes;
                job_transfer.rate_bps = progress.snapshot().rate_bps;
                job_transfer.state = TransferState::Done;
                job_transfer.error = None;
                let _ = self.update_transfer(job_transfer);
            }
            Ok(transfer::NativeCompletion::ProbeClosed(manifest)) => {
                let (_, bytes) = transfer::manifest_totals(&manifest);
                job_transfer.bytes_total = bytes;
                job_transfer.bytes_done = 0;
                job_transfer.rate_bps = 0;
                job_transfer.state = TransferState::Queued;
                job_transfer.error = None;
                let _ = self.update_transfer(job_transfer);
            }
            Ok(transfer::NativeCompletion::ConsentRequired(manifest)) => {
                let validation = self.clipboard.incoming.as_ref().and_then(|incoming| {
                    (transfer::transfer_id(&incoming.peer, &manifest.clip_id) == id)
                        .then(|| transfer::validate_manifest(incoming, &manifest))
                });
                let Some(Ok(_)) = validation else {
                    let peer = job_transfer.peer_id.clone();
                    let clip_id = manifest.clip_id.clone();
                    let matches_incoming =
                        self.clipboard.incoming.as_ref().is_some_and(|incoming| {
                            transfer::transfer_id(&incoming.peer, &incoming.announcement.clip_id)
                                == id
                        });
                    self.fail_native_transfer(
                        job_transfer,
                        "The clipboard transfer did not match its announcement.",
                    );
                    self.send_clip_failure(&peer, &clip_id);
                    if matches_incoming {
                        self.discard_incoming_clipboard();
                    }
                    return;
                };
                let (_, bytes) = transfer::manifest_totals(&manifest);
                let auto_limit = self
                    .state
                    .settings
                    .clipboard
                    .max_auto_mb
                    .saturating_mul(1024 * 1024);
                if bytes <= auto_limit {
                    match glide_xfer::Consent::approve(&manifest) {
                        Ok(consent) => {
                            if let Some(incoming) = self.clipboard.incoming.as_mut() {
                                incoming.native_manifest = Some(manifest.clone());
                                incoming.native_consent = Some(consent);
                                incoming.native_fetch_requested = false;
                                incoming.native_fetch_at = None;
                            }
                            job_transfer.bytes_total = bytes;
                            job_transfer.state = TransferState::Queued;
                            job_transfer.error = None;
                            let _ = self.update_transfer(job_transfer.clone());
                            let clip_id = manifest.clip_id.clone();
                            self.request_native_clipboard(&job_transfer.peer_id, &clip_id);
                        }
                        Err(_) => self.fail_native_transfer(
                            job_transfer,
                            "Glide could not approve the clipboard transfer safely.",
                        ),
                    }
                } else {
                    job_transfer.bytes_total = bytes;
                    job_transfer.bytes_done = 0;
                    job_transfer.state = TransferState::AwaitingConfirm;
                    job_transfer.error = None;
                    if let Some(incoming) = self.clipboard.incoming.as_mut() {
                        incoming.native_manifest = Some(manifest);
                        incoming.native_consent = None;
                        incoming.confirmation = Some(id);
                        incoming.native_fetch_requested = false;
                        incoming.native_fetch_at = None;
                    }
                    let _ = self.update_transfer(job_transfer);
                }
            }
            Ok(transfer::NativeCompletion::Received {
                manifest,
                received,
                contents,
            }) => {
                let valid = self.clipboard.incoming.as_ref().is_some_and(|incoming| {
                    transfer::transfer_id(&incoming.peer, &manifest.clip_id) == id
                        && incoming.peer == job_transfer.peer_id
                        && self.link.peer_token(&incoming.peer).ok() == Some(incoming.token)
                        && self.clipboard_peer_enabled(&incoming.peer)
                });
                if !valid {
                    return;
                }
                if let Some(incoming) = self.clipboard.incoming.as_mut() {
                    incoming.payloads = contents;
                    incoming.received = Some(received);
                    incoming.native_pending = false;
                    incoming.native_consent = None;
                    incoming.native_fetch_requested = false;
                    incoming.native_fetch_at = None;
                }
                let (items, bytes) = transfer::manifest_totals(&manifest);
                job_transfer.items = items;
                job_transfer.bytes_total = bytes;
                job_transfer.bytes_done = bytes;
                job_transfer.rate_bps = progress.snapshot().rate_bps;
                job_transfer.state = TransferState::Active;
                job_transfer.error = None;
                let _ = self.update_transfer(job_transfer);
            }
            Err(error) => {
                if self.state.transfers.iter().any(|item| {
                    item.id == id
                        && matches!(
                            item.state,
                            TransferState::Cancelled | TransferState::Failed | TransferState::Done
                        )
                }) {
                    return;
                }
                let current_peer_token = self.link.peer_token(&job_transfer.peer_id).ok();
                let retry_after_session_change = matches!(
                    &error,
                    glide_xfer::Error::Io(_) | glide_xfer::Error::Timeout
                ) && current_peer_token
                    != Some(starting_peer_token);
                if retry_after_session_change || matches!(&error, glide_xfer::Error::Busy) {
                    job_transfer.state = TransferState::Queued;
                    job_transfer.rate_bps = 0;
                    let _ = self.update_transfer(job_transfer.clone());
                    if job_transfer.direction == TransferDirection::Receive {
                        if let Some(incoming) =
                            self.clipboard.incoming.as_mut().filter(|incoming| {
                                transfer::transfer_id(
                                    &incoming.peer,
                                    &incoming.announcement.clip_id,
                                ) == id
                                    && incoming.native_pending
                                    && incoming.confirmation.is_none()
                            })
                        {
                            incoming.native_fetch_requested = false;
                            incoming.native_fetch_at =
                                Some(Instant::now() - Duration::from_secs(5));
                        }
                    }
                } else {
                    let actionable = matches!(
                        &error,
                        glide_xfer::Error::DiskSpace | glide_xfer::Error::Storage(_)
                    ) || (job_transfer.direction == TransferDirection::Send
                        && matches!(&error, glide_xfer::Error::Limit(_)));
                    if let glide_xfer::Error::Storage(io) = &error {
                        // Only the kind of failure is logged (never a path or OS message).
                        match io.kind() {
                            std::io::ErrorKind::PermissionDenied => tracing::info!(
                                "clipboard transfer storage failed: permission denied"
                            ),
                            std::io::ErrorKind::NotFound => tracing::info!(
                                "clipboard transfer storage failed: folder or file missing"
                            ),
                            std::io::ErrorKind::AlreadyExists => {
                                tracing::info!("clipboard transfer storage failed: already exists")
                            }
                            _ => tracing::info!("clipboard transfer storage failed: other"),
                        }
                    }
                    let body = match error {
                        glide_xfer::Error::Limit(_) => {
                            "The clipboard transfer exceeds Glide's size limit."
                        }
                        glide_xfer::Error::DiskSpace => {
                            "There is not enough usable disk space to complete the clipboard transfer."
                        }
                        glide_xfer::Error::Storage(_) => "Glide could not access transfer storage. Check disk space and folder permissions, then copy again.",
                        glide_xfer::Error::Timeout => "The clipboard transfer timed out.",
                        glide_xfer::Error::Integrity
                        | glide_xfer::Error::Invalid(_)
                        | glide_xfer::Error::Codec(_) => {
                            "The clipboard transfer did not pass verification."
                        }
                        glide_xfer::Error::Cancelled => "The other device cancelled the clipboard transfer.",
                        _ => "The clipboard transfer could not be completed.",
                    };
                    self.fail_native_transfer(job_transfer.clone(), body);
                    if actionable {
                        self.clipboard_notice(body);
                    }
                    if job_transfer.direction == TransferDirection::Receive {
                        let matching_incoming = self
                            .clipboard
                            .incoming
                            .as_ref()
                            .filter(|incoming| {
                                transfer::transfer_id(
                                    &incoming.peer,
                                    &incoming.announcement.clip_id,
                                ) == id
                            })
                            .map(|incoming| incoming.announcement.clip_id.clone());
                        if let Some(clip_id) = matching_incoming {
                            self.send_clip_failure(&job_transfer.peer_id, &clip_id);
                            self.discard_incoming_clipboard();
                        }
                    }
                }
            }
        }
    }

    fn fail_native_transfer(&mut self, mut transfer: Transfer, body: &str) {
        transfer.state = TransferState::Failed;
        transfer.error = Some(body.to_owned());
        let _ = self.update_transfer(transfer);
        tracing::info!("clipboard file transfer failed");
    }

    fn start_native_send(&mut self, peer: &str, fetch: wire::ClipFetch) -> Result<(), IpcError> {
        let probe = match fetch.format.as_str() {
            "native" => false,
            "native-manifest" => true,
            _ => return Ok(()),
        };
        if fetch.clip_id.is_empty()
            || fetch.clip_id.len() > 63
            || fetch.clip_id.chars().any(char::is_control)
            || !self.clipboard_peer_enabled(peer)
        {
            return Ok(());
        }
        let Some(outgoing) = self
            .clipboard
            .outgoing
            .as_ref()
            .filter(|outgoing| {
                outgoing.native_required && outgoing.announcement.clip_id == fetch.clip_id
            })
            .cloned()
        else {
            self.send_clip_failure(peer, &fetch.clip_id);
            return Ok(());
        };
        let Some(token) = self.link.peer_token(peer).ok() else {
            return Ok(());
        };
        let id = transfer::transfer_id(peer, &fetch.clip_id);
        let active_probe = self
            .clipboard
            .native_jobs
            .get(&id)
            .is_some_and(|job| job.transfer.direction == TransferDirection::Send && job.probe);
        let pending_request = self
            .clipboard
            .pending_native_sends
            .iter()
            .any(|pending| pending.peer == peer && pending.clip_id == fetch.clip_id);
        if pending_request
            || (self.clipboard.native_jobs.contains_key(&id) && !(active_probe && !probe))
        {
            return Ok(());
        }
        self.clipboard
            .native_send_limiter
            .retain(|_, last| last.elapsed() < Duration::from_secs(60));
        let rate_key = format!("{}:{id}", if probe { "probe" } else { "full" });
        let interval = if probe {
            Duration::from_secs(5)
        } else {
            Duration::from_millis(250)
        };
        if self
            .clipboard
            .native_send_limiter
            .get(&rate_key)
            .is_some_and(|last| last.elapsed() < interval)
        {
            return Ok(());
        }
        self.clipboard
            .native_send_limiter
            .insert(rate_key, Instant::now());
        if self.clipboard.pending_native_sends.len() >= 8 {
            self.send_clip_failure(peer, &fetch.clip_id);
            tracing::info!("clipboard transfer queue full");
            return Ok(());
        }
        if active_probe && !probe {
            self.clipboard
                .pending_native_sends
                .push_back(transfer::PendingNativeSend {
                    peer: peer.to_owned(),
                    token,
                    clip_id: fetch.clip_id,
                    probe: false,
                    manifest: Arc::new(std::sync::Mutex::new(None)),
                });
            self.schedule_native_sends();
            return Ok(());
        }
        let (items, bytes_total) = transfer::announced_totals(&outgoing.announcement);
        let queued = Transfer {
            id,
            direction: TransferDirection::Send,
            peer_id: peer.to_owned(),
            name: "Clipboard".into(),
            items,
            bytes_total,
            bytes_done: 0,
            rate_bps: 0,
            state: TransferState::Queued,
            error: None,
        };
        let _ = self.update_transfer(queued);
        self.clipboard
            .pending_native_sends
            .push_back(transfer::PendingNativeSend {
                peer: peer.to_owned(),
                token,
                clip_id: fetch.clip_id,
                probe,
                manifest: Arc::new(std::sync::Mutex::new(None)),
            });
        self.schedule_native_sends();
        Ok(())
    }

    fn schedule_native_sends(&mut self) {
        let mut pending_to_check = self.clipboard.pending_native_sends.len();
        while self.clipboard.native_jobs.len() < 2 && pending_to_check > 0 {
            pending_to_check -= 1;
            let Some(pending) = self.clipboard.pending_native_sends.pop_front() else {
                break;
            };
            let id = transfer::transfer_id(&pending.peer, &pending.clip_id);
            if self.clipboard.native_jobs.contains_key(&id) {
                self.clipboard.pending_native_sends.push_back(pending);
                continue;
            }
            let valid = self.clipboard_peer_enabled(&pending.peer)
                && self.link.peer_token(&pending.peer).ok() == Some(pending.token);
            let outgoing = self
                .clipboard
                .outgoing
                .as_ref()
                .filter(|outgoing| {
                    outgoing.announcement.clip_id == pending.clip_id && outgoing.native_required
                })
                .cloned();
            let (Some(outgoing), true) = (outgoing, valid) else {
                continue;
            };
            let (items, bytes_total) = transfer::announced_totals(&outgoing.announcement);
            let mut transfer = self
                .state
                .transfers
                .iter()
                .find(|transfer| transfer.id == id)
                .cloned()
                .unwrap_or(Transfer {
                    id: id.clone(),
                    direction: TransferDirection::Send,
                    peer_id: pending.peer.clone(),
                    name: "Clipboard".into(),
                    items,
                    bytes_total,
                    bytes_done: 0,
                    rate_bps: 0,
                    state: TransferState::Queued,
                    error: None,
                });
            transfer.state = TransferState::Active;
            transfer.error = None;
            let _ = self.update_transfer(transfer.clone());
            let (Some(link), Some(engine)) = (
                self.clipboard.native_link.clone(),
                self.clipboard.file_engine.clone(),
            ) else {
                self.send_clip_failure(&pending.peer, &pending.clip_id);
                self.fail_native_transfer(
                    transfer,
                    "Native clipboard transfer is unavailable on this connection.",
                );
                continue;
            };
            let progress = glide_xfer::Progress::new();
            let cancel = glide_xfer::Cancel::new();
            let task_progress = progress.clone();
            let task_cancel = cancel.clone();
            let data_dir = self.data_dir.clone();
            let task_outgoing = outgoing.clone();
            let manifest = pending.manifest.clone();
            let peer_token = pending.token;
            let probe = pending.probe;
            let task = tokio::spawn(async move {
                send_native_clipboard(
                    data_dir,
                    engine,
                    link,
                    pending,
                    task_outgoing,
                    task_cancel,
                    task_progress,
                )
                .await
            });
            self.clipboard.native_jobs.insert(
                id,
                transfer::NativeJob {
                    transfer,
                    peer_token,
                    probe,
                    progress,
                    cancel,
                    task,
                    last_progress_update: Instant::now(),
                    manifest,
                },
            );
        }
    }

    fn send_clip_failure(&self, peer: &str, clip_id: &str) {
        let Some(token) = self.link.peer_token(peer).ok() else {
            return;
        };
        let link = self.link.clone();
        let peer = peer.to_owned();
        let clip_id = clip_id.to_owned();
        tokio::spawn(async move {
            if link.peer_token(&peer).ok() == Some(token) {
                let _ = tokio::time::timeout(
                    Duration::from_secs(5),
                    link.send_reliable(
                        &peer,
                        WireMessage::Clipboard(ClipboardMessage::ClipFailure(wire::ClipFailure {
                            clip_id,
                            format: "native".into(),
                            reason: "Clipboard transfer unavailable.".into(),
                        })),
                    ),
                )
                .await;
            }
        });
    }

    fn announce_eager(&mut self, snapshot: ClipboardSnapshot) {
        let settings = self.state.settings.clipboard.clone();
        if settings.exclude_sensitive
            && (snapshot.sensitivity.should_exclude()
                || snapshot.contents.iter().any(|content| {
                    content.sensitivity.should_exclude()
                        || matches!(&content.data, ClipboardData::Files(files) if files.sensitivity.should_exclude())
                }))
        {
            tracing::info!("clipboard not shared: marked sensitive");
            self.clipboard.outgoing = None;
            return;
        }
        // A marker means Glide owns the current native clipboard; do not echo it back over the network.
        if snapshot.marker.is_some() {
            self.clipboard.outgoing = None;
            return;
        }
        let contents = snapshot
            .contents
            .iter()
            .filter(|content| enabled(&settings, &content.format))
            .filter(|content| {
                content.format == ClipboardFormat::Files || kind(&content.format).is_some()
            })
            .collect::<Vec<_>>();
        if contents.is_empty() {
            tracing::info!("clipboard not shared: no format that is switched on");
            self.clipboard.outgoing = None;
            return;
        }
        let mut formats = wire::ClipFormats::new();
        let mut files = wire::AnnouncedFiles::new();
        let mut total_bytes = 0u64;
        let mut has_files = false;
        for content in contents.iter().copied() {
            match (&content.format, &content.data) {
                (ClipboardFormat::Files, ClipboardData::Files(file_list)) => {
                    has_files = true;
                    let mut names = HashSet::new();
                    if file_list.entries.len() > wire::MAX_TRANSFER_ITEMS as usize
                        || file_list.entries.iter().any(|entry| {
                            entry.name.is_empty()
                                || entry.name == "."
                                || entry.name == ".."
                                || entry.name.contains('/')
                                || entry.name.contains('\\')
                                || entry.name.chars().any(char::is_control)
                                || !names.insert(entry.name.to_lowercase())
                        })
                    {
                        tracing::info!(
                            "clipboard files not shared: unsupported or duplicate names"
                        );
                        self.clipboard_notice(
                            "The clipboard file list contains unsupported names or duplicates.",
                        );
                        return;
                    }
                    for entry in &file_list.entries {
                        if files
                            .push(wire::AnnouncedFile {
                                name: entry.name.clone(),
                                size: if entry.is_dir { 0 } else { entry.size },
                                is_dir: entry.is_dir,
                            })
                            .is_err()
                        {
                            self.clipboard_notice(
                                "The clipboard file list is too large to transfer.",
                            );
                            return;
                        }
                    }
                }
                (ClipboardFormat::Files, _) => return,
                (format, ClipboardData::Bytes(bytes)) => {
                    let Some((kind, mime)) = kind(format) else {
                        continue;
                    };
                    total_bytes = match total_bytes.checked_add(bytes.len() as u64) {
                        Some(total) if total <= 64 * 1024 * 1024 => total,
                        _ => {
                            self.clipboard_notice(
                                "The clipboard contents exceed Glide's size limit.",
                            );
                            return;
                        }
                    };
                    if formats
                        .push(wire::ClipFormat {
                            kind: kind.into(),
                            mime: mime.into(),
                            size: bytes.len() as u64,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                (_, ClipboardData::Files(_)) => return,
            }
        }
        if formats.is_empty() && files.is_empty() {
            self.clipboard.outgoing = None;
            return;
        }
        let native_required = has_files || total_bytes as usize > EAGER_BYTES;
        let reusable = self.clipboard.outgoing.as_ref().filter(|outgoing| {
            outgoing.snapshot.contents == snapshot.contents
                && outgoing.snapshot.sensitivity == snapshot.sensitivity
                && outgoing.announcement.formats == formats
                && outgoing.announcement.files == files
                && outgoing.native_required == native_required
        });
        if reusable.is_none() {
            let clip_id = hex::encode(rand::random::<[u8; 16]>());
            let timestamp_ms = now_ms().max(self.clipboard.version.0.saturating_add(1));
            let origin = self.state.self_info.device_id.clone();
            let announcement = wire::ClipAnnounce {
                clip_id: clip_id.clone(),
                origin: origin.clone(),
                timestamp_ms,
                formats,
                files,
            };
            let eager = if native_required {
                Vec::new()
            } else {
                contents
                    .iter()
                    .copied()
                    .filter_map(|content| {
                        let (kind, _) = kind(&content.format)?;
                        let ClipboardData::Bytes(bytes) = &content.data else {
                            return None;
                        };
                        let data = wire::ClipboardBytes::try_from_vec(bytes.clone()).ok()?;
                        Some(ClipboardMessage::ClipData(wire::ClipData {
                            clip_id: clip_id.clone(),
                            format: kind.into(),
                            data,
                        }))
                    })
                    .collect()
            };
            self.clipboard.version = (timestamp_ms, origin, clip_id);
            self.clipboard.outgoing = Some(transfer::Outgoing {
                snapshot: Arc::new(snapshot),
                announcement,
                eager,
                native_required,
            });
        }
        if let Some(outgoing) = self.clipboard.outgoing.clone() {
            let peers = self
                .state
                .peers
                .iter()
                .filter(|peer| {
                    peer.clipboard_enabled && self.link.peer_token(&peer.device_id).is_ok()
                })
                .map(|peer| peer.device_id.clone())
                .collect::<Vec<_>>();
            self.send_clipboard_offer(peers, &outgoing);
        }
    }

    fn send_clipboard_offer(&mut self, peers: Vec<String>, outgoing: &transfer::Outgoing) {
        if peers.is_empty() {
            return;
        }
        let mut messages = Vec::with_capacity(outgoing.eager.len() + 1);
        messages.push(ClipboardMessage::ClipAnnounce(
            outgoing.announcement.clone(),
        ));
        messages.extend(outgoing.eager.iter().cloned());
        let peers = peers
            .into_iter()
            .filter_map(|peer| self.link.peer_token(&peer).ok().map(|token| (peer, token)))
            .collect::<Vec<_>>();
        let link = self.link.clone();
        self.clipboard.send.push(tokio::spawn(async move {
            for (peer, token) in peers {
                for message in &messages {
                    if link.peer_token(&peer).ok() != Some(token)
                        || !matches!(
                            tokio::time::timeout(
                                Duration::from_secs(5),
                                link.send_reliable(&peer, WireMessage::Clipboard(message.clone()))
                            )
                            .await,
                            Ok(Ok(()))
                        )
                    {
                        break;
                    }
                }
            }
        }));
    }

    fn request_native_clipboard(&mut self, peer: &str, clip_id: &str) {
        let Some(incoming) = self.clipboard.incoming.as_mut().filter(|incoming| {
            incoming.peer == peer
                && incoming.announcement.clip_id == clip_id
                && incoming.native_pending
                && incoming.confirmation.is_none()
        }) else {
            return;
        };
        if incoming.native_fetch_requested
            && incoming
                .native_fetch_at
                .is_some_and(|started| started.elapsed() < Duration::from_secs(5))
        {
            return;
        }
        incoming.native_fetch_requested = true;
        incoming.native_fetch_at = Some(Instant::now());
        let fetch_format = if incoming.native_consent.is_some() {
            "native"
        } else {
            "native-manifest"
        };
        let link = self.link.clone();
        let peer = peer.to_owned();
        let clip_id = clip_id.to_owned();
        let fetch_format = fetch_format.to_owned();
        let Some(token) = link.peer_token(&peer).ok() else {
            return;
        };
        tokio::spawn(async move {
            if link.peer_token(&peer).ok() == Some(token) {
                let _ = tokio::time::timeout(
                    Duration::from_secs(5),
                    link.send_reliable(
                        &peer,
                        WireMessage::Clipboard(ClipboardMessage::ClipFetch(wire::ClipFetch {
                            clip_id,
                            format: fetch_format,
                        })),
                    ),
                )
                .await;
            }
        });
    }

    pub(super) fn receive_clipboard(
        &mut self,
        peer: &str,
        message: ClipboardMessage,
    ) -> Result<(), IpcError> {
        if !self.clipboard_peer_enabled(peer) {
            return Ok(());
        }
        match message {
            ClipboardMessage::ClipAnnounce(announcement) => {
                if announcement.clip_id.is_empty()
                    || announcement.clip_id.len() > 63
                    || announcement.clip_id.chars().any(char::is_control)
                {
                    return Ok(());
                }
                let version = (
                    announcement.timestamp_ms,
                    announcement.origin.clone(),
                    announcement.clip_id.clone(),
                );
                if announcement.origin != peer
                    || announcement.timestamp_ms > now_ms().saturating_add(5000)
                    || version < self.clipboard.version
                {
                    return Ok(());
                }
                if version == self.clipboard.version {
                    let should_fetch = self.clipboard.incoming.as_ref().is_some_and(|incoming| {
                        incoming.peer == peer
                            && incoming.announcement.clip_id == announcement.clip_id
                            && incoming.native_pending
                            && incoming.confirmation.is_none()
                    });
                    if should_fetch {
                        self.request_native_clipboard(peer, &announcement.clip_id);
                    }
                    return Ok(());
                }
                let mut seen = HashSet::new();
                let total_bytes = announcement
                    .formats
                    .iter()
                    .try_fold(0u64, |sum, format| sum.checked_add(format.size));
                let valid = (!announcement.formats.is_empty() || !announcement.files.is_empty())
                    && announcement.files.len() <= wire::MAX_TRANSFER_ITEMS as usize
                    && announcement.formats.iter().all(|f| {
                        format(&f.kind).is_some_and(|format| {
                            enabled(&self.state.settings.clipboard, &format)
                                && kind(&format).is_some_and(|(_, mime)| f.mime == mime)
                                && f.size <= 64 * 1024 * 1024
                        }) && seen.insert(f.kind.clone())
                    })
                    && total_bytes.is_some_and(|sum| sum <= 64 * 1024 * 1024)
                    && (announcement.files.is_empty() || self.state.settings.clipboard.sync_files)
                    && announcement.files.iter().all(|file| {
                        !file.name.is_empty()
                            && file.name != "."
                            && file.name != ".."
                            && !file.name.contains('/')
                            && !file.name.contains('\\')
                            && !file.name.chars().any(char::is_control)
                    })
                    && announcement.files.iter().all(|file| {
                        !announcement.formats.iter().any(|format| {
                            transfer::payload_name(&announcement.clip_id, &format.kind)
                                .eq_ignore_ascii_case(&file.name)
                        })
                    });
                if !valid {
                    tracing::info!("clipboard offer ignored: invalid or turned off here");
                    return Ok(());
                }
                self.discard_incoming_clipboard();
                self.clipboard.version = version;
                self.clipboard.revoke_admission();
                self.clipboard.gate.fetch_add(1, Ordering::AcqRel);
                for send in self.clipboard.send.drain(..) {
                    send.abort();
                }
                let token = self
                    .link
                    .peer_token(peer)
                    .map_err(|_| error(ErrorCode::NotPaired, "Clipboard source disconnected."))?;
                let native_pending = !announcement.files.is_empty()
                    || total_bytes.unwrap_or(0) as usize > EAGER_BYTES;
                let baseline = self.clipboard.snapshot_token;
                let baseline_read = if baseline.is_none() {
                    let platform = self.platform.clone();
                    Some(tokio::spawn(async move {
                        tokio::task::spawn_blocking(move || {
                            platform.clipboard_backend().read_snapshot()
                        })
                        .await
                        .map_err(|_| glide_platform::BackendError::StateUnavailable)?
                    }))
                } else {
                    None
                };
                self.clipboard.incoming = Some(Incoming {
                    peer: peer.into(),
                    token,
                    announcement,
                    payloads: Vec::new(),
                    started: Instant::now(),
                    confirmation: None,
                    baseline,
                    baseline_read,
                    native_pending,
                    native_manifest: None,
                    native_consent: None,
                    native_fetch_requested: false,
                    native_fetch_at: None,
                    received: None,
                });
                if native_pending {
                    if self.clipboard.file_engine.is_none() || self.clipboard.native_link.is_none()
                    {
                        tracing::info!("clipboard files not fetched: transfer link unavailable");
                        self.discard_incoming_clipboard();
                    } else {
                        let clip_id = self.clipboard.version.2.clone();
                        self.request_native_clipboard(peer, &clip_id);
                    }
                }
            }
            ClipboardMessage::ClipData(data) => {
                let Some(incoming) = &mut self.clipboard.incoming else {
                    return Ok(());
                };
                if incoming.peer != peer
                    || incoming.announcement.clip_id != data.clip_id
                    || incoming.confirmation.is_some()
                    || incoming.native_pending
                {
                    return Ok(());
                }
                let Some(expected) = incoming
                    .announcement
                    .formats
                    .iter()
                    .find(|f| f.kind == data.format)
                else {
                    return Err(error(
                        ErrorCode::InvalidParams,
                        "Unannounced clipboard format.",
                    ));
                };
                let format = format(&data.format).ok_or_else(|| {
                    error(ErrorCode::InvalidParams, "Unsupported clipboard format.")
                })?;
                if expected.size != data.data.len() as u64
                    || incoming.payloads.iter().any(|c| c.format == format)
                    || matches!(format, ClipboardFormat::Text | ClipboardFormat::Html)
                        && std::str::from_utf8(&data.data).is_err()
                {
                    return Err(error(
                        ErrorCode::InvalidParams,
                        "Clipboard data does not match its announcement.",
                    ));
                }
                incoming.payloads.push(ClipboardContent {
                    format,
                    data: ClipboardData::Bytes(data.data.into_vec()),
                    sensitivity: ClipboardSensitivity::default(),
                });
            }
            ClipboardMessage::ClipFailure(failure) => {
                if self
                    .clipboard
                    .incoming
                    .as_ref()
                    .is_some_and(|c| c.peer == peer && c.announcement.clip_id == failure.clip_id)
                {
                    self.discard_incoming_clipboard();
                    tracing::info!("peer could not provide clipboard contents");
                }
            }
            ClipboardMessage::ClipFetch(fetch) => self.start_native_send(peer, fetch)?,
        }
        Ok(())
    }

    pub(super) fn confirm_clipboard(&mut self, id: &str, accept: bool) -> Result<(), IpcError> {
        if self
            .clipboard
            .incoming
            .as_ref()
            .is_some_and(|c| c.confirmation.as_deref() == Some(id))
        {
            if accept {
                if self
                    .clipboard
                    .incoming
                    .as_ref()
                    .is_some_and(|incoming| incoming.native_pending)
                {
                    let (peer, clip_id) = {
                        let incoming = self.clipboard.incoming.as_mut().expect("checked above");
                        let Some(manifest) = incoming.native_manifest.as_ref() else {
                            return Err(error(
                                ErrorCode::InvalidParams,
                                "Transfer approval is not ready.",
                            ));
                        };
                        incoming.native_consent =
                            Some(glide_xfer::Consent::approve(manifest).map_err(|_| {
                                error(ErrorCode::InvalidParams, "Transfer approval is invalid.")
                            })?);
                        incoming.confirmation = None;
                        (incoming.peer.clone(), incoming.announcement.clip_id.clone())
                    };
                    self.request_native_clipboard(&peer, &clip_id);
                    return Ok(());
                }
                if self.clipboard.write.is_some() {
                    return Err(error(
                        ErrorCode::InvalidParams,
                        "A clipboard update is still finishing. Retry confirmation.",
                    ));
                }
                self.publish_clipboard()?;
            } else {
                self.discard_incoming_clipboard();
            }
        }
        Ok(())
    }

    fn publish_clipboard(&mut self) -> Result<(), IpcError> {
        if self.clipboard.received_leases.len() >= 2
            && self
                .clipboard
                .incoming
                .as_ref()
                .is_some_and(|incoming| incoming.received.is_some())
        {
            return Err(error(
                ErrorCode::InvalidParams,
                "The previous file clipboard is still in use.",
            ));
        }
        if self.clipboard.incoming.as_ref().is_some_and(|incoming| {
            incoming.received.is_some()
                && self.clipboard_peer_enabled(&incoming.peer)
                && self.link.peer_token(&incoming.peer).ok() != Some(incoming.token)
        }) {
            // A verified native receipt can wait for this paired peer's authenticated session to return.
            return Ok(());
        }
        let Some(incoming) = self.clipboard.incoming.take() else {
            return Ok(());
        };
        if !self.clipboard_peer_enabled(&incoming.peer)
            || self.link.peer_token(&incoming.peer).ok() != Some(incoming.token)
        {
            return Ok(());
        }
        if self.clipboard.write.is_some() {
            return Err(error(ErrorCode::Internal, "Clipboard publication is busy."));
        }
        let Some(change_token) = incoming.baseline else {
            self.clipboard.incoming = Some(incoming);
            return Err(error(
                ErrorCode::InvalidParams,
                "Clipboard snapshot is still being read.",
            ));
        };
        let token = incoming.token;
        let expected_generation = self.clipboard.next_admission;
        self.clipboard.next_admission = expected_generation.wrapping_add(1).max(1);
        self.clipboard
            .admission_generation
            .store(expected_generation, Ordering::Release);
        let admission = ClipboardAdmission::for_generation(
            self.clipboard.admission_generation.clone(),
            expected_generation,
        );
        self.clipboard.admission = Some(admission.clone());
        let marker = ClipboardMarker(rand::random());
        if self.clipboard.markers.len() == 128 {
            self.clipboard.markers.pop_front();
        }
        self.clipboard.markers.push_back(marker);
        let platform = self.platform.clone();
        let link = self.link.clone();
        let peer = incoming.peer.clone();
        let payloads = incoming.payloads;
        let received = incoming.received;
        self.clipboard.writing_confirmation = incoming.confirmation.clone().or_else(|| {
            (received.is_some() || incoming.native_pending)
                .then(|| transfer::transfer_id(&peer, &incoming.announcement.clip_id))
        });
        self.clipboard.writing_peer = Some(peer.clone());
        let ready = self.clipboard.ready.clone();
        self.clipboard.write = Some(tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                if !admission.is_admitted() || link.peer_token(&peer).ok() != Some(token) {
                    return Ok((ClipboardPublish::Revoked, received, marker));
                }
                let result = platform.clipboard_backend().publish_snapshot(
                    payloads,
                    marker,
                    change_token,
                    admission,
                )?;
                Ok((result, received, marker))
            })
            .await
            .map_err(|_| glide_platform::BackendError::StateUnavailable)?;
            ready.notify_one();
            result
        }));
        Ok(())
    }
}

async fn send_native_clipboard(
    data_dir: PathBuf,
    engine: glide_xfer::FileEngine,
    link: glide_net::NativeLink,
    pending: transfer::PendingNativeSend,
    outgoing: transfer::Outgoing,
    cancel: glide_xfer::Cancel,
    progress: glide_xfer::Progress,
) -> Result<transfer::NativeCompletion, glide_xfer::Error> {
    let transfer::PendingNativeSend {
        peer,
        token: peer_token,
        clip_id,
        probe,
        manifest,
    } = pending;
    cancel.check()?;
    if link.peer_token(&peer).ok() != Some(peer_token) {
        return Err(transfer_io_error("clipboard peer disconnected"));
    }
    let id_for_temp = transfer::transfer_id(&peer, &clip_id);
    let snapshot = outgoing.snapshot.clone();
    let announcement = outgoing.announcement.clone();
    let (temporary, roots) = tokio::task::spawn_blocking(move || {
        create_outgoing_clipboard_inputs(&data_dir, &id_for_temp, &announcement, &snapshot)
    })
    .await
    .map_err(|_| glide_xfer::Error::Worker)??;
    let _temporary = temporary;
    let plan = glide_xfer::build_manifest(
        roots,
        transfer::wire_transfer_id(&clip_id).to_owned(),
        clip_id.clone(),
        engine.config().clone(),
        cancel.clone(),
    )
    .await
    .map_err(|error| match error {
        glide_xfer::Error::Io(error) => glide_xfer::Error::Storage(error),
        other => other,
    })?;
    transfer::validate_manifest_for_announcement(&peer, &outgoing.announcement, &plan.manifest)
        .map_err(glide_xfer::Error::Invalid)?;
    *manifest.lock().map_err(|_| glide_xfer::Error::Worker)? = Some(plan.manifest.clone());
    cancel.check()?;
    let lanes = engine.config().parallel_streams.clamp(1, 2) as u8;
    let streams = link
        .open_transfer(&peer, &clip_id, lanes)
        .await
        .map_err(|_| transfer_io_error("clipboard transfer connection failed"))?;
    if streams.peer_token != peer_token || link.peer_token(&peer).ok() != Some(peer_token) {
        streams.handle.cancel();
        return Err(transfer_io_error("clipboard peer changed"));
    }
    let manifest = plan.manifest.clone();
    match engine.send_native(plan, streams, &cancel, &progress).await {
        Ok(()) => {}
        Err(error)
            if probe
                && matches!(
                    error,
                    glide_xfer::Error::Io(_)
                        | glide_xfer::Error::Cancelled
                        | glide_xfer::Error::Timeout
                ) =>
        {
            return Ok(transfer::NativeCompletion::ProbeClosed(manifest));
        }
        Err(error) => return Err(error),
    }
    Ok(transfer::NativeCompletion::Sent(manifest))
}

fn transfer_io_error(message: &'static str) -> glide_xfer::Error {
    glide_xfer::Error::Io(std::io::Error::new(
        std::io::ErrorKind::ConnectionAborted,
        message,
    ))
}

struct ClipboardTempDir(PathBuf);

impl Drop for ClipboardTempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn create_outgoing_clipboard_inputs(
    data_dir: &Path,
    job_id: &str,
    announcement: &wire::ClipAnnounce,
    snapshot: &ClipboardSnapshot,
) -> Result<(ClipboardTempDir, Vec<PathBuf>), glide_xfer::Error> {
    let base = data_dir.join("clipboard-tmp");
    match std::fs::create_dir(&base) {
        Ok(()) => set_private_directory(&base)?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = std::fs::symlink_metadata(&base).map_err(glide_xfer::Error::Storage)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(glide_xfer::Error::Invalid("clipboard staging directory"));
            }
            set_private_directory(&base)?;
        }
        Err(error) => return Err(glide_xfer::Error::Storage(error)),
    }
    let suffix = hex::encode(rand::random::<[u8; 8]>());
    let safe_job_id = job_id
        .bytes()
        .filter(|byte| byte.is_ascii_alphanumeric())
        .take(100)
        .map(char::from)
        .collect::<String>();
    let directory = base.join(format!("{safe_job_id}-{suffix}"));
    std::fs::create_dir(&directory).map_err(glide_xfer::Error::Storage)?;
    set_private_directory(&directory)?;
    let guard = ClipboardTempDir(directory.clone());
    let mut roots = Vec::new();
    if !announcement.files.is_empty() {
        let Some(ClipboardContent {
            data: ClipboardData::Files(files),
            ..
        }) = snapshot
            .contents
            .iter()
            .find(|content| content.format == ClipboardFormat::Files)
        else {
            return Err(glide_xfer::Error::Invalid("clipboard file list missing"));
        };
        if files.entries.len() != announcement.files.len()
            || files
                .entries
                .iter()
                .zip(announcement.files.iter())
                .any(|(entry, announced)| {
                    entry.name != announced.name
                        || entry.is_dir != announced.is_dir
                        || (!entry.is_dir && entry.size != announced.size)
                })
        {
            return Err(glide_xfer::Error::Invalid("clipboard file list changed"));
        }
        roots.extend(files.entries.iter().map(|entry| entry.path.clone()));
    }
    for announced in announcement.formats.iter() {
        let Some(clipboard_format) = format(&announced.kind) else {
            return Err(glide_xfer::Error::Invalid("unsupported clipboard format"));
        };
        let Some(ClipboardContent {
            data: ClipboardData::Bytes(bytes),
            ..
        }) = snapshot
            .contents
            .iter()
            .find(|content| content.format == clipboard_format)
        else {
            return Err(glide_xfer::Error::Invalid("clipboard format changed"));
        };
        if bytes.len() as u64 != announced.size {
            return Err(glide_xfer::Error::Invalid("clipboard format size changed"));
        }
        let path = directory.join(transfer::payload_name(
            &announcement.clip_id,
            &announced.kind,
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        set_private_file(&mut options);
        let mut file = options.open(&path).map_err(glide_xfer::Error::Storage)?;
        std::io::Write::write_all(&mut file, bytes).map_err(glide_xfer::Error::Storage)?;
        file.sync_all().map_err(glide_xfer::Error::Storage)?;
        roots.push(path);
    }
    Ok((guard, roots))
}

fn purge_outgoing_clipboard_staging(data_dir: &Path) -> std::io::Result<()> {
    let base = data_dir.join("clipboard-tmp");
    let metadata = match std::fs::symlink_metadata(&base) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "clipboard staging path is not a private directory",
        ));
    }
    for entry in std::fs::read_dir(&base)? {
        let entry = entry?;
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            std::fs::remove_file(entry.path())?;
        } else if metadata.is_dir() {
            std::fs::remove_dir_all(entry.path())?;
        } else {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_private_directory(path: &Path) -> Result<(), glide_xfer::Error> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(glide_xfer::Error::Storage)
}

#[cfg(not(unix))]
fn set_private_directory(_path: &Path) -> Result<(), glide_xfer::Error> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file(options: &mut std::fs::OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
}

#[cfg(not(unix))]
fn set_private_file(_options: &mut std::fs::OpenOptions) {}
