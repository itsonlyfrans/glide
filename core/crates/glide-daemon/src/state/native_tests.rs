use super::*;
use glide_net::TestKeyStore;
use glide_platform::{
    ClipboardContent, ClipboardData, ClipboardFormat, ClipboardSensitivity, FileEntry, FileList,
    InputEvent, InputEventKind, Key, Monitor, Os, Point,
};
use glide_proto::{
    ipc::{ErrorCode, Event, PairingVerify, Request, Response, TransferState},
    wire::{self, InputKey, InputMessage, WireMessage},
};
use serde_json::{json, Value};
use std::{
    net::{Ipv4Addr, SocketAddrV4, UdpSocket},
    path::Path,
    sync::Arc,
    time::Duration,
};
use tokio::{sync::Mutex, task::JoinHandle};

pub(super) static NATIVE_TIMING: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

const TEST_TIMEOUT: Duration = Duration::from_secs(12);
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Default)]
struct PairingObservation {
    verify: Option<PairingVerify>,
    token: Option<glide_net::PeerToken>,
    connected: bool,
}

impl PairingObservation {
    fn check(&mut self, core: &mut Core, peer_id: &str) {
        let token = core.link.peer_token(peer_id).ok();
        if let Some(first) = self.token {
            assert_eq!(
                token,
                Some(first),
                "authenticated link dropped or was replaced after connecting"
            );
        } else {
            self.token = token;
        }
        for event in core.take_events() {
            match event {
                Event::Notification(notice) => {
                    assert_ne!(notice.level, "error", "{}: {}", notice.title, notice.body)
                }
                Event::PairingVerify(verify) => self.verify = Some(verify),
                Event::State(state) => {
                    if let Some(peer) = state.peers.iter().find(|p| p.device_id == peer_id) {
                        if self.connected {
                            assert_eq!(
                                peer.connection,
                                Connection::Connected,
                                "connection state regressed"
                            );
                            assert!(peer.online);
                        }
                        self.connected |= peer.connection == Connection::Connected;
                    }
                }
                _ => {}
            }
        }
        if let Some(peer) = core.state.peers.iter().find(|p| p.device_id == peer_id) {
            if self.connected {
                assert_eq!(peer.connection, Connection::Connected);
                assert!(peer.online);
            }
            self.connected |= peer.connection == Connection::Connected;
        }
    }
}

async fn pairing_turn(core: &mut Core, observed: &mut PairingObservation, peer_id: &str) {
    // Deliberately dispatch the link before the timer/manager pump, like stdio IPC can.
    let link = core.link();
    if let Ok(event) = tokio::time::timeout(Duration::from_millis(1), link.recv_event()).await {
        core.receive_link(event.expect("native link event"))
            .await
            .expect("peer dispatch");
    }
    observed.check(core, peer_id);
    core.tick().await.expect("native tick");
    observed.check(core, peer_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_pairing_twenty_times_never_notifies_or_drops_connected_link() {
    let _native_load = NATIVE_TIMING.read().await;
    let temp = real_tempdir().expect("dir");
    let store = TestKeyStore::default();
    for round in 0..20 {
        let mut a = native_core(
            &temp.path().join(format!("a-{round}")),
            0,
            Os::Windows,
            &store,
        )
        .await;
        let mut b = native_core(
            &temp.path().join(format!("b-{round}")),
            0,
            Os::Macos,
            &store,
        )
        .await;
        let a_id = a.state.self_info.device_id.clone();
        let b_id = b.state.self_info.device_id.clone();
        let mut a_seen = PairingObservation::default();
        let mut b_seen = PairingObservation::default();
        let hosted = request(&mut a, "pairing.start_host", json!({})).await;
        assert!(hosted.ok);
        let address = a
            .native_manager
            .as_ref()
            .expect("native")
            .local_addr()
            .expect("address")
            .to_string();
        assert!(b
            .handle(Request {
                id: 2,
                method: "pairing.join".into(),
                params: json!({"address":address,"code":hosted.result.as_ref().expect("host code")["code"]}),
            })
            .await
            .is_none());
        tokio::time::timeout(TEST_TIMEOUT, async {
            while a_seen.verify.is_none() || b_seen.verify.is_none() {
                pairing_turn(&mut a, &mut a_seen, &b_id).await;
                pairing_turn(&mut b, &mut b_seen, &a_id).await;
            }
        })
        .await
        .expect("SAS deadline");
        assert_eq!(
            a_seen.verify.as_ref().expect("A SAS").phrase,
            b_seen.verify.as_ref().expect("B SAS").phrase
        );
        assert!(
            request(&mut a, "pairing.confirm", json!({"accepted":true}))
                .await
                .ok
        );
        a_seen.check(&mut a, &b_id);
        assert!(
            request(&mut b, "pairing.confirm", json!({"accepted":true}))
                .await
                .ok
        );
        b_seen.check(&mut b, &a_id);
        tokio::time::timeout(TEST_TIMEOUT, async {
            while !a_seen.connected || !b_seen.connected {
                pairing_turn(&mut a, &mut a_seen, &b_id).await;
                pairing_turn(&mut b, &mut b_seen, &a_id).await;
            }
        })
        .await
        .expect("connected deadline");
        tokio::time::timeout(TEST_TIMEOUT, async {
            loop {
                pairing_turn(&mut a, &mut a_seen, &b_id).await;
                pairing_turn(&mut b, &mut b_seen, &a_id).await;
                if a.state.layout == b.state.layout && a.layout_version == b.layout_version {
                    break;
                }
            }
        })
        .await
        .expect("replicated layouts converge before deadline");
        assert_eq!(a.state.layout.devices.len(), 2);
        assert_eq!(
            a.state.layout, b.state.layout,
            "replicated layouts converge"
        );
        assert_eq!(a.layout_version, b.layout_version);
        assert!(b
            .take_responses()
            .iter()
            .any(|response| response.id == 2 && response.ok));
        a.shutdown().await.expect("A shutdown");
        b.shutdown().await.expect("B shutdown");
    }
}

#[tokio::test]
async fn native_pinned_link_recovers_pairing_when_manager_event_is_lost() {
    let _native_load = NATIVE_TIMING.read().await;
    let temp = real_tempdir().expect("dir");
    let store = TestKeyStore::default();
    let mut a = native_core(&temp.path().join("a"), 0, Os::Windows, &store).await;
    let mut b = native_core(&temp.path().join("b"), 0, Os::Macos, &store).await;
    pair_native_cores(&mut a, &mut b).await;
    let peer_id = b.state.self_info.device_id.clone();
    let token = a.link.peer_token(&peer_id).expect("pinned token");
    // A new subscription loses the already emitted Paired event, as a lagged queue can.
    a.peer_events = a.manager.events();
    a.state.peers.clear();
    a.state
        .layout
        .devices
        .retain(|d| d.device_id == a.state.self_info.device_id);
    a.take_events();
    a.receive_link(glide_net::LinkEvent::Reliable {
        peer_id: peer_id.clone(),
        peer_token: Some(token),
        message: WireMessage::Control(wire::ControlMessage::Heartbeat(wire::Heartbeat {
            seq: 1,
            ts: 0,
        })),
    })
    .await
    .expect("pinned message recovers peer before dispatch");
    assert!(a.state.peers.iter().any(|peer| peer.device_id == peer_id));
    assert_eq!(a.link.peer_token(&peer_id).expect("still connected"), token);
    assert!(!a
        .take_events()
        .iter()
        .any(|event| matches!(event, Event::Notification(_))));
    let mut connected_peer = a.state.peers[0].clone();
    connected_peer.online = true;
    connected_peer.connection = Connection::Connected;
    a.state.peers.clear();
    let manager = InMemoryPeerManager::new();
    a.peer_events = manager.events();
    assert!(manager.script_peer_event(PeerManagerEvent::PeerUpdated(connected_peer)));
    a.pump_peer_events().await;
    assert_eq!(
        a.state.peers[0].connection,
        Connection::Connected,
        "early online metadata must be applied after pairing"
    );
    assert_eq!(a.link.peer_token(&peer_id).expect("still connected"), token);
    assert!(!a
        .take_events()
        .iter()
        .any(|event| matches!(event, Event::Notification(_))));
    a.shutdown().await.expect("shutdown");
    b.shutdown().await.expect("shutdown");
}

pub(super) async fn native_core(data_dir: &Path, port: u16, os: Os, store: &TestKeyStore) -> Core {
    Core::native_with_test_keystore(data_dir, port, os, store)
        .await
        .expect("native Core with test keystore")
}

#[tokio::test]
async fn native_delayed_peer_registration_survives_failed_then_successful_save() {
    let _native_load = NATIVE_TIMING.read().await;
    let temp = real_tempdir().expect("dir");
    let store = TestKeyStore::default();
    let mut a = native_core(&temp.path().join("a"), 0, Os::Windows, &store).await;
    let mut b = native_core(&temp.path().join("b"), 0, Os::Macos, &store).await;
    pair_native_cores(&mut a, &mut b).await;
    let peer_id = b.state.self_info.device_id.clone();
    let token = a.link.peer_token(&peer_id).expect("authenticated token");
    let stale_peer = a.state.peers[0].clone();
    // Withhold all manager events: only the live network snapshot can restore membership.
    let withheld = InMemoryPeerManager::new();
    a.peer_events = withheld.events();
    a.state.peers.clear();
    a.state
        .layout
        .devices
        .retain(|d| d.device_id == a.state.self_info.device_id);
    let config = a.data_dir.join("config.json");
    let backup = a.data_dir.join("config.backup");
    std::fs::rename(&config, &backup).expect("preserve config");
    std::fs::create_dir(&config).expect("force atomic replacement failure");
    let early = || glide_net::LinkEvent::Reliable {
        peer_id: peer_id.clone(),
        peer_token: Some(token),
        message: WireMessage::Control(wire::ControlMessage::HeartbeatAck(wire::HeartbeatAck {
            seq: 1,
            ts: 0,
        })),
    };
    assert!(
        a.receive_link(early()).await.is_err(),
        "save failure is surfaced"
    );
    assert!(
        a.native_manager
            .as_ref()
            .expect("native")
            .paired_peers()
            .expect("pins")
            .iter()
            .any(|p| p.device_id == peer_id),
        "a failed config save must not revoke completed pairing"
    );
    assert_eq!(a.link.peer_token(&peer_id).expect("link retained"), token);
    assert!(a
        .take_events()
        .iter()
        .any(|e| matches!(e, Event::Notification(n) if n.level == "error")));
    // The only online metadata transition can arrive while persistence is blocked.
    assert!(withheld.script_peer_event(PeerManagerEvent::PeerUpdated(stale_peer.clone())));
    a.pump_peer_events().await;
    std::fs::remove_dir(&config).expect("release writer");
    std::fs::rename(&backup, &config).expect("restore config");
    a.tick()
        .await
        .expect("retry registration without another manager event");
    assert!(a.state.peers.iter().any(|p| p.device_id == peer_id));
    assert!(
        a.state
            .peers
            .iter()
            .any(|p| p.device_id == peer_id && p.online),
        "retry must retain the early online transition"
    );
    assert!(Config::load(&a.data_dir)
        .expect("saved config")
        .expect("config")
        .peers
        .iter()
        .any(|p| p.device_id == peer_id));
    a.receive_link(early())
        .await
        .expect("dispatch recovered peer");
    assert_eq!(a.link.peer_token(&peer_id).expect("same link"), token);
    // A later revoke wins over a pending projection retry, even if Core lost Unpaired.
    a.state.peers.clear();
    a.state
        .layout
        .devices
        .retain(|d| d.device_id == a.state.self_info.device_id);
    std::fs::rename(&config, &backup).expect("preserve config again");
    std::fs::create_dir(&config).expect("block writer again");
    assert!(a.receive_link(early()).await.is_err());
    a.native_manager
        .as_ref()
        .expect("native")
        .unpair(&peer_id)
        .await
        .expect("revoke pin while registration pending");
    std::fs::remove_dir(&config).expect("release writer again");
    std::fs::rename(&backup, &config).expect("restore config again");
    assert!(withheld.script_peer_event(PeerManagerEvent::Paired(stale_peer.clone())));
    assert!(withheld.script_peer_event(PeerManagerEvent::PeerUpdated(stale_peer)));
    a.tick().await.expect("reconcile revoked pending peer");
    assert!(!a.state.peers.iter().any(|p| p.device_id == peer_id));
    assert!(a.pending_pairings.is_empty());
    a.shutdown().await.expect("shutdown");
    b.shutdown().await.expect("shutdown");
}

async fn request(core: &mut Core, method: &str, params: Value) -> Response {
    core.handle(Request {
        id: 1,
        method: method.into(),
        params,
    })
    .await
    .expect("immediate IPC response")
}

fn take_pairing_verify(core: &mut Core) -> Option<PairingVerify> {
    core.take_events()
        .into_iter()
        .find_map(|event| match event {
            Event::PairingVerify(verify) => Some(verify),
            _ => None,
        })
}

async fn wait_pairing_verifies(
    host: &mut Core,
    joiner: &mut Core,
) -> (PairingVerify, PairingVerify) {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let mut host_verify = None;
        let mut joiner_verify = None;
        while host_verify.is_none() || joiner_verify.is_none() {
            host.tick().await.expect("host pairing tick");
            joiner.tick().await.expect("joiner pairing tick");
            host_verify = host_verify.or_else(|| take_pairing_verify(host));
            joiner_verify = joiner_verify.or_else(|| take_pairing_verify(joiner));
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        (
            host_verify.expect("host SAS"),
            joiner_verify.expect("joiner SAS"),
        )
    })
    .await
    .expect("pairing SAS deadline")
}

async fn wait_pairing_result(host: &mut Core, joiner: &mut Core) {
    let host_peer = joiner.state.self_info.device_id.clone();
    let joiner_peer = host.state.self_info.device_id.clone();
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            host.tick().await.expect("host pairing completion");
            joiner.tick().await.expect("joiner pairing completion");
            if host
                .state
                .peers
                .iter()
                .any(|peer| peer.device_id == host_peer)
                && joiner
                    .state
                    .peers
                    .iter()
                    .any(|peer| peer.device_id == joiner_peer)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("dual-confirm pairing deadline");
}

async fn wait_connected(host: &mut Core, joiner: &mut Core) {
    let host_peer = joiner.state.self_info.device_id.clone();
    let joiner_peer = host.state.self_info.device_id.clone();
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            host.tick().await.expect("host network tick");
            joiner.tick().await.expect("joiner network tick");
            let host_connected = host.link().peer_token(&host_peer).is_ok()
                && host
                    .state
                    .peers
                    .iter()
                    .any(|peer| peer.device_id == host_peer && peer.online);
            let joiner_connected = joiner.link().peer_token(&joiner_peer).is_ok()
                && joiner
                    .state
                    .peers
                    .iter()
                    .any(|peer| peer.device_id == joiner_peer && peer.online);
            if host_connected && joiner_connected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("authenticated connection deadline");
}

pub(super) async fn pair_native_cores(host: &mut Core, joiner: &mut Core) {
    assert_ne!(
        host.state.self_info.device_id, joiner.state.self_info.device_id,
        "per-data-dir TestKeyStore namespace must produce distinct device identities"
    );
    let host_started = request(host, "pairing.start_host", json!({})).await;
    assert!(host_started.ok, "host pairing window: {host_started:?}");
    let code = host_started.result.as_ref().expect("pairing response")["code"]
        .as_str()
        .expect("pairing code")
        .to_owned();
    let address = host
        .native_manager
        .as_ref()
        .expect("native host")
        .local_addr()
        .expect("bound loopback address")
        .to_string();
    let started = joiner
        .handle(Request {
            id: 2,
            method: "pairing.join".into(),
            params: json!({"address":address,"code":code}),
        })
        .await;
    assert!(started.is_none(), "join response is deferred until SAS");
    let (host_verify, joiner_verify) = wait_pairing_verifies(host, joiner).await;
    assert_eq!(host_verify.phrase, joiner_verify.phrase);
    assert!(
        request(host, "pairing.confirm", json!({"accepted":true}))
            .await
            .ok
    );
    assert!(
        request(joiner, "pairing.confirm", json!({"accepted":true}))
            .await
            .ok
    );
    wait_pairing_result(host, joiner).await;
    assert!(joiner
        .take_responses()
        .iter()
        .any(|response| response.id == 2 && response.ok));
    wait_connected(host, joiner).await;
    let members = |core: &Core| {
        let mut ids: Vec<_> = core
            .state
            .peers
            .iter()
            .map(|p| p.device_id.clone())
            .collect();
        ids.push(core.state.self_info.device_id.clone());
        ids.sort();
        ids
    };
    if members(host) != members(joiner) {
        // Partial three-device pairing has different local membership projections.
        // Its caller pumps all three after the complete graph has been paired.
        return;
    }
    // Connected is not setup-complete: drain layout gossip before entering a screen.
    tokio::time::timeout(TEST_TIMEOUT, async {
        let mut host_seen = PairingObservation::default();
        let mut joiner_seen = PairingObservation::default();
        loop {
            let host_id = host.state.self_info.device_id.clone();
            let joiner_id = joiner.state.self_info.device_id.clone();
            pairing_turn(host, &mut host_seen, &joiner_id).await;
            pairing_turn(joiner, &mut joiner_seen, &host_id).await;
            if host.state.layout == joiner.state.layout
                && host.layout_version == joiner.layout_version
            {
                break;
            }
        }
    })
    .await
    .expect("paired layout convergence");
}

pub(super) async fn spawn_native_pumps(core: Arc<Mutex<Core>>) -> Vec<JoinHandle<()>> {
    let link = core.lock().await.link();
    let events_core = core.clone();
    let events_link = link.clone();
    let events = tokio::spawn(async move {
        loop {
            let Ok(event) = events_link.recv_event().await else {
                break;
            };
            if let Err(failure) = events_core.lock().await.receive_link(event).await {
                eprintln!("native receive failure: {failure:?}");
            }
        }
    });
    let moves_core = core.clone();
    let moves_link = link;
    let moves = tokio::spawn(async move {
        loop {
            let ready = moves_link.mouse_ready().notified();
            tokio::pin!(ready);
            ready.as_mut().enable();
            match moves_link.try_recv_move() {
                Ok(Some(move_event)) => {
                    let _ = moves_core.lock().await.receive_move(move_event).await;
                    continue;
                }
                Ok(None) => {}
                Err(glide_net::LinkError::Busy) => {
                    tokio::task::yield_now().await;
                    continue;
                }
                Err(_) => break,
            }
            ready.await;
        }
    });
    let tick_core = core;
    let tick = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(10));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if tick_core.lock().await.tick().await.is_err() {
                break;
            }
        }
    });
    vec![events, moves, tick]
}

pub(super) async fn wait_native_layout(a: &Arc<Mutex<Core>>, b: &Arc<Mutex<Core>>) {
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            let (layout, version) = {
                let core = a.lock().await;
                (core.state.layout.clone(), core.layout_version.clone())
            };
            let ready = {
                let core = b.lock().await;
                core.state.layout == layout && core.layout_version == version
            };
            if ready {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("remote layout committed before input admission");
}

#[tokio::test]
async fn native_layout_readiness_observes_remote_commit_not_local_ipc_success() {
    let _native_load = NATIVE_TIMING.read().await;
    let temp = real_tempdir().expect("dir");
    let store = TestKeyStore::default();
    let mut a = native_core(&temp.path().join("a"), 0, Os::Windows, &store).await;
    let mut b = native_core(&temp.path().join("b"), 0, Os::Macos, &store).await;
    pair_native_cores(&mut a, &mut b).await;
    let a_id = a.state.self_info.device_id.clone();
    let b_id = b.state.self_info.device_id.clone();
    assert!(
        request(
            &mut a,
            "set_layout",
            json!({"devices":[
                {"device_id":a_id,"x":0.0,"y":0.0},
                {"device_id":b_id,"x":1920.0,"y":50.0}
            ]})
        )
        .await
        .ok
    );
    // B has deliberately not dispatched the layout message yet. Local IPC success
    // proves only A's commit and transport enqueue, never B's readiness for Enter.
    assert_ne!(a.state.layout, b.state.layout);
    let a = Arc::new(Mutex::new(a));
    let b = Arc::new(Mutex::new(b));
    let pumps = spawn_native_pumps(b.clone()).await;
    wait_native_layout(&a, &b).await;
    let expected = a.lock().await.state.layout.clone();
    let b_dir = b.lock().await.data_dir.clone();
    assert_eq!(
        Config::load(&b_dir).expect("load").expect("config").layout,
        expected
    );
    shutdown_native_pair(&a, &b, pumps).await;
}

async fn enter_edge(core: &mut Core, y: f64, dx: f64) -> Result<(), glide_proto::ipc::IpcError> {
    core.capture_input(InputEvent {
        injected: false,
        kind: InputEventKind::PointerMoved {
            position: Point { x: 0.0, y },
            delta_x: 0.0,
            delta_y: y,
        },
    })
    .await?;
    core.capture_input(InputEvent {
        injected: false,
        kind: InputEventKind::PointerMoved {
            position: Point { x: 1919.0, y },
            delta_x: dx,
            delta_y: 0.0,
        },
    })
    .await
}

async fn move_pointer(core: &mut Core, dx: f64, dy: f64) -> Result<(), glide_proto::ipc::IpcError> {
    core.capture_input(InputEvent {
        injected: false,
        kind: InputEventKind::PointerMoved {
            position: Point { x: 0.0, y: 200.0 },
            delta_x: dx,
            delta_y: dy,
        },
    })
    .await
}

async fn settle_clipboard(core: &Arc<Mutex<Core>>) {
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            let mut core = core.lock().await;
            core.poll_clipboard().await;
            let idle = core.clipboard.read.is_none()
                && core.clipboard.write.is_none()
                && core.clipboard.send.is_empty();
            drop(core);
            if idle {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("clipboard workers settle");
}

fn create_sparse_file(path: &Path, bytes: u64) {
    let file = std::fs::File::create(path).expect("create native transfer fixture");
    file.set_len(bytes).expect("size native transfer fixture");
}

fn create_pattern_file(path: &Path, bytes: u64) {
    use std::io::Write;

    let mut file = std::fs::File::create(path).expect("create native transfer fixture");
    let mut buffer = [0u8; 64 * 1024];
    let mut written = 0u64;
    let mut block = 0usize;
    while written < bytes {
        for (index, byte) in buffer.iter_mut().enumerate() {
            *byte = ((index.wrapping_mul(37).wrapping_add(block * 13)) % 251) as u8;
        }
        let length = (bytes - written).min(buffer.len() as u64) as usize;
        file.write_all(&buffer[..length])
            .expect("write native transfer fixture");
        written += length as u64;
        block = block.wrapping_add(1);
    }
}

async fn copy_file_to_mock_clipboard(core: &Arc<Mutex<Core>>, path: &Path, name: &str, bytes: u64) {
    core.lock()
        .await
        .mock_platform()
        .expect("mock platform")
        .clipboard
        .set_external_content(ClipboardContent::files(FileList {
            entries: vec![FileEntry {
                path: path.to_owned(),
                name: name.into(),
                size: bytes,
                is_dir: false,
            }],
            sensitivity: ClipboardSensitivity::default(),
        }))
        .expect("copy file list to native clipboard");
}

async fn wait_native_transfer(
    source: &Arc<Mutex<Core>>,
    receiver: &Arc<Mutex<Core>>,
    source_id: &str,
    expected: TransferState,
    timeout: Duration,
) -> glide_proto::ipc::Transfer {
    tokio::time::timeout(timeout, async {
        loop {
            source.lock().await.poll_clipboard().await;
            receiver.lock().await.poll_clipboard().await;
            let found = receiver
                .lock()
                .await
                .state
                .transfers
                .iter()
                .rev()
                .find(|transfer| transfer.peer_id == source_id)
                .cloned();
            if let Some(transfer) = found {
                if transfer.state == expected {
                    return transfer;
                }
                assert!(
                    !matches!(
                        transfer.state,
                        TransferState::Failed | TransferState::Cancelled
                    ),
                    "native transfer ended early: {transfer:?}"
                );
            }
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("native transfer did not reach {expected:?}"))
}

// macOS keeps its temp folder behind a system link (/var -> /private/var) and the transfer store refuses links in
// any path, so tests that move files must start from the real location (this broke every file-transfer test on macOS).
fn real_tempdir() -> std::io::Result<tempfile::TempDir> {
    let root = std::env::temp_dir();
    let root = if cfg!(unix) {
        root.canonicalize()?
    } else {
        root
    };
    tempfile::Builder::new().tempdir_in(root)
}

fn assert_files_equal(left: &Path, right: &Path) {
    use std::io::Read;

    let mut left = std::fs::File::open(left).expect("source file");
    let mut right = std::fs::File::open(right).expect("verified received file");
    let mut left_buf = [0u8; 64 * 1024];
    let mut right_buf = [0u8; 64 * 1024];
    loop {
        let left_len = left.read(&mut left_buf).expect("read source payload");
        let right_len = right.read(&mut right_buf).expect("read received payload");
        assert_eq!(left_len, right_len, "verified file length");
        assert_eq!(&left_buf[..left_len], &right_buf[..right_len]);
        if left_len == 0 {
            break;
        }
    }
}

async fn wait_udp_port_released(port: u16) {
    // The test-only native constructor binds this exact loopback address.
    let bind_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match UdpSocket::bind(bind_addr) {
                Ok(socket) => {
                    drop(socket);
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                Err(error) => panic!("checking released native UDP port: {error}"),
            }
        }
    })
    .await
    .expect("native endpoint released its listening port");
}

async fn shutdown_native_pair(
    source: &Arc<Mutex<Core>>,
    receiver: &Arc<Mutex<Core>>,
    tasks: Vec<JoinHandle<()>>,
) {
    stop_native_pumps(tasks).await;
    for core in [source, receiver] {
        let mut core = core.lock().await;
        assert!(request(&mut core, "app.shutdown", json!({})).await.ok);
        core.shutdown().await.expect("native daemon shutdown");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_repeated_reconnects_do_not_exhaust_accept_queue() {
    let _native_load = NATIVE_TIMING.read().await;
    const RECONNECTS: usize = 40;
    let a_dir = real_tempdir().expect("A directory");
    let b_dir = real_tempdir().expect("B directory");
    let store = TestKeyStore::default();
    let mut a_core = native_core(a_dir.path(), 0, Os::Windows, &store).await;
    let mut b_core = native_core(b_dir.path(), 0, Os::Macos, &store).await;
    pair_native_cores(&mut a_core, &mut b_core).await;

    let a_id = a_core.state.self_info.device_id.clone();
    let b_id = b_core.state.self_info.device_id.clone();
    let a_addr = a_core
        .native_manager
        .as_ref()
        .expect("A native manager")
        .local_addr()
        .expect("A loopback address")
        .to_string();
    let b_addr = b_core
        .native_manager
        .as_ref()
        .expect("B native manager")
        .local_addr()
        .expect("B loopback address")
        .to_string();
    let (dialer, acceptor, dialer_id, acceptor_id, acceptor_addr) = if a_id < b_id {
        (
            Arc::new(Mutex::new(a_core)),
            Arc::new(Mutex::new(b_core)),
            a_id,
            b_id,
            b_addr,
        )
    } else {
        (
            Arc::new(Mutex::new(b_core)),
            Arc::new(Mutex::new(a_core)),
            b_id,
            a_id,
            a_addr,
        )
    };

    let mut tasks = Vec::new();
    tasks.extend(spawn_native_pumps(dialer.clone()).await);
    tasks.extend(spawn_native_pumps(acceptor.clone()).await);
    let mut dialer_token = dialer
        .lock()
        .await
        .link()
        .peer_token(&acceptor_id)
        .expect("initial dialer peer token");
    let mut acceptor_token = acceptor
        .lock()
        .await
        .link()
        .peer_token(&dialer_id)
        .expect("initial acceptor peer token");

    tokio::time::timeout(Duration::from_secs(60), async {
        for reconnect in 0..RECONNECTS {
            dialer
                .lock()
                .await
                .link()
                .close(&acceptor_id)
                .await
                .expect("close connection before explicit reconnect");

            tokio::time::timeout(TEST_TIMEOUT, async {
                loop {
                    dialer
                        .lock()
                        .await
                        .tick()
                        .await
                        .expect("dialer disconnect tick");
                    acceptor
                        .lock()
                        .await
                        .tick()
                        .await
                        .expect("acceptor disconnect tick");
                    // The dialer's automatic redial can restore the session before the acceptor
                    // notices the close, so "gone" means the old session's token is gone.
                    let dialer_disconnected =
                        dialer.lock().await.link().peer_token(&acceptor_id).ok()
                            != Some(dialer_token);
                    let acceptor_disconnected =
                        acceptor.lock().await.link().peer_token(&dialer_id).ok()
                            != Some(acceptor_token);
                    if dialer_disconnected && acceptor_disconnected {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("both peers disconnect before reconnect {reconnect}"));

            let dialer_link = dialer.lock().await.link();
            tokio::time::timeout(
                TEST_TIMEOUT,
                dialer_link.connect(&acceptor_addr, Some(&acceptor_id)),
            )
            .await
            .unwrap_or_else(|_| panic!("explicit connection {reconnect} timed out"))
            .unwrap_or_else(|error| panic!("explicit connection {reconnect} failed: {error}"));

            let (next_dialer_token, next_acceptor_token) =
                tokio::time::timeout(TEST_TIMEOUT, async {
                    loop {
                        dialer
                            .lock()
                            .await
                            .tick()
                            .await
                            .expect("dialer reconnect tick");
                        acceptor
                            .lock()
                            .await
                            .tick()
                            .await
                            .expect("acceptor reconnect tick");
                        let dialer_core = dialer.lock().await;
                        let next_dialer_token = dialer_core.link().peer_token(&acceptor_id).ok();
                        let dialer_online = dialer_core
                            .state
                            .peers
                            .iter()
                            .any(|peer| peer.device_id == acceptor_id && peer.online);
                        drop(dialer_core);
                        let acceptor_core = acceptor.lock().await;
                        let next_acceptor_token = acceptor_core.link().peer_token(&dialer_id).ok();
                        let acceptor_online = acceptor_core
                            .state
                            .peers
                            .iter()
                            .any(|peer| peer.device_id == dialer_id && peer.online);
                        drop(acceptor_core);
                        if let (Some(dialer_token), Some(acceptor_token)) =
                            (next_dialer_token, next_acceptor_token)
                        {
                            if dialer_online && acceptor_online {
                                break (dialer_token, acceptor_token);
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(2)).await;
                    }
                })
                .await
                .unwrap_or_else(|_| {
                    panic!("peer tokens were not restored after reconnect {reconnect}")
                });

            assert_ne!(
                next_dialer_token, dialer_token,
                "dialer must expose the current authenticated token after reconnect {reconnect}"
            );
            assert_ne!(
                next_acceptor_token, acceptor_token,
                "acceptor must expose the current authenticated token after reconnect {reconnect}"
            );
            dialer_token = next_dialer_token;
            acceptor_token = next_acceptor_token;
        }
    })
    .await
    .expect("40 authenticated QUIC reconnects complete without exhausting accept queue");

    shutdown_native_pair(&dialer, &acceptor, tasks).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_pairing_rejects_wrong_code_and_requires_dual_matching_sas() {
    let _native_load = NATIVE_TIMING.read().await;
    let host_dir = real_tempdir().expect("host directory");
    let joiner_dir = real_tempdir().expect("joiner directory");
    let store = TestKeyStore::default();
    let mut host = native_core(host_dir.path(), 0, Os::Windows, &store).await;
    let mut joiner = native_core(joiner_dir.path(), 0, Os::Macos, &store).await;
    let started = request(&mut host, "pairing.start_host", json!({})).await;
    assert!(started.ok);
    let code = started.result.as_ref().expect("host code")["code"]
        .as_str()
        .expect("code")
        .to_owned();
    let address = host
        .native_manager
        .as_ref()
        .expect("manager")
        .local_addr()
        .expect("address")
        .to_string();
    let wrong = if code == "000000" { "999999" } else { "000000" };
    assert!(joiner
        .handle(Request {
            id: 9,
            method: "pairing.join".into(),
            params: json!({"address":address,"code":wrong}),
        })
        .await
        .is_none());
    let wrong_code = tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            joiner.tick().await.expect("wrong-code tick");
            if let Some(response) = joiner.take_responses().into_iter().next() {
                break response;
            }
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
    })
    .await
    .expect("wrong-code response");
    assert_eq!(
        wrong_code.error.as_ref().expect("bad-code error").code,
        ErrorCode::BadCode
    );
    assert!(host.state.peers.is_empty() && joiner.state.peers.is_empty());

    // The one-time host window remains available after a single incorrect attempt.
    let correct = host_started_code(&mut host).await;
    assert_eq!(correct, code);
    let address = host
        .native_manager
        .as_ref()
        .expect("manager")
        .local_addr()
        .expect("address")
        .to_string();
    assert!(joiner
        .handle(Request {
            id: 2,
            method: "pairing.join".into(),
            params: json!({"address":address,"code":correct}),
        })
        .await
        .is_none());
    let (host_verify, joiner_verify) = wait_pairing_verifies(&mut host, &mut joiner).await;
    assert_eq!(host_verify.phrase, joiner_verify.phrase);
    assert!(host.state.peers.is_empty() && joiner.state.peers.is_empty());
    assert!(
        request(&mut host, "pairing.confirm", json!({"accepted":true}))
            .await
            .ok
    );
    assert!(
        request(&mut joiner, "pairing.confirm", json!({"accepted":true}))
            .await
            .ok
    );
    wait_pairing_result(&mut host, &mut joiner).await;
    wait_connected(&mut host, &mut joiner).await;
    assert_eq!(host.state.peers[0].os, Os::Macos);
    assert_eq!(joiner.state.peers[0].os, Os::Windows);
}

async fn host_started_code(host: &mut Core) -> String {
    request(host, "pairing.start_host", json!({}))
        .await
        .result
        .as_ref()
        .expect("host start")
        .get("code")
        .and_then(Value::as_str)
        .expect("active code")
        .to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_sas_decline_never_pins_either_side() {
    let _native_load = NATIVE_TIMING.read().await;
    let host_dir = real_tempdir().expect("host directory");
    let joiner_dir = real_tempdir().expect("joiner directory");
    let store = TestKeyStore::default();
    let mut host = native_core(host_dir.path(), 0, Os::Windows, &store).await;
    let mut joiner = native_core(joiner_dir.path(), 0, Os::Macos, &store).await;
    let code = host_started_code(&mut host).await;
    let address = host
        .native_manager
        .as_ref()
        .expect("manager")
        .local_addr()
        .expect("address")
        .to_string();
    assert!(joiner
        .handle(Request {
            id: 3,
            method: "pairing.join".into(),
            params: json!({"address":address,"code":code}),
        })
        .await
        .is_none());
    let (host_verify, joiner_verify) = wait_pairing_verifies(&mut host, &mut joiner).await;
    assert_eq!(host_verify.phrase, joiner_verify.phrase);
    assert!(
        request(&mut host, "pairing.confirm", json!({"accepted":true}))
            .await
            .ok
    );
    assert!(
        request(&mut joiner, "pairing.confirm", json!({"accepted":false}))
            .await
            .ok
    );
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            host.tick().await.expect("host decline tick");
            joiner.tick().await.expect("joiner decline tick");
            if !host.state.peers.is_empty() || !joiner.state.peers.is_empty() {
                panic!("declined SAS must not pin");
            }
            let events = host.take_events();
            let join_events = joiner.take_events();
            if events
                .iter()
                .chain(join_events.iter())
                .any(|event| matches!(event, Event::PairingResult(result) if !result.ok))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("declined pairing event");
    assert!(host
        .native_manager
        .as_ref()
        .expect("manager")
        .paired_peers()
        .expect("pins")
        .is_empty());
    assert!(joiner
        .native_manager
        .as_ref()
        .expect("manager")
        .paired_peers()
        .expect("pins")
        .is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_clipboard_publishes_text_html_and_image_as_one_eager_bundle() {
    let _native_load = NATIVE_TIMING.read().await;
    let source_dir = real_tempdir().expect("source directory");
    let receiver_dir = real_tempdir().expect("receiver directory");
    let store = TestKeyStore::default();
    let mut source = native_core(source_dir.path(), 0, Os::Windows, &store).await;
    let mut receiver = native_core(receiver_dir.path(), 0, Os::Macos, &store).await;
    pair_native_cores(&mut source, &mut receiver).await;
    let receiver_id = receiver.state.self_info.device_id.clone();
    assert!(
        request(
            &mut source,
            "peer.configure",
            json!({"device_id":receiver_id,"clipboard_enabled":false})
        )
        .await
        .ok
    );
    assert!(
        request(
            &mut source,
            "set_settings",
            json!({"patch":{"clipboard":{"exclude_sensitive":true}}})
        )
        .await
        .ok
    );
    let source = Arc::new(Mutex::new(source));
    let receiver = Arc::new(Mutex::new(receiver));
    let tasks = [
        spawn_native_pumps(source.clone()).await,
        spawn_native_pumps(receiver.clone()).await,
    ];
    let png = hex::decode("89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c48900000010494441547801010500faff000000000000050001647895380000000049454e44ae426082").expect("pixel PNG");
    {
        let source = source.lock().await;
        source
            .mock_platform()
            .expect("mock")
            .clipboard
            .set_external_content(
                ClipboardContent::bytes(
                    ClipboardFormat::Text,
                    b"disabled peer probe".to_vec(),
                    ClipboardSensitivity::default(),
                )
                .expect("disabled peer probe"),
            )
            .expect("copy while peer sync is disabled");
    }
    settle_clipboard(&source).await;
    settle_clipboard(&receiver).await;
    assert!(receiver
        .lock()
        .await
        .platform
        .clipboard_backend()
        .read_snapshot()
        .expect("peer disabled snapshot")
        .contents
        .is_empty());
    assert!(
        request(
            &mut *source.lock().await,
            "peer.configure",
            json!({"device_id":receiver_id,"clipboard_enabled":true})
        )
        .await
        .ok
    );
    {
        let source = source.lock().await;
        source
            .mock_platform()
            .expect("mock")
            .clipboard
            .set_external_content(
                ClipboardContent::bytes(
                    ClipboardFormat::Text,
                    b"sensitive probe".to_vec(),
                    ClipboardSensitivity {
                        sensitive: true,
                        ..ClipboardSensitivity::default()
                    },
                )
                .expect("sensitive probe"),
            )
            .expect("copy sensitive text");
    }
    settle_clipboard(&source).await;
    settle_clipboard(&receiver).await;
    assert!(receiver
        .lock()
        .await
        .platform
        .clipboard_backend()
        .read_snapshot()
        .expect("sensitive exclusion snapshot")
        .contents
        .is_empty());
    {
        let source = source.lock().await;
        let clipboard = &source.mock_platform().expect("mock").clipboard;
        for content in [
            ClipboardContent::bytes(
                ClipboardFormat::Text,
                b"eager text".to_vec(),
                ClipboardSensitivity::default(),
            )
            .expect("text"),
            ClipboardContent::bytes(
                ClipboardFormat::Html,
                b"<b>eager</b>".to_vec(),
                ClipboardSensitivity::default(),
            )
            .expect("HTML"),
            ClipboardContent::bytes(
                ClipboardFormat::Png,
                png.clone(),
                ClipboardSensitivity::default(),
            )
            .expect("PNG"),
        ] {
            clipboard.set_external_content(content).expect("local copy");
        }
    }
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            settle_clipboard(&source).await;
            settle_clipboard(&receiver).await;
            let snapshot = receiver
                .lock()
                .await
                .platform
                .clipboard_backend()
                .read_snapshot()
                .expect("receiver clipboard snapshot");
            if snapshot.contents.len() == 3 {
                let content = |format: ClipboardFormat| {
                    snapshot
                        .contents
                        .iter()
                        .find(|content| content.format == format)
                        .expect("eager clipboard format")
                };
                assert_eq!(
                    content(ClipboardFormat::Text).data,
                    ClipboardData::Bytes(b"eager text".to_vec())
                );
                assert_eq!(
                    content(ClipboardFormat::Html).data,
                    ClipboardData::Bytes(b"<b>eager</b>".to_vec())
                );
                assert_eq!(
                    content(ClipboardFormat::Png).data,
                    ClipboardData::Bytes(png.clone())
                );
                assert!(snapshot.marker.is_some());
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("atomic eager clipboard completion");
    for task in tasks.into_iter().flatten() {
        task.abort();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_200mb_file_prefetch_reports_progress_and_publishes_verified_file() {
    let _native_load = NATIVE_TIMING.read().await;
    const FILE_BYTES: u64 = 200 * 1024 * 1024;
    const CHILD_BYTES: u64 = 1024 * 1024;
    const LARGE_IMAGE_BYTES: usize = 9 * 1024 * 1024;
    let source_dir = real_tempdir().expect("source directory");
    let receiver_dir = real_tempdir().expect("receiver directory");
    let store = TestKeyStore::default();
    let mut source_core = native_core(source_dir.path(), 0, Os::Windows, &store).await;
    let mut receiver_core = native_core(receiver_dir.path(), 0, Os::Macos, &store).await;
    pair_native_cores(&mut source_core, &mut receiver_core).await;
    let source_id = source_core.state.self_info.device_id.clone();
    let receiver_id = receiver_core.state.self_info.device_id.clone();
    assert!(
        request(
            &mut receiver_core,
            "set_settings",
            json!({"patch":{"clipboard":{"max_auto_mb":512}}})
        )
        .await
        .ok
    );
    let source_path = source_dir.path().join("payload.bin");
    let folder_path = source_dir.path().join("folder");
    std::fs::create_dir_all(&folder_path).expect("create clipboard folder root");
    let child_path = folder_path.join("child.bin");
    create_pattern_file(&source_path, FILE_BYTES);
    create_pattern_file(&child_path, CHILD_BYTES);
    let large_image = vec![0x5a; LARGE_IMAGE_BYTES];
    let source = Arc::new(Mutex::new(source_core));
    let receiver = Arc::new(Mutex::new(receiver_core));
    let mut tasks = Vec::new();
    tasks.extend(spawn_native_pumps(source.clone()).await);
    tasks.extend(spawn_native_pumps(receiver.clone()).await);
    {
        let source = source.lock().await;
        let clipboard = &source.mock_platform().expect("mock").clipboard;
        clipboard
            .set_external_content(ClipboardContent::files(FileList {
                entries: vec![
                    FileEntry {
                        path: source_path.clone(),
                        name: "payload.bin".into(),
                        size: FILE_BYTES,
                        is_dir: false,
                    },
                    FileEntry {
                        path: folder_path,
                        name: "folder".into(),
                        size: 0,
                        is_dir: true,
                    },
                ],
                sensitivity: ClipboardSensitivity::default(),
            }))
            .expect("copy 200 MB file list");
        clipboard
            .set_external_content(
                ClipboardContent::bytes(
                    ClipboardFormat::Png,
                    large_image.clone(),
                    ClipboardSensitivity::default(),
                )
                .expect("large clipboard image"),
            )
            .expect("copy large clipboard image");
    }

    let mut progress_events = 0usize;
    let completion = tokio::time::timeout(TRANSFER_TIMEOUT, async {
        loop {
            source.lock().await.poll_clipboard().await;
            receiver.lock().await.poll_clipboard().await;
            let mut receiver_core = receiver.lock().await;
            progress_events += receiver_core
                .take_events()
                .into_iter()
                .filter(|event| matches!(event, Event::TransferProgress(_)))
                .count();
            let received = receiver_core
                .state
                .transfers
                .iter()
                .find(|transfer| transfer.peer_id == source_id)
                .cloned();
            if let Some(transfer) = received.as_ref() {
                assert!(
                    !matches!(
                        transfer.state,
                        TransferState::Failed | TransferState::Cancelled
                    ),
                    "native file prefetch failed: {transfer:?}"
                );
            }
            drop(receiver_core);

            let sent = source
                .lock()
                .await
                .state
                .transfers
                .iter()
                .rev()
                .find(|transfer| {
                    transfer.peer_id == receiver_id
                        && transfer.direction == glide_proto::ipc::TransferDirection::Send
                })
                .cloned();
            if let (Some(received), Some(sent)) = (received, sent) {
                if received.state == TransferState::Done && sent.state == TransferState::Done {
                    break (received, sent);
                }
                assert!(
                    !matches!(sent.state, TransferState::Failed | TransferState::Cancelled),
                    "native sender failed: {sent:?}"
                );
            }
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
    })
    .await;
    if completion.is_err() {
        for (label, core) in [("source", &source), ("receiver", &receiver)] {
            let core = core.lock().await;
            eprintln!(
                "prefetch timeout {label}: peers={:?}; transfers={:?}",
                core.state
                    .peers
                    .iter()
                    .map(|p| (&p.device_id, p.online, core.link.peer_token(&p.device_id)))
                    .collect::<Vec<_>>(),
                core.state
                    .transfers
                    .iter()
                    .map(|t| (
                        t.direction,
                        t.state,
                        t.bytes_done,
                        t.bytes_total,
                        t.error.as_deref()
                    ))
                    .collect::<Vec<_>>()
            );
        }
    }
    let (transfer, sent_transfer) = completion.expect("200 MB verified file prefetch deadline");
    let expected_bytes = FILE_BYTES + CHILD_BYTES + LARGE_IMAGE_BYTES as u64;
    assert_eq!(transfer.bytes_total, expected_bytes);
    assert_eq!(transfer.bytes_done, expected_bytes);
    assert_eq!(sent_transfer.bytes_total, expected_bytes);
    assert_eq!(sent_transfer.bytes_done, expected_bytes);
    assert!(
        progress_events >= 2,
        "progress and completion events are visible"
    );

    let snapshot = receiver
        .lock()
        .await
        .platform
        .clipboard_backend()
        .read_snapshot()
        .expect("published native file snapshot");
    let received = snapshot
        .contents
        .iter()
        .find(|content| content.format == ClipboardFormat::Files)
        .expect("CF_HDROP/file URL representation");
    let ClipboardData::Files(files) = &received.data else {
        panic!("native clipboard file representation");
    };
    assert_eq!(files.entries.len(), 2);
    let received_file = files
        .entries
        .iter()
        .find(|entry| entry.name == "payload.bin")
        .expect("verified file root");
    assert_eq!(received_file.size, FILE_BYTES);
    assert_files_equal(&source_path, &received_file.path);
    let received_folder = files
        .entries
        .iter()
        .find(|entry| entry.name == "folder" && entry.is_dir)
        .expect("verified directory root");
    let received_child = received_folder.path.join("child.bin");
    assert_files_equal(&child_path, &received_child);
    let received_image = snapshot
        .contents
        .iter()
        .find(|content| content.format == ClipboardFormat::Png)
        .expect("large byte format included in the verified bundle");
    assert_eq!(received_image.data, ClipboardData::Bytes(large_image));
    shutdown_native_pair(&source, &receiver, tasks).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_file_transfer_resumes_after_quic_reconnect_with_same_clipboard_identity() {
    let _native_load = NATIVE_TIMING.read().await;
    const FILE_BYTES: u64 = 512 * 1024 * 1024;
    let source_dir = real_tempdir().expect("source directory");
    let receiver_dir = real_tempdir().expect("receiver directory");
    let store = TestKeyStore::default();
    let mut source_core = native_core(source_dir.path(), 0, Os::Windows, &store).await;
    let mut receiver_core = native_core(receiver_dir.path(), 0, Os::Macos, &store).await;
    pair_native_cores(&mut source_core, &mut receiver_core).await;
    let source_id = source_core.state.self_info.device_id.clone();
    let receiver_id = receiver_core.state.self_info.device_id.clone();
    let source_path = source_dir.path().join("resume.bin");
    create_sparse_file(&source_path, FILE_BYTES);
    assert!(
        request(
            &mut receiver_core,
            "set_settings",
            json!({"patch":{"clipboard":{"max_auto_mb":1024}}})
        )
        .await
        .ok
    );
    let source = Arc::new(Mutex::new(source_core));
    let receiver = Arc::new(Mutex::new(receiver_core));
    let mut tasks = Vec::new();
    tasks.extend(spawn_native_pumps(source.clone()).await);
    tasks.extend(spawn_native_pumps(receiver.clone()).await);
    copy_file_to_mock_clipboard(&source, &source_path, "resume.bin", FILE_BYTES).await;

    let before_disconnect = tokio::time::timeout(TRANSFER_TIMEOUT, async {
        loop {
            source.lock().await.poll_clipboard().await;
            receiver.lock().await.poll_clipboard().await;
            if let Some(transfer) = receiver
                .lock()
                .await
                .state
                .transfers
                .iter()
                .rev()
                .find(|transfer| transfer.peer_id == source_id)
                .cloned()
            {
                if transfer.state == TransferState::Active && transfer.bytes_done > 0 {
                    break transfer;
                }
                assert_ne!(
                    transfer.state,
                    TransferState::Done,
                    "transfer finished before link interruption"
                );
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("observe bytes in flight before reconnect");
    assert!(before_disconnect.bytes_done < FILE_BYTES);
    source
        .lock()
        .await
        .link()
        .close(&receiver_id)
        .await
        .expect("interrupt real QUIC transfer connection");

    let reconnected = tokio::time::timeout(TRANSFER_TIMEOUT, async {
        loop {
            source
                .lock()
                .await
                .tick()
                .await
                .expect("source reconnect tick");
            receiver
                .lock()
                .await
                .tick()
                .await
                .expect("receiver reconnect tick");
            let source_online = {
                let source = source.lock().await;
                source.link().peer_token(&receiver_id).is_ok()
                    && source
                        .state
                        .peers
                        .iter()
                        .any(|peer| peer.device_id == receiver_id && peer.online)
            };
            let receiver_online = {
                let receiver = receiver.lock().await;
                receiver.link().peer_token(&source_id).is_ok()
                    && receiver
                        .state
                        .peers
                        .iter()
                        .any(|peer| peer.device_id == source_id && peer.online)
            };
            if source_online && receiver_online {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    if reconnected.is_err() {
        let a = source.lock().await;
        let b = receiver.lock().await;
        eprintln!(
            "reconnect state: tokens={:?}/{:?}, online={:?}/{:?}, transfers={:?}/{:?}",
            a.link().peer_token(&receiver_id),
            b.link().peer_token(&source_id),
            a.state.peers.iter().map(|p| p.online).collect::<Vec<_>>(),
            b.state.peers.iter().map(|p| p.online).collect::<Vec<_>>(),
            a.state
                .transfers
                .iter()
                .map(|t| t.state)
                .collect::<Vec<_>>(),
            b.state
                .transfers
                .iter()
                .map(|t| t.state)
                .collect::<Vec<_>>()
        );
    }
    reconnected.expect("NativePeerManager authenticated reconnect");

    let resumed = tokio::time::timeout(TRANSFER_TIMEOUT, async {
        loop {
            source.lock().await.poll_clipboard().await;
            receiver.lock().await.poll_clipboard().await;
            if let Some(transfer) = receiver
                .lock()
                .await
                .state
                .transfers
                .iter()
                .find(|transfer| transfer.id == before_disconnect.id)
                .cloned()
            {
                assert!(
                    !matches!(
                        transfer.state,
                        TransferState::Failed | TransferState::Cancelled
                    ),
                    "resumed transfer failed: {transfer:?}"
                );
                if transfer.state == TransferState::Done {
                    break transfer;
                }
            }
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
    })
    .await
    .expect("resumed file transfer completed with original identity");
    assert_eq!(resumed.id, before_disconnect.id);
    assert_eq!(resumed.bytes_total, FILE_BYTES);
    assert_eq!(resumed.bytes_done, FILE_BYTES);
    let snapshot = receiver
        .lock()
        .await
        .platform
        .clipboard_backend()
        .read_snapshot()
        .expect("resumed clipboard publication");
    let files = snapshot
        .contents
        .iter()
        .find(|content| content.format == ClipboardFormat::Files)
        .expect("resumed file list");
    let ClipboardData::Files(files) = &files.data else {
        panic!("resumed native clipboard file list");
    };
    assert_eq!(files.entries[0].name, "resume.bin");
    assert_files_equal(&source_path, &files.entries[0].path);
    shutdown_native_pair(&source, &receiver, tasks).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_clipboard_file_requires_confirmation_above_threshold_and_cancel_preserves_old_copy()
{
    let _native_load = NATIVE_TIMING.read().await;
    const FILE_BYTES: u64 = 64 * 1024;
    let source_dir = real_tempdir().expect("source directory");
    let receiver_dir = real_tempdir().expect("receiver directory");
    let store = TestKeyStore::default();
    let mut source_core = native_core(source_dir.path(), 0, Os::Windows, &store).await;
    let mut receiver_core = native_core(receiver_dir.path(), 0, Os::Macos, &store).await;
    pair_native_cores(&mut source_core, &mut receiver_core).await;
    let source_id = source_core.state.self_info.device_id.clone();
    assert!(
        request(
            &mut receiver_core,
            "set_settings",
            json!({"patch":{"clipboard":{"max_auto_mb":0}}})
        )
        .await
        .ok
    );
    let accepted_path = source_dir.path().join("accepted.bin");
    let cancelled_path = source_dir.path().join("cancelled.bin");
    create_sparse_file(&accepted_path, FILE_BYTES);
    create_sparse_file(&cancelled_path, FILE_BYTES);
    let source = Arc::new(Mutex::new(source_core));
    let receiver = Arc::new(Mutex::new(receiver_core));
    let mut tasks = Vec::new();
    tasks.extend(spawn_native_pumps(source.clone()).await);
    tasks.extend(spawn_native_pumps(receiver.clone()).await);

    copy_file_to_mock_clipboard(&source, &accepted_path, "accepted.bin", FILE_BYTES).await;
    let first = wait_native_transfer(
        &source,
        &receiver,
        &source_id,
        TransferState::AwaitingConfirm,
        TRANSFER_TIMEOUT,
    )
    .await;
    assert!(receiver
        .lock()
        .await
        .platform
        .clipboard_backend()
        .read_snapshot()
        .expect("clipboard unchanged while consent is pending")
        .contents
        .is_empty());
    assert!(receiver
        .lock()
        .await
        .take_events()
        .iter()
        .any(|event| matches!(event, Event::Notification(notification) if notification.action.as_ref().is_some_and(|action| action["method"] == "transfer.confirm"))));
    assert!(
        request(
            &mut *receiver.lock().await,
            "transfer.confirm",
            json!({"id":first.id,"accept":true})
        )
        .await
        .ok
    );
    let first_done = wait_native_transfer(
        &source,
        &receiver,
        &source_id,
        TransferState::Done,
        TRANSFER_TIMEOUT,
    )
    .await;
    assert_eq!(first_done.bytes_done, FILE_BYTES);
    let original_snapshot = receiver
        .lock()
        .await
        .platform
        .clipboard_backend()
        .read_snapshot()
        .expect("accepted clipboard copy");
    let original_files = original_snapshot
        .contents
        .iter()
        .find(|content| content.format == ClipboardFormat::Files)
        .expect("accepted file list");
    let ClipboardData::Files(original_files) = &original_files.data else {
        panic!("published file list");
    };
    assert_eq!(original_files.entries[0].name, "accepted.bin");

    copy_file_to_mock_clipboard(&source, &cancelled_path, "cancelled.bin", FILE_BYTES).await;
    let second = wait_native_transfer(
        &source,
        &receiver,
        &source_id,
        TransferState::AwaitingConfirm,
        TRANSFER_TIMEOUT,
    )
    .await;
    assert!(
        request(
            &mut *receiver.lock().await,
            "transfer.cancel",
            json!({"id":second.id})
        )
        .await
        .ok
    );
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            receiver.lock().await.poll_clipboard().await;
            let state = receiver
                .lock()
                .await
                .state
                .transfers
                .iter()
                .find(|transfer| transfer.id == second.id)
                .map(|transfer| transfer.state);
            if state == Some(TransferState::Cancelled) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
    })
    .await
    .expect("cancelled transfer terminal state");
    let after_cancel = receiver
        .lock()
        .await
        .platform
        .clipboard_backend()
        .read_snapshot()
        .expect("clipboard after cancel");
    assert_eq!(after_cancel.change_token, original_snapshot.change_token);
    let after_files = after_cancel
        .contents
        .iter()
        .find(|content| content.format == ClipboardFormat::Files)
        .expect("prior file copy remains published");
    let ClipboardData::Files(after_files) = &after_files.data else {
        panic!("prior file list");
    };
    assert_eq!(after_files.entries[0].name, "accepted.bin");
    shutdown_native_pair(&source, &receiver, tasks).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_unpair_mid_clipboard_and_input_forward_cancels_and_releases_keys() {
    interrupt_native_clipboard_and_input(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_during_active_native_transfer_releases_input_within_two_seconds() {
    interrupt_native_clipboard_and_input(true).await;
}

async fn interrupt_native_clipboard_and_input(shutdown: bool) {
    let _native_load = NATIVE_TIMING.write().await;
    const FILE_BYTES: u64 = 512 * 1024 * 1024;
    let source_dir = real_tempdir().expect("source directory");
    let receiver_dir = real_tempdir().expect("receiver directory");
    let store = TestKeyStore::default();
    let mut source_core = native_core(source_dir.path(), 0, Os::Windows, &store).await;
    let mut receiver_core = native_core(receiver_dir.path(), 0, Os::Macos, &store).await;
    pair_native_cores(&mut source_core, &mut receiver_core).await;
    let source_id = source_core.state.self_info.device_id.clone();
    let receiver_id = receiver_core.state.self_info.device_id.clone();
    assert!(
        request(
            &mut source_core,
            "set_layout",
            json!({"devices":[
                {"device_id":source_id,"x":0.0,"y":0.0},
                {"device_id":receiver_id,"x":1920.0,"y":0.0}
            ]})
        )
        .await
        .ok
    );
    assert!(
        request(
            &mut source_core,
            "set_settings",
            json!({"patch":{"switching":{"edge_delay_ms":0,"corner_dead_zone_px":0}}})
        )
        .await
        .ok
    );
    let source_path = source_dir.path().join("interrupt.bin");
    create_sparse_file(&source_path, FILE_BYTES);
    assert!(
        request(
            &mut receiver_core,
            "set_settings",
            json!({"patch":{"clipboard":{"max_auto_mb":1024}}})
        )
        .await
        .ok
    );
    let source = Arc::new(Mutex::new(source_core));
    let receiver = Arc::new(Mutex::new(receiver_core));
    let mut tasks = Vec::new();
    tasks.extend(spawn_native_pumps(source.clone()).await);
    tasks.extend(spawn_native_pumps(receiver.clone()).await);
    wait_native_layout(&source, &receiver).await;
    enter_edge(&mut *source.lock().await, 200.0, 2000.0)
        .await
        .expect("A forwards to B");
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            if receiver.lock().await.receiving_from.as_deref() == Some(&source_id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
    })
    .await
    .expect("B admits A's input epoch");
    let epoch = source.lock().await.outgoing_epoch;
    source
        .lock()
        .await
        .capture_input(InputEvent {
            injected: false,
            kind: InputEventKind::Key {
                key: Key(4),
                down: true,
            },
        })
        .await
        .expect("forwarded key down");
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            if receiver
                .lock()
                .await
                .mock_platform()
                .expect("receiver mock")
                .input
                .held_keys()
                .expect("held keys")
                .contains(&Key(4))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
    })
    .await
    .expect("B holds A's forwarded key");
    copy_file_to_mock_clipboard(&source, &source_path, "interrupt.bin", FILE_BYTES).await;

    let transfer = tokio::time::timeout(TRANSFER_TIMEOUT, async {
        loop {
            source.lock().await.poll_clipboard().await;
            receiver.lock().await.poll_clipboard().await;
            let transfer = receiver
                .lock()
                .await
                .state
                .transfers
                .iter()
                .find(|transfer| transfer.peer_id == source_id)
                .cloned();
            if let Some(transfer) = transfer {
                if transfer.state == TransferState::Active && transfer.bytes_done > 0 {
                    break transfer;
                }
                assert!(
                    !matches!(
                        transfer.state,
                        TransferState::Failed | TransferState::Cancelled | TransferState::Done
                    ),
                    "unpair test missed active transfer: {transfer:?}"
                );
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("observe active native file transfer before unpair");
    assert!(
        transfer.bytes_done < FILE_BYTES,
        "unpair must interrupt a partial transfer"
    );
    if shutdown {
        let started = Instant::now();
        {
            let mut core = receiver.lock().await;
            let input = core.mock_platform().expect("receiver input").input.clone();
            assert!(request(&mut core, "app.shutdown", json!({})).await.ok);
            core.shutdown().await.expect("shutdown during transfer");
            assert!(started.elapsed() < Duration::from_secs(2));
            assert!(input.held_keys().expect("released keys").is_empty());
            assert_eq!(input.mode().expect("local capture"), CaptureMode::Local);
            assert_eq!(
                core.state
                    .transfers
                    .iter()
                    .find(|entry| entry.id == transfer.id)
                    .expect("transfer remains visible")
                    .state,
                TransferState::Cancelled
            );
        }
        for task in tasks {
            task.abort();
            let _ = task.await;
        }
        source
            .lock()
            .await
            .shutdown()
            .await
            .expect("source cleanup");
        return;
    }
    assert!(
        request(
            &mut *receiver.lock().await,
            "peer.unpair",
            json!({"device_id":source_id.clone()})
        )
        .await
        .ok
    );
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            let source_home = source.lock().await.state.active_device_id == source_id;
            let receiver_core = receiver.lock().await;
            let receiver_home = receiver_core.receiving_from.is_none()
                && receiver_core.state.active_device_id == receiver_id
                && receiver_core
                    .mock_platform()
                    .expect("receiver mock")
                    .input
                    .held_keys()
                    .expect("released remote keys")
                    .is_empty();
            if source_home && receiver_home {
                break;
            }
            drop(receiver_core);
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
    })
    .await
    .expect("unpair returns home and releases the held key");
    assert!(source.lock().await.link().peer_token(&receiver_id).is_err());
    assert!(receiver.lock().await.link().peer_token(&source_id).is_err());
    assert!(receiver
        .lock()
        .await
        .link()
        .admit_input_epoch(&source_id, epoch)
        .is_err());
    let cancelled = wait_native_transfer(
        &source,
        &receiver,
        &source_id,
        TransferState::Cancelled,
        TEST_TIMEOUT,
    )
    .await;
    assert_eq!(cancelled.id, transfer.id);
    assert!(receiver
        .lock()
        .await
        .platform
        .clipboard_backend()
        .read_snapshot()
        .expect("no partial clipboard publication")
        .contents
        .is_empty());
    shutdown_native_pair(&source, &receiver, tasks).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_three_device_layout_crossing_routes_a_to_b_to_c_back_to_a() {
    let _native_load = NATIVE_TIMING.write().await;
    let dirs = [
        real_tempdir().expect("A directory"),
        real_tempdir().expect("B directory"),
        real_tempdir().expect("C directory"),
    ];
    let store = TestKeyStore::default();
    let mut a = native_core(dirs[0].path(), 0, Os::Windows, &store).await;
    let mut b = native_core(dirs[1].path(), 0, Os::Macos, &store).await;
    let mut c = native_core(dirs[2].path(), 0, Os::Windows, &store).await;
    pair_native_cores(&mut a, &mut b).await;
    pair_native_cores(&mut b, &mut c).await;
    pair_native_cores(&mut c, &mut a).await;
    let ids = [
        a.state.self_info.device_id.clone(),
        b.state.self_info.device_id.clone(),
        c.state.self_info.device_id.clone(),
    ];
    let devices = json!([
        {"device_id":ids[0],"x":0.0,"y":0.0},
        {"device_id":ids[1],"x":1920.0,"y":0.0},
        {"device_id":ids[2],"x":960.0,"y":1080.0}
    ]);
    assert!(
        request(&mut a, "set_layout", json!({"devices":devices}))
            .await
            .ok
    );
    for core in [&mut a, &mut b, &mut c] {
        assert!(
            request(
                core,
                "set_settings",
                json!({"patch":{"switching":{"edge_delay_ms":0,"corner_dead_zone_px":0}}})
            )
            .await
            .ok
        );
    }
    let a = Arc::new(Mutex::new(a));
    let b = Arc::new(Mutex::new(b));
    let c = Arc::new(Mutex::new(c));
    let mut tasks = Vec::new();
    for core in [a.clone(), b.clone(), c.clone()] {
        tasks.extend(spawn_native_pumps(core).await);
    }
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            let a_layout = a.lock().await.state.layout.clone();
            let b_layout = b.lock().await.state.layout.clone();
            let c_layout = c.lock().await.state.layout.clone();
            if a_layout == b_layout && a_layout == c_layout {
                break;
            }
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
    })
    .await
    .expect("LWW layout replication to B and C");
    {
        let mut a_core = a.lock().await;
        enter_edge(&mut a_core, 200.0, 2000.0)
            .await
            .expect("A to B edge crossing");
    }
    {
        tokio::time::timeout(TEST_TIMEOUT, async {
            loop {
                let admitted = {
                    let a_core = a.lock().await;
                    let b_core = b.lock().await;
                    a_core.state.active_device_id == ids[1]
                        && b_core.receiving_from.as_deref() == Some(&ids[0])
                };
                if admitted {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(3)).await;
            }
        })
        .await
        .expect("A to B accepted Enter epoch");
        let a_core = a.lock().await;
        let b_core = b.lock().await;
        a_core
            .mock_platform()
            .expect("A mock")
            .input
            .take_injected_events()
            .expect("A input");
        b_core
            .mock_platform()
            .expect("B mock")
            .input
            .take_injected_events()
            .expect("B input");
    }
    {
        let mut a_core = a.lock().await;
        a_core
            .capture_input(InputEvent {
                injected: false,
                kind: InputEventKind::Key {
                    key: Key(0xe0),
                    down: true,
                },
            })
            .await
            .expect("forward Windows Ctrl to Mac");
    }
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            let events = b
                .lock()
                .await
                .mock_platform()
                .expect("B mock")
                .input
                .take_injected_events()
                .expect("B events");
            if events.iter().any(|event| {
                matches!(
                    event.kind,
                    InputEventKind::Key {
                        key: Key(0xe3),
                        down: true
                    }
                )
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
    })
    .await
    .expect("Windows Ctrl to Mac Command translation");
    {
        let mut a_core = a.lock().await;
        a_core
            .capture_input(InputEvent {
                injected: false,
                kind: InputEventKind::Key {
                    key: Key(0xe0),
                    down: false,
                },
            })
            .await
            .expect("release translated modifier");
    }
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            if b.lock()
                .await
                .mock_platform()
                .expect("B mock")
                .input
                .held_keys()
                .expect("B held keys")
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
    })
    .await
    .expect("translated modifier release");
    {
        let mut a_core = a.lock().await;
        move_pointer(&mut a_core, 0.0, 1000.0)
            .await
            .expect("same A brain crosses B to C");
    }
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            if a.lock().await.state.active_device_id == ids[2]
                && c.lock().await.receiving_from.as_deref() == Some(&ids[0])
                && b.lock().await.receiving_from.is_none()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
    })
    .await
    .expect("B to C accepted Enter epoch");

    {
        let mut a_core = a.lock().await;
        move_pointer(&mut a_core, -500.0, 0.0)
            .await
            .expect("move inside C toward A's display span");
        move_pointer(&mut a_core, 0.0, -1000.0)
            .await
            .expect("same A brain crosses C directly to A");
    }
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            if a.lock().await.state.active_device_id == ids[0]
                && b.lock().await.receiving_from.is_none()
                && c.lock().await.receiving_from.is_none()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
    })
    .await
    .expect("same A brain returns directly from C to A");
    assert_eq!(a.lock().await.state.active_device_id, ids[0]);

    // A local physical event on B explicitly takes control from that remote brain.
    {
        let mut a_core = a.lock().await;
        enter_edge(&mut a_core, 200.0, 2000.0)
            .await
            .expect("re-enter B for takeover");
    }
    {
        let mut b_core = b.lock().await;
        b_core
            .capture_input(InputEvent {
                injected: false,
                kind: InputEventKind::Key {
                    key: Key(4),
                    down: true,
                },
            })
            .await
            .expect("B local takeover");
    }
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            if a.lock().await.state.active_device_id == ids[1] {
                break;
            }
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
    })
    .await
    .expect("TakeOver changes the active brain to B");
    b.lock()
        .await
        .capture_input(InputEvent {
            injected: false,
            kind: InputEventKind::Key {
                key: Key(4),
                down: false,
            },
        })
        .await
        .expect("release B takeover key");

    // B is macOS after TakeOver; crossing into Windows C verifies the reverse mapping.
    {
        let mut b_core = b.lock().await;
        let dx = 2400.0 - b_core.engine.cursor().x;
        move_pointer(&mut b_core, dx, 0.0)
            .await
            .expect("move inside B toward C's display span");
        move_pointer(&mut b_core, 0.0, 1200.0)
            .await
            .expect("B crosses its lower edge to C");
    }
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            if b.lock().await.state.active_device_id == ids[2]
                && c.lock().await.receiving_from.as_deref() == Some(&ids[1])
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
    })
    .await
    .expect("B to C accepted Mac-origin input epoch");
    {
        let mut b_core = b.lock().await;
        b_core
            .capture_input(InputEvent {
                injected: false,
                kind: InputEventKind::Key {
                    key: Key(0xe3),
                    down: true,
                },
            })
            .await
            .expect("forward Mac Cmd to Windows");
    }
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            let events = c
                .lock()
                .await
                .mock_platform()
                .expect("C mock")
                .input
                .take_injected_events()
                .expect("C events");
            if events.iter().any(|event| {
                matches!(
                    event.kind,
                    InputEventKind::Key {
                        key: Key(0xe0),
                        down: true
                    }
                )
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
    })
    .await
    .expect("Mac Cmd to Windows Ctrl translation (Cmd+C must never reach Windows as the Win key)");
    {
        let mut b_core = b.lock().await;
        b_core
            .capture_input(InputEvent {
                injected: false,
                kind: InputEventKind::Key {
                    key: Key(0xe3),
                    down: false,
                },
            })
            .await
            .expect("release translated Mac modifier");
    }
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            if c.lock()
                .await
                .mock_platform()
                .expect("C mock")
                .input
                .held_keys()
                .expect("C held keys")
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
    })
    .await
    .expect("translated Mac modifier release");
    assert!(b
        .lock()
        .await
        .mock_platform()
        .expect("B mock")
        .input
        .held_keys()
        .expect("B held keys")
        .is_empty());
    stop_native_pumps(tasks).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_stale_input_epoch_is_rejected_by_link_and_core() {
    let _native_load = NATIVE_TIMING.write().await;
    let a_dir = real_tempdir().expect("A directory");
    let b_dir = real_tempdir().expect("B directory");
    let store = TestKeyStore::default();
    let mut a = native_core(a_dir.path(), 0, Os::Windows, &store).await;
    let mut b = native_core(b_dir.path(), 0, Os::Macos, &store).await;
    pair_native_cores(&mut a, &mut b).await;
    let a_id = a.state.self_info.device_id.clone();
    let b_id = b.state.self_info.device_id.clone();
    assert!(
        request(
            &mut a,
            "set_layout",
            json!({"devices":[
                {"device_id":a_id,"x":0.0,"y":0.0},
                {"device_id":b_id,"x":1920.0,"y":0.0}
            ]})
        )
        .await
        .ok
    );
    assert!(
        request(
            &mut a,
            "set_settings",
            json!({"patch":{"switching":{"edge_delay_ms":0,"corner_dead_zone_px":0}}})
        )
        .await
        .ok
    );
    let a = Arc::new(Mutex::new(a));
    let b = Arc::new(Mutex::new(b));
    let mut tasks = Vec::new();
    tasks.extend(spawn_native_pumps(a.clone()).await);
    tasks.extend(spawn_native_pumps(b.clone()).await);
    wait_native_layout(&a, &b).await;
    {
        enter_edge(&mut *a.lock().await, 200.0, 2000.0)
            .await
            .expect("first epoch");
    }
    {
        let mut a_core = a.lock().await;
        assert!(request(&mut a_core, "return_home", json!({})).await.ok);
    }
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            if b.lock().await.receiving_from.is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
    })
    .await
    .expect("first Leave revokes admission");
    {
        enter_edge(&mut *a.lock().await, 200.0, 2000.0)
            .await
            .expect("second epoch");
    }
    let current_epoch = a.lock().await.outgoing_epoch;
    assert_eq!(current_epoch, 2);
    let peer_token = b.lock().await.link().peer_token(&a_id).expect("peer token");
    b.lock()
        .await
        .mock_platform()
        .expect("B mock")
        .input
        .take_injected_events()
        .expect("clear Enter events");
    let stale = WireMessage::Input(InputMessage::Key(InputKey {
        epoch: current_epoch - 1,
        seq: 1,
        hid_usage: Key(4),
        down: true,
    }));
    assert!(a
        .lock()
        .await
        .link()
        .send_reliable(&b_id, stale.clone())
        .await
        .is_err());
    b.lock()
        .await
        .receive_link(glide_net::LinkEvent::Reliable {
            peer_token: Some(peer_token),
            peer_id: a_id,
            message: stale,
        })
        .await
        .expect("stale Core event is ignored");
    assert!(b
        .lock()
        .await
        .mock_platform()
        .expect("B mock")
        .input
        .held_keys()
        .expect("no stale key hold")
        .is_empty());
    stop_native_pumps(tasks).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_link_loss_returns_home_and_releases_remote_key_holds() {
    let _native_load = NATIVE_TIMING.write().await;
    let a_dir = real_tempdir().expect("A directory");
    let b_dir = real_tempdir().expect("B directory");
    let store = TestKeyStore::default();
    let mut a = native_core(a_dir.path(), 0, Os::Windows, &store).await;
    let mut b = native_core(b_dir.path(), 0, Os::Macos, &store).await;
    pair_native_cores(&mut a, &mut b).await;
    let a_id = a.state.self_info.device_id.clone();
    let b_id = b.state.self_info.device_id.clone();
    assert!(
        request(
            &mut a,
            "set_layout",
            json!({"devices":[
                {"device_id":a_id,"x":0.0,"y":0.0},
                {"device_id":b_id,"x":1920.0,"y":0.0}
            ]})
        )
        .await
        .ok
    );
    assert!(
        request(
            &mut a,
            "set_settings",
            json!({"patch":{"switching":{"edge_delay_ms":0,"corner_dead_zone_px":0}}})
        )
        .await
        .ok
    );
    let a = Arc::new(Mutex::new(a));
    let b = Arc::new(Mutex::new(b));
    let mut tasks = Vec::new();
    tasks.extend(spawn_native_pumps(a.clone()).await);
    tasks.extend(spawn_native_pumps(b.clone()).await);
    wait_native_layout(&a, &b).await;
    enter_edge(&mut *a.lock().await, 200.0, 2000.0)
        .await
        .expect("A enters B");
    a.lock()
        .await
        .capture_input(InputEvent {
            injected: false,
            kind: InputEventKind::Key {
                key: Key(4),
                down: true,
            },
        })
        .await
        .expect("key down delivered");
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            if b.lock()
                .await
                .mock_platform()
                .expect("B mock")
                .input
                .held_keys()
                .expect("B key state")
                .contains(&Key(4))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
    })
    .await
    .expect("real QUIC key is held on B");
    a.lock()
        .await
        .link()
        .close(&b_id)
        .await
        .expect("kill authenticated connection");
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            let a_core = a.lock().await;
            let b_core = b.lock().await;
            if a_core.state.active_device_id == a_id
                && b_core
                    .mock_platform()
                    .expect("B mock")
                    .input
                    .held_keys()
                    .expect("released keys")
                    .is_empty()
            {
                break;
            }
            drop(a_core);
            drop(b_core);
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
    })
    .await
    .expect("link loss returns home and releases keys");
    stop_native_pumps(tasks).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_name_only_metadata_update_preserves_active_input_and_key_holds() {
    let _native_load = NATIVE_TIMING.write().await;
    let a_dir = real_tempdir().expect("A directory");
    let b_dir = real_tempdir().expect("B directory");
    let store = TestKeyStore::default();
    let mut a = native_core(a_dir.path(), 0, Os::Windows, &store).await;
    let mut b = native_core(b_dir.path(), 0, Os::Macos, &store).await;
    pair_native_cores(&mut a, &mut b).await;
    let a_id = a.state.self_info.device_id.clone();
    let b_id = b.state.self_info.device_id.clone();
    assert!(
        request(
            &mut a,
            "set_layout",
            json!({"devices":[
                {"device_id":a_id,"x":0.0,"y":0.0},
                {"device_id":b_id,"x":1920.0,"y":0.0}
            ]})
        )
        .await
        .ok
    );
    assert!(
        request(
            &mut a,
            "set_settings",
            json!({"patch":{"switching":{"edge_delay_ms":0,"corner_dead_zone_px":0}}})
        )
        .await
        .ok
    );
    let a = Arc::new(Mutex::new(a));
    let b = Arc::new(Mutex::new(b));
    let mut tasks = Vec::new();
    tasks.extend(spawn_native_pumps(a.clone()).await);
    tasks.extend(spawn_native_pumps(b.clone()).await);
    wait_native_layout(&a, &b).await;
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            if a.lock().await.state.layout == b.lock().await.state.layout {
                break;
            }
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
    })
    .await
    .expect("layout reaches both native Cores");

    {
        let mut a_core = a.lock().await;
        enter_edge(&mut a_core, 200.0, 2000.0)
            .await
            .expect("A enters B");
    }
    {
        let mut a_core = a.lock().await;
        a_core
            .capture_input(InputEvent {
                injected: false,
                kind: InputEventKind::Key {
                    key: Key(4),
                    down: true,
                },
            })
            .await
            .expect("A key down delivered");
    }
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            if b.lock()
                .await
                .mock_platform()
                .expect("B mock")
                .input
                .held_keys()
                .expect("B held keys")
                .contains(&Key(4))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
    })
    .await
    .expect("remote key is held before metadata update");

    let outgoing_epoch = a.lock().await.outgoing_epoch;
    let receiving_epoch = b.lock().await.receiving_epoch.expect("admitted epoch");
    assert_eq!(outgoing_epoch, receiving_epoch);
    let (manager, monitors) = {
        let b_core = b.lock().await;
        (
            b_core
                .native_manager
                .as_ref()
                .expect("B native manager")
                .clone(),
            b_core.state.self_info.monitors.clone(),
        )
    };
    manager
        .update_local_metadata("B Renamed".into(), monitors)
        .expect("publish name-only peer metadata");

    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            if a.lock()
                .await
                .state
                .peers
                .iter()
                .any(|peer| peer.device_id == b_id && peer.name == "B Renamed")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
    })
    .await
    .expect("name-only metadata reaches the connected peer");

    assert_eq!(a.lock().await.state.active_device_id, b_id);
    assert_eq!(a.lock().await.outgoing_epoch, outgoing_epoch);
    assert_eq!(
        b.lock().await.receiving_from.as_deref(),
        Some(a_id.as_str())
    );
    assert_eq!(b.lock().await.receiving_epoch, Some(receiving_epoch));
    assert!(b
        .lock()
        .await
        .mock_platform()
        .expect("B mock")
        .input
        .held_keys()
        .expect("B held keys")
        .contains(&Key(4)));

    {
        let mut a_core = a.lock().await;
        a_core
            .capture_input(InputEvent {
                injected: false,
                kind: InputEventKind::Key {
                    key: Key(4),
                    down: false,
                },
            })
            .await
            .expect("A key up remains on the active input epoch");
    }
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            let b_core = b.lock().await;
            let released = b_core
                .mock_platform()
                .expect("B mock")
                .input
                .held_keys()
                .expect("B released keys")
                .is_empty();
            if released {
                assert_eq!(b_core.receiving_from.as_deref(), Some(a_id.as_str()));
                assert_eq!(b_core.receiving_epoch, Some(receiving_epoch));
                break;
            }
            drop(b_core);
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
    })
    .await
    .expect("key up releases B hold without ending forwarding");
    assert_eq!(a.lock().await.state.active_device_id, b_id);
    assert_eq!(a.lock().await.outgoing_epoch, outgoing_epoch);
    shutdown_native_pair(&a, &b, tasks).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_return_home_hotkey_completes_after_link_is_down() {
    let _native_load = NATIVE_TIMING.write().await;
    let a_dir = real_tempdir().expect("A directory");
    let b_dir = real_tempdir().expect("B directory");
    let store = TestKeyStore::default();
    let mut a = native_core(a_dir.path(), 0, Os::Windows, &store).await;
    let mut b = native_core(b_dir.path(), 0, Os::Macos, &store).await;
    pair_native_cores(&mut a, &mut b).await;
    let a_id = a.state.self_info.device_id.clone();
    let b_id = b.state.self_info.device_id.clone();
    assert!(
        request(
            &mut a,
            "set_layout",
            json!({"devices":[
                {"device_id":a_id,"x":0.0,"y":0.0},
                {"device_id":b_id,"x":1920.0,"y":0.0}
            ]})
        )
        .await
        .ok
    );
    assert!(
        request(
            &mut a,
            "set_settings",
            json!({"patch":{"switching":{"edge_delay_ms":0,"corner_dead_zone_px":0}}})
        )
        .await
        .ok
    );
    let a = Arc::new(Mutex::new(a));
    let b = Arc::new(Mutex::new(b));
    let tasks = spawn_native_pumps(b.clone()).await;
    wait_native_layout(&a, &b).await;
    enter_edge(&mut *a.lock().await, 200.0, 2000.0)
        .await
        .expect("A enters B");
    b.lock()
        .await
        .link()
        .close(&a_id)
        .await
        .expect("drop link without pumping A");
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut core = a.lock().await;
        for key in [0xe0, 0xe2, 0xe1, 0x4a] {
            let _ = core
                .capture_input(InputEvent {
                    injected: false,
                    kind: InputEventKind::Key {
                        key: Key(key),
                        down: true,
                    },
                })
                .await;
        }
        assert_eq!(core.state.active_device_id, a_id);
    })
    .await
    .expect("offline escape hotkey deadline");
    stop_native_pumps(tasks).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_metadata_hotplug_layout_prefs_and_pins_survive_restart() {
    let _native_load = NATIVE_TIMING.read().await;
    let a_dir = real_tempdir().expect("A directory");
    let b_dir = real_tempdir().expect("B directory");
    let store = TestKeyStore::default();
    let mut a = native_core(a_dir.path(), 0, Os::Windows, &store).await;
    let mut b = native_core(b_dir.path(), 0, Os::Macos, &store).await;
    let port_a = a.state.self_info.listen_port;
    let port_b = b.state.self_info.listen_port;
    pair_native_cores(&mut a, &mut b).await;
    let a_id = a.state.self_info.device_id.clone();
    let b_id = b.state.self_info.device_id.clone();
    assert!(
        request(
            &mut a,
            "set_layout",
            json!({"devices":[
                {"device_id":a_id,"x":0.0,"y":0.0},
                {"device_id":b_id,"x":1920.0,"y":0.0}
            ]})
        )
        .await
        .ok
    );
    assert!(
        request(
            &mut a,
            "peer.configure",
            json!({"device_id":b_id,"clipboard_enabled":false})
        )
        .await
        .ok
    );
    let renamed = request(
        &mut a,
        "set_settings",
        json!({"patch":{"device_name":"A Renamed","clipboard":{"max_auto_mb":37}}}),
    )
    .await;
    assert!(renamed.ok, "live native rename: {renamed:?}");
    let configured_port = a.state.settings.network.port;
    let alternate_port = if configured_port == u16::MAX {
        configured_port - 1
    } else {
        configured_port + 1
    };
    let port_change = request(
        &mut a,
        "set_settings",
        json!({"patch":{"network":{"port":alternate_port}}}),
    )
    .await;
    assert!(!port_change.ok);
    assert!(port_change
        .error
        .as_ref()
        .expect("restart-required error")
        .message
        .contains("restart"));
    let a = Arc::new(Mutex::new(a));
    let b = Arc::new(Mutex::new(b));
    let mut tasks = Vec::new();
    tasks.extend(spawn_native_pumps(a.clone()).await);
    tasks.extend(spawn_native_pumps(b.clone()).await);
    wait_native_layout(&a, &b).await;
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            b.lock().await.tick().await.expect("name update tick");
            if b.lock()
                .await
                .state
                .peers
                .iter()
                .any(|peer| peer.device_id == a_id && peer.name == "A Renamed")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
    })
    .await
    .expect("live name metadata reaches the connected peer");
    let hotplugged = Monitor {
        id: "display-hotplug".into(),
        x: 0.0,
        y: 0.0,
        w: 2100.0,
        h: 1080.0,
        scale: 1.0,
        primary: true,
    };
    a.lock()
        .await
        .mock_platform()
        .expect("A mock")
        .input
        .set_monitors(vec![hotplugged.clone()])
        .expect("simulate monitor hotplug");
    a.lock().await.tick().await.expect("apply hotplug metadata");
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            b.lock().await.tick().await.expect("monitor metadata tick");
            if b.lock()
                .await
                .state
                .peers
                .iter()
                .any(|peer| peer.device_id == a_id && peer.monitors == vec![hotplugged.clone()])
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
    })
    .await
    .expect("hotplug metadata reaches the connected peer");
    let overlapping = request(
        &mut *a.lock().await,
        "set_layout",
        json!({"devices":[
            {"device_id":a_id,"x":0.0,"y":0.0},
            {"device_id":b_id,"x":1920.0,"y":0.0}
        ]}),
    )
    .await;
    assert!(
        !overlapping.ok,
        "monitor enlargement must reject overlapping layout"
    );
    move_pointer(&mut *a.lock().await, 2200.0, 0.0)
        .await
        .expect("monitor overlap must fail closed");
    assert_eq!(a.lock().await.state.active_device_id, a_id);
    {
        let mut a_core = a.lock().await;
        let repaired = request(
            &mut a_core,
            "set_layout",
            json!({"devices":[
                {"device_id":a_id,"x":0.0,"y":0.0},
                {"device_id":b_id,"x":2100.0,"y":0.0}
            ]}),
        )
        .await;
        assert!(repaired.ok, "repair overlap after hotplug: {repaired:?}");
    }
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            if a.lock().await.state.layout == b.lock().await.state.layout {
                break;
            }
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
    })
    .await
    .expect("repaired layout replicates");
    {
        let mut a_core = a.lock().await;
        let result = request(&mut a_core, "app.shutdown", json!({})).await;
        assert!(result.ok);
    }
    {
        let mut b_core = b.lock().await;
        assert!(request(&mut b_core, "app.shutdown", json!({})).await.ok);
    }
    stop_native_pumps(tasks).await;
    a.lock().await.shutdown().await.expect("A daemon shutdown");
    b.lock().await.shutdown().await.expect("B daemon shutdown");
    drop(a);
    drop(b);
    wait_udp_port_released(port_a).await;
    wait_udp_port_released(port_b).await;

    let mut restarted_a = native_core(a_dir.path(), port_a, Os::Windows, &store).await;
    let mut restarted_b = native_core(b_dir.path(), port_b, Os::Macos, &store).await;
    assert_eq!(restarted_a.state.self_info.device_id, a_id);
    assert_eq!(restarted_b.state.self_info.device_id, b_id);
    assert_eq!(restarted_a.state.settings.device_name, "A Renamed");
    assert_eq!(restarted_a.state.settings.clipboard.max_auto_mb, 37);
    assert!(restarted_a
        .state
        .peers
        .iter()
        .any(|peer| peer.device_id == b_id && !peer.clipboard_enabled));
    assert_eq!(
        restarted_a
            .state
            .layout
            .devices
            .iter()
            .find(|device| device.device_id == b_id)
            .expect("persisted B layout")
            .x,
        2100.0
    );
    assert!(restarted_a
        .native_manager
        .as_ref()
        .expect("restarted native manager")
        .paired_peers()
        .expect("persisted pins")
        .iter()
        .any(|peer| peer.device_id == b_id));
    wait_connected(&mut restarted_a, &mut restarted_b).await;
    assert!(restarted_b
        .state
        .peers
        .iter()
        .any(|peer| peer.device_id == a_id && peer.name == "A Renamed"));
}

pub(super) async fn stop_native_pumps(tasks: Vec<JoinHandle<()>>) {
    for task in &tasks {
        task.abort();
    }
    for task in tasks {
        let _ = task.await;
    }
}
