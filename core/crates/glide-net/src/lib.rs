//! Network boundary for Glide.
//!
//! `native-transport` (enabled by default) provides keystore identities,
//! pinned TLS 1.3 QUIC, exporter-bound PAKE and discovery. See
//! `NativePeerManager` and the crate README for initialization and limits.
//! The explicitly mock-only in-memory implementations remain available with
//! or without that feature; they never provide authentication.

#[cfg(feature = "native-transport")]
mod native;
#[cfg(feature = "native-transport")]
pub use native::{
    NativeConfig, NativeError, NativeIdentity, NativeLink, NativePeerManager, PairingRepair,
    PinSet, TransferHandle, TransferIo, TransferStreams,
};
/// Used by executable compile-time guards; production must leave this false.
pub const TEST_SUPPORT_ENABLED: bool = cfg!(feature = "test-support");
#[cfg(feature = "test-support")]
pub use native::TestKeyStore;

use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard},
};

use glide_platform::{Monitor, Os, Point};
use glide_proto::{
    ipc::{
        Connection, DiscoveredPeer, ErrorCode, IpcError, PairingVerify, Peer, PeerStats,
        VerificationPeer,
    },
    wire::{Move, WireMessage},
};
use tokio::sync::{broadcast, mpsc};
use zeroize::Zeroizing;

const MOCK_QUEUE_CAPACITY: usize = 128;
const PAIRING_CODE_LIFETIME_MS: u64 = 120_000;
const PAIRING_VERIFY_TIMEOUT_MS: u64 = 60_000;
const PAIRING_ATTEMPT_LIMIT: u8 = 3;

/// Boxed, sendable future used to keep the transport traits object-safe.
pub type NetFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// An established peer transport endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerConnection {
    /// The peer's pinned device identifier in production implementations.
    pub device_id: String,
    /// The endpoint used for this connection.
    pub address: String,
}

/// Transport failures surfaced to the daemon.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum LinkError {
    /// The requested peer is not in the implementation's reachable set.
    #[error("peer is unreachable")]
    Unreachable,
    /// The peer is not currently connected.
    #[error("peer is not connected")]
    NotConnected,
    /// The expected device identifier did not match the configured peer.
    #[error("peer identity did not match")]
    IdentityMismatch,
    /// A bounded mock queue is full.
    #[error("transport queue is full")]
    QueueFull,
    /// A non-blocking datagram enqueue could not acquire its state lock.
    #[error("transport is busy")]
    Busy,
    /// The receive side has been closed.
    #[error("transport is closed")]
    Closed,
    /// The transport encountered an implementation error.
    #[error("transport error: {0}")]
    Internal(String),
}

/// An event delivered by a peer transport.
#[derive(Clone, Debug)]
pub enum LinkEvent {
    /// A peer sent a reliable control, input, clipboard, or transfer message.
    Reliable {
        /// Sender's device identifier.
        peer_id: String,
        /// Native connection generation. Explicit unauthenticated mocks may use None.
        peer_token: Option<PeerToken>,
        /// Decoded message. Production links reject malformed wire data first.
        message: WireMessage,
    },
    /// A peer sent its latest absolute mouse position.
    Move {
        /// Sender's device identifier.
        peer_id: String,
        /// Position in this receiver's logical-pixel space.
        movement: Move,
    },
    /// A connected peer closed its session.
    Disconnected {
        /// The peer that disconnected.
        peer_id: String,
        /// Optional non-sensitive reason suitable for diagnostics.
        reason: Option<String>,
    },
}

/// Connection and message boundary implemented by the production network
/// crate. Methods are non-blocking at the call site; async work is represented
/// by NetFuture.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct PeerToken {
    pub slot: u8,
    pub generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReceivedMove {
    pub peer: PeerToken,
    pub movement: Move,
}

pub trait MouseReceiver: Send + Sync {
    fn try_recv_move(&self) -> Result<Option<ReceivedMove>, LinkError>;
}

pub trait Link: MouseReceiver {
    /// Admit a validated Enter after the daemon checks policy and injection.
    /// Native links acknowledge this epoch before the sender may send input.
    fn admit_input_epoch(&self, peer_id: &str, epoch: u64) -> Result<(), LinkError> {
        if epoch == 0 {
            return Err(LinkError::Closed);
        }
        self.peer_token(peer_id).map(|_| ())
    }
    /// Resolve the connection's published token-to-ID mapping outside the mouse path.
    fn peer_token(&self, peer_id: &str) -> Result<PeerToken, LinkError>;
    /// Allocation-free wakeup for the separate mouse consumer.
    fn mouse_ready(&self) -> &tokio::sync::Notify;
    /// Everything except mouse moves. Keep this future alive across mouse wakeups.
    fn recv_event(&self) -> NetFuture<'_, Result<LinkEvent, LinkError>>;
    /// Connect to an address, optionally requiring the expected device ID.
    ///
    /// Production implementations authenticate a pinned peer before exposing
    /// application messages. The future may wait for connection establishment
    /// and returns an error if authentication or setup fails.
    fn connect<'a>(
        &'a self,
        address: &'a str,
        device_id: Option<&'a str>,
    ) -> NetFuture<'a, Result<PeerConnection, LinkError>>;

    /// Accept the next authenticated incoming peer connection.
    ///
    /// This future waits without blocking a runtime worker. Production
    /// implementations accept sessions only from paired peers, except on the
    /// separate, explicitly opened pairing path.
    fn accept(&self) -> NetFuture<'_, Result<PeerConnection, LinkError>>;

    /// Send a reliable wire message to a connected peer.
    ///
    /// The message is owned by the returned future. Implementations preserve
    /// per-peer ordering, bound queued data, and return backpressure or
    /// disconnect errors instead of silently dropping reliable messages.
    fn send_reliable<'a>(
        &'a self,
        peer_id: &'a str,
        message: WireMessage,
    ) -> NetFuture<'a, Result<(), LinkError>>;

    /// Enqueue the newest mouse position for a connected peer.
    ///
    /// This is synchronous so the capture-to-network mouse path can avoid
    /// allocation and waiting. Implementations keep at most one pending move
    /// per peer, replacing stale pending positions (latest-wins), and return
    /// immediately with NotConnected for a missing peer or Busy if enqueue
    /// state cannot be acquired without waiting.
    fn send_datagram(&self, peer_id: &str, movement: Move) -> Result<(), LinkError>;

    /// Wait for the next decoded inbound message or disconnect event.
    ///
    /// Production implementations discard stale move sequence numbers and
    /// close a peer on any wire decode failure. The future does not block a
    /// runtime worker while waiting for data. It must be cancellation-safe:
    /// dropping a pending future must not consume an undelivered event.
    fn recv(&self) -> NetFuture<'_, Result<LinkEvent, LinkError>>;

    /// Close a peer session and stop delivering its messages.
    fn close<'a>(&'a self, peer_id: &'a str) -> NetFuture<'a, Result<(), LinkError>>;
}

/// Target for joining an already discovered or manually addressed peer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PairTarget {
    /// Resolve by the peer's current network address.
    Address(String),
    /// Resolve by a discovered device identifier.
    DeviceId(String),
}

/// Information shown while this device hosts a pairing window.
#[derive(Clone, PartialEq, Eq)]
pub struct PairingHost {
    /// Short-lived code shown to the user.
    pub code: Zeroizing<String>,
    /// Absolute expiry time in Unix milliseconds.
    pub expires_at_ms: u64,
}

impl std::fmt::Debug for PairingHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingHost")
            .field("code", &"[redacted]")
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

/// Unpinned peer candidate and mock verification phrase returned after code
/// verification. A candidate becomes trusted only after both confirmations.
#[derive(Clone, Debug, PartialEq)]
pub struct PairingSession {
    /// Candidate peer. This is not in the paired-peer store yet.
    pub peer: Peer,
    /// Three-word phrase shown to both users before either confirms.
    pub verification: PairingVerify,
}

/// Peer-management events consumed by the daemon event pump.
#[derive(Clone, Debug)]
pub enum PeerManagerEvent {
    /// A nearby unpaired peer was discovered.
    Discovered(DiscoveredPeer),
    /// An unpaired discovery hint was removed (also on discovery disable).
    DiscoveryRemoved { device_id: String },
    /// A peer's online state or advertised capabilities changed.
    PeerUpdated(Peer),
    /// Link statistics sampled for one connected peer.
    Stats(PeerStats),
    /// The target can no longer inject input; forwarding must stop.
    InjectionDenied {
        /// The affected peer.
        device_id: String,
    },
    /// A peer's heartbeat arrived.
    Heartbeat {
        /// The sending peer.
        device_id: String,
    },
    /// A target reported physical local input and requested control.
    TakeOver {
        /// The peer taking control.
        device_id: String,
        /// Cursor position to use on the new brain.
        pos: Point,
    },
    /// A paired peer disappeared from discovery or lost its session.
    Disappeared {
        /// The peer that disappeared.
        device_id: String,
    },
    /// A remote user opened a pairing request.
    PairingIncoming {
        /// Display name supplied by the remote device.
        name: String,
        /// Operating system supplied by the remote device.
        os: Os,
        /// Address supplied by discovery or the pairing request.
        address: String,
    },
    /// A pairing attempt completed.
    PairingResult {
        /// Whether the attempt succeeded.
        ok: bool,
        /// Device identifier when a peer was successfully added.
        device_id: Option<String>,
        /// Stable IPC error code on failure.
        error: Option<ErrorCode>,
    },
    /// A candidate and human verification phrase are ready for display.
    PairingVerify(PairingVerify),
    /// Both users confirmed the phrase and the peer was pinned.
    Paired(Peer),
    /// A paired peer was removed.
    Unpaired(String),
}

/// Peer lifecycle and pairing boundary implemented by the production network
/// crate. Implementations emit events without blocking hook threads; events
/// returns a receiver for one daemon event pump.
pub trait PeerManager: Send + Sync {
    /// Open a 120-second pairing window and return its user-visible code.
    ///
    /// Caller-supplied time makes expiry deterministic. Production
    /// implementations enforce PAKE, TLS exporter binding, one-time use, and
    /// per-source rate limits.
    fn pair_host<'a>(&'a self, now_ms: u64) -> NetFuture<'a, Result<PairingHost, IpcError>>;

    /// Join a remote pairing host using its one-time code.
    ///
    /// A valid code creates only a candidate and verification prompt. Production
    /// implementations pin and persist the peer only after both users confirm.
    /// Callers must not treat this mock boundary as secure pairing.
    fn pair_join<'a>(
        &'a self,
        target: PairTarget,
        code: &'a str,
        now_ms: u64,
    ) -> NetFuture<'a, Result<PairingSession, IpcError>>;

    /// Record this user's confirmation and finish pairing once the remote user
    /// has also confirmed the same phrase.
    ///
    /// Returns the pinned peer when both sides accepted, or None while waiting
    /// for the remote response. Rejection and expiry clear the candidate.
    fn confirm_pairing(
        &self,
        accepted: bool,
        now_ms: u64,
    ) -> NetFuture<'_, Result<Option<Peer>, IpcError>>;

    /// Close the local pairing window, if one is open.
    fn cancel_pair_host(&self) -> NetFuture<'_, Result<(), IpcError>>;

    /// Start discovery and return currently visible unpaired peers.
    ///
    /// Production discovery advertises only non-secret peer metadata. A name
    /// or address is never proof of peer identity.
    fn discover(&self) -> NetFuture<'_, Result<Vec<DiscoveredPeer>, IpcError>>;

    /// Add a manually supplied host-and-port address if it is reachable.
    ///
    /// This only discovers the endpoint; it must not pair or trust it.
    fn add_manual<'a>(
        &'a self,
        address: &'a str,
    ) -> NetFuture<'a, Result<DiscoveredPeer, IpcError>>;

    /// Revoke a paired peer and close its active transport immediately.
    fn unpair<'a>(&'a self, device_id: &'a str) -> NetFuture<'a, Result<(), IpcError>>;

    /// Subscribe to pairing and discovery events.
    fn events(&self) -> broadcast::Receiver<PeerManagerEvent>;
}

/// In-memory transport used by tests and explicit mock-backend mode only.
///
/// It never opens sockets or authenticates identity. Script reachable peers
/// and inbound events with the script methods. Reliable sends are recorded in
/// a bounded outbox; datagrams overwrite one pre-created slot per peer.
pub struct InMemoryLink {
    state: Mutex<LinkState>,
    accepted_tx: mpsc::Sender<PeerConnection>,
    accepted_rx: tokio::sync::Mutex<mpsc::Receiver<PeerConnection>>,
    incoming_tx: mpsc::Sender<LinkEvent>,
    incoming_rx: tokio::sync::Mutex<mpsc::Receiver<LinkEvent>>,
    move_notify: tokio::sync::Notify,
}

#[derive(Default)]
struct LinkState {
    reachable: HashMap<String, PeerConnection>,
    connected: HashMap<String, String>,
    latest_moves: HashMap<String, Option<Move>>,
    inbound_moves: HashMap<String, (Option<u64>, Option<Move>)>,
    reliable_outbox: VecDeque<(String, WireMessage)>,
    tokens: HashMap<String, PeerToken>,
    generation: u64,
}

impl LinkState {
    fn install_token(&mut self, id: &str) -> Result<(), LinkError> {
        if self.tokens.contains_key(id) {
            return Ok(());
        }
        let slot = (0..32u8)
            .find(|slot| self.tokens.values().all(|token| token.slot != *slot))
            .ok_or(LinkError::QueueFull)?;
        self.generation = self.generation.checked_add(1).ok_or(LinkError::Closed)?;
        self.tokens.insert(
            id.to_owned(),
            PeerToken {
                slot,
                generation: self.generation,
            },
        );
        Ok(())
    }
}

impl InMemoryLink {
    /// Create an empty mock transport. This does no network initialization.
    pub fn new() -> Self {
        let (accepted_tx, accepted_rx) = mpsc::channel(MOCK_QUEUE_CAPACITY);
        let (incoming_tx, incoming_rx) = mpsc::channel(MOCK_QUEUE_CAPACITY);
        Self {
            state: Mutex::new(LinkState::default()),
            accepted_tx,
            accepted_rx: tokio::sync::Mutex::new(accepted_rx),
            incoming_tx,
            incoming_rx: tokio::sync::Mutex::new(incoming_rx),
            move_notify: tokio::sync::Notify::new(),
        }
    }

    /// Configure an endpoint that connect can reach in this mock.
    pub fn script_reachable(&self, address: impl Into<String>, device_id: impl Into<String>) {
        let connection = PeerConnection {
            device_id: device_id.into(),
            address: address.into(),
        };
        lock(&self.state)
            .reachable
            .insert(connection.address.clone(), connection);
    }

    /// Queue an incoming connection for the next accept call.
    pub fn script_accept(&self, connection: PeerConnection) -> Result<(), LinkError> {
        self.accepted_tx
            .try_send(connection)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => LinkError::QueueFull,
                mpsc::error::TrySendError::Closed(_) => LinkError::Closed,
            })
    }

    /// Queue an inbound message or disconnect for the next recv call.
    pub fn script_event(&self, event: LinkEvent) -> Result<(), LinkError> {
        match event {
            LinkEvent::Move { peer_id, movement } => {
                let mut state = try_lock(&self.state)?;
                let slot = state
                    .inbound_moves
                    .get_mut(&peer_id)
                    .ok_or(LinkError::NotConnected)?;
                if slot.0.is_none_or(|last_seq| movement.seq > last_seq) {
                    slot.0 = Some(movement.seq);
                    slot.1 = Some(movement);
                    self.move_notify.notify_one();
                }
                Ok(())
            }
            LinkEvent::Disconnected { peer_id, reason } => {
                let mut state = try_lock(&self.state)?;
                state.connected.remove(&peer_id);
                state.tokens.remove(&peer_id);
                state.latest_moves.remove(&peer_id);
                state.inbound_moves.remove(&peer_id);
                drop(state);
                self.incoming_tx
                    .try_send(LinkEvent::Disconnected { peer_id, reason })
                    .map_err(|error| match error {
                        mpsc::error::TrySendError::Full(_) => LinkError::QueueFull,
                        mpsc::error::TrySendError::Closed(_) => LinkError::Closed,
                    })
            }
            event => self
                .incoming_tx
                .try_send(event)
                .map_err(|error| match error {
                    mpsc::error::TrySendError::Full(_) => LinkError::QueueFull,
                    mpsc::error::TrySendError::Closed(_) => LinkError::Closed,
                }),
        }
    }

    /// Drain messages recorded by mock reliable sends.
    pub fn take_sent_reliable(&self) -> Vec<(String, WireMessage)> {
        lock(&self.state).reliable_outbox.drain(..).collect()
    }

    /// Read the newest queued mock mouse position for a connected peer.
    pub fn latest_move(&self, peer_id: &str) -> Option<Move> {
        lock(&self.state)
            .latest_moves
            .get(peer_id)
            .copied()
            .flatten()
    }
}

impl Default for InMemoryLink {
    fn default() -> Self {
        Self::new()
    }
}

impl Link for InMemoryLink {
    fn peer_token(&self, id: &str) -> Result<PeerToken, LinkError> {
        try_lock(&self.state)?
            .tokens
            .get(id)
            .copied()
            .ok_or(LinkError::NotConnected)
    }
    fn mouse_ready(&self) -> &tokio::sync::Notify {
        &self.move_notify
    }
    fn recv_event(&self) -> NetFuture<'_, Result<LinkEvent, LinkError>> {
        Box::pin(async move {
            self.incoming_rx
                .lock()
                .await
                .recv()
                .await
                .ok_or(LinkError::Closed)
        })
    }
    fn connect<'a>(
        &'a self,
        address: &'a str,
        device_id: Option<&'a str>,
    ) -> NetFuture<'a, Result<PeerConnection, LinkError>> {
        Box::pin(async move {
            let mut state = lock(&self.state);
            let connection = state
                .reachable
                .get(address)
                .cloned()
                .ok_or(LinkError::Unreachable)?;
            if device_id.is_some_and(|expected| expected != connection.device_id) {
                return Err(LinkError::IdentityMismatch);
            }
            state.install_token(&connection.device_id)?;
            state
                .connected
                .insert(connection.device_id.clone(), connection.address.clone());
            state
                .latest_moves
                .entry(connection.device_id.clone())
                .or_insert(None);
            state
                .inbound_moves
                .entry(connection.device_id.clone())
                .or_insert((None, None));
            Ok(connection)
        })
    }

    fn accept(&self) -> NetFuture<'_, Result<PeerConnection, LinkError>> {
        Box::pin(async move {
            let connection = self
                .accepted_rx
                .lock()
                .await
                .recv()
                .await
                .ok_or(LinkError::Closed)?;
            let mut state = lock(&self.state);
            state.install_token(&connection.device_id)?;
            state
                .connected
                .insert(connection.device_id.clone(), connection.address.clone());
            state
                .latest_moves
                .entry(connection.device_id.clone())
                .or_insert(None);
            state
                .inbound_moves
                .entry(connection.device_id.clone())
                .or_insert((None, None));
            Ok(connection)
        })
    }

    fn send_reliable<'a>(
        &'a self,
        peer_id: &'a str,
        message: WireMessage,
    ) -> NetFuture<'a, Result<(), LinkError>> {
        Box::pin(async move {
            let mut state = lock(&self.state);
            if !state.connected.contains_key(peer_id) {
                return Err(LinkError::NotConnected);
            }
            if state.reliable_outbox.len() >= MOCK_QUEUE_CAPACITY {
                return Err(LinkError::QueueFull);
            }
            state
                .reliable_outbox
                .push_back((peer_id.to_owned(), message));
            Ok(())
        })
    }

    fn send_datagram(&self, peer_id: &str, movement: Move) -> Result<(), LinkError> {
        let mut state = try_lock(&self.state)?;
        let slot = state
            .latest_moves
            .get_mut(peer_id)
            .ok_or(LinkError::NotConnected)?;
        if match *slot {
            Some(latest) => movement.seq > latest.seq,
            None => true,
        } {
            *slot = Some(movement);
        }
        Ok(())
    }

    fn recv(&self) -> NetFuture<'_, Result<LinkEvent, LinkError>> {
        Box::pin(async move {
            loop {
                {
                    let mut receiver = self.incoming_rx.lock().await;
                    match receiver.try_recv() {
                        Ok(event) => return Ok(event),
                        Err(mpsc::error::TryRecvError::Disconnected) => {
                            return Err(LinkError::Closed);
                        }
                        Err(mpsc::error::TryRecvError::Empty) => {}
                    }
                }
                if let Some((peer_id, movement)) = take_inbound_move(&self.state) {
                    return Ok(LinkEvent::Move { peer_id, movement });
                }
                tokio::select! {
                    event = async { self.incoming_rx.lock().await.recv().await } => {
                        return event.ok_or(LinkError::Closed);
                    }
                    _ = self.move_notify.notified() => {}
                }
            }
        })
    }

    fn close<'a>(&'a self, peer_id: &'a str) -> NetFuture<'a, Result<(), LinkError>> {
        Box::pin(async move {
            let mut state = lock(&self.state);
            state.connected.remove(peer_id);
            state.tokens.remove(peer_id);
            state.latest_moves.remove(peer_id);
            state.inbound_moves.remove(peer_id);
            Ok(())
        })
    }
}

impl MouseReceiver for InMemoryLink {
    fn try_recv_move(&self) -> Result<Option<ReceivedMove>, LinkError> {
        let mut state = try_lock(&self.state)?;
        let LinkState {
            inbound_moves,
            tokens,
            ..
        } = &mut *state;
        for (id, (_, movement)) in inbound_moves {
            if let Some(peer) = tokens.get(id).copied() {
                if let Some(movement) = movement.take() {
                    return Ok(Some(ReceivedMove { peer, movement }));
                }
            }
        }
        Ok(None)
    }
}

/// Explicitly insecure, deterministic peer manager for tests and UI mock mode.
///
/// It does not speak PAKE, pin certificates, enforce source IP rate limits,
/// persist identity, advertise via mDNS, or open sockets. Pairing checks model
/// only one-time code expiry, three-attempt lockout, and the two-confirmation
/// gate. Its verification phrase is fixed mock data and carries no identity
/// proof.
#[derive(Clone)]
pub struct InMemoryPeerManager {
    state: Arc<Mutex<ManagerState>>,
    event_tx: broadcast::Sender<PeerManagerEvent>,
}

struct ManagerState {
    next_code: u32,
    observed_now_ms: u64,
    local_pairing: Option<MockPairingCode>,
    reachable: HashMap<String, MockRemote>,
    discovered: HashMap<String, DiscoveredPeer>,
    paired: HashMap<String, Peer>,
    pending_pairing: Option<PendingPairing>,
}

struct MockRemote {
    peer: DiscoveredPeer,
    pairing_code: Option<MockPairingCode>,
    confirmation: Option<bool>,
}

struct PendingPairing {
    peer: Peer,
    expires_at_ms: u64,
    local_confirmation: bool,
    remote_confirmation: Option<bool>,
}

struct MockPairingCode {
    value: Zeroizing<String>,
    expires_at_ms: u64,
    failed_attempts: u8,
}

impl MockPairingCode {
    fn check(&mut self, candidate: &str, now_ms: u64) -> Result<(), ErrorCode> {
        if now_ms >= self.expires_at_ms {
            return Err(ErrorCode::CodeExpired);
        }
        if self.failed_attempts >= PAIRING_ATTEMPT_LIMIT {
            return Err(ErrorCode::LockedOut);
        }
        if candidate != self.value.as_str() {
            self.failed_attempts += 1;
            return Err(if self.failed_attempts >= PAIRING_ATTEMPT_LIMIT {
                ErrorCode::LockedOut
            } else {
                ErrorCode::BadCode
            });
        }
        Ok(())
    }
}

impl InMemoryPeerManager {
    /// Create an empty mock manager with no reachable peers or pairing code.
    pub fn new() -> Self {
        let (event_tx, _) = broadcast::channel(MOCK_QUEUE_CAPACITY);
        Self {
            state: Arc::new(Mutex::new(ManagerState {
                next_code: 1,
                observed_now_ms: 0,
                local_pairing: None,
                reachable: HashMap::new(),
                discovered: HashMap::new(),
                paired: HashMap::new(),
                pending_pairing: None,
            })),
            event_tx,
        }
    }

    /// Seed peers loaded from mock-mode configuration after daemon restart.
    pub fn seed_paired_peers(&self, peers: Vec<Peer>) {
        let mut state = lock(&self.state);
        for peer in peers {
            state.paired.insert(peer.device_id.clone(), peer);
        }
    }

    /// Return the mock manager's paired peers for in-memory daemon state.
    pub fn paired_peers(&self) -> Vec<Peer> {
        lock(&self.state).paired.values().cloned().collect()
    }

    /// Configure a reachable unpaired peer. No endpoint is contacted.
    pub fn script_reachable_peer(&self, peer: DiscoveredPeer) {
        let mut state = lock(&self.state);
        state
            .discovered
            .insert(peer.device_id.clone(), peer.clone());
        state.reachable.insert(
            peer.device_id.clone(),
            MockRemote {
                peer,
                pairing_code: None,
                confirmation: None,
            },
        );
    }

    /// Configure a discoverable peer that is not reachable for connection.
    pub fn script_discovered_peer(&self, peer: DiscoveredPeer) {
        lock(&self.state)
            .discovered
            .insert(peer.device_id.clone(), peer);
    }

    /// Configure the one-time code accepted for a scripted remote host.
    pub fn script_remote_pairing_code(
        &self,
        device_id: &str,
        code: impl Into<String>,
        expires_at_ms: u64,
    ) -> bool {
        let mut state = lock(&self.state);
        let Some(remote) = state.reachable.get_mut(device_id) else {
            return false;
        };
        remote.pairing_code = Some(MockPairingCode {
            value: Zeroizing::new(code.into()),
            expires_at_ms,
            failed_attempts: 0,
        });
        true
    }

    /// Configure the scripted remote user's confirmation. If a local user has
    /// already confirmed, an accept completes and pins the peer; a reject
    /// aborts the candidate.
    pub fn script_remote_confirmation(&self, device_id: &str, accepted: bool) -> bool {
        let now_ms = lock(&self.state).observed_now_ms;
        self.script_remote_confirmation_at(device_id, accepted, now_ms)
    }

    /// Script a remote answer at an explicit clock value for deterministic tests.
    pub fn script_remote_confirmation_at(
        &self,
        device_id: &str,
        accepted: bool,
        now_ms: u64,
    ) -> bool {
        let result = {
            let mut state = lock(&self.state);
            state.observed_now_ms = now_ms;
            let Some(remote) = state.reachable.get_mut(device_id) else {
                return false;
            };
            remote.confirmation = Some(accepted);
            if let Some(pending) = state
                .pending_pairing
                .as_mut()
                .filter(|pending| pending.peer.device_id == device_id)
            {
                pending.remote_confirmation = Some(accepted);
                Some(resolve_pending(&mut state, now_ms))
            } else {
                None
            }
        };
        match result {
            Some(Ok(Some(peer))) => {
                let _ = self.event_tx.send(PeerManagerEvent::Paired(peer));
            }
            Some(Err(error)) if error.code != ErrorCode::InvalidParams => {
                let _ = self.event_tx.send(PeerManagerEvent::PairingResult {
                    ok: false,
                    device_id: None,
                    error: Some(error.code),
                });
            }
            _ => {}
        }
        true
    }

    /// Emit a scripted incoming pairing request.
    pub fn script_incoming_pairing(&self, name: String, os: Os, address: String) {
        let _ = self
            .event_tx
            .send(PeerManagerEvent::PairingIncoming { name, os, address });
    }

    /// Publish a lifecycle or control event to the bounded mock event stream.
    ///
    /// Returns false when there is no active event receiver. Broadcast storage
    /// is capped at MOCK_QUEUE_CAPACITY and never waits for a receiver.
    pub fn script_peer_event(&self, event: PeerManagerEvent) -> bool {
        self.event_tx.send(event).is_ok()
    }
}

impl Default for InMemoryPeerManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PeerManager for InMemoryPeerManager {
    fn pair_host<'a>(&'a self, now_ms: u64) -> NetFuture<'a, Result<PairingHost, IpcError>> {
        Box::pin(async move {
            let mut state = lock(&self.state);
            if let Some(pairing) = state
                .local_pairing
                .as_ref()
                .filter(|pairing| now_ms < pairing.expires_at_ms)
            {
                return Ok(PairingHost {
                    code: pairing.value.clone(),
                    expires_at_ms: pairing.expires_at_ms,
                });
            }
            let code_number = state.next_code % 1_000_000;
            state.next_code = state.next_code.wrapping_add(1);
            let code = Zeroizing::new(format!("{code_number:06}"));
            let expires_at_ms = now_ms.saturating_add(PAIRING_CODE_LIFETIME_MS);
            state.local_pairing = Some(MockPairingCode {
                value: code.clone(),
                expires_at_ms,
                failed_attempts: 0,
            });
            Ok(PairingHost {
                code,
                expires_at_ms,
            })
        })
    }

    fn pair_join<'a>(
        &'a self,
        target: PairTarget,
        code: &'a str,
        now_ms: u64,
    ) -> NetFuture<'a, Result<PairingSession, IpcError>> {
        Box::pin(async move {
            let session = {
                let mut state = lock(&self.state);
                state.observed_now_ms = now_ms;
                if state
                    .pending_pairing
                    .as_ref()
                    .is_some_and(|pending| now_ms < pending.expires_at_ms)
                {
                    return Err(ipc_error(
                        ErrorCode::InvalidParams,
                        "another pairing verification is pending",
                    ));
                }
                state.pending_pairing = None;
                let remote = match target {
                    PairTarget::Address(address) => state
                        .reachable
                        .values_mut()
                        .find(|remote| remote.peer.address == address),
                    PairTarget::DeviceId(device_id) => state.reachable.get_mut(&device_id),
                };
                let remote = remote
                    .ok_or_else(|| ipc_error(ErrorCode::Unreachable, "peer is unreachable"))?;
                let pairing = remote.pairing_code.as_mut().ok_or_else(|| {
                    ipc_error(ErrorCode::Unreachable, "peer has no scripted pairing host")
                })?;
                if let Err(error) = pairing.check(code, now_ms) {
                    let message = pairing_error_message(&error);
                    return Err(ipc_error(error, message));
                }
                let expires_at_ms = now_ms.saturating_add(PAIRING_VERIFY_TIMEOUT_MS);
                let verification = PairingVerify {
                    phrase: ["mock", "test", "link"].map(str::to_owned),
                    peer: VerificationPeer {
                        name: remote.peer.name.clone(),
                        os: remote.peer.os,
                    },
                    expires_at_ms,
                };
                let peer = mock_peer(&remote.peer);
                let remote_confirmation = remote.confirmation;
                remote.pairing_code = None;
                let session = PairingSession {
                    peer: peer.clone(),
                    verification: verification.clone(),
                };
                state.pending_pairing = Some(PendingPairing {
                    peer,
                    expires_at_ms,
                    local_confirmation: false,
                    remote_confirmation,
                });
                session
            };
            let _ = self.event_tx.send(PeerManagerEvent::PairingVerify(
                session.verification.clone(),
            ));
            Ok(session)
        })
    }

    fn confirm_pairing(
        &self,
        accepted: bool,
        now_ms: u64,
    ) -> NetFuture<'_, Result<Option<Peer>, IpcError>> {
        Box::pin(async move {
            let result = {
                let mut state = lock(&self.state);
                state.observed_now_ms = now_ms;
                confirm_candidate(&mut state, accepted, now_ms)
            };
            if let Ok(Some(peer)) = &result {
                let _ = self.event_tx.send(PeerManagerEvent::Paired(peer.clone()));
            }
            result
        })
    }

    fn cancel_pair_host(&self) -> NetFuture<'_, Result<(), IpcError>> {
        Box::pin(async move {
            lock(&self.state).local_pairing = None;
            Ok(())
        })
    }

    fn discover(&self) -> NetFuture<'_, Result<Vec<DiscoveredPeer>, IpcError>> {
        Box::pin(async move {
            let peers = lock(&self.state)
                .discovered
                .values()
                .cloned()
                .collect::<Vec<_>>();
            for peer in &peers {
                let _ = self
                    .event_tx
                    .send(PeerManagerEvent::Discovered(peer.clone()));
            }
            Ok(peers)
        })
    }

    fn add_manual<'a>(
        &'a self,
        address: &'a str,
    ) -> NetFuture<'a, Result<DiscoveredPeer, IpcError>> {
        Box::pin(async move {
            lock(&self.state)
                .reachable
                .values()
                .find(|remote| remote.peer.address == address)
                .map(|remote| remote.peer.clone())
                .ok_or_else(|| ipc_error(ErrorCode::Unreachable, "peer is unreachable"))
        })
    }

    fn unpair<'a>(&'a self, device_id: &'a str) -> NetFuture<'a, Result<(), IpcError>> {
        Box::pin(async move {
            let peer = lock(&self.state)
                .paired
                .remove(device_id)
                .ok_or_else(|| ipc_error(ErrorCode::NotPaired, "peer is not paired"))?;
            let _ = self
                .event_tx
                .send(PeerManagerEvent::Unpaired(peer.device_id));
            Ok(())
        })
    }

    fn events(&self) -> broadcast::Receiver<PeerManagerEvent> {
        self.event_tx.subscribe()
    }
}

fn mock_peer(discovered: &DiscoveredPeer) -> Peer {
    Peer {
        device_id: discovered.device_id.clone(),
        name: discovered.name.clone(),
        os: discovered.os,
        fingerprint: format!("mock-only:{}", discovered.device_id),
        online: true,
        connection: Connection::Connected,
        address: Some(discovered.address.clone()),
        latency_ms: None,
        monitors: vec![Monitor {
            id: "mock-display-1".to_owned(),
            x: 0.0,
            y: 0.0,
            w: 1920.0,
            h: 1080.0,
            scale: 1.0,
            primary: true,
        }],
        clipboard_enabled: true,
        wake_mac: None,
        last_monitors: Vec::new(),
        app_version: None,
        model: None,
    }
}

fn ipc_error(code: ErrorCode, message: &str) -> IpcError {
    IpcError {
        code,
        message: message.to_owned(),
    }
}

fn pairing_error_message(code: &ErrorCode) -> &'static str {
    match code {
        ErrorCode::BadCode => "pairing code is incorrect",
        ErrorCode::CodeExpired => "pairing code expired",
        ErrorCode::LockedOut => "pairing code locked after three attempts",
        _ => "pairing failed",
    }
}

fn confirm_candidate(
    state: &mut ManagerState,
    accepted: bool,
    now_ms: u64,
) -> Result<Option<Peer>, IpcError> {
    let Some(expires_at_ms) = state
        .pending_pairing
        .as_ref()
        .map(|pending| pending.expires_at_ms)
    else {
        return Err(ipc_error(
            ErrorCode::InvalidParams,
            "no pairing verification is pending",
        ));
    };
    if now_ms >= expires_at_ms {
        state.pending_pairing = None;
        return Err(ipc_error(
            ErrorCode::CodeExpired,
            "pairing verification expired",
        ));
    }
    if !accepted {
        state.pending_pairing = None;
        return Err(ipc_error(ErrorCode::PermissionDenied, "pairing rejected"));
    }
    if let Some(pending) = state.pending_pairing.as_mut() {
        pending.local_confirmation = true;
    }
    resolve_pending(state, now_ms)
}

fn resolve_pending(state: &mut ManagerState, now_ms: u64) -> Result<Option<Peer>, IpcError> {
    let Some((expires_at_ms, local_confirmation, remote_confirmation)) =
        state.pending_pairing.as_ref().map(|pending| {
            (
                pending.expires_at_ms,
                pending.local_confirmation,
                pending.remote_confirmation,
            )
        })
    else {
        return Err(ipc_error(
            ErrorCode::InvalidParams,
            "no pairing verification is pending",
        ));
    };
    if now_ms >= expires_at_ms {
        state.pending_pairing = None;
        return Err(ipc_error(
            ErrorCode::CodeExpired,
            "pairing verification expired",
        ));
    }
    if remote_confirmation == Some(false) {
        state.pending_pairing = None;
        return Err(ipc_error(
            ErrorCode::PermissionDenied,
            "remote user rejected pairing",
        ));
    }
    if !local_confirmation || remote_confirmation != Some(true) {
        return Ok(None);
    }
    let Some(pending) = state.pending_pairing.take() else {
        return Err(ipc_error(
            ErrorCode::InvalidParams,
            "no pairing verification is pending",
        ));
    };
    state
        .paired
        .insert(pending.peer.device_id.clone(), pending.peer.clone());
    Ok(Some(pending.peer))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn try_lock<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>, LinkError> {
    match mutex.try_lock() {
        Ok(guard) => Ok(guard),
        Err(std::sync::TryLockError::Poisoned(error)) => Ok(error.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => Err(LinkError::Busy),
    }
}

fn take_inbound_move(state: &Mutex<LinkState>) -> Option<(String, Move)> {
    lock(state)
        .inbound_moves
        .iter_mut()
        .find_map(|(peer_id, (_, pending))| {
            pending.take().map(|movement| (peer_id.clone(), movement))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn discovered() -> DiscoveredPeer {
        DiscoveredPeer {
            device_id: "peer-1".to_owned(),
            name: "Mock peer".to_owned(),
            os: Os::Windows,
            address: "127.0.0.1:24800".to_owned(),
        }
    }

    #[tokio::test]
    async fn mock_pairing_checks_code_expiry_and_three_attempt_lockout() {
        let manager = InMemoryPeerManager::new();
        manager.script_reachable_peer(discovered());
        assert!(manager.script_remote_pairing_code("peer-1", "123456", 121_000));

        let wrong = manager
            .pair_join(PairTarget::DeviceId("peer-1".into()), "000000", 1_000)
            .await
            .expect_err("wrong code should fail");
        assert_eq!(wrong.code, ErrorCode::BadCode);
        let wrong = manager
            .pair_join(PairTarget::DeviceId("peer-1".into()), "000000", 1_001)
            .await
            .expect_err("second wrong code should fail");
        assert_eq!(wrong.code, ErrorCode::BadCode);
        let wrong = manager
            .pair_join(PairTarget::DeviceId("peer-1".into()), "000000", 1_002)
            .await
            .expect_err("third wrong code should lock the code");
        assert_eq!(wrong.code, ErrorCode::LockedOut);
        let locked = manager
            .pair_join(PairTarget::DeviceId("peer-1".into()), "123456", 1_003)
            .await
            .expect_err("locked code must remain burned");
        assert_eq!(locked.code, ErrorCode::LockedOut);

        manager.script_remote_pairing_code("peer-1", "123456", 2_000);
        let expired = manager
            .pair_join(PairTarget::DeviceId("peer-1".into()), "123456", 2_000)
            .await
            .expect_err("code at its expiry instant must be expired");
        assert_eq!(expired.code, ErrorCode::CodeExpired);
    }

    #[tokio::test]
    async fn mock_pairing_pins_only_after_both_confirmations() {
        let manager = InMemoryPeerManager::new();
        let mut events = manager.events();
        manager.script_reachable_peer(discovered());
        assert!(manager.script_remote_pairing_code("peer-1", "123456", 121_000));

        let session = manager
            .pair_join(
                PairTarget::Address("127.0.0.1:24800".into()),
                "123456",
                1_000,
            )
            .await
            .expect("valid code should start verification");
        let peer = session.peer;
        assert_eq!(peer.device_id, "peer-1");
        assert!(peer.fingerprint.starts_with("mock-only:"));
        assert_eq!(peer.monitors[0].w, 1920.0);
        assert!(matches!(
            events.recv().await,
            Ok(PeerManagerEvent::PairingVerify(_))
        ));
        assert!(manager.paired_peers().is_empty());
        let concurrent = manager
            .pair_join(PairTarget::DeviceId("peer-1".into()), "123456", 1_000)
            .await
            .expect_err("a second verification must be rejected");
        assert_eq!(concurrent.code, ErrorCode::InvalidParams);
        assert_eq!(
            manager
                .confirm_pairing(true, 1_001)
                .await
                .expect("local confirmation should wait for remote"),
            None
        );
        assert!(manager.paired_peers().is_empty());
        assert!(manager.script_remote_confirmation("peer-1", true));
        assert!(matches!(
            events.recv().await,
            Ok(PeerManagerEvent::Paired(paired)) if paired.device_id == "peer-1"
        ));
        assert_eq!(manager.paired_peers().len(), 1);
        manager
            .unpair("peer-1")
            .await
            .expect("paired peer should be removable");
        assert!(manager.paired_peers().is_empty());
    }

    #[tokio::test]
    async fn scripted_remote_confirmation_can_precede_the_local_confirmation() {
        let manager = InMemoryPeerManager::new();
        manager.script_reachable_peer(discovered());
        assert!(manager.script_remote_pairing_code("peer-1", "123456", 121_000));
        assert!(manager.script_remote_confirmation("peer-1", true));
        manager
            .pair_join(PairTarget::DeviceId("peer-1".into()), "123456", 1_000)
            .await
            .expect("valid code should start verification");
        assert!(manager.paired_peers().is_empty());

        let completed = manager
            .confirm_pairing(true, 1_001)
            .await
            .expect("both confirmations should finish pairing")
            .expect("completed pairing returns the pinned peer");
        assert_eq!(completed.device_id, "peer-1");
        assert_eq!(manager.paired_peers().len(), 1);
    }

    #[tokio::test]
    async fn pairing_rejection_and_verification_timeout_never_pin() {
        let manager = InMemoryPeerManager::new();
        manager.script_reachable_peer(discovered());
        assert!(manager.script_remote_pairing_code("peer-1", "123456", 121_000));

        manager
            .pair_join(PairTarget::DeviceId("peer-1".into()), "123456", 1_000)
            .await
            .expect("valid code should start verification");
        let rejected = manager
            .confirm_pairing(false, 1_001)
            .await
            .expect_err("a local rejection aborts pairing");
        assert_eq!(rejected.code, ErrorCode::PermissionDenied);
        assert!(manager.paired_peers().is_empty());

        assert!(manager.script_remote_pairing_code("peer-1", "654321", 122_000));
        let session = manager
            .pair_join(PairTarget::DeviceId("peer-1".into()), "654321", 2_000)
            .await
            .expect("next code should start a verification");
        assert_eq!(session.verification.expires_at_ms, 62_000);
        let timeout = manager
            .confirm_pairing(true, 62_000)
            .await
            .expect_err("verification expires at its deadline");
        assert_eq!(timeout.code, ErrorCode::CodeExpired);
        assert!(manager.paired_peers().is_empty());
    }

    #[tokio::test]
    async fn remote_rejection_aborts_an_accepted_local_candidate() {
        let manager = InMemoryPeerManager::new();
        let mut events = manager.events();
        manager.script_reachable_peer(discovered());
        assert!(manager.script_remote_pairing_code("peer-1", "123456", 121_000));
        manager
            .pair_join(PairTarget::DeviceId("peer-1".into()), "123456", 1_000)
            .await
            .expect("valid code should start verification");

        assert_eq!(
            manager
                .confirm_pairing(true, 1_001)
                .await
                .expect("waiting for remote confirmation"),
            None
        );
        assert!(manager.script_remote_confirmation("peer-1", false));
        assert!(manager.paired_peers().is_empty());
        assert!(matches!(
            events.recv().await,
            Ok(PeerManagerEvent::PairingVerify(_))
        ));
        assert!(matches!(
            events.recv().await,
            Ok(PeerManagerEvent::PairingResult {
                ok: false,
                error: Some(ErrorCode::PermissionDenied),
                ..
            })
        ));
    }

    #[tokio::test]
    async fn link_uses_a_bounded_latest_wins_move_slot() {
        let link = InMemoryLink::new();
        link.script_reachable("127.0.0.1:24800", "peer-1");
        link.connect("127.0.0.1:24800", Some("peer-1"))
            .await
            .expect("scripted peer should connect");
        link.send_datagram(
            "peer-1",
            Move {
                seq: 1,
                x: 1.0,
                y: 2.0,
            },
        )
        .expect("connected peer should accept a move");
        link.send_datagram(
            "peer-1",
            Move {
                seq: 2,
                x: 3.0,
                y: 4.0,
            },
        )
        .expect("new move replaces stale pending position");
        assert_eq!(
            link.latest_move("peer-1"),
            Some(Move {
                seq: 2,
                x: 3.0,
                y: 4.0
            })
        );
        link.send_datagram(
            "peer-1",
            Move {
                seq: 1,
                x: 9.0,
                y: 9.0,
            },
        )
        .expect("stale positions should be dropped");
        assert_eq!(
            link.latest_move("peer-1"),
            Some(Move {
                seq: 2,
                x: 3.0,
                y: 4.0
            })
        );
        assert_eq!(
            link.send_datagram(
                "missing",
                Move {
                    seq: 3,
                    x: 0.0,
                    y: 0.0
                }
            ),
            Err(LinkError::NotConnected)
        );
    }

    #[tokio::test]
    async fn inbound_datagrams_coalesce_and_drop_stale_sequences() {
        let link = InMemoryLink::new();
        link.script_reachable("127.0.0.1:24800", "peer-1");
        link.connect("127.0.0.1:24800", Some("peer-1"))
            .await
            .expect("scripted peer should connect");
        for movement in [
            Move {
                seq: 4,
                x: 1.0,
                y: 2.0,
            },
            Move {
                seq: 5,
                x: 3.0,
                y: 4.0,
            },
        ] {
            link.script_event(LinkEvent::Move {
                peer_id: "peer-1".to_owned(),
                movement,
            })
            .expect("connected peer should enqueue latest position");
        }
        let Ok(LinkEvent::Move { movement, .. }) = link.recv().await else {
            panic!("receive should return the latest move");
        };
        assert_eq!(movement.seq, 5);
        link.script_event(LinkEvent::Move {
            peer_id: "peer-1".to_owned(),
            movement: Move {
                seq: 4,
                x: 9.0,
                y: 9.0,
            },
        })
        .expect("stale incoming moves should be dropped");
        assert!(take_inbound_move(&link.state).is_none());
    }
}
