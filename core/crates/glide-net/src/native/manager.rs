use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, SocketAddr, UdpSocket},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, Weak,
    },
    time::{Duration, Instant},
};

use glide_proto::{
    ipc::{
        Connection as PeerState, DiscoveredPeer, ErrorCode, IpcError, PairingVerify, Peer,
        VerificationPeer,
    },
    wire,
};
use quinn::{Connection, Endpoint};
use ring::rand::{SecureRandom, SystemRandom};
use tokio::sync::{broadcast, mpsc, Semaphore};
use zeroize::Zeroizing;

use super::{
    discovery::{Discovery, DiscoveryUpdate},
    identity::NativeIdentity,
    ipc,
    link::{timed, NativeLink},
    mutex,
    pairing::{self, AuthenticatedPairing, DecisionStage, PairHello},
    tls, transport_config, unix_ms, NativeConfig, NativeError, PinSet, MAX_PEERS, PAIR_ALPN,
};

#[cfg(windows)]
const UDP_RECEIVE_BUFFER_BYTES: i32 = 2 * 1024 * 1024;

fn open_endpoint(
    server: quinn::ServerConfig,
    bind_addr: SocketAddr,
) -> Result<Endpoint, NativeError> {
    let socket = UdpSocket::bind(bind_addr)?;
    #[cfg(windows)]
    configure_udp_receive_buffer(&socket)?;
    Ok(Endpoint::new(
        quinn::EndpointConfig::default(),
        Some(server),
        socket,
        Arc::new(quinn::TokioRuntime),
    )?)
}

#[cfg(windows)]
fn configure_udp_receive_buffer(socket: &UdpSocket) -> std::io::Result<()> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{
        setsockopt, WSAGetLastError, SOCKET, SOCKET_ERROR, SOL_SOCKET, SO_RCVBUF,
    };

    // At 200 MiB/s the default 64 KiB socket buffer holds less than 1 ms of
    // traffic. Bound this socket at 2 MiB to absorb brief receive-scheduling
    // bursts while keeping per-daemon kernel buffering finite.
    // SAFETY: `value` is a live 32-bit integer for the documented SO_RCVBUF
    // option, and `socket` owns the valid Winsock handle for this call.
    let result = unsafe {
        let value = UDP_RECEIVE_BUFFER_BYTES;
        setsockopt(
            socket.as_raw_socket() as SOCKET,
            SOL_SOCKET,
            SO_RCVBUF,
            (&value as *const i32).cast(),
            std::mem::size_of::<i32>() as i32,
        )
    };
    if result == SOCKET_ERROR {
        // SAFETY: WSAGetLastError is the required error source immediately after
        // a failed Winsock call on this thread.
        return Err(std::io::Error::from_raw_os_error(unsafe {
            WSAGetLastError()
        }));
    }
    Ok(())
}

#[cfg(all(test, windows))]
fn udp_receive_buffer_size(socket: &UdpSocket) -> std::io::Result<usize> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{
        getsockopt, WSAGetLastError, SOCKET, SOCKET_ERROR, SOL_SOCKET, SO_RCVBUF,
    };

    let mut value = 0i32;
    let mut length = std::mem::size_of::<i32>() as i32;
    // SAFETY: `value` and `length` are writable buffers with the required
    // sizes, and `socket` owns the valid Winsock handle for this call.
    let result = unsafe {
        getsockopt(
            socket.as_raw_socket() as SOCKET,
            SOL_SOCKET,
            SO_RCVBUF,
            (&mut value as *mut i32).cast(),
            &mut length,
        )
    };
    if result == SOCKET_ERROR {
        // SAFETY: WSAGetLastError is the required error source immediately after
        // a failed Winsock call on this thread.
        return Err(std::io::Error::from_raw_os_error(unsafe {
            WSAGetLastError()
        }));
    }
    usize::try_from(value)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "negative SO_RCVBUF"))
}
use crate::{NetFuture, PairTarget, PairingHost, PairingSession, PeerManager, PeerManagerEvent};

const HOST_LIFETIME: Duration = Duration::from_secs(120);
const VERIFY_LIFETIME: Duration = Duration::from_secs(60);

#[cfg(test)]
mod recovery_tests;

/// Owns native identity, pinned peers, discovery and the QUIC listener.
///
/// ```no_run
/// # async fn start(config: glide_net::NativeConfig) -> Result<(), glide_net::NativeError> {
/// let manager = glide_net::NativePeerManager::new(config).await?;
/// let link = manager.link(); // install both in the daemon using its existing seam
/// # let _ = link;
/// # Ok(()) }
/// ```
///
/// Keep the manager alive while using its link. Dropping it closes every session.
/// Keystore absence is an initialization error; no plaintext identity fallback exists.
/// During a pairing host window only pairing handshakes are admitted on the same
/// port. Existing authenticated normal sessions continue; new normal dials retry.
pub struct NativePeerManager {
    shared: Arc<Shared>,
}

/// Explicit repair result. No key in this report was made trusted by repair.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct PairingRepair {
    pub removed_files: Vec<String>,
    pub removed_peer_ids: Vec<String>,
}

struct Shared {
    config: NativeConfig,
    identity: Arc<NativeIdentity>,
    pins: PinSet,
    link: NativeLink,
    pairing_open: Arc<AtomicBool>,
    host: Mutex<Option<HostCode>>,
    rates: Mutex<HashMap<IpAddr, Rate>>,
    pending: Mutex<Option<Pending>>,
    next_pairing: AtomicU64,
    paired: Mutex<HashMap<String, Peer>>,
    revoked: Mutex<HashSet<String>>,
    revocation_version: AtomicU64,
    discovered: Mutex<HashMap<String, DiscoveredPeer>>,
    store_lock: tokio::sync::Mutex<()>,
    event_tx: broadcast::Sender<PeerManagerEvent>,
    discovery: Mutex<Option<Discovery>>,
    discovery_enabled: AtomicBool,
    discovery_generation: AtomicU64,
    inbound_limit: Arc<Semaphore>,
    #[cfg(test)]
    verification_lifetime_ms: AtomicU64,
}

struct HostCode {
    id: u64,
    code: Zeroizing<String>,
    expires_at_ms: u64,
    deadline: Instant,
    failed: u8,
    busy: bool,
}

struct Rate {
    start: Instant,
    attempts: u8,
}

struct Pending {
    id: u64,
    expires_at_ms: u64,
    deadline: Instant,
    decision: mpsc::Sender<bool>,
    result: Arc<Mutex<Option<Result<Peer, IpcError>>>>,
    conn: Connection,
}

impl Drop for NativePeerManager {
    fn drop(&mut self) {
        self.shared.pairing_open.store(false, Ordering::Release);
        self.shared.link.0.endpoint.close(0u32.into(), b"shutdown");
        if let Ok(mut discovery) = self.shared.discovery.lock() {
            discovery.take();
        }
    }
}

impl NativePeerManager {
    /// Call on a blocking worker before creating a manager, with exclusive ownership
    /// of its data directory. Discard staged trust; never replay/activate a pin.
    /// Ambiguous partial journals conservatively discard all stored pins.
    pub fn repair_unfinished_pairing(data_dir: &Path) -> Result<PairingRepair, NativeError> {
        repair_unfinished_pairing(data_dir)
    }
    /// Load/create an OS-keystore identity and initialize the native transport.
    /// This must run inside a Tokio runtime. No mDNS socket opens when disabled.
    pub async fn new(config: NativeConfig) -> Result<Self, NativeError> {
        let data_dir = config.data_dir.clone();
        let identity =
            tokio::task::spawn_blocking(move || NativeIdentity::load_or_create(&data_dir))
                .await
                .map_err(|_| NativeError::Internal("identity worker failed".into()))??;
        Self::initialize(config, identity).await
    }

    /// Test-only keystore injection; the full PAKE/SAS/pinned trust path remains.
    #[cfg(feature = "test-support")]
    pub async fn with_test_keystore(
        config: NativeConfig,
        store: &super::TestKeyStore,
    ) -> Result<Self, NativeError> {
        let identity = store.load_or_create(&config.data_dir)?;
        Self::initialize(config, identity).await
    }

    async fn initialize(
        config: NativeConfig,
        identity: NativeIdentity,
    ) -> Result<Self, NativeError> {
        let hello = config.hello(&identity)?;
        let identity = Arc::new(identity);
        let path = config.data_dir.join("network-peers.json");
        let (mut peers, revoked) = tokio::task::spawn_blocking(move || {
            let pending = path.with_file_name("pairing-pending.json");
            if pending.try_exists()?
                || pending.with_extension("json.tmp").try_exists()?
                || path.with_file_name("pairing-repair.json").try_exists()?
                || path
                    .with_file_name("pairing-repair.json.tmp")
                    .try_exists()?
            {
                return Err(NativeError::Security(
                    "unfinished pairing transaction preserved; storage repair required".into(),
                ));
            }
            let revoked = read_revocations(&path.with_file_name("revoked-peers.json"))?;
            Ok::<_, NativeError>((read_peers(&path)?, revoked))
        })
        .await
        .map_err(|_| NativeError::Internal("pin storage worker failed".into()))??;
        let pins = PinSet::new();
        peers.retain(|id, _| !revoked.contains(id));
        for peer in peers.values() {
            pins.insert(&peer.device_id)?;
        }
        let pairing_open = Arc::new(AtomicBool::new(false));
        let mut normal = tls::dispatch_server(&identity, pins.clone(), pairing_open.clone())?;
        normal
            .max_incoming(MAX_PEERS)
            .incoming_buffer_size(16 * 1024)
            .incoming_buffer_size_total((MAX_PEERS * 16 * 1024) as u64);
        normal.transport_config(transport_config());
        let endpoint = open_endpoint(normal, config.bind_addr)?;
        let (event_tx, _) = broadcast::channel(128);
        let link = NativeLink::new(
            endpoint,
            identity.clone(),
            pins.clone(),
            hello,
            event_tx.clone(),
        );
        let shared = Arc::new(Shared {
            discovery_enabled: AtomicBool::new(config.discovery),
            discovery_generation: AtomicU64::new(0),
            config,
            identity,
            pins,
            link,
            pairing_open,
            host: Mutex::new(None),
            rates: Mutex::new(HashMap::new()),
            pending: Mutex::new(None),
            next_pairing: AtomicU64::new(1),
            paired: Mutex::new(peers),
            revoked: Mutex::new(revoked),
            revocation_version: AtomicU64::new(0),
            discovered: Mutex::new(HashMap::new()),
            store_lock: tokio::sync::Mutex::new(()),
            event_tx,
            discovery: Mutex::new(None),
            inbound_limit: Arc::new(Semaphore::new(MAX_PEERS)),
            #[cfg(test)]
            verification_lifetime_ms: AtomicU64::new(VERIFY_LIFETIME.as_millis() as u64),
        });
        spawn_listener(&shared);
        spawn_reconnect(&shared);
        let manager = Self { shared };
        if manager.shared.config.discovery {
            manager.start_discovery()?;
        }
        Ok(manager)
    }

    #[cfg(test)]
    pub(crate) async fn for_test(config: NativeConfig) -> Result<Self, NativeError> {
        Self::initialize(config, NativeIdentity::ephemeral()?).await
    }

    #[cfg(test)]
    pub(crate) fn pin_for_test(&self, id: &str) -> Result<(), NativeError> {
        self.shared.pins.insert(id)
    }

    #[cfg(test)]
    pub(crate) fn expire_host_for_test(&self) {
        if let Ok(mut host) = self.shared.host.lock() {
            if let Some(host) = host.as_mut() {
                host.deadline = Instant::now();
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn expire_verification_for_test(&self) {
        if let Ok(mut pending) = self.shared.pending.lock() {
            if let Some(pending) = pending.as_mut() {
                pending.deadline = Instant::now();
            }
        }
    }

    #[cfg(test)]
    pub(super) async fn pairing_connection_for_test(
        &self,
        address: &str,
    ) -> Result<(Connection, PairHello, u8), IpcError> {
        self.pairing_connect(address, 1).await
    }

    #[cfg(test)]
    pub(super) fn verification_lifetime_for_test(&self, value: Duration) {
        self.shared
            .verification_lifetime_ms
            .store(value.as_millis() as u64, Ordering::Relaxed);
    }

    /// Shared pinned link; InMemory remains a separate explicit testing backend.
    pub fn link(&self) -> NativeLink {
        self.shared.link.clone()
    }

    /// Public identity metadata. Private bytes cannot be extracted through this manager.
    pub fn identity(&self) -> &NativeIdentity {
        &self.shared.identity
    }

    /// UDP port actually bound (including ephemeral loopback port zero).
    pub fn local_addr(&self) -> Result<SocketAddr, NativeError> {
        Ok(self.shared.link.0.endpoint.local_addr()?)
    }

    /// Update validated Hello/reconnect, advertisement and authenticated peers.
    /// A saturated peer is closed and receives the current snapshot on reconnect.
    pub fn update_local_metadata(
        &self,
        name: String,
        monitors: Vec<glide_platform::Monitor>,
    ) -> Result<(), NativeError> {
        let mut config = self.shared.config.clone();
        config.name = name;
        config.monitors = monitors;
        let hello = config.hello(&self.shared.identity)?;
        let handle = self
            .shared
            .discovery
            .lock()
            .map_err(|_| NativeError::Security("discovery state poisoned".into()))?;
        let mut current = self
            .shared
            .link
            .0
            .hello
            .lock()
            .map_err(|_| NativeError::Security("metadata state poisoned".into()))?;
        if let Some(discovery) = handle.as_ref() {
            discovery.update(
                self.shared.identity.device_id(),
                &hello.name,
                hello.os,
                self.local_addr()?.port(),
            )?;
        }
        *current = hello.clone();
        self.shared
            .link
            .broadcast_metadata(wire::MetadataUpdate {
                version: wire::PROTOCOL_VERSION,
                name: hello.name,
                monitors: hello.monitors,
            })
            .map_err(|_| NativeError::Security("metadata publication failed".into()))
    }

    /// Port rebinding is deliberately unsupported; restarting preserves pins.
    pub fn update_listen_port(&self, port: u16) -> Result<(), NativeError> {
        if self.local_addr()?.port() == port {
            Ok(())
        } else {
            Err(NativeError::Internal(
                "live port changes require restart".into(),
            ))
        }
    }

    /// Persisted paired snapshots; they remain offline until a pinned session exists.
    pub fn paired_peers(&self) -> Result<Vec<Peer>, NativeError> {
        self.shared
            .paired
            .lock()
            .map(|p| p.values().cloned().collect())
            .map_err(|_| NativeError::Security("pin state poisoned".into()))
    }

    /// Apply `settings.network.discovery` without changing any trusted pin.
    pub fn set_discovery(&self, enabled: bool) -> Result<(), NativeError> {
        self.shared
            .discovery_enabled
            .store(enabled, Ordering::Release);
        if enabled {
            self.start_discovery()
        } else {
            let mut handle = self
                .shared
                .discovery
                .lock()
                .map_err(|_| NativeError::Internal("discovery state poisoned".into()))?;
            self.shared
                .discovery_generation
                .fetch_add(1, Ordering::AcqRel);
            handle.take();
            if let Ok(mut discovered) = self.shared.discovered.lock() {
                for id in discovered.keys() {
                    if !self.shared.pins.contains(id) {
                        let _ = self
                            .shared
                            .event_tx
                            .send(PeerManagerEvent::DiscoveryRemoved {
                                device_id: id.clone(),
                            });
                    }
                }
                discovered.clear();
            }
            Ok(())
        }
    }

    fn start_discovery(&self) -> Result<(), NativeError> {
        let mut handle = self
            .shared
            .discovery
            .lock()
            .map_err(|_| NativeError::Internal("discovery state poisoned".into()))?;
        if handle.is_some() || !self.shared.discovery_enabled.load(Ordering::Acquire) {
            return Ok(());
        }
        let (tx, mut rx) = mpsc::channel(MAX_PEERS);
        let generation = self.shared.discovery_generation.load(Ordering::Acquire);
        *handle = Some(Discovery::start(
            self.shared.identity.device_id(),
            &self
                .shared
                .link
                .0
                .hello
                .lock()
                .map_err(|_| NativeError::Security("metadata state poisoned".into()))?
                .name,
            self.shared.config.os,
            self.local_addr()?.port(),
            tx,
        )?);
        let shared = Arc::downgrade(&self.shared);
        tokio::spawn(async move {
            while let Some(update) = rx.recv().await {
                let Some(shared) = shared.upgrade() else {
                    break;
                };
                if !shared.discovery_enabled.load(Ordering::Acquire) {
                    continue;
                }
                match update {
                    DiscoveryUpdate::Found(peer) => {
                        if peer.device_id == shared.identity.device_id() {
                            continue;
                        }
                        if let Ok(mut found) = shared.discovered.lock() {
                            if !shared.discovery_enabled.load(Ordering::Acquire)
                                || shared.discovery_generation.load(Ordering::Acquire) != generation
                            {
                                break;
                            }
                            if found.len() < MAX_PEERS || found.contains_key(&peer.device_id) {
                                found.insert(peer.device_id.clone(), peer.clone());
                                if !shared.pins.contains(&peer.device_id) {
                                    let _ =
                                        shared.event_tx.send(PeerManagerEvent::Discovered(peer));
                                }
                            }
                        }
                    }
                    DiscoveryUpdate::Removed(id) => {
                        if let Ok(mut found) = shared.discovered.lock() {
                            if !shared.discovery_enabled.load(Ordering::Acquire)
                                || shared.discovery_generation.load(Ordering::Acquire) != generation
                            {
                                break;
                            }
                            found.remove(&id);
                        }
                        if !shared.pins.contains(&id) {
                            let _ = shared.event_tx.send(PeerManagerEvent::DiscoveryRemoved {
                                device_id: id.clone(),
                            });
                        }
                        // mDNS loss alone must not terminate a still-authenticated active QUIC session.
                        if shared.pins.contains(&id) && !shared.link.connected(&id) {
                            let _ = shared
                                .event_tx
                                .send(PeerManagerEvent::Disappeared { device_id: id });
                        }
                    }
                }
            }
        });
        Ok(())
    }

    async fn pairing_connect(
        &self,
        address: &str,
        operation: u8,
    ) -> Result<(Connection, PairHello, u8), IpcError> {
        if address.len() > 512 {
            return Err(ipc(ErrorCode::InvalidParams, "address is too long"));
        }
        let address = timed(tokio::net::lookup_host(address))
            .await
            .map_err(|_| ipc(ErrorCode::Unreachable, "peer is unreachable"))?
            .next()
            .ok_or_else(|| ipc(ErrorCode::Unreachable, "peer is unreachable"))?;
        let mut config = tls::pairing_client(&self.shared.identity)
            .map_err(|_| ipc(ErrorCode::Internal, "pairing TLS configuration failed"))?;
        config.transport_config(transport_config());
        let connecting = self
            .shared
            .link
            .0
            .endpoint
            .connect_with(config, address, "glide.local")
            .map_err(|_| ipc(ErrorCode::Unreachable, "peer is unreachable"))?;
        let conn = timed(connecting)
            .await
            .map_err(|_| ipc(ErrorCode::Unreachable, "peer is not accepting pairing"))?;
        let result = async {
            let (mut send, mut recv) = timed(conn.open_bi())
                .await
                .map_err(|_| ipc(ErrorCode::Unreachable, "pairing hello failed"))?;
            timed(send.write_all(&[operation]))
                .await
                .map_err(|_| ipc(ErrorCode::Unreachable, "pairing hello failed"))?;
            send.finish()
                .map_err(|_| ipc(ErrorCode::Unreachable, "pairing hello failed"))?;
            let response: ProbeReply = read_json(&mut recv).await?;
            if let Some(error) = response.error {
                return Err(error);
            }
            let hello = response
                .hello
                .ok_or_else(|| ipc(ErrorCode::InvalidParams, "missing peer hello"))?;
            validate_hello(&hello)?;
            let id = tls::peer_device_id(&conn)
                .map_err(|_| ipc(ErrorCode::PermissionDenied, "invalid peer identity"))?;
            if hello.device_id != id {
                return Err(ipc(
                    ErrorCode::PermissionDenied,
                    "peer hello identity mismatch",
                ));
            }
            Ok((hello, response.attempts_remaining))
        }
        .await;
        match result {
            Ok((hello, attempts)) => Ok((conn, hello, attempts)),
            Err(error) => {
                conn.close(1u32.into(), b"pairing rejected");
                Err(error)
            }
        }
    }
}

impl PeerManager for NativePeerManager {
    fn pair_host<'a>(&'a self, now_ms: u64) -> NetFuture<'a, Result<PairingHost, IpcError>> {
        Box::pin(async move {
            if mutex(&self.shared.pending).map_err(state_error)?.is_some() {
                return Err(ipc(
                    ErrorCode::InvalidParams,
                    "pairing verification already pending",
                ));
            }
            let mut host = mutex(&self.shared.host).map_err(state_error)?;
            if let Some(code) = host
                .as_ref()
                .filter(|h| h.deadline > Instant::now() && now_ms < h.expires_at_ms && h.failed < 3)
            {
                return Ok(PairingHost {
                    code: code.code.clone(),
                    expires_at_ms: code.expires_at_ms,
                });
            }
            let code = Zeroizing::new(random_code()?);
            let expires_at_ms = now_ms.saturating_add(HOST_LIFETIME.as_millis() as u64);
            let deadline = Instant::now() + HOST_LIFETIME;
            let id = self.shared.next_pairing.fetch_add(1, Ordering::Relaxed);
            *host = Some(HostCode {
                id,
                code: code.clone(),
                expires_at_ms,
                deadline,
                failed: 0,
                busy: false,
            });
            self.shared.pairing_open.store(true, Ordering::Release);
            let owner = Arc::downgrade(&self.shared);
            tokio::spawn(async move {
                tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
                let Some(owner) = owner.upgrade() else {
                    return;
                };
                if let Ok(mut host) = owner.host.lock() {
                    if host.as_ref().is_some_and(|h| h.id == id) {
                        host.take();
                        owner.pairing_open.store(false, Ordering::Release);
                    }
                };
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
            if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
                return Err(ipc(
                    ErrorCode::InvalidParams,
                    "pairing code must contain six digits",
                ));
            }
            if mutex(&self.shared.pending).map_err(state_error)?.is_some() {
                return Err(ipc(
                    ErrorCode::InvalidParams,
                    "pairing verification already pending",
                ));
            }
            let (address, expected) = match target {
                PairTarget::Address(address) => (address, None),
                PairTarget::DeviceId(id) => {
                    let peer = mutex(&self.shared.discovered)
                        .map_err(state_error)?
                        .get(&id)
                        .cloned()
                        .ok_or_else(|| ipc(ErrorCode::Unreachable, "peer is not discovered"))?;
                    (peer.address, Some(id))
                }
            };
            let (conn, hello, attempts_remaining) = self.pairing_connect(&address, 1).await?;
            if hello.device_id == self.shared.identity.device_id()
                || self.shared.pins.contains(&hello.device_id)
            {
                conn.close(1u32.into(), b"already paired or self identity");
                return Err(ipc(
                    ErrorCode::InvalidParams,
                    "peer is already paired or has this device's identity",
                ));
            }
            if expected.is_some_and(|id| id != hello.device_id) {
                conn.close(1u32.into(), b"identity mismatch");
                return Err(ipc(
                    ErrorCode::PermissionDenied,
                    "discovery identity mismatch",
                ));
            }
            let local = self.shared.local_hello()?;
            let result = tokio::time::timeout(
                super::IO_TIMEOUT,
                pairing::pair_exchange(
                    &conn,
                    false,
                    Zeroizing::new(code.to_owned()),
                    local,
                    self.shared.identity.device_id(),
                    &hello.device_id,
                ),
            )
            .await
            .unwrap_or_else(|_| Err(ipc(ErrorCode::Unreachable, "pairing exchange timed out")));
            match result {
                Ok(pairing) => prepare_pending(&self.shared, conn, pairing, false, now_ms),
                Err(mut error) => {
                    if attempts_remaining == 1 && error.code == ErrorCode::BadCode {
                        error = ipc(
                            ErrorCode::LockedOut,
                            "pairing code burned after three incorrect attempts",
                        );
                    }
                    conn.close(1u32.into(), b"pairing failed");
                    Err(error)
                }
            }
        })
    }

    fn confirm_pairing(
        &self,
        accepted: bool,
        now_ms: u64,
    ) -> NetFuture<'_, Result<Option<Peer>, IpcError>> {
        Box::pin(async move {
            let mut state = mutex(&self.shared.pending).map_err(state_error)?;
            let pending = state.as_mut().ok_or_else(|| {
                ipc(
                    ErrorCode::InvalidParams,
                    "no pairing verification is pending",
                )
            })?;
            if let Some(result) = mutex(&pending.result).map_err(state_error)?.clone() {
                return result.map(Some);
            }
            if now_ms >= pending.expires_at_ms || Instant::now() >= pending.deadline {
                pending.conn.close(1u32.into(), b"verification expired");
                return Err(ipc(ErrorCode::CodeExpired, "pairing verification expired"));
            }
            pending.decision.try_send(accepted).map_err(|_| {
                ipc(
                    ErrorCode::InvalidParams,
                    "pairing decision already submitted",
                )
            })?;
            Ok(None)
        })
    }

    fn cancel_pair_host(&self) -> NetFuture<'_, Result<(), IpcError>> {
        Box::pin(async move {
            self.shared.close_host();
            if let Some(pending) = mutex(&self.shared.pending).map_err(state_error)?.take() {
                pending.conn.close(1u32.into(), b"pairing cancelled");
            }
            Ok(())
        })
    }

    fn discover(&self) -> NetFuture<'_, Result<Vec<DiscoveredPeer>, IpcError>> {
        Box::pin(async move {
            self.start_discovery()
                .map_err(|_| ipc(ErrorCode::Internal, "discovery unavailable"))?;
            Ok(mutex(&self.shared.discovered)
                .map_err(state_error)?
                .values()
                .filter(|p| !self.shared.pins.contains(&p.device_id))
                .cloned()
                .collect())
        })
    }

    fn add_manual<'a>(
        &'a self,
        address: &'a str,
    ) -> NetFuture<'a, Result<DiscoveredPeer, IpcError>> {
        Box::pin(async move {
            let (conn, hello, _) = self.pairing_connect(address, 0).await?;
            let peer = DiscoveredPeer {
                device_id: hello.device_id,
                name: hello.name,
                os: hello.os,
                address: conn.remote_address().to_string(),
            };
            conn.close(0u32.into(), b"probe complete");
            let mut discovered = mutex(&self.shared.discovered).map_err(state_error)?;
            if discovered.len() >= MAX_PEERS && !discovered.contains_key(&peer.device_id) {
                return Err(ipc(ErrorCode::InvalidParams, "discovery limit reached"));
            }
            discovered.insert(peer.device_id.clone(), peer.clone());
            let _ = self
                .shared
                .event_tx
                .send(PeerManagerEvent::Discovered(peer.clone()));
            Ok(peer)
        })
    }

    fn unpair<'a>(&'a self, id: &'a str) -> NetFuture<'a, Result<(), IpcError>> {
        Box::pin(async move {
            self.shared.revoke(id).await?;
            let _ = self
                .shared
                .event_tx
                .send(PeerManagerEvent::Unpaired(id.to_owned()));
            Ok(())
        })
    }

    fn events(&self) -> broadcast::Receiver<PeerManagerEvent> {
        self.shared.event_tx.subscribe()
    }
}

impl Shared {
    fn local_hello(&self) -> Result<PairHello, IpcError> {
        let metadata = self
            .link
            .0
            .hello
            .lock()
            .map_err(|_| ipc(ErrorCode::Internal, "metadata state poisoned"))?;
        Ok(PairHello {
            device_id: self.identity.device_id().to_owned(),
            name: metadata.name.clone(),
            os: self.config.os,
        })
    }

    fn close_host(&self) {
        let Ok(mut host) = self.host.lock() else {
            self.link
                .0
                .endpoint
                .close(1u32.into(), b"pairing state poisoned");
            return;
        };
        host.take();
        self.pairing_open.store(false, Ordering::Release);
    }

    fn close_host_if(&self, id: u64, require_unexpired: bool) -> bool {
        let Ok(mut host) = self.host.lock() else {
            self.link
                .0
                .endpoint
                .close(1u32.into(), b"pairing state poisoned");
            return false;
        };
        if !host
            .as_ref()
            .is_some_and(|h| h.id == id && (!require_unexpired || h.deadline > Instant::now()))
        {
            return false;
        }
        host.take();
        self.pairing_open.store(false, Ordering::Release);
        true
    }

    fn rate_check(&self, ip: IpAddr) -> Result<(), IpcError> {
        let mut rates = mutex(&self.rates).map_err(state_error)?;
        rates.retain(|_, r| r.start.elapsed() < HOST_LIFETIME);
        if !rates.contains_key(&ip) && rates.len() >= 256 {
            return Err(ipc(ErrorCode::LockedOut, "pairing source limit reached"));
        }
        let rate = rates.entry(ip).or_insert(Rate {
            start: Instant::now(),
            attempts: 0,
        });
        if rate.attempts >= 12 {
            return Err(ipc(ErrorCode::LockedOut, "pairing rate limit reached"));
        }
        rate.attempts += 1;
        Ok(())
    }

    async fn persist(&self, peers: Vec<Peer>) -> Result<(), IpcError> {
        let path = self.config.data_dir.join("network-peers.json");
        tokio::task::spawn_blocking(move || write_peers(&path, &peers))
            .await
            .map_err(|_| ipc(ErrorCode::Internal, "pin storage worker failed"))?
            .map_err(|_| {
                ipc(
                    ErrorCode::Internal,
                    "could not persist pin metadata; secure transport stopped",
                )
            })
    }

    async fn stage_pin(&self, peer: &Peer) -> Result<u64, IpcError> {
        let _guard = self.store_lock.lock().await;
        let revision = self.revocation_version.load(Ordering::Acquire);
        let peers = {
            let mut peers = mutex(&self.paired).map_err(state_error)?.clone();
            if peers.len() >= MAX_PEERS && !peers.contains_key(&peer.device_id) {
                return Err(ipc(ErrorCode::InvalidParams, "paired peer limit reached"));
            }
            peers.insert(peer.device_id.clone(), peer.clone());
            peers.values().cloned().collect()
        };
        // Keep staged trust unusable after a process crash. Startup preserves
        // this marker and refuses all transport until the transaction is repaired.
        let path = self.config.data_dir.join("pairing-pending.json");
        let id = peer.device_id.clone();
        tokio::task::spawn_blocking(move || write_json_store(&path, &id))
            .await
            .map_err(|_| ipc(ErrorCode::Internal, "pairing journal worker failed"))?
            .map_err(|_| ipc(ErrorCode::Internal, "pairing journal unavailable"))?;
        self.persist(peers).await?;
        Ok(revision)
    }

    async fn activate_pin(&self, peer: Peer, revision: u64) -> Result<Peer, IpcError> {
        let _guard = self.store_lock.lock().await;
        let revoked = {
            let mut revoked = mutex(&self.revoked).map_err(state_error)?;
            if revision != self.revocation_version.load(Ordering::Acquire) {
                return Err(ipc(ErrorCode::NotPaired, "revocation interrupted pairing"));
            }
            revoked.remove(&peer.device_id);
            revoked.clone()
        };
        self.persist_revocations(revoked).await?;
        self.clear_pairing_journal().await?;
        let _revocations = mutex(&self.revoked).map_err(state_error)?;
        if revision != self.revocation_version.load(Ordering::Acquire) {
            return Err(ipc(ErrorCode::NotPaired, "revocation interrupted pairing"));
        }
        // Snapshot readers must see the paired record once a pin admits normal traffic.
        let mut paired = mutex(&self.paired).map_err(state_error)?;
        self.pins
            .insert(&peer.device_id)
            .map_err(|_| ipc(ErrorCode::Internal, "pin state failed closed"))?;
        paired.insert(peer.device_id.clone(), peer.clone());
        Ok(peer)
    }

    async fn clear_pairing_journal(&self) -> Result<(), IpcError> {
        let path = self.config.data_dir.join("pairing-pending.json");
        tokio::task::spawn_blocking(move || {
            storage_boundary(&path, "before_remove")?;
            std::fs::remove_file(&path)?;
            storage_boundary(&path, "remove")?;
            #[cfg(unix)]
            if let Some(parent) = path.parent() {
                std::fs::File::open(parent)?.sync_all()?;
            }
            Ok::<_, NativeError>(())
        })
        .await
        .map_err(|_| ipc(ErrorCode::Internal, "pairing journal worker failed"))?
        .map_err(|_| ipc(ErrorCode::Internal, "pairing journal repair required"))
    }

    async fn rollback_pin(&self, id: &str) {
        let result = async {
            self.pins
                .remove(id)
                .map_err(|_| ipc(ErrorCode::Internal, "pin revocation failed"))?;
            self.link.revoke_now(id);
            let _guard = self.store_lock.lock().await;
            let revoked = {
                let mut revoked = mutex(&self.revoked).map_err(state_error)?;
                revoked.insert(id.to_owned());
                self.revocation_version.fetch_add(1, Ordering::AcqRel);
                revoked.clone()
            };
            self.persist_revocations(revoked).await?;
            let peers = {
                let mut peers = mutex(&self.paired).map_err(state_error)?;
                peers.remove(id);
                peers.values().cloned().collect()
            };
            self.persist(peers).await?;
            if self
                .config
                .data_dir
                .join("pairing-pending.json")
                .try_exists()
                .map_err(|_| ipc(ErrorCode::Internal, "pairing journal unavailable"))?
            {
                self.clear_pairing_journal().await?;
            }
            Ok::<_, IpcError>(())
        }
        .await;
        if result.is_err() {
            self.link
                .0
                .endpoint
                .close(1u32.into(), b"pairing rollback storage failed");
        }
    }

    async fn revoke(&self, id: &str) -> Result<(), IpcError> {
        if !mutex(&self.paired).map_err(state_error)?.contains_key(id) {
            return Err(ipc(ErrorCode::NotPaired, "peer is not paired"));
        }
        // Live trust is removed before disk I/O. A revoked queued message cannot
        // pass NativeLink's generation/PinSet delivery checks.
        self.pins
            .remove(id)
            .map_err(|_| ipc(ErrorCode::Internal, "pin revocation failed"))?;
        self.link.revoke_now(id);
        {
            let mut revoked = mutex(&self.revoked).map_err(state_error)?;
            revoked.insert(id.to_owned());
            self.revocation_version.fetch_add(1, Ordering::AcqRel);
        }
        let _guard = self.store_lock.lock().await;
        // A durable deny record is committed first. Failure to rewrite the
        // main peer snapshot cannot resurrect this key during a later startup.
        let revoked = mutex(&self.revoked).map_err(state_error)?.clone();
        if let Err(error) = self.persist_revocations(revoked).await {
            self.link
                .0
                .endpoint
                .close(1u32.into(), b"revocation storage failed");
            return Err(error);
        }
        let peers = {
            let mut peers = mutex(&self.paired).map_err(state_error)?;
            if peers.remove(id).is_none() {
                return Err(ipc(ErrorCode::NotPaired, "peer is not paired"));
            }
            peers.values().cloned().collect()
        };
        if let Err(error) = self.persist(peers).await {
            self.link
                .0
                .endpoint
                .close(1u32.into(), b"pin storage failed");
            return Err(error);
        }
        Ok(())
    }

    async fn persist_revocations(&self, ids: HashSet<String>) -> Result<(), IpcError> {
        let path = self.config.data_dir.join("revoked-peers.json");
        let mut ids: Vec<_> = ids.into_iter().collect();
        if ids.len() > 4096 {
            return Err(ipc(
                ErrorCode::Internal,
                "revocation record limit reached; secure transport stopped",
            ));
        }
        ids.sort_unstable();
        tokio::task::spawn_blocking(move || write_json_store(&path, &ids))
            .await
            .map_err(|_| ipc(ErrorCode::Internal, "revocation storage worker failed"))?
            .map_err(|_| {
                ipc(
                    ErrorCode::Internal,
                    "revocation is not durable; secure transport stopped, storage repair required",
                )
            })
    }
}

fn spawn_listener(shared: &Arc<Shared>) {
    let weak = Arc::downgrade(shared);
    let endpoint = shared.link.0.endpoint.clone();
    tokio::spawn(async move {
        while let Some(incoming) = endpoint.accept().await {
            let Some(shared) = weak.upgrade() else {
                incoming.refuse();
                break;
            };
            if !incoming.remote_address_validated() {
                let _ = incoming.retry();
                continue;
            }
            let Ok(permit) = shared.inbound_limit.clone().try_acquire_owned() else {
                incoming.refuse();
                continue;
            };
            tokio::spawn(async move {
                let _permit = permit;
                let Ok(connecting) = incoming.accept() else {
                    return;
                };
                let Ok(conn) = timed(connecting).await else {
                    return;
                };
                let protocol = conn
                    .handshake_data()
                    .and_then(|d| d.downcast::<quinn::crypto::rustls::HandshakeData>().ok())
                    .and_then(|d| d.protocol);
                if protocol.as_deref() == Some(wire::ALPN) {
                    let closing = conn.clone();
                    if shared.link.install(conn, false, None).await.is_err() {
                        closing.close(1u32.into(), b"normal session rejected");
                    }
                } else if protocol.as_deref() == Some(PAIR_ALPN)
                    && shared.pairing_open.load(Ordering::Acquire)
                {
                    incoming_pair(shared, conn).await;
                } else {
                    conn.close(1u32.into(), b"unsupported protocol");
                }
            });
        }
    });
}

async fn incoming_pair(shared: Arc<Shared>, conn: Connection) {
    let mut generation = None;
    let result = async {
        let (mut send, mut recv) = timed(conn.accept_bi())
            .await
            .map_err(|_| ipc(ErrorCode::Unreachable, "pairing hello timed out"))?;
        let mut operation = [0];
        timed(recv.read_exact(&mut operation))
            .await
            .map_err(|_| ipc(ErrorCode::InvalidParams, "invalid pairing hello"))?;
        let mut extra = [0];
        if timed(recv.read(&mut extra))
            .await
            .map_err(|_| ipc(ErrorCode::InvalidParams, "invalid pairing hello"))?
            .is_some()
        {
            return Err(ipc(ErrorCode::InvalidParams, "invalid pairing hello"));
        }
        if operation[0] > 1 {
            return Err(ipc(ErrorCode::InvalidParams, "unknown pairing operation"));
        }
        let reservation = (|| {
            shared.rate_check(conn.remote_address().ip())?;
            if operation[0] == 1 {
                let mut host = mutex(&shared.host).map_err(state_error)?;
                let host = host
                    .as_mut()
                    .ok_or_else(|| ipc(ErrorCode::CodeExpired, "pairing is closed"))?;
                if host.failed >= 3 {
                    return Err(ipc(ErrorCode::LockedOut, "pairing code burned"));
                }
                if host.deadline <= Instant::now() {
                    return Err(ipc(ErrorCode::CodeExpired, "pairing code expired"));
                }
                if host.busy || mutex(&shared.pending).map_err(state_error)?.is_some() {
                    return Err(ipc(ErrorCode::LockedOut, "pairing attempt already active"));
                }
                host.busy = true;
                generation = Some(host.id);
                Ok(Some((
                    Zeroizing::new(host.code.to_string()),
                    host.id,
                    3 - host.failed,
                )))
            } else {
                Ok(None)
            }
        })();
        let code = match reservation {
            Ok(code) => code,
            Err(error) => {
                write_json(
                    &mut send,
                    &ProbeReply {
                        hello: None,
                        error: Some(error.clone()),
                        attempts_remaining: 0,
                    },
                )
                .await?;
                let _ = send.finish();
                let _ = timed(send.stopped()).await;
                return Err(error);
            }
        };
        write_json(
            &mut send,
            &ProbeReply {
                hello: Some(shared.local_hello()?),
                error: None,
                attempts_remaining: code.as_ref().map_or(0, |(_, _, left)| *left),
            },
        )
        .await?;
        send.finish()
            .map_err(|_| ipc(ErrorCode::Unreachable, "pairing hello failed"))?;
        let Some((code, generation, _)) = code else {
            // Retain the QUIC owner until the probe reply reaches the peer.
            let _ = timed(send.stopped()).await;
            return Ok(());
        };
        let remote_id = tls::peer_device_id(&conn)
            .map_err(|_| ipc(ErrorCode::PermissionDenied, "invalid peer certificate"))?;
        let exchange = tokio::time::timeout(
            super::IO_TIMEOUT,
            pairing::pair_exchange(
                &conn,
                true,
                code,
                shared.local_hello()?,
                shared.identity.device_id(),
                &remote_id,
            ),
        )
        .await
        .unwrap_or_else(|_| Err(ipc(ErrorCode::BadCode, "pairing exchange timed out")));
        match exchange {
            Ok(pairing) => {
                if !shared.close_host_if(generation, true) {
                    return Err(ipc(
                        ErrorCode::CodeExpired,
                        "pairing window closed during authentication",
                    ));
                }
                let _ = shared.event_tx.send(PeerManagerEvent::PairingIncoming {
                    name: pairing.remote.name.clone(),
                    os: pairing.remote.os,
                    address: conn.remote_address().to_string(),
                });
                prepare_pending(&shared, conn.clone(), pairing, true, unix_ms())?;
                Ok(())
            }
            Err(error) => {
                let burned = {
                    let mut host = mutex(&shared.host).map_err(state_error)?;
                    if let Some(host) = host.as_mut().filter(|h| h.id == generation) {
                        host.failed = host.failed.saturating_add(1);
                        host.busy = false;
                        host.failed >= 3
                    } else {
                        false
                    }
                };
                if burned {
                    shared.close_host_if(generation, false);
                }
                Err(error)
            }
        }
    }
    .await;
    if let Err(error) = result {
        conn.close(1u32.into(), b"pairing failed");
        let _ = shared.event_tx.send(PeerManagerEvent::PairingResult {
            ok: false,
            device_id: None,
            error: Some(error.code),
        });
        // A failed/abandoned attempt cannot leave a permanently busy host window.
        if let Ok(mut host) = shared.host.lock() {
            if let Some(host) = host.as_mut().filter(|h| Some(h.id) == generation) {
                host.busy = false;
            }
        }
    }
}

fn prepare_pending(
    shared: &Arc<Shared>,
    conn: Connection,
    pairing: AuthenticatedPairing,
    is_host: bool,
    now_ms: u64,
) -> Result<PairingSession, IpcError> {
    #[cfg(not(test))]
    let lifetime = VERIFY_LIFETIME;
    #[cfg(test)]
    let lifetime = Duration::from_millis(shared.verification_lifetime_ms.load(Ordering::Relaxed));
    let expires_at_ms = now_ms.saturating_add(lifetime.as_millis() as u64);
    let verification = PairingVerify {
        phrase: pairing.phrase.clone(),
        peer: VerificationPeer {
            name: pairing.remote.name.clone(),
            os: pairing.remote.os,
        },
        expires_at_ms,
    };
    let peer = Peer {
        device_id: pairing.remote.device_id.clone(),
        name: pairing.remote.name.clone(),
        os: pairing.remote.os,
        fingerprint: tls::human_fingerprint(&pairing.remote.device_id),
        online: false,
        connection: PeerState::Offline,
        address: Some(conn.remote_address().to_string()),
        latency_ms: None,
        monitors: Vec::new(),
        clipboard_enabled: true,
        wake_mac: None,
        last_monitors: Vec::new(),
        app_version: None,
        model: None,
    };
    let (decision, decisions) = mpsc::channel(1);
    let result = Arc::new(Mutex::new(None));
    let id = shared.next_pairing.fetch_add(1, Ordering::Relaxed);
    let deadline = Instant::now() + lifetime;
    {
        let mut pending = mutex(&shared.pending).map_err(state_error)?;
        if pending.is_some() {
            conn.close(1u32.into(), b"pairing busy");
            return Err(ipc(
                ErrorCode::InvalidParams,
                "pairing verification already pending",
            ));
        }
        *pending = Some(Pending {
            id,
            expires_at_ms,
            deadline,
            decision,
            result: result.clone(),
            conn: conn.clone(),
        });
    }
    let _ = shared
        .event_tx
        .send(PeerManagerEvent::PairingVerify(verification.clone()));
    spawn_verification(
        Arc::downgrade(shared),
        conn,
        pairing,
        is_host,
        peer.clone(),
        id,
        result,
        decisions,
        deadline,
    );
    Ok(PairingSession { peer, verification })
}

#[allow(clippy::too_many_arguments)]
fn spawn_verification(
    shared: Weak<Shared>,
    conn: Connection,
    mut pairing: AuthenticatedPairing,
    is_host: bool,
    peer: Peer,
    id: u64,
    result_slot: Arc<Mutex<Option<Result<Peer, IpcError>>>>,
    mut decisions: mpsc::Receiver<bool>,
    deadline: Instant,
) {
    tokio::spawn(async move {
        let exchange = async {
            {
                let mut local = false;
                let mut remote = false;
                let remote_read = pairing::read_decision(
                    &mut pairing.recv,
                    &pairing.key,
                    &pairing.phrase,
                    !is_host,
                    DecisionStage::Confirm,
                );
                tokio::pin!(remote_read);
                while !local || !remote {
                    tokio::select! {
                        accepted = decisions.recv(), if !local => {
                            let accepted = accepted.ok_or_else(|| ipc(ErrorCode::PermissionDenied, "pairing cancelled"))?;
                            pairing::send_decision(&mut pairing.send, &pairing.key, &pairing.phrase, is_host, DecisionStage::Confirm, accepted).await?;
                            if !accepted { return Err(ipc(ErrorCode::PermissionDenied, "pairing declined")); }
                            local = true;
                        }
                        accepted = &mut remote_read, if !remote => {
                            if !accepted? { return Err(ipc(ErrorCode::PermissionDenied, "remote user declined pairing")); }
                            remote = true;
                        }
                    }
                }
            }
            for stage in [DecisionStage::Ready, DecisionStage::Commit] {
                pairing::send_decision(
                    &mut pairing.send,
                    &pairing.key,
                    &pairing.phrase,
                    is_host,
                    stage,
                    true,
                )
                .await?;
                if !pairing::read_decision(
                    &mut pairing.recv,
                    &pairing.key,
                    &pairing.phrase,
                    !is_host,
                    stage,
                )
                .await?
                {
                    return Err(ipc(ErrorCode::PermissionDenied, "pairing commit declined"));
                }
            }
            Ok::<(), IpcError>(())
        };
        let authorized = tokio::select! {
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => Err(ipc(ErrorCode::CodeExpired, "pairing verification expired")),
            _ = conn.closed() => Err(ipc(ErrorCode::Unreachable, "pairing disconnected")),
            result = exchange => result,
        };
        // Disk writes are never cancelled: a dropped spawn_blocking future can
        // still commit afterward. Await completion, then explicitly roll back
        // if the authenticated peer disconnects or misses the final barrier.
        let result = if let Err(error) = authorized {
            Err(error)
        } else if let Some(owner) = shared.upgrade() {
            match owner.stage_pin(&peer).await {
                Err(error) => {
                    owner.rollback_pin(&peer.device_id).await;
                    Err(error)
                }
                Ok(revision) => {
                    let completion = async {
                        if conn.close_reason().is_some() || Instant::now() >= deadline {
                            return Err(ipc(
                                ErrorCode::Unreachable,
                                "pairing disconnected during persistence",
                            ));
                        }
                        pairing::send_decision(
                            &mut pairing.send,
                            &pairing.key,
                            &pairing.phrase,
                            is_host,
                            DecisionStage::Complete,
                            true,
                        )
                        .await?;
                        if !pairing::read_decision(
                            &mut pairing.recv,
                            &pairing.key,
                            &pairing.phrase,
                            !is_host,
                            DecisionStage::Complete,
                        )
                        .await?
                        {
                            return Err(ipc(
                                ErrorCode::PermissionDenied,
                                "peer could not persist pairing",
                            ));
                        }
                        Ok(())
                    };
                    match tokio::time::timeout_at(
                        tokio::time::Instant::from_std(deadline),
                        completion,
                    )
                    .await
                    {
                        Ok(Ok(())) => match owner.activate_pin(peer.clone(), revision).await {
                            Ok(pinned) => Ok(pinned),
                            Err(error) => {
                                owner.rollback_pin(&peer.device_id).await;
                                Err(error)
                            }
                        },
                        failure => {
                            let error = failure.ok().and_then(Result::err).unwrap_or_else(|| {
                                ipc(ErrorCode::CodeExpired, "pairing completion expired")
                            });
                            owner.rollback_pin(&peer.device_id).await;
                            Err(error)
                        }
                    }
                }
            }
        } else {
            Err(ipc(ErrorCode::Unreachable, "network owner stopped"))
        };
        if result.is_err() {
            conn.close(1u32.into(), b"pairing aborted");
        }
        if let Ok(mut slot) = result_slot.lock() {
            *slot = Some(result.clone());
        }
        if let Some(shared) = shared.upgrade() {
            if let Ok(mut pending) = shared.pending.lock() {
                if pending.as_ref().is_some_and(|p| p.id == id) {
                    pending.take();
                }
            }
            match result {
                Ok(peer) => {
                    let _ = shared.event_tx.send(PeerManagerEvent::Paired(peer.clone()));
                    let _ = shared.event_tx.send(PeerManagerEvent::PairingResult {
                        ok: true,
                        device_id: Some(peer.device_id),
                        error: None,
                    });
                }
                Err(error) => {
                    let _ = shared.event_tx.send(PeerManagerEvent::PairingResult {
                        ok: false,
                        device_id: None,
                        error: Some(error.code),
                    });
                }
            }
        }
        // Completed peers have both explicitly acknowledged durable storage.
        // Retain the last flight until QUIC acknowledges it, so the counterpart
        // can read Complete before implicit close on Drop. No heuristic sleep.
        if result_slot
            .lock()
            .is_ok_and(|slot| slot.as_ref().is_some_and(Result::is_ok))
        {
            let _ = pairing.send.finish();
            let _ = timed(pairing.send.stopped()).await;
        }
    });
}

/// The computer with the smaller device ID always dials a lost peer. The other one steps in only after the peer has been
/// unreachable this long, so a failing dial from one side (a changed address, a blocked port, a sleeping process) can
/// never leave both computers waiting for each other. Simultaneous dials are resolved by the usual duplicate handling.
const LARGER_ID_REDIAL_GRACE: Duration = Duration::from_secs(5);
/// Longest wait between two dial attempts to the same peer; a local network does not need a slower cadence.
const MAX_REDIAL_BACKOFF: Duration = Duration::from_secs(5);

fn may_auto_dial(own_id: &str, peer_id: &str, down_for: Duration) -> bool {
    match own_id.cmp(peer_id) {
        std::cmp::Ordering::Less => true,
        std::cmp::Ordering::Equal => false,
        std::cmp::Ordering::Greater => down_for >= LARGER_ID_REDIAL_GRACE,
    }
}

fn spawn_reconnect(shared: &Arc<Shared>) {
    let weak = Arc::downgrade(shared);
    let mut events = shared.event_tx.subscribe();
    tokio::spawn(async move {
        let mut backoff: HashMap<String, (Instant, Duration)> = HashMap::new();
        let mut down_since: HashMap<String, Instant> = HashMap::new();
        let mut timer = tokio::time::interval(Duration::from_millis(500));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let event =
                tokio::select! { _ = timer.tick() => None, event = events.recv() => event.ok() };
            let Some(shared) = weak.upgrade() else {
                break;
            };
            if let Some(PeerManagerEvent::Unpaired(id)) = event {
                if mutex(&shared.paired).is_ok_and(|peers| peers.contains_key(&id)) {
                    let _ = shared.revoke(&id).await;
                }
            }
            if shared.link.0.endpoint.local_addr().is_err() {
                break;
            }
            if let Ok(mut host) = shared.host.lock() {
                if host.as_ref().is_some_and(|h| h.deadline <= Instant::now()) {
                    host.take();
                    shared.pairing_open.store(false, Ordering::Release);
                }
            }
            let peers = mutex(&shared.paired)
                .map(|p| p.values().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            backoff.retain(|id, _| shared.pins.contains(id));
            down_since.retain(|id, _| shared.pins.contains(id));
            for peer in peers {
                if !shared.pins.contains(&peer.device_id) || shared.link.connected(&peer.device_id)
                {
                    backoff.remove(&peer.device_id);
                    down_since.remove(&peer.device_id);
                    continue;
                }
                // The smaller ID dials at once; the other side joins in after a short grace period.
                // Explicit dials from either side remain legal and use the same deterministic dedupe.
                let now = Instant::now();
                let down_for = now.saturating_duration_since(
                    *down_since.entry(peer.device_id.clone()).or_insert(now),
                );
                if !may_auto_dial(shared.identity.device_id(), &peer.device_id, down_for) {
                    continue;
                }
                let state = backoff
                    .entry(peer.device_id.clone())
                    .or_insert((Instant::now(), Duration::from_millis(500)));
                if state.0 > Instant::now() {
                    continue;
                }
                let address = mutex(&shared.discovered)
                    .ok()
                    .and_then(|d| d.get(&peer.device_id).map(|d| d.address.clone()))
                    .or(peer.address);
                let Some(address) = address else {
                    continue;
                };
                use crate::Link;
                if shared
                    .link
                    .connect(&address, Some(&peer.device_id))
                    .await
                    .is_ok()
                {
                    backoff.remove(&peer.device_id);
                } else {
                    state.0 = Instant::now() + state.1;
                    state.1 = (state.1 * 2).min(MAX_REDIAL_BACKOFF);
                }
            }
        }
    });
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ProbeReply {
    hello: Option<PairHello>,
    error: Option<IpcError>,
    attempts_remaining: u8,
}

async fn write_json<T: serde::Serialize>(
    send: &mut quinn::SendStream,
    message: &T,
) -> Result<(), IpcError> {
    let bytes = serde_json::to_vec(message)
        .map_err(|_| ipc(ErrorCode::Internal, "pairing hello encoding failed"))?;
    if bytes.len() > 1024 {
        return Err(ipc(ErrorCode::InvalidParams, "pairing hello exceeds limit"));
    }
    super::link::write_frame(send, &bytes)
        .await
        .map_err(|_| ipc(ErrorCode::Unreachable, "pairing hello send failed"))
}

async fn read_json<T: serde::de::DeserializeOwned>(
    recv: &mut quinn::RecvStream,
) -> Result<T, IpcError> {
    let mut header = [0; 4];
    timed(recv.read_exact(&mut header))
        .await
        .map_err(|_| ipc(ErrorCode::Unreachable, "pairing hello timed out"))?;
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 || length > 1024 {
        return Err(ipc(ErrorCode::InvalidParams, "pairing hello exceeds limit"));
    }
    let mut bytes = [0; 1024];
    timed(recv.read_exact(&mut bytes[..length]))
        .await
        .map_err(|_| ipc(ErrorCode::Unreachable, "pairing hello timed out"))?;
    serde_json::from_slice(&bytes[..length])
        .map_err(|_| ipc(ErrorCode::InvalidParams, "invalid pairing hello"))
}

fn validate_hello(hello: &PairHello) -> Result<(), IpcError> {
    if hello.device_id.len() != 64
        || !hello
            .device_id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || hello.name.is_empty()
        || hello.name.len() > 128
        || hello.name.chars().any(char::is_control)
    {
        return Err(ipc(ErrorCode::InvalidParams, "invalid peer metadata"));
    }
    Ok(())
}

fn read_peers(path: &Path) -> Result<HashMap<String, Peer>, NativeError> {
    if path.with_extension("json.tmp").try_exists()? {
        return Err(NativeError::Security(
            "unfinished pin transaction preserved; storage repair required".into(),
        ));
    }
    read_peer_snapshot(path)
}

fn read_peer_snapshot(path: &Path) -> Result<HashMap<String, Peer>, NativeError> {
    use std::io::Read;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(e) => return Err(e.into()),
    };
    let mut bytes = Vec::new();
    file.take(128 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 128 * 1024 {
        return Err(NativeError::Security("pin store exceeds limit".into()));
    }
    let peers: Vec<Peer> = serde_json::from_slice(&bytes).map_err(|_| {
        NativeError::Security("invalid pin store; preserved without trusting it".into())
    })?;
    if peers.len() > MAX_PEERS {
        return Err(NativeError::Security("too many stored pins".into()));
    }
    let mut map = HashMap::new();
    for mut peer in peers {
        let hello = PairHello {
            device_id: peer.device_id.clone(),
            name: peer.name.clone(),
            os: peer.os,
        };
        if validate_hello(&hello).is_err()
            || peer.fingerprint != tls::human_fingerprint(&peer.device_id)
            || peer.address.as_ref().is_some_and(|a| a.len() > 512)
            || peer.monitors.len() > wire::MAX_MONITORS
            || map.contains_key(&peer.device_id)
        {
            return Err(NativeError::Security("invalid pin metadata".into()));
        }
        peer.online = false;
        peer.connection = PeerState::Offline;
        peer.latency_ms = None;
        map.insert(peer.device_id.clone(), peer);
    }
    Ok(map)
}

fn write_peers(path: &Path, peers: &[Peer]) -> Result<(), NativeError> {
    let mut peers = peers.to_vec();
    peers.sort_by(|a, b| a.device_id.cmp(&b.device_id));
    write_json_store(path, &peers)
}

fn write_json_store(path: &Path, value: &impl serde::Serialize) -> Result<(), NativeError> {
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or_else(|| NativeError::Internal("invalid pin store path".into()))?;
    std::fs::create_dir_all(parent)?;
    let bytes = serde_json::to_vec(value)
        .map_err(|_| NativeError::Internal("pin encoding failed".into()))?;
    let temporary = PathBuf::from(path).with_extension("json.tmp");
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    storage_boundary(path, "create")?;
    file.write_all(&bytes)?;
    storage_boundary(path, "write")?;
    file.sync_all()?;
    storage_boundary(path, "sync")?;
    drop(file);
    std::fs::rename(&temporary, path)?;
    storage_boundary(path, "rename")?;
    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;
    storage_boundary(path, "directory_sync")?;
    Ok(())
}

fn storage_boundary(_path: &Path, _boundary: &'static str) -> Result<(), NativeError> {
    #[cfg(test)]
    if STORAGE_FAULT.with(|fault| {
        fault.get().is_some_and(|(name, boundary)| {
            _path.file_name().is_some_and(|file| file == name) && boundary == _boundary
        })
    }) {
        return Err(std::io::Error::other("injected durable storage failure").into());
    }
    Ok(())
}

#[cfg(test)]
thread_local! { static STORAGE_FAULT: std::cell::Cell<Option<(&'static str, &'static str)>> = const { std::cell::Cell::new(None) }; }

fn repair_unfinished_pairing(data_dir: &Path) -> Result<PairingRepair, NativeError> {
    use std::fs;
    if !fs::symlink_metadata(data_dir)?.file_type().is_dir() {
        return Err(NativeError::Security(
            "repair data directory is not a regular directory".into(),
        ));
    }
    let names = [
        "pairing-pending.json",
        "pairing-pending.json.tmp",
        "network-peers.json.tmp",
        "revoked-peers.json.tmp",
        "pairing-repair.json",
        "pairing-repair.json.tmp",
    ];
    let mut report = PairingRepair::default();
    for name in names {
        let path = data_dir.join(name);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_file() => {
                report.removed_files.push(name.into())
            }
            Ok(_) => {
                return Err(NativeError::Security(
                    "repair journal is not a regular file".into(),
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    if report.removed_files.is_empty() {
        return Ok(report);
    }
    let peer_path = data_dir.join("network-peers.json");
    let mut peers = read_peer_snapshot(&peer_path)?;
    // Read a bounded journal only. A partial/ambiguous transaction cannot identify
    // its candidate safely and therefore must not preserve any possibly staged pin.
    fn journal<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
        use std::io::Read;
        let file = std::fs::File::open(path).ok()?;
        let mut bytes = Vec::new();
        file.take(512 * 1024 + 1).read_to_end(&mut bytes).ok()?;
        if bytes.len() > 512 * 1024 {
            return None;
        }
        serde_json::from_slice(&bytes).ok()
    }
    let marker = data_dir.join("pairing-repair.json");
    let mut discard = HashSet::new();
    let mut ambiguous = false;
    let mut candidate = false;
    for name in &report.removed_files {
        match name.as_str() {
            "pairing-pending.json" | "pairing-pending.json.tmp" => {
                match journal::<String>(&data_dir.join(name))
                    .filter(|id| super::identity::valid_device_id(id))
                {
                    Some(id) => {
                        discard.insert(id);
                        candidate = true;
                    }
                    None => ambiguous = true,
                }
            }
            "revoked-peers.json.tmp" => {
                match journal::<Vec<String>>(&data_dir.join(name)).filter(|ids| {
                    ids.len() <= 4096 && ids.iter().all(|id| super::identity::valid_device_id(id))
                }) {
                    Some(ids) => discard.extend(ids),
                    None => ambiguous = true,
                }
            }
            "pairing-repair.json" => match journal::<PairingRepair>(&marker) {
                Some(previous) => discard.extend(previous.removed_peer_ids),
                None => ambiguous = true,
            },
            "pairing-repair.json.tmp" => ambiguous = true,
            _ => {}
        }
    }
    if report
        .removed_files
        .iter()
        .any(|name| name == "network-peers.json.tmp")
        && !candidate
    {
        ambiguous = true;
    }
    if ambiguous {
        discard.extend(peers.keys().cloned());
    }
    report.removed_peer_ids = discard.into_iter().collect();
    report.removed_peer_ids.sort();
    // Commit a repair deny marker before deleting any old evidence. Startup rejects
    // this marker at every interruption; an explicit retry can only discard trust.
    if !marker.try_exists()? {
        use std::io::Write;
        // A partial final marker also denies startup. Never delete an incomplete
        // temporary marker before a replacement denial is durable.
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)?;
        storage_boundary(&marker, "create")?;
        let bytes = serde_json::to_vec(&report)
            .map_err(|_| NativeError::Internal("repair encoding failed".into()))?;
        file.write_all(&bytes)?;
        storage_boundary(&marker, "write")?;
        file.sync_all()?;
        storage_boundary(&marker, "sync")?;
        #[cfg(unix)]
        fs::File::open(data_dir)?.sync_all()?;
        storage_boundary(&marker, "directory_sync")?;
    }
    peers.retain(|id, _| !report.removed_peer_ids.contains(id));
    let temporary = peer_path.with_extension("json.tmp");
    if temporary.try_exists()? {
        fs::remove_file(&temporary)?;
        storage_boundary(&temporary, "remove")?;
    }
    write_peers(&peer_path, &peers.into_values().collect::<Vec<_>>())?;
    for name in &report.removed_files {
        if name == "pairing-repair.json" || name == "network-peers.json.tmp" {
            continue;
        }
        let path = data_dir.join(name);
        if path.try_exists()? {
            fs::remove_file(&path)?;
            storage_boundary(&path, "remove")?;
        }
    }
    storage_boundary(&marker, "before_remove")?;
    fs::remove_file(&marker)?;
    storage_boundary(&marker, "remove")?;
    #[cfg(unix)]
    fs::File::open(data_dir)?.sync_all()?;
    storage_boundary(&marker, "final_directory_sync")?;
    if !report
        .removed_files
        .iter()
        .any(|name| name == "pairing-repair.json")
    {
        report.removed_files.push("pairing-repair.json".into());
    }
    Ok(report)
}

fn read_revocations(path: &Path) -> Result<HashSet<String>, NativeError> {
    use std::io::Read;
    if path.with_extension("json.tmp").try_exists()? {
        return Err(NativeError::Security(
            "unfinished revocation transaction preserved; storage repair required".into(),
        ));
    }
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashSet::new()),
        Err(e) => return Err(e.into()),
    };
    let mut bytes = Vec::new();
    file.take(512 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 512 * 1024 {
        return Err(NativeError::Security(
            "revocation store exceeds limit".into(),
        ));
    }
    let ids: Vec<String> = serde_json::from_slice(&bytes)
        .map_err(|_| NativeError::Security("invalid revocation store preserved".into()))?;
    if ids.len() > 4096 || ids.iter().any(|id| !super::identity::valid_device_id(id)) {
        return Err(NativeError::Security("invalid revocation metadata".into()));
    }
    let count = ids.len();
    let set: HashSet<_> = ids.into_iter().collect();
    if set.len() != count {
        return Err(NativeError::Security("duplicate revocation record".into()));
    }
    Ok(set)
}

fn state_error(_: crate::LinkError) -> IpcError {
    ipc(ErrorCode::Internal, "network state failed closed")
}

fn random_code() -> Result<String, IpcError> {
    loop {
        let mut bytes = [0u8; 4];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| ipc(ErrorCode::Internal, "secure pairing randomness unavailable"))?;
        let value = u32::from_le_bytes(bytes);
        // Rejection sampling avoids modulo bias; failure to read OS randomness
        // returns an error rather than panic or an insecure fallback.
        if value < u32::MAX - u32::MAX % 1_000_000 {
            return Ok(format!("{:06}", value % 1_000_000));
        }
    }
}

#[cfg(all(test, windows))]
mod socket_tests {
    use super::*;

    #[test]
    fn udp_receive_buffer_is_configured_before_quinn_wraps_socket() {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("loopback socket");
        configure_udp_receive_buffer(&socket).expect("configure UDP receive buffer");
        assert_eq!(
            udp_receive_buffer_size(&socket).expect("read UDP receive buffer"),
            UDP_RECEIVE_BUFFER_BYTES as usize
        );
    }
}

#[cfg(test)]
mod redial_tests {
    use super::*;

    // Bug: after Glide restarted on the PC the MacBook showed offline until Glide was restarted on the Mac too,
    // because only the PC ever dialed and nothing else tried when its dials kept failing.
    #[test]
    fn the_larger_id_joins_in_only_after_the_grace_period_and_the_smaller_always_dials() {
        let small = "0dc0";
        let large = "b402";
        assert!(may_auto_dial(small, large, Duration::ZERO));
        assert!(!may_auto_dial(large, small, Duration::ZERO));
        assert!(!may_auto_dial(
            large,
            small,
            LARGER_ID_REDIAL_GRACE - Duration::from_millis(1)
        ));
        assert!(may_auto_dial(large, small, LARGER_ID_REDIAL_GRACE));
        assert!(!may_auto_dial(small, small, Duration::from_secs(3600)));
    }
}
