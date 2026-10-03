use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, OnceLock, Weak,
    },
    time::{Duration, Instant},
};

use bytes::BytesMut;
use glide_proto::{
    codec,
    ipc::{Connection as PeerState, Peer, PeerStats},
    wire::{self, ControlMessage, WireMessage},
};
use quinn::{Connection, Endpoint, RecvStream, SendStream};
use tokio::sync::{broadcast, mpsc, oneshot, Notify, OwnedSemaphorePermit, Semaphore};

use super::{
    identity::NativeIdentity, mutex, tls, transport_config, PinSet, HEARTBEAT_INTERVAL,
    HEARTBEAT_TIMEOUT, IO_TIMEOUT, MAX_PEERS,
};
use crate::{
    Link, LinkError, LinkEvent, MouseReceiver, NetFuture, PeerConnection, PeerManagerEvent,
    PeerToken, ReceivedMove,
};

const CONTROL: u8 = 0;
const INPUT: u8 = 1;
const CLIPBOARD: u8 = 2;
const FILE: u8 = 3;
const BULK_BUDGET: usize = 96 * 1024 * 1024;

/// Pinned QUIC transport obtained from [`super::NativePeerManager::link`].
/// Receiving is cancellation-safe; queues, streams, frames and mouse slots are bounded.
#[derive(Clone)]
pub struct NativeLink(pub(crate) Arc<Inner>);

pub(crate) struct Inner {
    pub endpoint: Endpoint,
    identity: Arc<NativeIdentity>,
    pub pins: PinSet,
    pub(super) hello: Mutex<wire::Hello>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    pub(super) session_notify: Notify,
    accepted_tx: mpsc::Sender<PeerConnection>,
    accepted_rx: tokio::sync::Mutex<mpsc::Receiver<PeerConnection>>,
    control_tx: mpsc::Sender<Inbound>,
    control_rx: tokio::sync::Mutex<mpsc::Receiver<Inbound>>,
    input_tx: mpsc::Sender<Inbound>,
    input_rx: tokio::sync::Mutex<mpsc::Receiver<Inbound>>,
    bulk_tx: mpsc::Sender<Inbound>,
    bulk_rx: tokio::sync::Mutex<mpsc::Receiver<Inbound>>,
    move_notify: Notify,
    event_tx: broadcast::Sender<PeerManagerEvent>,
    memory: Arc<Semaphore>,
    next_generation: AtomicU64,
    pub(super) transfer_tx: mpsc::Sender<super::TransferStreams>,
    pub(super) transfer_rx: tokio::sync::Mutex<mpsc::Receiver<super::TransferStreams>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.endpoint.close(0u32.into(), b"shutdown");
    }
}

struct Inbound {
    generation: usize,
    event: LinkEvent,
    _memory: Option<OwnedSemaphorePermit>,
}

enum OutboundData {
    Control(ControlMessage),
    Input(wire::InputMessage),
    Encoded(Vec<u8>),
}

struct Outbound {
    data: OutboundData,
    result: oneshot::Sender<Result<(), LinkError>>,
    _memory: Option<OwnedSemaphorePermit>,
}

pub(super) struct Session {
    pub(super) transfers: Arc<Mutex<std::collections::HashSet<String>>>,
    metadata: Mutex<Peer>,
    epochs: Mutex<InputEpochs>,
    outgoing_epoch: AtomicU64,
    outgoing_last_epoch: AtomicU64,
    enter_ack: Mutex<Option<(u64, oneshot::Sender<()>)>>,
    pub(super) token: OnceLock<PeerToken>,
    #[cfg(windows)]
    _timer_resolution: TimerResolution,
    pub(super) conn: Connection,
    pub(super) peer: PeerConnection,
    preferred: bool,
    control: mpsc::Sender<Outbound>,
    input: mpsc::Sender<Outbound>,
    clipboard: mpsc::Sender<Outbound>,
    files: mpsc::Sender<Outbound>,
    outbound_move: Mutex<(Option<u64>, Option<wire::Move>)>,
    inbound_move: Mutex<(Option<u64>, Option<wire::Move>)>,
    move_notify: Notify,
    last_heartbeat: Mutex<Instant>,
    heartbeat_seq: AtomicU64,
    heartbeat_ts: AtomicU64,
    input_seen: AtomicBool,
    clipboard_seen: AtomicBool,
    file_readers: Arc<Semaphore>,
    incoming_tasks: Arc<Semaphore>,
    tx_bytes: AtomicU64,
    rx_bytes: AtomicU64,
}

#[derive(Default)]
struct InputEpochs {
    last: u64,
    pending: Option<u64>,
    active: Option<u64>,
}

pub(super) struct HandshakeGuard(pub Option<Connection>);
impl Drop for HandshakeGuard {
    fn drop(&mut self) {
        if let Some(connection) = self.0.take() {
            connection.close(1u32.into(), b"interrupted admission");
        }
    }
}

// Bound Windows granularity for QUIC timers/backpressure; the mouse pump itself
// sends immediately without periodic pacing. Balance every successful request.
#[cfg(windows)]
struct TimerResolution;

#[cfg(windows)]
impl TimerResolution {
    fn acquire() -> Result<Self, LinkError> {
        // SAFETY: timeBeginPeriod takes only a bounded millisecond value; this
        // RAII owner balances the successful request with timeEndPeriod.
        if unsafe { windows_sys::Win32::Media::timeBeginPeriod(1) } == 0 {
            Ok(Self)
        } else {
            Err(LinkError::Internal(
                "1 ms transport timer unavailable".into(),
            ))
        }
    }
}

#[cfg(windows)]
impl Drop for TimerResolution {
    fn drop(&mut self) {
        // SAFETY: this instance owns exactly one successful 1 ms request.
        let _ = unsafe { windows_sys::Win32::Media::timeEndPeriod(1) };
    }
}

impl NativeLink {
    pub(super) fn broadcast_metadata(&self, update: wire::MetadataUpdate) -> Result<(), LinkError> {
        for session in mutex(&self.0.sessions)?.values() {
            if enqueue_control(session, ControlMessage::MetadataUpdate(update.clone())).is_err() {
                session
                    .conn
                    .close(1u32.into(), b"metadata update unavailable");
            }
        }
        Ok(())
    }
    pub(crate) fn new(
        endpoint: Endpoint,
        identity: Arc<NativeIdentity>,
        pins: PinSet,
        hello: wire::Hello,
        event_tx: broadcast::Sender<PeerManagerEvent>,
    ) -> Self {
        let (accepted_tx, accepted_rx) = mpsc::channel(MAX_PEERS);
        let (control_tx, control_rx) = mpsc::channel(128);
        let (input_tx, input_rx) = mpsc::channel(128);
        let (bulk_tx, bulk_rx) = mpsc::channel(2);
        let (transfer_tx, transfer_rx) = mpsc::channel(MAX_PEERS * 4);
        Self(Arc::new(Inner {
            endpoint,
            identity,
            pins,
            hello: Mutex::new(hello),
            sessions: Mutex::new(HashMap::with_capacity(MAX_PEERS)),
            session_notify: Notify::new(),
            accepted_tx,
            accepted_rx: tokio::sync::Mutex::new(accepted_rx),
            control_tx,
            control_rx: tokio::sync::Mutex::new(control_rx),
            input_tx,
            input_rx: tokio::sync::Mutex::new(input_rx),
            bulk_tx,
            bulk_rx: tokio::sync::Mutex::new(bulk_rx),
            move_notify: Notify::new(),
            event_tx,
            memory: Arc::new(Semaphore::new(BULK_BUDGET)),
            next_generation: AtomicU64::new(1),
            transfer_tx,
            transfer_rx: tokio::sync::Mutex::new(transfer_rx),
        }))
    }

    pub(crate) fn connected(&self, id: &str) -> bool {
        mutex(&self.0.sessions).is_ok_and(|sessions| {
            sessions
                .get(id)
                .is_some_and(|s| s.conn.close_reason().is_none())
        })
    }

    pub(crate) fn close_now(&self, id: &str) {
        match mutex(&self.0.sessions) {
            Ok(sessions) => {
                if let Some(session) = sessions.get(id) {
                    session.conn.close(0u32.into(), b"closed");
                }
            }
            Err(_) => self.0.endpoint.close(1u32.into(), b"state poisoned"),
        }
    }

    pub(crate) fn revoke_now(&self, id: &str) {
        match mutex(&self.0.sessions) {
            Ok(sessions) => {
                if let Some(session) = sessions.get(id) {
                    session.conn.close(2u32.into(), b"unpaired");
                }
            }
            Err(_) => self.0.endpoint.close(1u32.into(), b"state poisoned"),
        }
    }

    #[cfg(test)]
    pub(crate) fn session_count(&self) -> usize {
        mutex(&self.0.sessions).map_or(MAX_PEERS, |s| s.len())
    }

    #[cfg(test)]
    pub(crate) fn raw_connection(&self, id: &str) -> Option<Connection> {
        mutex(&self.0.sessions)
            .ok()?
            .get(id)
            .map(|s| s.conn.clone())
    }

    #[cfg(test)]
    pub(crate) fn connection_can_deliver_for_test(
        &self,
        id: &str,
        conn: &Connection,
        token: PeerToken,
    ) -> bool {
        self.valid_inbound(&Inbound {
            generation: conn.stable_id(),
            event: LinkEvent::Reliable {
                peer_id: id.to_owned(),
                peer_token: Some(token),
                message: WireMessage::Control(ControlMessage::Unpaired),
            },
            _memory: None,
        })
    }

    #[cfg(test)]
    pub(crate) async fn next_input_for_test(&self) -> Result<LinkEvent, LinkError> {
        self.0
            .input_rx
            .lock()
            .await
            .recv()
            .await
            .map(|i| i.event)
            .ok_or(LinkError::Closed)
    }

    #[cfg(test)]
    pub(crate) async fn send_input_unchecked_for_test(
        &self,
        id: &str,
        input: wire::InputMessage,
    ) -> Result<(), LinkError> {
        let session = self.session(id)?;
        let (result, reply) = oneshot::channel();
        session
            .input
            .send(Outbound {
                data: OutboundData::Input(input),
                result,
                _memory: None,
            })
            .await
            .map_err(|_| LinkError::Closed)?;
        reply.await.map_err(|_| LinkError::Closed)?
    }

    pub(crate) async fn install(
        &self,
        conn: Connection,
        dialer: bool,
        expected: Option<&str>,
    ) -> Result<PeerConnection, LinkError> {
        let id = tls::peer_device_id(&conn).map_err(|_| LinkError::IdentityMismatch)?;
        if !self.0.pins.contains(&id) || expected.is_some_and(|v| v != id) {
            conn.close(1u32.into(), b"identity");
            return Err(LinkError::IdentityMismatch);
        }
        let (mut control_send, mut control_recv) = timed(async {
            if dialer {
                conn.open_bi().await
            } else {
                conn.accept_bi().await
            }
        })
        .await?;
        control_send
            .set_priority(100)
            .map_err(|_| LinkError::Closed)?;
        let local = codec::encode_control(&ControlMessage::Hello(mutex(&self.0.hello)?.clone()))
            .map_err(protocol_error)?;
        write_frame(&mut control_send, &local).await?;
        let (remote, _memory) = tokio::time::timeout(
            IO_TIMEOUT,
            read_frame(&mut control_recv, CONTROL, &self.0.memory),
        )
        .await
        .map_err(|_| LinkError::Unreachable)??;
        let ControlMessage::Hello(hello) =
            codec::decode_control(&remote).map_err(protocol_error)?
        else {
            conn.close(1u32.into(), b"hello required");
            return Err(LinkError::IdentityMismatch);
        };
        if hello.device_id != id
            || hello.name.len() > 128
            || hello.name.is_empty()
            || hello.name.chars().any(char::is_control)
            || !self.0.pins.contains(&id)
        {
            conn.close(1u32.into(), b"invalid hello");
            return Err(LinkError::IdentityMismatch);
        }
        let peer = PeerConnection {
            device_id: id.clone(),
            address: conn.remote_address().to_string(),
        };
        let (control, control_rx) = mpsc::channel(32);
        let (input, input_rx) = mpsc::channel(128);
        let (clipboard, clipboard_rx) = mpsc::channel(2);
        let (files, files_rx) = mpsc::channel(4);
        // Reserve persistent lanes before exposing the session: a file sender
        // must never consume all uni-stream credit ahead of the first key.
        let input_send = open_lane(&conn, INPUT).await?;
        let clipboard_send = open_lane(&conn, CLIPBOARD).await?;
        let session = Arc::new(Session {
            transfers: Arc::new(Mutex::new(std::collections::HashSet::new())),
            metadata: Mutex::new(Peer {
                device_id: id.clone(),
                name: hello.name.clone(),
                os: hello.os,
                fingerprint: tls::human_fingerprint(&id),
                online: true,
                connection: PeerState::Connected,
                address: Some(peer.address.clone()),
                latency_ms: None,
                monitors: hello.monitors.clone().into_vec(),
                clipboard_enabled: true,
            }),
            epochs: Mutex::new(InputEpochs::default()),
            outgoing_epoch: AtomicU64::new(0),
            outgoing_last_epoch: AtomicU64::new(0),
            enter_ack: Mutex::new(None),
            token: OnceLock::new(),
            #[cfg(windows)]
            _timer_resolution: TimerResolution::acquire()?,
            conn,
            peer: peer.clone(),
            preferred: dialer == (self.0.identity.device_id() < id.as_str()),
            control,
            input,
            clipboard,
            files,
            outbound_move: Mutex::new((None, None)),
            inbound_move: Mutex::new((None, None)),
            move_notify: Notify::new(),
            last_heartbeat: Mutex::new(Instant::now()),
            heartbeat_seq: AtomicU64::new(0),
            heartbeat_ts: AtomicU64::new(0),
            input_seen: AtomicBool::new(false),
            clipboard_seen: AtomicBool::new(false),
            file_readers: Arc::new(Semaphore::new(4)),
            // Two persistent lanes, four fully-read/backpressured files and
            // at most eight QUIC-credit-limited unfinished streams.
            incoming_tasks: Arc::new(Semaphore::new(14)),
            tx_bytes: AtomicU64::new(0),
            rx_bytes: AtomicU64::new(0),
        });
        {
            let mut sessions = mutex(&self.0.sessions)?;
            if !self.0.pins.contains(&id) {
                session.conn.close(2u32.into(), b"unpaired");
                return Err(LinkError::IdentityMismatch);
            }
            if let Some(existing) = sessions
                .get(&id)
                .filter(|old| old.conn.close_reason().is_none())
            {
                if !session.preferred || existing.preferred {
                    session.conn.close(0u32.into(), b"duplicate");
                    return Ok(existing.peer.clone());
                }
                existing.conn.close(0u32.into(), b"duplicate");
            } else if sessions.len() >= MAX_PEERS {
                session.conn.close(1u32.into(), b"connection limit");
                return Err(LinkError::QueueFull);
            }
            let slot = if let Some(previous) = sessions.get(&id).and_then(|s| s.token.get()) {
                previous.slot
            } else {
                (0..MAX_PEERS as u8)
                    .find(|slot| {
                        sessions
                            .values()
                            .all(|s| s.token.get().is_none_or(|t| t.slot != *slot))
                    })
                    .ok_or(LinkError::QueueFull)?
            };
            let generation = self
                .0
                .next_generation
                .try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                    value.checked_add(1)
                })
                .map_err(|_| LinkError::Closed)?;
            session
                .token
                .set(PeerToken { slot, generation })
                .map_err(|_| LinkError::Closed)?;
            sessions.insert(id.clone(), session.clone());
            let snapshot = Peer {
                device_id: id.clone(),
                name: hello.name,
                os: hello.os,
                fingerprint: tls::human_fingerprint(&id),
                online: true,
                connection: PeerState::Connected,
                address: Some(peer.address.clone()),
                latency_ms: None,
                monitors: hello.monitors.into_vec(),
                clipboard_enabled: true,
            };
            let _ = self
                .0
                .event_tx
                .send(PeerManagerEvent::PeerUpdated(snapshot));
        }
        // A metadata update may have committed after the handshake's Hello but
        // before insertion. Serialize this snapshot with metadata broadcasts.
        {
            let hello = mutex(&self.0.hello)?;
            let (result, _) = oneshot::channel();
            session
                .control
                .try_send(Outbound {
                    data: OutboundData::Control(ControlMessage::MetadataUpdate(
                        wire::MetadataUpdate {
                            version: wire::PROTOCOL_VERSION,
                            name: hello.name.clone(),
                            monitors: hello.monitors.clone(),
                        },
                    )),
                    result,
                    _memory: None,
                })
                .map_err(|_| LinkError::QueueFull)?;
        }
        self.0.session_notify.notify_waiters();
        spawn_writer(
            session.clone(),
            control_rx,
            control_send,
            wire::MAX_CONTROL_FRAME_BYTES,
        );
        spawn_writer(
            session.clone(),
            input_rx,
            input_send,
            wire::MAX_INPUT_FRAME_BYTES,
        );
        spawn_writer(session.clone(), clipboard_rx, clipboard_send, 0);
        spawn_files(session.clone(), files_rx);
        spawn_control(Arc::downgrade(&self.0), session.clone(), control_recv);
        spawn_streams(Arc::downgrade(&self.0), session.clone());
        super::transfer::spawn_accept(Arc::downgrade(&self.0), session.clone());
        spawn_moves(Arc::downgrade(&self.0), session.clone());
        spawn_heartbeat(Arc::downgrade(&self.0), session.clone());
        let weak = Arc::downgrade(&self.0);
        let closing_session = session.clone();
        tokio::spawn(async move {
            let closed = closing_session.conn.closed().await;
            let Some(inner) = weak.upgrade() else {
                return;
            };
            if matches!(closed, quinn::ConnectionError::ApplicationClosed(ref close) if close.reason.as_ref() == b"unpaired")
            {
                let _ = inner.pins.remove(&closing_session.peer.device_id);
                let _ = inner.event_tx.send(PeerManagerEvent::Unpaired(
                    closing_session.peer.device_id.clone(),
                ));
            }
            let Ok(mut sessions) = mutex(&inner.sessions) else {
                inner.endpoint.close(1u32.into(), b"state poisoned");
                return;
            };
            let removed = {
                if sessions
                    .get(&closing_session.peer.device_id)
                    .is_some_and(|v| v.conn.stable_id() == closing_session.conn.stable_id())
                {
                    sessions.remove(&closing_session.peer.device_id);
                    true
                } else {
                    false
                }
            };
            if removed {
                let _ = inner.event_tx.send(PeerManagerEvent::Disappeared {
                    device_id: closing_session.peer.device_id.clone(),
                });
                // Losing a safety event means this consumer is too slow: close all sessions.
                if inner
                    .control_tx
                    .try_send(Inbound {
                        generation: closing_session.conn.stable_id(),
                        event: LinkEvent::Disconnected {
                            peer_id: closing_session.peer.device_id.clone(),
                            reason: Some("secure session lost".into()),
                        },
                        _memory: None,
                    })
                    .is_err()
                {
                    for s in sessions.values() {
                        s.conn.close(1u32.into(), b"slow reader");
                    }
                }
            }
        });
        if !dialer && self.0.accepted_tx.try_send(peer.clone()).is_err() {
            session.conn.close(1u32.into(), b"accept queue full");
            return Err(LinkError::QueueFull);
        }
        Ok(peer)
    }

    pub(super) fn session(&self, id: &str) -> Result<Arc<Session>, LinkError> {
        if !self.0.pins.contains(id) {
            return Err(LinkError::NotConnected);
        }
        mutex(&self.0.sessions)?
            .get(id)
            .filter(|s| s.conn.close_reason().is_none())
            .cloned()
            .ok_or(LinkError::NotConnected)
    }

    fn valid_inbound(&self, inbound: &Inbound) -> bool {
        let id = match &inbound.event {
            LinkEvent::Disconnected { peer_id, .. } => {
                return mutex(&self.0.sessions).is_ok_and(|s| {
                    s.get(peer_id)
                        .is_none_or(|s| s.conn.stable_id() == inbound.generation)
                });
            }
            LinkEvent::Reliable { peer_id, .. } | LinkEvent::Move { peer_id, .. } => peer_id,
        };
        self.0.pins.contains(id)
            && mutex(&self.0.sessions).is_ok_and(|sessions| {
                sessions.get(id).is_some_and(|s| {
                    s.conn.stable_id() == inbound.generation
                        && s.conn.close_reason().is_none()
                        // Quinn's stable_id is an address, unique only while the
                        // connection lives. Queued events also bind our epoch.
                        && !matches!(&inbound.event, LinkEvent::Reliable { peer_token: Some(token), .. } if s.token.get() != Some(token))
                        && match &inbound.event {
                            LinkEvent::Reliable {
                                message: WireMessage::Input(input),
                                ..
                            } => mutex(&s.epochs)
                                .is_ok_and(|epochs| epochs.active == Some(input.epoch())),
                            LinkEvent::Reliable {
                                message: WireMessage::Control(ControlMessage::Enter(enter)),
                                ..
                            } => mutex(&s.epochs)
                                .is_ok_and(|epochs| epochs.pending == Some(enter.epoch)),
                            _ => true,
                        }
                })
            })
    }

    fn take_move(&self) -> Result<Option<LinkEvent>, LinkError> {
        let sessions = mutex(&self.0.sessions)?;
        for (id, session) in sessions.iter() {
            if !self.0.pins.contains(id) || session.conn.close_reason().is_some() {
                continue;
            }
            let epochs = mutex(&session.epochs)?;
            if epochs.active.is_none() {
                continue;
            }
            if let Some(movement) = mutex(&session.inbound_move)?.1.take() {
                return Ok(Some(LinkEvent::Move {
                    peer_id: id.clone(),
                    movement,
                }));
            }
        }
        Ok(None)
    }
}

impl Link for NativeLink {
    fn admit_input_epoch(&self, id: &str, epoch: u64) -> Result<(), LinkError> {
        let session = self.session(id)?;
        let mut epochs = mutex(&session.epochs)?;
        if epoch == 0 || epochs.pending != Some(epoch) {
            return Err(LinkError::Closed);
        }
        epochs.pending = None;
        epochs.active = Some(epoch);
        if let Err(error) = enqueue_control(&session, ControlMessage::EnterAck { epoch }) {
            epochs.active = None;
            session.conn.close(1u32.into(), b"input admission failed");
            return Err(error);
        }
        Ok(())
    }
    fn peer_token(&self, id: &str) -> Result<PeerToken, LinkError> {
        self.session(id)?
            .token
            .get()
            .copied()
            .ok_or(LinkError::NotConnected)
    }
    fn mouse_ready(&self) -> &Notify {
        &self.0.move_notify
    }
    fn recv_event(&self) -> NetFuture<'_, Result<LinkEvent, LinkError>> {
        self.recv_inner(false)
    }
    fn connect<'a>(
        &'a self,
        address: &'a str,
        device_id: Option<&'a str>,
    ) -> NetFuture<'a, Result<PeerConnection, LinkError>> {
        Box::pin(async move {
            if let Some(id) = device_id {
                if self.connected(id) {
                    return Ok(self.session(id)?.peer.clone());
                }
            }
            let address = timed(tokio::net::lookup_host(address))
                .await?
                .next()
                .ok_or(LinkError::Unreachable)?;
            let mut config = tls::normal_client(&self.0.identity, self.0.pins.clone())
                .map_err(|_| LinkError::IdentityMismatch)?;
            config.transport_config(transport_config());
            let connecting = self
                .0
                .endpoint
                .connect_with(config, address, "glide.local")
                .map_err(|_| LinkError::Unreachable)?;
            let conn = timed(connecting).await?;
            let closing = conn.clone();
            match self.install(conn, true, device_id).await {
                Ok(peer) => Ok(peer),
                Err(error) => {
                    let duplicate = matches!(closing.close_reason(), Some(quinn::ConnectionError::ApplicationClosed(ref close)) if close.reason.as_ref() == b"duplicate");
                    closing.close(1u32.into(), b"session rejected");
                    if duplicate {
                        if let Some(id) = device_id {
                            return tokio::time::timeout(IO_TIMEOUT, async {
                                loop {
                                    let notify = self.0.session_notify.notified();
                                    tokio::pin!(notify);
                                    notify.as_mut().enable();
                                    if self.connected(id) {
                                        return Ok(self.session(id)?.peer.clone());
                                    }
                                    notify.await;
                                }
                            })
                            .await
                            .map_err(|_| LinkError::Unreachable)?;
                        }
                    }
                    Err(error)
                }
            }
        })
    }

    fn accept(&self) -> NetFuture<'_, Result<PeerConnection, LinkError>> {
        Box::pin(async move {
            let mut receiver = self.0.accepted_rx.lock().await;
            loop {
                let peer = receiver.recv().await.ok_or(LinkError::Closed)?;
                if self.connected(&peer.device_id) && self.0.pins.contains(&peer.device_id) {
                    return Ok(peer);
                }
            }
        })
    }

    fn send_reliable<'a>(
        &'a self,
        id: &'a str,
        message: WireMessage,
    ) -> NetFuture<'a, Result<(), LinkError>> {
        Box::pin(async move {
            let session = self.session(id)?;
            let acknowledgement =
                if let WireMessage::Control(ControlMessage::Enter(enter)) = &message {
                    let (sender, receiver) = oneshot::channel();
                    let mut pending = mutex(&session.enter_ack)?;
                    if pending.is_some()
                        || enter.epoch == 0
                        || enter.epoch <= session.outgoing_last_epoch.load(Ordering::Acquire)
                    {
                        return Err(LinkError::Closed);
                    }
                    session
                        .outgoing_last_epoch
                        .store(enter.epoch, Ordering::Release);
                    session.outgoing_epoch.store(0, Ordering::Release);
                    *pending = Some((enter.epoch, sender));
                    Some(receiver)
                } else {
                    if let WireMessage::Input(input) = &message {
                        if session.outgoing_epoch.load(Ordering::Acquire) != input.epoch() {
                            return Err(LinkError::NotConnected);
                        }
                    }
                    if let WireMessage::Control(ControlMessage::Leave(leave)) = &message {
                        let _ = session.outgoing_epoch.compare_exchange(
                            leave.epoch,
                            0,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        );
                    }
                    None
                };
            let mut admission_guard =
                HandshakeGuard(acknowledgement.as_ref().map(|_| session.conn.clone()));
            let (sender, data, bulk) = match message {
                WireMessage::Control(value) => {
                    (&session.control, OutboundData::Control(value), false)
                }
                WireMessage::Input(value) => (&session.input, OutboundData::Input(value), false),
                WireMessage::Clipboard(value) => (
                    &session.clipboard,
                    OutboundData::Encoded(codec::encode_clipboard(&value).map_err(protocol_error)?),
                    true,
                ),
                WireMessage::Transfer(value) => (
                    &session.files,
                    OutboundData::Encoded(codec::encode_transfer(&value).map_err(protocol_error)?),
                    true,
                ),
            };
            let memory = if bulk {
                Some(
                    self.0
                        .memory
                        .clone()
                        .try_acquire_many_owned(match &data {
                            OutboundData::Encoded(bytes) => bytes.len() as u32,
                            _ => 0,
                        })
                        .map_err(|_| LinkError::QueueFull)?,
                )
            } else {
                None
            };
            let (result, response) = oneshot::channel();
            sender
                .try_send(Outbound {
                    data,
                    result,
                    _memory: memory,
                })
                .map_err(|e| match e {
                    mpsc::error::TrySendError::Full(_) => LinkError::QueueFull,
                    _ => LinkError::Closed,
                })?;
            // Cancellation after enqueue does not reorder/drop keys; the lane actor owns the write.
            response.await.map_err(|_| LinkError::Closed)??;
            if let Some(ack) = acknowledgement {
                if timed(ack).await.is_err() {
                    session
                        .conn
                        .close(1u32.into(), b"enter acknowledgement timeout");
                    return Err(LinkError::Closed);
                }
            }
            admission_guard.0 = None;
            Ok(())
        })
    }

    fn send_datagram(&self, id: &str, movement: wire::Move) -> Result<(), LinkError> {
        if !movement.x.is_finite() || !movement.y.is_finite() {
            return Err(LinkError::Internal("invalid mouse coordinates".into()));
        }
        if !self.0.pins.try_contains(id)? {
            return Err(LinkError::NotConnected);
        }
        let sessions = self.0.sessions.try_lock().map_err(|e| match e {
            std::sync::TryLockError::WouldBlock => LinkError::Busy,
            _ => LinkError::Closed,
        })?;
        let session = sessions
            .get(id)
            .filter(|s| s.conn.close_reason().is_none())
            .ok_or(LinkError::NotConnected)?;
        if session.outgoing_epoch.load(Ordering::Acquire) == 0 {
            return Err(LinkError::NotConnected);
        }
        let mut slot = session.outbound_move.try_lock().map_err(|e| match e {
            std::sync::TryLockError::WouldBlock => LinkError::Busy,
            _ => LinkError::Closed,
        })?;
        if slot.0.is_none_or(|last| movement.seq > last) {
            slot.0 = Some(movement.seq);
            slot.1 = Some(movement);
            session.move_notify.notify_one();
        }
        Ok(())
    }

    fn recv(&self) -> NetFuture<'_, Result<LinkEvent, LinkError>> {
        self.recv_inner(true)
    }

    fn close<'a>(&'a self, id: &'a str) -> NetFuture<'a, Result<(), LinkError>> {
        Box::pin(async move {
            self.close_now(id);
            Ok(())
        })
    }
}

impl MouseReceiver for NativeLink {
    fn try_recv_move(&self) -> Result<Option<ReceivedMove>, LinkError> {
        let sessions = self.0.sessions.try_lock().map_err(|e| match e {
            std::sync::TryLockError::WouldBlock => LinkError::Busy,
            _ => LinkError::Closed,
        })?;
        for (id, session) in sessions.iter() {
            if !self.0.pins.try_contains(id)? || session.conn.close_reason().is_some() {
                continue;
            }
            let epochs = session.epochs.try_lock().map_err(|e| match e {
                std::sync::TryLockError::WouldBlock => LinkError::Busy,
                _ => LinkError::Closed,
            })?;
            if epochs.active.is_none() {
                continue;
            }
            let mut slot = session.inbound_move.try_lock().map_err(|e| match e {
                std::sync::TryLockError::WouldBlock => LinkError::Busy,
                _ => LinkError::Closed,
            })?;
            if let Some(peer) = session.token.get().copied() {
                if let Some(movement) = slot.1.take() {
                    return Ok(Some(ReceivedMove { peer, movement }));
                }
            }
        }
        Ok(None)
    }
}

impl NativeLink {
    fn recv_inner(&self, include_moves: bool) -> NetFuture<'_, Result<LinkEvent, LinkError>> {
        Box::pin(async move {
            let mut control = self.0.control_rx.lock().await;
            let mut input = self.0.input_rx.lock().await;
            let mut bulk = self.0.bulk_rx.lock().await;
            loop {
                let notified = self.0.move_notify.notified();
                tokio::pin!(notified);
                if include_moves {
                    notified.as_mut().enable();
                }
                if let Ok(inbound) = control.try_recv() {
                    if self.valid_inbound(&inbound) {
                        return Ok(inbound.event);
                    }
                    continue;
                }
                if let Ok(inbound) = input.try_recv() {
                    if self.valid_inbound(&inbound) {
                        return Ok(inbound.event);
                    }
                    continue;
                }
                if include_moves {
                    if let Some(movement) = self.take_move()? {
                        return Ok(movement);
                    }
                }
                let inbound = tokio::select! { biased;
                    item = control.recv() => item,
                    item = input.recv() => item,
                    _ = &mut notified, if include_moves => { continue; },
                    item = bulk.recv() => item,
                }
                .ok_or(LinkError::Closed)?;
                if self.valid_inbound(&inbound) {
                    return Ok(inbound.event);
                }
            }
        })
    }
}

async fn open_lane(conn: &Connection, lane: u8) -> Result<SendStream, LinkError> {
    let mut send = timed(conn.open_uni()).await?;
    send.set_priority(if lane == INPUT { 90 } else { 20 })
        .map_err(|_| LinkError::Closed)?;
    timed(send.write_all(&[lane])).await?;
    Ok(send)
}

fn spawn_writer(
    session: Arc<Session>,
    mut queue: mpsc::Receiver<Outbound>,
    mut send: SendStream,
    storage_size: usize,
) {
    tokio::spawn(async move {
        let mut storage = vec![0; storage_size];
        let result = async {
            loop {
                let Some(outbound) = (tokio::select! { _ = session.conn.closed() => None, item = queue.recv() => item }) else { return Ok::<(), LinkError>(()); };
                let bytes = match &outbound.data {
                    OutboundData::Control(message) => codec::encode_control_into(message, &mut storage),
                    OutboundData::Input(message) => codec::encode_input_into(message, &mut storage),
                    OutboundData::Encoded(bytes) => Ok(bytes.as_slice()),
                };
                let bytes = match bytes { Ok(bytes) => bytes, Err(error) => { let _ = outbound.result.send(Err(protocol_error(error))); continue; } };
                let result = write_frame(&mut send, bytes).await;
                if result.is_ok() { session.tx_bytes.fetch_add(bytes.len() as u64, Ordering::Relaxed); }
                let failed = result.is_err();
                let _ = outbound.result.send(result);
                if failed { return Err(LinkError::Closed); }
            }
        }.await;
        if result.is_err() {
            session.conn.close(1u32.into(), b"stream write failed");
        }
    });
}

fn spawn_files(session: Arc<Session>, mut queue: mpsc::Receiver<Outbound>) {
    tokio::spawn(async move {
        let limit = Arc::new(Semaphore::new(4));
        loop {
            let permit = tokio::select! { _ = session.conn.closed() => break, p = limit.clone().acquire_owned() => match p { Ok(p) => p, Err(_) => break } };
            let Some(outbound) =
                (tokio::select! { _ = session.conn.closed() => None, item = queue.recv() => item })
            else {
                break;
            };
            let session = session.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let result = async {
                    let mut send = timed(session.conn.open_uni()).await?;
                    send.set_priority(0).map_err(|_| LinkError::Closed)?;
                    timed(send.write_all(&[FILE])).await?;
                    let OutboundData::Encoded(bytes) = &outbound.data else {
                        return Err(LinkError::Closed);
                    };
                    write_frame(&mut send, bytes).await?;
                    send.finish().map_err(|_| LinkError::Closed)?;
                    // Hold the concurrency slot until acknowledged, bounding blocked file streams.
                    timed(send.stopped()).await?;
                    session
                        .tx_bytes
                        .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    Ok::<(), LinkError>(())
                }
                .await;
                if result.is_err() {
                    session.conn.close(1u32.into(), b"file stream stalled");
                }
                let _ = outbound.result.send(result);
            });
        }
    });
}

fn spawn_control(inner: Weak<Inner>, session: Arc<Session>, mut recv: RecvStream) {
    tokio::spawn(async move {
        let result = async {
            loop {
                let Some(inner) = inner.upgrade() else {
                    return Ok(());
                };
                let (frame, memory) = read_frame(&mut recv, CONTROL, &inner.memory).await?;
                let message = codec::decode_control(&frame).map_err(protocol_error)?;
                if !inner.pins.contains(&session.peer.device_id)
                    || !mutex(&inner.sessions)?
                        .get(&session.peer.device_id)
                        .is_some_and(|s| s.conn.stable_id() == session.conn.stable_id())
                {
                    return Err(LinkError::IdentityMismatch);
                }
                match &message {
                    ControlMessage::Hello(_) => return Err(LinkError::IdentityMismatch),
                    ControlMessage::MetadataUpdate(update) => {
                        let mut metadata = mutex(&session.metadata)?;
                        metadata.name = update.name.clone();
                        metadata.monitors = update.monitors.clone().into_vec();
                        let _ = inner
                            .event_tx
                            .send(PeerManagerEvent::PeerUpdated(metadata.clone()));
                        continue;
                    }
                    ControlMessage::Enter(enter) => {
                        let mut epochs = mutex(&session.epochs)?;
                        if enter.epoch <= epochs.last {
                            return Err(LinkError::Closed);
                        }
                        epochs.last = enter.epoch;
                        epochs.active = None;
                        epochs.pending = Some(enter.epoch);
                        mutex(&session.inbound_move)?.1 = None;
                    }
                    ControlMessage::Leave(leave) => {
                        let mut epochs = mutex(&session.epochs)?;
                        if epochs.active == Some(leave.epoch) || epochs.pending == Some(leave.epoch)
                        {
                            epochs.active = None;
                            epochs.pending = None;
                            mutex(&session.inbound_move)?.1 = None;
                        } else {
                            continue;
                        }
                    }
                    ControlMessage::EnterAck { epoch } => {
                        let mut pending = mutex(&session.enter_ack)?;
                        if pending
                            .as_ref()
                            .is_some_and(|(expected, _)| expected == epoch)
                        {
                            if let Some((_, sender)) = pending.take() {
                                session.outgoing_epoch.store(*epoch, Ordering::Release);
                                let _ = sender.send(());
                            }
                        } else {
                            return Err(LinkError::Closed);
                        }
                        continue;
                    }
                    ControlMessage::Heartbeat(beat) => {
                        enqueue_control(
                            &session,
                            ControlMessage::HeartbeatAck(wire::HeartbeatAck {
                                seq: beat.seq,
                                ts: beat.ts,
                            }),
                        )?;
                        let _ = inner.event_tx.send(PeerManagerEvent::Heartbeat {
                            device_id: session.peer.device_id.clone(),
                        });
                    }
                    ControlMessage::HeartbeatAck(ack) => {
                        if ack.seq == session.heartbeat_seq.load(Ordering::Relaxed)
                            && ack.ts == session.heartbeat_ts.load(Ordering::Relaxed)
                        {
                            *mutex(&session.last_heartbeat)? = Instant::now();
                        }
                        continue;
                    }
                    ControlMessage::TakeOver(takeover) => {
                        let _ = inner.event_tx.send(PeerManagerEvent::TakeOver {
                            device_id: session.peer.device_id.clone(),
                            pos: takeover.pos,
                        });
                    }
                    ControlMessage::Unpaired => {
                        let _ = inner.pins.remove(&session.peer.device_id);
                        let _ = inner
                            .event_tx
                            .send(PeerManagerEvent::Unpaired(session.peer.device_id.clone()));
                        session.conn.close(2u32.into(), b"unpaired");
                        return Ok(());
                    }
                    _ => {}
                }
                deliver(
                    &inner,
                    &session,
                    WireMessage::Control(message),
                    memory,
                    CONTROL,
                )
                .await?;
            }
        }
        .await;
        if result.is_err() {
            session.conn.close(1u32.into(), b"invalid control stream");
        }
    });
}

fn spawn_streams(inner: Weak<Inner>, session: Arc<Session>) {
    tokio::spawn(async move {
        while let Ok(mut recv) = session.conn.accept_uni().await {
            let Ok(task_permit) = session.incoming_tasks.clone().try_acquire_owned() else {
                session.conn.close(1u32.into(), b"stream task limit");
                break;
            };
            let weak = inner.clone();
            let session = session.clone();
            tokio::spawn(async move {
                let _task_permit = task_permit;
                let result = async {
                    let mut marker = [0];
                    timed(recv.read_exact(&mut marker)).await?;
                    let lane = marker[0];
                    let _permit = match lane {
                        INPUT if !session.input_seen.swap(true, Ordering::Relaxed) => None,
                        CLIPBOARD if !session.clipboard_seen.swap(true, Ordering::Relaxed) => None,
                        FILE => Some(tokio::select! {
                            _ = session.conn.closed() => return Err(LinkError::Closed),
                            permit = timed(session.file_readers.clone().acquire_owned()) => permit?,
                        }),
                        _ => return Err(LinkError::Internal("unexpected stream lane".into())),
                    };
                    loop {
                        let Some(inner) = weak.upgrade() else {
                            return Ok(());
                        };
                        let (frame, memory) = read_frame(&mut recv, lane, &inner.memory).await?;
                        let message = match lane {
                            INPUT => WireMessage::Input(
                                codec::decode_input(&frame).map_err(protocol_error)?,
                            ),
                            CLIPBOARD => WireMessage::Clipboard(
                                codec::decode_clipboard(&frame).map_err(protocol_error)?,
                            ),
                            FILE => WireMessage::Transfer(
                                codec::decode_transfer(&frame).map_err(protocol_error)?,
                            ),
                            _ => return Err(LinkError::Closed),
                        };
                        session
                            .rx_bytes
                            .fetch_add(frame.len() as u64, Ordering::Relaxed);
                        if let WireMessage::Input(input) = &message {
                            if mutex(&session.epochs)?.active != Some(input.epoch()) {
                                continue;
                            }
                        }
                        deliver(&inner, &session, message, memory, lane).await?;
                        if lane == FILE {
                            let mut extra = [0];
                            if timed(recv.read(&mut extra)).await?.is_some() {
                                return Err(LinkError::Internal("extra file frame".into()));
                            }
                            return Ok(());
                        }
                    }
                }
                .await;
                if result.is_err() {
                    session
                        .conn
                        .close(1u32.into(), b"invalid or stalled stream");
                }
            });
        }
    });
}

fn spawn_moves(inner: Weak<Inner>, session: Arc<Session>) {
    let sender_inner = inner.clone();
    let receiver = session.clone();
    tokio::spawn(async move {
        while let Ok(bytes) = receiver.conn.read_datagram().await {
            let Ok(movement) = codec::decode_move(&bytes) else {
                receiver.conn.close(1u32.into(), b"invalid datagram");
                break;
            };
            let Ok(epochs) = mutex(&receiver.epochs) else {
                break;
            };
            if epochs.active.is_none() {
                continue;
            }
            let Ok(mut slot) = mutex(&receiver.inbound_move) else {
                receiver.conn.close(1u32.into(), b"state poisoned");
                break;
            };
            if slot.0.is_none_or(|last| movement.seq > last) {
                slot.0 = Some(movement.seq);
                slot.1 = Some(movement);
                if let Some(inner) = inner.upgrade() {
                    inner.move_notify.notify_one();
                }
            }
        }
    });
    tokio::spawn(async move {
        // Bytes needs owned QUIC payloads: prewarm shared backing storage, then reclaim
        // only when Quinn releases it. Busy slots cause coalescing, never allocation.
        let mut pool: [BytesMut; 8] = std::array::from_fn(|_| {
            let mut slot = BytesMut::with_capacity(wire::MAX_MOVE_FRAME_BYTES);
            slot.extend_from_slice(&[0]);
            drop(slot.split_to(1).freeze());
            slot
        });
        let mut scratch = [0u8; wire::MAX_MOVE_FRAME_BYTES];
        loop {
            tokio::select! { _ = session.conn.closed() => break, _ = session.move_notify.notified() => {} }
            let movement = match mutex(&session.outbound_move) {
                Ok(mut slot) => slot.1.take(),
                Err(_) => {
                    session.conn.close(1u32.into(), b"state poisoned");
                    break;
                }
            };
            let Some(movement) = movement else {
                continue;
            };
            let Ok(encoded) = codec::encode_move_into(&movement, &mut scratch) else {
                session.conn.close(1u32.into(), b"invalid mouse move");
                break;
            };
            if let Some(buffer) = pool.iter_mut().find_map(|buffer| {
                buffer
                    .try_reclaim(wire::MAX_MOVE_FRAME_BYTES)
                    .then_some(buffer)
            }) {
                buffer.extend_from_slice(encoded);
                let bytes = buffer.split_to(encoded.len()).freeze();
                if !sender_inner
                    .upgrade()
                    .is_some_and(|i| i.pins.contains(&session.peer.device_id))
                {
                    session.conn.close(2u32.into(), b"unpaired");
                    break;
                }
                if session.conn.send_datagram(bytes).is_err() {
                    session.conn.close(1u32.into(), b"datagrams unavailable");
                    break;
                }
            } else {
                if let Ok(mut slot) = mutex(&session.outbound_move) {
                    if slot.1.is_none() {
                        slot.1 = Some(movement);
                    }
                }
                // No reusable payload slot: retry on the bounded cadence,
                // rather than spinning while QUIC still owns every buffer.
                tokio::time::sleep(Duration::from_millis(1)).await;
                session.move_notify.notify_one();
            }
        }
    });
}

fn spawn_heartbeat(inner: Weak<Inner>, session: Arc<Session>) {
    tokio::spawn(async move {
        let start = Instant::now();
        let mut timer = tokio::time::interval_at(
            tokio::time::Instant::now() + HEARTBEAT_INTERVAL,
            HEARTBEAT_INTERVAL,
        );
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut seq = 0u64;
        let mut last_tx = 0;
        let mut last_rx = 0;
        loop {
            tokio::select! { _ = session.conn.closed() => break, _ = timer.tick() => {} }
            if mutex(&session.last_heartbeat)
                .map_or(true, |last| last.elapsed() >= HEARTBEAT_TIMEOUT)
            {
                session.conn.close(1u32.into(), b"heartbeat timeout");
                break;
            }
            let Some(next) = seq.checked_add(1) else {
                session.conn.close(1u32.into(), b"sequence exhausted");
                break;
            };
            seq = next;
            let ts = start.elapsed().as_micros().min(u64::MAX as u128) as u64;
            session.heartbeat_seq.store(seq, Ordering::Relaxed);
            session.heartbeat_ts.store(ts, Ordering::Relaxed);
            if enqueue_control(
                &session,
                ControlMessage::Heartbeat(wire::Heartbeat { seq, ts }),
            )
            .is_err()
            {
                session.conn.close(1u32.into(), b"control queue full");
                break;
            }
            if seq.is_multiple_of(2) {
                let tx = session.tx_bytes.load(Ordering::Relaxed);
                let rx = session.rx_bytes.load(Ordering::Relaxed);
                if let Some(inner) = inner.upgrade() {
                    let _ = inner.event_tx.send(PeerManagerEvent::Stats(PeerStats {
                        device_id: session.peer.device_id.clone(),
                        latency_ms: session.conn.rtt().as_secs_f64() * 1000.0,
                        rx_bps: rx.saturating_sub(last_rx),
                        tx_bps: tx.saturating_sub(last_tx),
                    }));
                }
                last_tx = tx;
                last_rx = rx;
            }
        }
    });
}

fn enqueue_control(session: &Session, message: ControlMessage) -> Result<(), LinkError> {
    let (result, _) = oneshot::channel();
    session
        .control
        .try_send(Outbound {
            data: OutboundData::Control(message),
            result,
            _memory: None,
        })
        .map_err(|_| LinkError::QueueFull)
}

async fn deliver(
    inner: &Inner,
    session: &Session,
    message: WireMessage,
    memory: Option<OwnedSemaphorePermit>,
    lane: u8,
) -> Result<(), LinkError> {
    if !inner.pins.contains(&session.peer.device_id) {
        return Err(LinkError::IdentityMismatch);
    }
    let sender = match lane {
        CONTROL => &inner.control_tx,
        INPUT => &inner.input_tx,
        _ => &inner.bulk_tx,
    };
    let inbound = Inbound {
        generation: session.conn.stable_id(),
        event: LinkEvent::Reliable {
            peer_id: session.peer.device_id.clone(),
            peer_token: session.token.get().copied(),
            message,
        },
        _memory: memory,
    };
    // Bulk backpressure cannot occupy the separate control/input delivery queues.
    if lane == CONTROL || lane == INPUT {
        sender.try_send(inbound).map_err(|_| LinkError::QueueFull)
    } else {
        timed(sender.send(inbound)).await
    }
}

pub(crate) async fn timed<T, E>(
    future: impl std::future::Future<Output = Result<T, E>>,
) -> Result<T, LinkError> {
    tokio::time::timeout(IO_TIMEOUT, future)
        .await
        .map_err(|_| LinkError::Unreachable)?
        .map_err(|_| LinkError::Closed)
}

pub(crate) async fn write_frame(send: &mut SendStream, bytes: &[u8]) -> Result<(), LinkError> {
    let length = u32::try_from(bytes.len())
        .map_err(|_| LinkError::QueueFull)?
        .to_be_bytes();
    timed(async {
        send.write_all(&length).await?;
        send.write_all(bytes).await
    })
    .await
}

async fn read_frame(
    recv: &mut RecvStream,
    lane: u8,
    budget: &Arc<Semaphore>,
) -> Result<(Vec<u8>, Option<OwnedSemaphorePermit>), LinkError> {
    let mut header = [0u8; 4];
    // Idle input/clipboard streams are legal. Once a frame begins, every further
    // read has a deadline; file streams must begin promptly as well.
    if lane == FILE {
        timed(recv.read_exact(&mut header[..1])).await?;
    } else {
        recv.read_exact(&mut header[..1])
            .await
            .map_err(|_| LinkError::Closed)?;
    }
    timed(recv.read_exact(&mut header[1..])).await?;
    let length = u32::from_be_bytes(header) as usize;
    let max = match lane {
        CONTROL => wire::MAX_CONTROL_FRAME_BYTES,
        INPUT => wire::MAX_INPUT_FRAME_BYTES,
        CLIPBOARD => wire::MAX_CLIP_DATA_FRAME_BYTES,
        FILE => wire::MAX_FILE_CONTROL_FRAME_BYTES,
        _ => 0,
    };
    if length == 0 || length > max {
        return Err(LinkError::Internal("frame size limit".into()));
    }
    // All current postcard message discriminants occupy exactly one byte. Read it
    // before allocation so variant-specific limits cannot be bypassed by a header.
    let mut variant = [0];
    timed(recv.read_exact(&mut variant)).await?;
    let variant_max = match (lane, variant[0]) {
        (CONTROL, 0..=10) => wire::MAX_CONTROL_FRAME_BYTES,
        (INPUT, 0..=3) => wire::MAX_INPUT_FRAME_BYTES,
        (CLIPBOARD, 0) => wire::MAX_CLIP_ANNOUNCE_FRAME_BYTES,
        (CLIPBOARD, 1 | 3) => wire::MAX_INPUT_FRAME_BYTES,
        (CLIPBOARD, 2) => wire::MAX_CLIP_DATA_FRAME_BYTES,
        (FILE, 1) => wire::MAX_FILE_CHUNK_FRAME_BYTES,
        (FILE, 0 | 2..=4) => wire::MAX_FILE_CONTROL_FRAME_BYTES,
        _ => 0,
    };
    if length > variant_max {
        return Err(LinkError::Internal("frame variant limit".into()));
    }
    let memory = if lane >= CLIPBOARD {
        Some(
            budget
                .clone()
                // Reserve the frame and owned decoded payload before allocating.
                .try_acquire_many_owned((length * 2) as u32)
                .map_err(|_| LinkError::QueueFull)?,
        )
    } else {
        None
    };
    let mut frame = Vec::new();
    frame
        .try_reserve_exact(length)
        .map_err(|_| LinkError::QueueFull)?;
    frame.resize(length, 0);
    frame[0] = variant[0];
    timed(recv.read_exact(&mut frame[1..])).await?;
    Ok((frame, memory))
}

fn protocol_error(_: codec::CodecError) -> LinkError {
    LinkError::Internal("invalid protocol frame".into())
}
