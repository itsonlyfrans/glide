use super::*;
use crate::{Link, LinkEvent, MouseReceiver, PairTarget, PeerManager, PeerManagerEvent};
use glide_proto::{
    ipc::{ErrorCode, PairingVerify},
    wire::{self, ControlMessage, InputKey, InputMessage, Move, WireMessage},
};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::{Duration, Instant},
};
use tokio::sync::broadcast;

// Count only the calling test thread; transport/runtime worker allocations are
// excluded so this checks precisely the synchronous capture-side enqueue seam.
thread_local! {
    static COUNT_ALLOCATIONS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static ALLOCATION_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

struct CountingAllocator;

fn count_allocation() {
    let _ = COUNT_ALLOCATIONS.try_with(|enabled| {
        if enabled.get() {
            let _ = ALLOCATION_COUNT.try_with(|count| count.set(count.get() + 1));
        }
    });
}

// SAFETY: All allocation operations are forwarded unchanged to System; the
// counters use allocation-free thread-local Cells and never modify pointers.
unsafe impl std::alloc::GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        count_allocation();
        // SAFETY: The allocator caller supplies System's required valid layout.
        unsafe { std::alloc::GlobalAlloc::alloc(&std::alloc::System, layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        count_allocation();
        // SAFETY: The allocator caller supplies System's required valid layout.
        unsafe { std::alloc::GlobalAlloc::alloc_zeroed(&std::alloc::System, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, size: usize) -> *mut u8 {
        count_allocation();
        // SAFETY: The allocator caller guarantees pointer/layout ownership and size.
        unsafe { std::alloc::GlobalAlloc::realloc(&std::alloc::System, ptr, layout, size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        // SAFETY: The allocator caller supplies the original pointer and layout.
        unsafe { std::alloc::GlobalAlloc::dealloc(&std::alloc::System, ptr, layout) }
    }
}

#[global_allocator]
static TEST_ALLOCATOR: CountingAllocator = CountingAllocator;

async fn manager(name: &str) -> (tempfile::TempDir, NativePeerManager) {
    let data = tempfile::tempdir().expect("test data directory");
    let config = NativeConfig {
        data_dir: data.path().to_owned(),
        bind_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        name: name.to_owned(),
        os: glide_platform::Os::Windows,
        monitors: Vec::new(),
        discovery: false,
    };
    let manager = NativePeerManager::for_test(config)
        .await
        .expect("loopback manager");
    (data, manager)
}

async fn event(
    receiver: &mut broadcast::Receiver<PeerManagerEvent>,
    predicate: impl Fn(&PeerManagerEvent) -> bool,
) -> PeerManagerEvent {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let value = receiver.recv().await.expect("event channel");
            if predicate(&value) {
                return value;
            }
            if let PeerManagerEvent::PairingResult {
                ok: false, error, ..
            } = value
            {
                panic!("unexpected pairing failure: {error:?}");
            }
        }
    })
    .await
    .expect("expected event deadline")
}

async fn verification(receiver: &mut broadcast::Receiver<PeerManagerEvent>) -> PairingVerify {
    let PeerManagerEvent::PairingVerify(prompt) = event(receiver, |e| {
        matches!(e, PeerManagerEvent::PairingVerify(_))
    })
    .await
    else {
        panic!("verification");
    };
    prompt
}

async fn pair(a: &NativePeerManager, b: &NativePeerManager) {
    let mut host_events = a.events();
    let mut join_events = b.events();
    let host = a.pair_host(unix_ms()).await.expect("host window");
    let session = b
        .pair_join(
            PairTarget::Address(a.local_addr().expect("address").to_string()),
            &host.code,
            unix_ms(),
        )
        .await
        .expect("PAKE join");
    let host_prompt = verification(&mut host_events).await;
    assert_eq!(
        host_prompt.phrase, session.verification.phrase,
        "both screens must show identical SAS"
    );
    assert!(a.paired_peers().expect("pins").is_empty());
    assert!(b.paired_peers().expect("pins").is_empty());
    assert!(a
        .confirm_pairing(true, unix_ms())
        .await
        .expect("host accepts")
        .is_none());
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        a.paired_peers().expect("pins").is_empty(),
        "one confirmation cannot pin"
    );
    b.confirm_pairing(true, unix_ms())
        .await
        .expect("joiner accepts");
    event(&mut host_events, |e| {
        matches!(e, PeerManagerEvent::Paired(_))
    })
    .await;
    event(&mut join_events, |e| {
        matches!(e, PeerManagerEvent::Paired(_))
    })
    .await;
    assert_eq!(
        a.paired_peers().expect("pins")[0].device_id,
        b.identity().device_id()
    );
    assert_eq!(
        b.paired_peers().expect("pins")[0].device_id,
        a.identity().device_id()
    );
}

async fn connected(a: &NativePeerManager, b: &NativePeerManager) -> (NativeLink, NativeLink) {
    pair(a, b).await;
    let left = a.link();
    let right = b.link();
    // Pairing schedules the smaller-ID peer's pinned dial. An additional
    // explicit dial races deduplication and can replace the measured session.
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let a_ready = left.0.session_notify.notified();
            let b_ready = right.0.session_notify.notified();
            tokio::pin!(a_ready, b_ready);
            a_ready.as_mut().enable();
            b_ready.as_mut().enable();
            if left.connected(b.identity().device_id()) && right.connected(a.identity().device_id())
            {
                break;
            }
            tokio::select! { _ = a_ready => {}, _ = b_ready => {} }
        }
    })
    .await
    .expect("automatic pinned QUIC on both ends");
    enter_for_test(
        &left,
        &right,
        b.identity().device_id(),
        a.identity().device_id(),
        1,
    )
    .await;
    enter_for_test(
        &right,
        &left,
        a.identity().device_id(),
        b.identity().device_id(),
        1,
    )
    .await;
    (left, right)
}

async fn enter_for_test(
    sender: &NativeLink,
    receiver: &NativeLink,
    target: &str,
    source: &str,
    epoch: u64,
) {
    let send = sender.send_reliable(
        target,
        WireMessage::Control(ControlMessage::Enter(wire::Enter {
            epoch,
            pos: glide_platform::Point { x: 0.0, y: 0.0 },
            modifiers_down: wire::ModifierKeys::new(),
        })),
    );
    let admit = async {
        loop {
            if let LinkEvent::Reliable {
                message: WireMessage::Control(ControlMessage::Enter(enter)),
                ..
            } = receiver.recv_event().await.expect("Enter")
            {
                assert_eq!(enter.epoch, epoch);
                receiver
                    .admit_input_epoch(source, epoch)
                    .expect("admit Enter");
                break;
            }
        }
    };
    let (sent, ()) = tokio::join!(send, admit);
    sent.expect("acknowledged Enter");
}

async fn reliable(link: &NativeLink) -> WireMessage {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let LinkEvent::Reliable { message, .. } = link.recv().await.expect("link receive") {
                if !matches!(message, WireMessage::Control(ControlMessage::Heartbeat(_))) {
                    return message;
                }
            }
        }
    })
    .await
    .expect("reliable delivery")
}

fn key(seq: u64) -> WireMessage {
    WireMessage::Input(InputMessage::Key(InputKey {
        epoch: 1,
        seq,
        hid_usage: glide_platform::Key(4),
        down: seq.is_multiple_of(2),
    }))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pairing_happy_path_and_pinned_input() {
    let (_a_data, a) = manager("A").await;
    let (_b_data, b) = manager("B").await;
    let (left, right) = connected(&a, &b).await;
    let message = key(1);
    left.send_reliable(b.identity().device_id(), message.clone())
        .await
        .expect("input send");
    assert_eq!(reliable(&right).await, message);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wrong_code_mitm_and_three_strike_burn() {
    let (_a_data, a) = manager("A").await;
    let (_b_data, b) = manager("Attacker").await;
    let mut events = a.events();
    let host = a.pair_host(unix_ms()).await.expect("host window");
    let wrong = if host.code.as_str() == "000000" {
        "999999"
    } else {
        "000000"
    };
    for attempt in 0..3 {
        let error = b
            .pair_join(
                PairTarget::Address(a.local_addr().expect("address").to_string()),
                wrong,
                unix_ms(),
            )
            .await
            .expect_err("wrong PAKE code cannot authenticate");
        assert_eq!(
            error.code,
            if attempt == 2 {
                ErrorCode::LockedOut
            } else {
                ErrorCode::BadCode
            }
        );
        event(&mut events, |e| {
            matches!(e, PeerManagerEvent::PairingResult { ok: false, .. })
        })
        .await;
    }
    assert!(
        b.pair_join(
            PairTarget::Address(a.local_addr().expect("address").to_string()),
            &host.code,
            unix_ms()
        )
        .await
        .is_err(),
        "burned code is unusable"
    );
    assert!(a.paired_peers().expect("pins").is_empty());
    assert!(b.paired_peers().expect("pins").is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn decline_and_verification_expiry_pin_nothing() {
    for expires in [false, true] {
        let (_a_data, a) = manager("A").await;
        let (_b_data, b) = manager("B").await;
        let mut a_events = a.events();
        let mut b_events = b.events();
        let host = a.pair_host(unix_ms()).await.expect("host window");
        b.pair_join(
            PairTarget::Address(a.local_addr().expect("address").to_string()),
            &host.code,
            unix_ms(),
        )
        .await
        .expect("PAKE join");
        verification(&mut a_events).await;
        if expires {
            a.expire_verification_for_test();
            assert_eq!(
                a.confirm_pairing(true, unix_ms())
                    .await
                    .expect_err("expired prompt")
                    .code,
                ErrorCode::CodeExpired
            );
        } else {
            a.confirm_pairing(false, unix_ms())
                .await
                .expect("decline queued");
        }
        event(&mut b_events, |e| {
            matches!(e, PeerManagerEvent::PairingResult { ok: false, .. })
        })
        .await;
        assert!(a.paired_peers().expect("pins").is_empty());
        assert!(b.paired_peers().expect("pins").is_empty());
    }
}

#[tokio::test]
async fn host_expiry_and_closed_pairing_reject_probes() {
    let (_a_data, a) = manager("A").await;
    let (_b_data, b) = manager("B").await;
    assert!(b
        .add_manual(&a.local_addr().expect("address").to_string())
        .await
        .is_err());
    a.pair_host(unix_ms()).await.expect("host window");
    let peer = b
        .add_manual(&a.local_addr().expect("address").to_string())
        .await
        .expect("non-sensitive hello");
    assert_eq!(peer.device_id, a.identity().device_id());
    assert!(b.paired_peers().expect("pins").is_empty());
    a.expire_host_for_test();
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(b
        .add_manual(&a.local_addr().expect("address").to_string())
        .await
        .is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unpaired_certificate_is_rejected_before_server_application_bytes() {
    let (_a_data, a) = manager("Server").await;
    let (_b_data, b) = manager("Unpaired").await;
    // Only trust the server from the client's side, forcing rejection by the
    // server's ClientCertVerifier rather than by the client's server verifier.
    b.pin_for_test(a.identity().device_id())
        .expect("test server pin");
    let left = a.link();
    let right = b.link();
    assert!(right
        .connect(
            &a.local_addr().expect("address").to_string(),
            Some(a.identity().device_id())
        )
        .await
        .is_err());
    assert_eq!(left.session_count(), 0);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), left.accept())
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), left.recv())
            .await
            .is_err()
    );
    // Even with pairing open, offering the normal ALPN cannot enter it.
    a.pair_host(unix_ms()).await.expect("pair window");
    assert!(right
        .connect(
            &a.local_addr().expect("address").to_string(),
            Some(a.identity().device_id())
        )
        .await
        .is_err());
    assert_eq!(left.session_count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_and_mixed_alpn_are_rejected_before_application_data() {
    let (_a_data, a) = manager("Server").await;
    let (_b_data, b) = manager("Client").await;
    a.pin_for_test(b.identity().device_id())
        .expect("server pin");
    b.pin_for_test(a.identity().device_id())
        .expect("client pin");
    a.pair_host(unix_ms()).await.expect("pairing open");
    for alpns in [
        vec![],
        vec![wire::ALPN.to_vec(), PAIR_ALPN.to_vec()],
        vec![PAIR_ALPN.to_vec(), wire::ALPN.to_vec()],
        vec![b"unknown".to_vec()],
    ] {
        let pins = PinSet::new();
        pins.insert(a.identity().device_id()).expect("pin");
        let config =
            tls::client_config_alpns(b.identity(), Some(pins), alpns).expect("client config");
        let dial = b
            .link()
            .0
            .endpoint
            .connect_with(config, a.local_addr().expect("address"), "glide.invalid")
            .expect("dial");
        assert!(tokio::time::timeout(Duration::from_secs(2), dial)
            .await
            .expect("handshake timeout")
            .is_err());
        assert_eq!(a.link().session_count(), 0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unpair_revokes_live_and_future_sessions() {
    let (_a_data, a) = manager("A").await;
    let (_b_data, b) = manager("B").await;
    let (left, right) = connected(&a, &b).await;
    a.unpair(b.identity().device_id()).await.expect("revoke");
    assert!(left
        .send_reliable(b.identity().device_id(), key(1))
        .await
        .is_err());
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(right
        .connect(
            &a.local_addr().expect("address").to_string(),
            Some(a.identity().device_id())
        )
        .await
        .is_err());
    assert!(a.paired_peers().expect("pins").is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deny_record_survives_failed_peer_snapshot_and_restart() {
    let (data, a) = manager("A").await;
    let (_b_data, b) = manager("B").await;
    let (left, _) = connected(&a, &b).await;
    // Block only the peer-snapshot temporary path; the independent deny journal
    // must become durable before this snapshot failure occurs.
    let blocked = data.path().join("network-peers.json.tmp");
    std::fs::create_dir(&blocked).expect("test failure injection");
    assert!(a.unpair(b.identity().device_id()).await.is_err());
    assert!(left
        .send_datagram(
            b.identity().device_id(),
            Move {
                seq: 1,
                x: 1.0,
                y: 1.0
            }
        )
        .is_err());
    drop(a);
    let denied: Vec<String> = serde_json::from_slice(
        &std::fs::read(data.path().join("revoked-peers.json")).expect("durable deny record"),
    )
    .expect("deny store");
    assert!(denied.iter().any(|id| id == b.identity().device_id()));
    let config = NativeConfig {
        data_dir: data.path().to_owned(),
        bind_addr: "127.0.0.1:0".parse().expect("address"),
        name: "A restart".into(),
        os: glide_platform::Os::Windows,
        monitors: Vec::new(),
        discovery: false,
    };
    assert!(
        NativePeerManager::for_test(config.clone()).await.is_err(),
        "unfinished transaction fails startup closed"
    );
    std::fs::remove_dir(&blocked).expect("remove empty test obstacle");
    let restart = NativePeerManager::for_test(config)
        .await
        .expect("restart after explicit obstacle repair");
    assert!(
        restart.paired_peers().expect("pins").is_empty(),
        "old snapshot cannot resurrect revoked pin"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unattended_verification_timeout_and_disconnect_pin_nothing() {
    for disconnected in [false, true] {
        let (_a_data, a) = manager("A").await;
        let (_b_data, b) = manager("B").await;
        a.verification_lifetime_for_test(Duration::from_millis(250));
        b.verification_lifetime_for_test(Duration::from_millis(250));
        let mut a_events = a.events();
        let host = a.pair_host(unix_ms()).await.expect("pair host");
        b.pair_join(
            PairTarget::Address(a.local_addr().expect("address").to_string()),
            &host.code,
            unix_ms(),
        )
        .await
        .expect("PAKE join");
        verification(&mut a_events).await;
        if disconnected {
            b.cancel_pair_host()
                .await
                .expect("disconnect pending candidate");
        }
        event(&mut a_events, |e| {
            matches!(e, PeerManagerEvent::PairingResult { ok: false, .. })
        })
        .await;
        assert!(a.paired_peers().expect("pins").is_empty());
        assert!(b.paired_peers().expect("pins").is_empty());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mismatched_sas_mac_aborts_without_a_pin() {
    let (_a_data, a) = manager("A").await;
    let (_b_data, b) = manager("B").await;
    let mut events = a.events();
    let host = a.pair_host(unix_ms()).await.expect("pair host");
    let (conn, hello, _) = b
        .pairing_connection_for_test(&a.local_addr().expect("address").to_string())
        .await
        .expect("pair hello");
    let local = super::pairing::PairHello {
        device_id: b.identity().device_id().to_owned(),
        name: "B".into(),
        os: glide_platform::Os::Windows,
    };
    let mut candidate = super::pairing::pair_exchange(
        &conn,
        false,
        host.code,
        local,
        b.identity().device_id(),
        &hello.device_id,
    )
    .await
    .expect("PAKE exchange");
    verification(&mut events).await;
    candidate.phrase[0] = "mismatched".into();
    a.confirm_pairing(true, unix_ms())
        .await
        .expect("host accepts displayed phrase");
    super::pairing::send_decision(
        &mut candidate.send,
        &candidate.key,
        &candidate.phrase,
        false,
        super::pairing::DecisionStage::Confirm,
        true,
    )
    .await
    .expect("mismatched phrase decision");
    event(&mut events, |e| {
        matches!(e, PeerManagerEvent::PairingResult { ok: false, .. })
    })
    .await;
    assert!(a.paired_peers().expect("pins").is_empty());
    assert!(b.paired_peers().expect("pins").is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tls_terminating_relay_without_code_cannot_pair() {
    let (_a_data, a) = manager("Real host").await;
    let (_b_data, b) = manager("Joiner").await;
    let (_c_data, attacker) = manager("Relay").await;
    let host = a.pair_host(unix_ms()).await.expect("host window");
    let open = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let mut config = super::tls::pairing_server(attacker.identity(), open).expect("relay TLS");
    config.transport_config(transport_config());
    let endpoint = quinn::Endpoint::server(config, "127.0.0.1:0".parse().expect("loopback"))
        .expect("relay endpoint");
    let relay_address = endpoint.local_addr().expect("address").to_string();
    let host_address = a.local_addr().expect("address").to_string();
    let relay = tokio::spawn(async move {
        let downstream = endpoint.accept().await.expect("joiner").await.expect("TLS");
        let (upstream, _, _) = attacker
            .pairing_connection_for_test(&host_address)
            .await
            .expect("real host probe");
        let hello = super::pairing::PairHello {
            device_id: attacker.identity().device_id().to_owned(),
            name: "Relay".into(),
            os: glide_platform::Os::Windows,
        };
        let (mut probe_send, mut probe_recv) = downstream.accept_bi().await.expect("probe");
        let mut request = [0];
        probe_recv.read_exact(&mut request).await.expect("request");
        assert_eq!(request, [1]);
        let response = serde_json::to_vec(&serde_json::json!({
            "hello": hello, "error": null, "attempts_remaining": 3,
        }))
        .expect("public hello");
        super::link::write_frame(&mut probe_send, &response)
            .await
            .expect("probe reply");
        probe_send.finish().expect("reply finish");
        let (mut down_send, mut down_recv) = downstream.accept_bi().await.expect("PAKE stream");
        let (mut up_send, mut up_recv) = upstream.open_bi().await.expect("upstream PAKE");
        // Rewrite only the public hellos so the certificate-ID check succeeds
        // on both TLS legs. Relay SPAKE and authentication bytes unchanged;
        // differing TLS exporters/fingerprints must still defeat the attacker.
        let mut public_hello = vec![1];
        public_hello.extend(serde_json::to_vec(&hello).expect("hello JSON"));
        for (recv, send) in [
            (&mut down_recv, &mut up_send),
            (&mut up_recv, &mut down_send),
        ] {
            let mut length = [0; 2];
            recv.read_exact(&mut length).await.expect("hello frame");
            let length = u16::from_be_bytes(length) as usize;
            assert!(length <= 1024);
            let mut bytes = vec![0; length];
            recv.read_exact(&mut bytes).await.expect("hello bytes");
            assert_eq!(bytes[0], 1);
            send.write_all(&(public_hello.len() as u16).to_be_bytes())
                .await
                .expect("hello length");
            send.write_all(&public_hello)
                .await
                .expect("rewritten hello");
        }
        let _ = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(
                tokio::io::copy(&mut down_recv, &mut up_send),
                tokio::io::copy(&mut up_recv, &mut down_send)
            )
        })
        .await;
        assert!(attacker.paired_peers().expect("attacker pins").is_empty());
        downstream.close(1u32.into(), b"relay done");
        upstream.close(1u32.into(), b"relay done");
    });
    assert!(b
        .pair_join(PairTarget::Address(relay_address), &host.code, unix_ms())
        .await
        .is_err());
    relay.await.expect("relay task");
    assert!(a.paired_peers().expect("host pins").is_empty());
    assert!(b.paired_peers().expect("joiner pins").is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn per_source_probe_rate_is_bounded() {
    let (_a_data, a) = manager("A").await;
    let (_b_data, b) = manager("B").await;
    a.pair_host(unix_ms()).await.expect("host window");
    let address = a.local_addr().expect("address").to_string();
    for _ in 0..12 {
        b.add_manual(&address).await.expect("allowed public probe");
    }
    assert_eq!(
        b.add_manual(&address).await.expect_err("source limit").code,
        ErrorCode::LockedOut
    );
    assert!(a.paired_peers().expect("pins").is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn staged_pairing_disconnect_and_crash_marker_fail_closed() {
    let (data, a) = manager("A").await;
    let (_b_data, b) = manager("B").await;
    let mut events = a.events();
    let host = a.pair_host(unix_ms()).await.expect("host window");
    let (conn, hello, _) = b
        .pairing_connection_for_test(&a.local_addr().expect("address").to_string())
        .await
        .expect("pair hello");
    let local = super::pairing::PairHello {
        device_id: b.identity().device_id().to_owned(),
        name: "B".into(),
        os: glide_platform::Os::Windows,
    };
    let mut candidate = super::pairing::pair_exchange(
        &conn,
        false,
        host.code,
        local,
        b.identity().device_id(),
        &hello.device_id,
    )
    .await
    .expect("PAKE");
    verification(&mut events).await;
    a.confirm_pairing(true, unix_ms())
        .await
        .expect("host confirms");
    for stage in [
        super::pairing::DecisionStage::Confirm,
        super::pairing::DecisionStage::Ready,
        super::pairing::DecisionStage::Commit,
    ] {
        super::pairing::send_decision(
            &mut candidate.send,
            &candidate.key,
            &candidate.phrase,
            false,
            stage,
            true,
        )
        .await
        .expect("joiner confirms");
        assert!(super::pairing::read_decision(
            &mut candidate.recv,
            &candidate.key,
            &candidate.phrase,
            true,
            stage
        )
        .await
        .expect("host confirms"));
    }
    assert!(super::pairing::read_decision(
        &mut candidate.recv,
        &candidate.key,
        &candidate.phrase,
        true,
        super::pairing::DecisionStage::Complete
    )
    .await
    .expect("durable staging"));
    assert!(data.path().join("pairing-pending.json").exists());
    assert!(a.paired_peers().expect("not yet pinned").is_empty());
    b.pin_for_test(a.identity().device_id())
        .expect("client-only pin");
    assert!(
        b.link()
            .connect(
                &a.local_addr().expect("address").to_string(),
                Some(a.identity().device_id())
            )
            .await
            .is_err(),
        "staged key is rejected at normal handshake"
    );
    conn.close(1u32.into(), b"disconnect before completion");
    event(&mut events, |e| {
        matches!(e, PeerManagerEvent::PairingResult { ok: false, .. })
    })
    .await;
    assert!(!data.path().join("pairing-pending.json").exists());
    assert!(a.paired_peers().expect("rollback").is_empty());
    drop(a);
    // Model a crash after staged metadata reached disk but before completion.
    std::fs::write(data.path().join("pairing-pending.json"), b"\"unfinished\"")
        .expect("crash marker");
    let config = NativeConfig {
        data_dir: data.path().to_owned(),
        bind_addr: "127.0.0.1:0".parse().expect("address"),
        name: "Restart".into(),
        os: glide_platform::Os::Windows,
        monitors: Vec::new(),
        discovery: false,
    };
    assert!(NativePeerManager::for_test(config).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn datagrams_coalesce_and_drop_stale_sequences() {
    let (_a_data, a) = manager("A").await;
    let (_b_data, b) = manager("B").await;
    let (left, right) = connected(&a, &b).await;
    for seq in 1..=1000 {
        loop {
            match left.send_datagram(
                b.identity().device_id(),
                Move {
                    seq,
                    x: seq as f64,
                    y: 2.0,
                },
            ) {
                Err(crate::LinkError::Busy) => tokio::task::yield_now().await,
                result => {
                    result.expect("bounded mouse enqueue");
                    break;
                }
            }
        }
    }
    tokio::time::sleep(Duration::from_millis(30)).await;
    let newest = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let LinkEvent::Move { movement, .. } = right.recv().await.expect("mouse receive") {
                if movement.seq == 1000 {
                    return movement;
                }
            }
        }
    })
    .await
    .expect("latest movement");
    assert_eq!(newest.x, 1000.0);
    left.send_datagram(
        b.identity().device_id(),
        Move {
            seq: 1,
            x: -1.0,
            y: -1.0,
        },
    )
    .expect("stale enqueue discarded");
    assert!(
        tokio::time::timeout(Duration::from_millis(30), right.recv())
            .await
            .is_err()
    );
    let conn = left
        .raw_connection(b.identity().device_id())
        .expect("QUIC connection");
    conn.send_datagram(bytes::Bytes::from(
        codec::encode_move(&Move {
            seq: 2,
            x: -2.0,
            y: -2.0,
        })
        .expect("move encode"),
    ))
    .expect("stale wire datagram");
    assert!(
        tokio::time::timeout(Duration::from_millis(30), right.recv())
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_and_oversize_frames_drop_connection() {
    for (oversize, datagram, trailing) in [
        (true, false, false),
        (false, false, false),
        (false, true, false),
        (false, false, true),
    ] {
        let (_a_data, a) = manager("A").await;
        let (_b_data, b) = manager("B").await;
        let (left, right) = connected(&a, &b).await;
        let conn = left
            .raw_connection(b.identity().device_id())
            .expect("QUIC connection");
        let bad_remote = right
            .raw_connection(a.identity().device_id())
            .expect("remote session");
        let bad_token = right
            .peer_token(a.identity().device_id())
            .expect("remote token");
        if datagram {
            conn.send_datagram(bytes::Bytes::from_static(&[255]))
                .expect("garbage datagram");
        } else {
            let mut stream = conn.open_uni().await.expect("file stream");
            stream.write_all(&[3]).await.expect("lane");
            if oversize {
                stream
                    .write_all(&((wire::MAX_FILE_CONTROL_FRAME_BYTES + 1) as u32).to_be_bytes())
                    .await
                    .expect("oversize header");
            } else {
                let frame = if trailing {
                    let mut bytes = codec::encode_transfer(&wire::TransferMessage::FileCancel(
                        wire::FileCancel {
                            transfer_id: "t".into(),
                        },
                    ))
                    .expect("encode");
                    bytes.push(0);
                    bytes
                } else {
                    vec![255]
                };
                super::link::write_frame(&mut stream, &frame)
                    .await
                    .expect("bad frame send");
            }
            stream.finish().expect("finish malformed stream");
        }
        tokio::time::timeout(Duration::from_secs(2), conn.closed())
            .await
            .expect("bad frame must close connection");
        tokio::time::timeout(Duration::from_secs(2), bad_remote.closed())
            .await
            .expect("offending remote connection closes");
        assert!(!right
            .peer_token(a.identity().device_id())
            .is_ok_and(|token| token == bad_token));
        assert!(!right.connection_can_deliver_for_test(
            a.identity().device_id(),
            &bad_remote,
            bad_token
        ));
        assert!(
            conn.open_uni().await.is_err(),
            "closed session cannot send input"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reliable_delivery_binds_tokens_even_if_driver_addresses_match() {
    let (_a_data, a) = manager("A").await;
    let (_b_data, b) = manager("B").await;
    let (_, right) = connected(&a, &b).await;
    let id = a.identity().device_id();
    let connection = right.raw_connection(id).expect("connection");
    let token = right.peer_token(id).expect("token");
    assert!(right.connection_can_deliver_for_test(id, &connection, token));
    let retired = crate::PeerToken {
        slot: token.slot,
        generation: token.generation - 1,
    };
    assert!(!right.connection_can_deliver_for_test(id, &connection, retired));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn simultaneous_dials_dedupe_to_the_same_connection() {
    let (_a_data, a) = manager("A").await;
    let (_b_data, b) = manager("B").await;
    pair(&a, &b).await;
    let left = a.link();
    let right = b.link();
    let a_addr = a.local_addr().expect("address").to_string();
    let b_addr = b.local_addr().expect("address").to_string();
    let (first, second) = tokio::join!(
        left.connect(&b_addr, Some(b.identity().device_id())),
        right.connect(&a_addr, Some(a.identity().device_id()))
    );
    first.expect("dial A");
    second.expect("dial B");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(left.session_count(), 1);
    assert_eq!(right.session_count(), 1);
    enter_for_test(
        &left,
        &right,
        b.identity().device_id(),
        a.identity().device_id(),
        1,
    )
    .await;
    enter_for_test(
        &right,
        &left,
        a.identity().device_id(),
        b.identity().device_id(),
        1,
    )
    .await;
    left.send_reliable(b.identity().device_id(), key(1))
        .await
        .expect("deduped input");
    assert_eq!(reliable(&right).await, key(1));
    right
        .send_reliable(a.identity().device_id(), key(2))
        .await
        .expect("reverse input");
    assert_eq!(reliable(&left).await, key(2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paired_online_peers_reconnect_after_session_loss() {
    let (_a_data, a) = manager("A").await;
    let (_b_data, b) = manager("B").await;
    let (left, right) = connected(&a, &b).await;
    let old = left
        .raw_connection(b.identity().device_id())
        .expect("old session")
        .stable_id();
    left.close(b.identity().device_id())
        .await
        .expect("session loss");
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if left.connected(b.identity().device_id())
                && right.connected(a.identity().device_id())
                && left
                    .raw_connection(b.identity().device_id())
                    .is_some_and(|conn| conn.stable_id() != old)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("automatic pinned reconnect");
    enter_for_test(
        &left,
        &right,
        b.identity().device_id(),
        a.identity().device_id(),
        1,
    )
    .await;
    left.send_reliable(b.identity().device_id(), key(5))
        .await
        .expect("reconnected input");
    assert_eq!(reliable(&right).await, key(5));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pairing_windows_keep_established_sessions_and_reconnects_working() {
    let (_a_data, a) = manager("A").await;
    let (_b_data, b) = manager("B").await;
    let (left, right) = connected(&a, &b).await;
    a.pair_host(unix_ms()).await.expect("A pairing window");
    b.pair_host(unix_ms()).await.expect("B pairing window");
    left.send_reliable(b.identity().device_id(), key(1))
        .await
        .expect("established session");
    assert_eq!(reliable(&right).await, key(1));
    let old = left
        .peer_token(b.identity().device_id())
        .expect("old token");
    left.close(b.identity().device_id()).await.expect("close");
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if left
                .peer_token(b.identity().device_id())
                .is_ok_and(|token| token != old)
                && right.connected(a.identity().device_id())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("reconnect during pairing");
    enter_for_test(
        &left,
        &right,
        b.identity().device_id(),
        a.identity().device_id(),
        1,
    )
    .await;
    left.send_reliable(b.identity().device_id(), key(2))
        .await
        .expect("new session");
    assert_eq!(reliable(&right).await, key(2));
    a.unpair(b.identity().device_id())
        .await
        .expect("revoke during pairing");
    tokio::time::timeout(Duration::from_secs(1), async {
        while right.connected(a.identity().device_id()) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("revoked session closes before new handshake");
    b.pin_for_test(a.identity().device_id())
        .expect("client still trusts server");
    assert!(right
        .connect(
            &a.local_addr().expect("address").to_string(),
            Some(a.identity().device_id())
        )
        .await
        .is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mouse_receiver_has_zero_steady_allocations_for_both_links() {
    let (_a_data, a) = manager("A").await;
    let (_b_data, b) = manager("B").await;
    let (left, right) = connected(&a, &b).await;
    let mock = crate::InMemoryLink::new();
    mock.script_reachable("mock", "peer");
    mock.connect("mock", None).await.expect("mock connect");
    let token = right.peer_token(a.identity().device_id()).expect("token");
    for seq in 1..=32 {
        loop {
            if left
                .send_datagram(
                    b.identity().device_id(),
                    Move {
                        seq,
                        x: 1.0,
                        y: 2.0,
                    },
                )
                .is_ok()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        tokio::time::timeout(Duration::from_secs(1), right.mouse_ready().notified())
            .await
            .expect("move ready");
        mock.script_event(LinkEvent::Move {
            peer_id: "peer".into(),
            movement: Move {
                seq,
                x: 1.0,
                y: 2.0,
            },
        })
        .expect("mock move");
        let movement = loop {
            ALLOCATION_COUNT.with(|count| count.set(0));
            COUNT_ALLOCATIONS.with(|enabled| enabled.set(true));
            let result = right.try_recv_move();
            let mock_result = if !matches!(result, Err(crate::LinkError::Busy)) {
                mock.try_recv_move()
            } else {
                Ok(None)
            };
            COUNT_ALLOCATIONS.with(|enabled| enabled.set(false));
            assert_eq!(ALLOCATION_COUNT.with(std::cell::Cell::get), 0);
            match result {
                Ok(Some(movement)) => {
                    assert_eq!(mock_result.expect("mock").expect("move").movement.seq, seq);
                    break movement;
                }
                Err(crate::LinkError::Busy) => tokio::task::yield_now().await,
                other => panic!("missing movement: {other:?}"),
            }
        };
        assert_eq!(movement.peer, token);
        assert_eq!(movement.movement.seq, seq);
    }
    let old = mock.peer_token("peer").expect("token");
    mock.close("peer").await.expect("close");
    mock.connect("mock", None).await.expect("reconnect");
    let new = mock.peer_token("peer").expect("new token");
    assert_eq!(old.slot, new.slot);
    assert_ne!(old.generation, new.generation);
    assert!(mock.try_recv_move().expect("empty").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn partial_file_body_times_out_and_drops_connection() {
    let (_a_data, a) = manager("A").await;
    let (_b_data, b) = manager("B").await;
    let (left, _) = connected(&a, &b).await;
    let conn = left
        .raw_connection(b.identity().device_id())
        .expect("session");
    let mut stream = conn.open_uni().await.expect("file stream");
    stream.write_all(&[3]).await.expect("file marker");
    stream
        .write_all(&100u32.to_be_bytes())
        .await
        .expect("bounded length");
    stream
        .write_all(&[0])
        .await
        .expect("valid variant, incomplete body");
    let start = Instant::now();
    tokio::time::timeout(Duration::from_secs(7), conn.closed())
        .await
        .expect("slow-body deadline");
    assert!(
        start.elapsed() >= Duration::from_millis(4500),
        "body deadline, not heartbeat failure"
    );
    drop(stream);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn heartbeat_timeout_emits_peer_loss() {
    let (_data, manager) = manager("Heartbeat client").await;
    let raw_identity = NativeIdentity::ephemeral().expect("raw server identity");
    manager
        .pin_for_test(raw_identity.device_id())
        .expect("server pin");
    let pins = PinSet::new();
    pins.insert(manager.identity().device_id())
        .expect("client pin");
    let mut config = super::tls::normal_server(&raw_identity, pins).expect("strict server TLS");
    config.transport_config(transport_config());
    let endpoint = quinn::Endpoint::server(config, "127.0.0.1:0".parse().expect("loopback"))
        .expect("raw server");
    let address = endpoint.local_addr().expect("address").to_string();
    let server_id = raw_identity.device_id().to_owned();
    let task = tokio::spawn(async move {
        let conn = endpoint
            .accept()
            .await
            .expect("client")
            .await
            .expect("mTLS");
        let (mut send, mut recv) = conn.accept_bi().await.expect("control");
        let mut length = [0; 4];
        recv.read_exact(&mut length).await.expect("hello length");
        let mut hello = vec![0; u32::from_be_bytes(length) as usize];
        recv.read_exact(&mut hello).await.expect("hello body");
        assert!(matches!(
            codec::decode_control(&hello),
            Ok(ControlMessage::Hello(_))
        ));
        let hello = wire::Hello {
            proto_version: wire::PROTOCOL_VERSION,
            device_id: raw_identity.device_id().to_owned(),
            name: "Silent peer".into(),
            os: glide_platform::Os::Windows,
            monitors: wire::Monitors::new(),
            app_version: "test".into(),
        };
        super::link::write_frame(
            &mut send,
            &codec::encode_control(&ControlMessage::Hello(hello)).expect("hello encode"),
        )
        .await
        .expect("hello send");
        // Even authenticated inbound heartbeats cannot mask a broken reverse
        // application path: only an ACK for our outstanding sequence proves it.
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        loop {
            tokio::select! {
                _ = conn.closed() => break,
                _ = tick.tick() => {
                    let frame = codec::encode_control(&ControlMessage::Heartbeat(wire::Heartbeat {
                        seq: 999, ts: unix_ms(),
                    })).expect("heartbeat encode");
                    if super::link::write_frame(&mut send, &frame).await.is_err() { break; }
                }
            }
        }
        drop((send, recv, endpoint));
    });
    let mut events = manager.events();
    let start = Instant::now();
    manager
        .link()
        .connect(&address, Some(&server_id))
        .await
        .expect("authenticated silent peer");
    event(&mut events, |e| {
        matches!(e, PeerManagerEvent::Disappeared { .. })
    })
    .await;
    assert!(start.elapsed() >= Duration::from_millis(1400));
    task.await.expect("raw server task");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn synchronous_mouse_enqueue_has_zero_steady_allocations() {
    let (_a_data, a) = manager("A").await;
    let (_b_data, b) = manager("B").await;
    let (left, _right) = connected(&a, &b).await;
    let id = b.identity().device_id();
    let _ = left.send_datagram(
        id,
        Move {
            seq: 1,
            x: 1.0,
            y: 2.0,
        },
    );
    tokio::time::sleep(Duration::from_millis(20)).await;
    ALLOCATION_COUNT.with(|count| count.set(0));
    COUNT_ALLOCATIONS.with(|enabled| enabled.set(true));
    let mut accepted = 0;
    for seq in 2..=1001 {
        if left
            .send_datagram(
                id,
                Move {
                    seq,
                    x: 1.0,
                    y: 2.0,
                },
            )
            .is_ok()
        {
            accepted += 1;
        }
    }
    COUNT_ALLOCATIONS.with(|enabled| enabled.set(false));
    assert!(accepted > 0, "measured successful enqueue calls");
    assert_eq!(ALLOCATION_COUNT.with(std::cell::Cell::get), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn saturated_file_streams_do_not_block_input() {
    const CHUNK_BYTES: usize = 512 * 1024;
    let (_a_data, a) = manager("A").await;
    let (_b_data, b) = manager("B").await;
    let (left, right) = connected(&a, &b).await;
    let mut writers = Vec::new();
    let completed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for index in 0..4 {
        let left = left.clone();
        let id = b.identity().device_id().to_owned();
        let completed = completed.clone();
        writers.push(tokio::spawn(async move {
            let connection = left.raw_connection(&id).expect("connected writer");
            for chunk_index in 0..16 {
                let data =
                    wire::ChunkBytes::try_from_vec(vec![7; CHUNK_BYTES]).expect("bounded chunk");
                left.send_reliable(
                    &id,
                    WireMessage::Transfer(wire::TransferMessage::FileChunk(wire::FileChunk {
                        transfer_id: "saturation".into(),
                        file_id: index,
                        chunk_index,
                        offset: u64::from(chunk_index) * CHUNK_BYTES as u64,
                        data,
                        uncompressed_size: CHUNK_BYTES as u32,
                        compressed: false,
                        blake3_hash: [0; 32],
                    })),
                )
                .await
                .unwrap_or_else(|error| {
                    panic!(
                        "saturating file writer: {error:?}; close: {:?}",
                        connection.close_reason()
                    )
                });
                completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }));
    }
    // Intentionally leave the two-slot bulk delivery queue unread.
    tokio::time::timeout(Duration::from_secs(3), async {
        while completed.load(std::sync::atomic::Ordering::Relaxed) < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("file traffic starts");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        writers.iter().all(|writer| !writer.is_finished()),
        "all file producers still backpressured"
    );
    let sent = completed.load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        (2..64).contains(&sent),
        "actual data sent, but bulk queue/window stopped further progress: {sent}"
    );
    let mut samples = Vec::new();
    for seq in 1..=40 {
        let start = Instant::now();
        left.send_reliable(b.identity().device_id(), key(seq))
            .await
            .expect("input lane remains writable");
        let received =
            tokio::time::timeout(Duration::from_millis(500), right.next_input_for_test())
                .await
                .expect("input independent of bulk")
                .expect("decoded input");
        assert!(matches!(
            received,
            LinkEvent::Reliable {
                message: WireMessage::Input(_),
                ..
            }
        ));
        samples.push(start.elapsed());
    }
    samples.sort_unstable();
    println!(
        "saturated-file input p50={:?} p99={:?}; file chunks completed={}/64",
        samples[20],
        samples[39],
        completed.load(std::sync::atomic::Ordering::Relaxed)
    );
    assert!(samples[39] < Duration::from_millis(500));
    assert!(
        writers.iter().all(|writer| !writer.is_finished()),
        "file saturation persists throughout input measurement"
    );
    for writer in writers {
        writer.abort();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "explicit real QUIC latency micro-benchmark"]
async fn loopback_latency_microbenchmark() {
    let (_a_data, a) = manager("Bench A").await;
    let (_b_data, b) = manager("Bench B").await;
    let (left, right) = connected(&a, &b).await;
    let mut input = Vec::with_capacity(1000);
    let mut mouse = Vec::with_capacity(1000);
    for seq in 1..=1000 {
        let start = Instant::now();
        left.send_reliable(b.identity().device_id(), key(seq))
            .await
            .expect("input");
        assert_eq!(reliable(&right).await, key(seq));
        input.push(start.elapsed());
        let start = Instant::now();
        loop {
            match left.send_datagram(
                b.identity().device_id(),
                Move {
                    seq,
                    x: 1.0,
                    y: 2.0,
                },
            ) {
                Ok(()) => break,
                Err(crate::LinkError::Busy) => tokio::task::yield_now().await,
                Err(error) => panic!("mouse enqueue: {error:?}"),
            }
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "mouse enqueue deadline"
            );
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let LinkEvent::Move { movement, .. } = right.recv().await.expect("mouse") {
                    assert_eq!(movement.seq, seq);
                    break;
                }
            }
        })
        .await
        .expect("mouse timeout");
        mouse.push(start.elapsed());
    }
    input.sort_unstable();
    mouse.sort_unstable();
    println!(
        "QUIC loopback 1000 samples: input p50={:?} p99={:?}; datagram p50={:?} p99={:?}",
        input[500], input[990], mouse[500], mouse[990]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_epochs_acknowledge_admission_and_drop_old_keys_and_buttons() {
    let (_a_data, a) = manager("A").await;
    let (_b_data, b) = manager("B").await;
    let (left, right) = connected(&a, &b).await;
    let old_key = InputMessage::Key(InputKey {
        epoch: 1,
        seq: 10,
        hid_usage: glide_platform::Key(4),
        down: true,
    });
    let old_button = InputMessage::Button(wire::InputButton {
        epoch: 1,
        seq: 11,
        button: glide_platform::Button::Left,
        down: true,
    });
    enter_for_test(
        &left,
        &right,
        b.identity().device_id(),
        a.identity().device_id(),
        2,
    )
    .await;
    left.send_input_unchecked_for_test(b.identity().device_id(), old_key)
        .await
        .expect("delayed key");
    left.send_input_unchecked_for_test(b.identity().device_id(), old_button)
        .await
        .expect("delayed button");
    let current = WireMessage::Input(InputMessage::Key(InputKey {
        epoch: 2,
        seq: 12,
        hid_usage: glide_platform::Key(5),
        down: false,
    }));
    left.send_reliable(b.identity().device_id(), current.clone())
        .await
        .expect("current input");
    assert_eq!(
        reliable(&right).await,
        current,
        "ordered input lane fences both discarded old inputs"
    );
    left.send_reliable(
        b.identity().device_id(),
        WireMessage::Control(ControlMessage::Leave(wire::Leave { epoch: 2 })),
    )
    .await
    .expect("leave");
    assert!(matches!(
        reliable(&right).await,
        WireMessage::Control(ControlMessage::Leave(wire::Leave { epoch: 2 }))
    ));
    assert!(
        left.send_reliable(b.identity().device_id(), current.clone())
            .await
            .is_err(),
        "sender cannot input after Leave"
    );
    let WireMessage::Input(current) = current else {
        panic!("input");
    };
    left.send_input_unchecked_for_test(b.identity().device_id(), current)
        .await
        .expect("malicious post-Leave input");
    enter_for_test(
        &left,
        &right,
        b.identity().device_id(),
        a.identity().device_id(),
        3,
    )
    .await;
    let sentinel = WireMessage::Input(InputMessage::Key(InputKey {
        epoch: 3,
        seq: 13,
        hid_usage: glide_platform::Key(6),
        down: false,
    }));
    left.send_reliable(b.identity().device_id(), sentinel.clone())
        .await
        .expect("new epoch");
    assert_eq!(
        reliable(&right).await,
        sentinel,
        "post-Leave input never reaches injection seam"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metadata_updates_validate_notify_and_survive_reconnect() {
    let (_a_data, a) = manager("A").await;
    let (_b_data, b) = manager("B").await;
    let (left, _) = connected(&a, &b).await;
    let mut events = b.events();
    a.update_local_metadata("Hotplug".into(), Vec::new())
        .expect("update");
    let update = event(
        &mut events,
        |event| matches!(event, PeerManagerEvent::PeerUpdated(peer) if peer.name == "Hotplug"),
    )
    .await;
    assert!(
        matches!(update, PeerManagerEvent::PeerUpdated(peer) if peer.device_id == a.identity().device_id())
    );
    assert!(a
        .update_local_metadata("bad\nname".into(), Vec::new())
        .is_err());
    assert!(a
        .update_listen_port(a.local_addr().expect("address").port().wrapping_add(1))
        .is_err());
    left.close(b.identity().device_id()).await.expect("close");
    event(
        &mut events,
        |event| matches!(event, PeerManagerEvent::PeerUpdated(peer) if peer.name == "Hotplug"),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_streams_are_distinct_bound_and_cancellable() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (_a_data, a) = manager("A").await;
    let (_b_data, b) = manager("B").await;
    let (left, right) = connected(&a, &b).await;
    let (outbound, inbound) = tokio::join!(
        left.open_transfer(b.identity().device_id(), "transfer", 4),
        right.accept_transfer()
    );
    let outbound = outbound.expect("open");
    let mut inbound = inbound.expect("accept");
    assert_eq!(inbound.peer_id, a.identity().device_id());
    assert_eq!(
        inbound.peer_token,
        right.peer_token(a.identity().device_id()).expect("token")
    );
    let ids: std::collections::HashSet<_> =
        outbound.chunks.iter().map(|io| io.write.id()).collect();
    assert_eq!(ids.len(), 4);
    for (lane, (mut send, mut recv)) in outbound
        .chunks
        .into_iter()
        .zip(inbound.chunks.drain(..))
        .enumerate()
    {
        let byte = [lane as u8];
        let (sent, received) = tokio::join!(send.write_all(&byte), recv.read_u8());
        sent.expect("write");
        assert_eq!(received.expect("lane"), lane as u8);
    }
    let handle = inbound.handle.clone();
    let reader = tokio::spawn(async move {
        let mut byte = [0];
        inbound.control.read_exact(&mut byte).await
    });
    handle.cancel();
    assert_eq!(
        reader
            .await
            .expect("reader")
            .expect_err("cancelled read")
            .kind(),
        std::io::ErrorKind::Interrupted
    );
    assert!(
        left.connected(b.identity().device_id()),
        "job cancellation preserves input session"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malicious_transfer_lane_and_job_floods_close_the_offending_session() {
    for kind in 0..3 {
        let (_a_data, a) = manager("A").await;
        let (_b_data, b) = manager("B").await;
        let (left, right) = connected(&a, &b).await;
        let conn = left
            .raw_connection(b.identity().device_id())
            .expect("connection");
        let mut jobs = Vec::new();
        if kind == 2 {
            for index in 0..4 {
                let id = format!("job{index}");
                let (send, recv) = tokio::join!(
                    left.open_transfer(b.identity().device_id(), &id, 1),
                    right.accept_transfer()
                );
                jobs.push((send.expect("job"), recv.expect("accepted job")));
            }
            assert!(matches!(
                left.open_transfer(b.identity().device_id(), "local-fifth", 1)
                    .await,
                Err(crate::LinkError::QueueFull)
            ));
            assert!(left.connected(b.identity().device_id()));
        }
        let (mut send, _recv) = conn.open_bi().await.expect("malicious stream");
        let mut header = b"GLDX\0\x01".to_vec();
        header.extend_from_slice(&[
            if kind == 0 { 5 } else { 1 },
            if kind == 1 { 1 } else { 0 },
            3,
        ]);
        header.extend_from_slice(b"bad");
        send.write_all(&header).await.expect("preamble");
        tokio::time::timeout(Duration::from_secs(2), conn.closed())
            .await
            .expect("malicious streams close session");
    }
}
