#![cfg(feature = "test-support")]

use glide_net::{
    Link, LinkEvent, MouseReceiver, NativeConfig, NativeLink, NativePeerManager, PairTarget,
    PeerManager, PeerManagerEvent, TestKeyStore,
};
use glide_platform::{Key, Os, Point};
use glide_proto::wire::{self, ControlMessage, InputKey, InputMessage, Move, WireMessage};
use glide_xfer::{build_manifest, Cancel, Config, Consent, FileEngine, Progress};
use std::{
    io::{Read, Write},
    path::Path,
    time::{Duration, Instant},
};

fn temporary() -> tempfile::TempDir {
    tempfile::tempdir_in(std::env::var_os("CARGO_TARGET_DIR").expect("external target"))
        .expect("directory")
}

async fn paired() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    NativePeerManager,
    NativePeerManager,
) {
    let a_dir = temporary();
    let b_dir = temporary();
    let store = TestKeyStore::default();
    let config = |dir: &Path, name: &str| NativeConfig {
        data_dir: dir.to_owned(),
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        name: name.into(),
        os: Os::Windows,
        monitors: Vec::new(),
        discovery: false,
    };
    let a = NativePeerManager::with_test_keystore(config(a_dir.path(), "A"), &store)
        .await
        .expect("A");
    let b = NativePeerManager::with_test_keystore(config(b_dir.path(), "B"), &store)
        .await
        .expect("B");
    assert_ne!(a.identity().device_id(), b.identity().device_id());
    let mut a_events = a.events();
    let mut b_events = b.events();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis() as u64;
    let host = a.pair_host(now).await.expect("host");
    let session = b
        .pair_join(
            PairTarget::Address(a.local_addr().expect("address").to_string()),
            &host.code,
            now,
        )
        .await
        .expect("real PAKE");
    let phrase = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let PeerManagerEvent::PairingVerify(prompt) = a_events.recv().await.expect("verify")
            {
                break prompt.phrase;
            }
        }
    })
    .await
    .expect("SAS");
    assert_eq!(phrase, session.verification.phrase);
    a.confirm_pairing(true, now).await.expect("A confirms");
    b.confirm_pairing(true, now).await.expect("B confirms");
    for events in [&mut a_events, &mut b_events] {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if matches!(
                    events.recv().await.expect("pair event"),
                    PeerManagerEvent::Paired(_)
                ) {
                    break;
                }
            }
        })
        .await
        .expect("persisted pins");
    }
    assert_eq!(a.paired_peers().expect("A trust").len(), 1);
    assert_eq!(b.paired_peers().expect("B trust").len(), 1);
    wait_connections(&a, &b).await;
    (a_dir, b_dir, a, b)
}

async fn wait_connections(a: &NativePeerManager, b: &NativePeerManager) {
    let mut a_events = a.events();
    let mut b_events = b.events();
    tokio::time::timeout(Duration::from_secs(5), async {
        while a.link().peer_token(b.identity().device_id()).is_err()
            || b.link().peer_token(a.identity().device_id()).is_err()
        {
            tokio::select! { _ = a_events.recv() => {}, _ = b_events.recv() => {} }
        }
    })
    .await
    .expect("pinned sessions");
}

fn source(path: &Path, bytes: usize) {
    let mut block = vec![0u8; 1024 * 1024];
    let mut state = 0x1234abcd_u32;
    for byte in &mut block {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        *byte = state as u8;
    }
    let mut file = std::fs::File::create(path).expect("source");
    for _ in 0..bytes / block.len() {
        file.write_all(&block).expect("write");
    }
    file.sync_all().expect("sync");
}

fn incompressible_source(path: &Path, bytes: usize) {
    let mut block = vec![0u8; 1024 * 1024];
    let mut state = 0x1234abcd_u32;
    let mut file = std::fs::File::create(path).expect("source");
    let mut remaining = bytes;
    while remaining > 0 {
        for byte in &mut block {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            *byte = state as u8;
        }
        let count = remaining.min(block.len());
        file.write_all(&block[..count]).expect("write");
        remaining -= count;
    }
    file.sync_all().expect("sync");
}

fn hash(path: &Path) -> blake3::Hash {
    let mut file = std::fs::File::open(path).expect("file");
    let mut buffer = [0u8; 65536];
    let mut hash = blake3::Hasher::new();
    loop {
        let count = file.read(&mut buffer).expect("read");
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    hash.finalize()
}

async fn admit(a: &NativePeerManager, b: &NativePeerManager, epoch: u64) {
    let left = a.link();
    let right = b.link();
    let message = WireMessage::Control(ControlMessage::Enter(wire::Enter {
        epoch,
        pos: Point { x: 0.0, y: 0.0 },
        modifiers_down: wire::ModifierKeys::new(),
    }));
    let receive = async {
        loop {
            if matches!(
                right.recv_event().await.expect("enter"),
                LinkEvent::Reliable {
                    message: WireMessage::Control(ControlMessage::Enter(_)),
                    ..
                }
            ) {
                right
                    .admit_input_epoch(a.identity().device_id(), epoch)
                    .expect("admission");
                break;
            }
        }
    };
    let (sent, ()) = tokio::join!(
        left.send_reliable(b.identity().device_id(), message),
        receive
    );
    sent.expect("Enter ACK");
}

async fn samples(
    left: &NativeLink,
    right: &NativeLink,
    id: &str,
    receiver_id: &str,
    next: &mut u64,
    count: usize,
    phase: &str,
    progress: Option<&Progress>,
) -> (Duration, Duration) {
    let mut reliable = Vec::with_capacity(count);
    let mut mouse = Vec::with_capacity(count);
    for _ in 0..count {
        *next += 1;
        let input = WireMessage::Input(InputMessage::Key(InputKey {
            epoch: 1,
            seq: *next,
            hid_usage: Key(4),
            down: false,
        }));
        let start = Instant::now();
        left.send_reliable(id, input.clone()).await.expect("input");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let LinkEvent::Reliable {
                    message: WireMessage::Input(value),
                    ..
                } = right.recv_event().await.expect("input receive")
                {
                    assert_eq!(WireMessage::Input(value), input);
                    break;
                }
            }
        })
        .await
        .expect("input deadline");
        reliable.push(start.elapsed());
        let start = Instant::now();
        loop {
            match left.send_datagram(
                id,
                Move {
                    seq: *next,
                    x: 1.0,
                    y: 2.0,
                },
            ) {
                Ok(()) => break,
                Err(glide_net::LinkError::Busy) => tokio::task::yield_now().await,
                Err(error) => panic!("datagram: {error}"),
            }
        }
        let delivery = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let ready = right.mouse_ready().notified();
                tokio::pin!(ready);
                ready.as_mut().enable();
                match right.try_recv_move() {
                    Ok(Some(movement)) => {
                        assert_eq!(movement.movement.seq, *next);
                        break;
                    }
                    Err(glide_net::LinkError::Busy) => tokio::task::yield_now().await,
                    Ok(None) => ready.await,
                    Err(error) => panic!("mouse: {error}"),
                }
            }
        })
        .await;
        if delivery.is_err() {
            let received = progress.map(Progress::snapshot);
            eprintln!(
                "mouse datagram deadline phase={phase} sample={}; connected={}/{}; tokens={:?}/{:?}; receive_progress={received:?}",
                *next,
                left.peer_token(id).is_ok(),
                right.peer_token(receiver_id).is_ok(),
                left.peer_token(id),
                right.peer_token(receiver_id),
            );
        }
        delivery.expect("datagram deadline");
        mouse.push(start.elapsed());
    }
    reliable.sort_unstable();
    mouse.sort_unstable();
    (reliable[count * 99 / 100], mouse[count * 99 / 100])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupted_quic_transfer_resumes_verified_staging() {
    let (a_dir, b_dir, a, b) = paired().await;
    let input = a_dir.path().join("source.mp4");
    source(&input, 32 * 1024 * 1024);
    let config = Config {
        chunk_size: 256 * 1024,
        parallel_streams: 4,
        rate_limit_bps: Some(16 * 1024 * 1024),
        ..Config::default()
    };
    let sender = FileEngine::new(a_dir.path(), config.clone())
        .await
        .expect("sender");
    let receiver = FileEngine::new(b_dir.path(), config.clone())
        .await
        .expect("receiver");
    let plan = build_manifest(
        vec![input.clone()],
        "resume-job".into(),
        "clip".into(),
        config,
        Cancel::new(),
    )
    .await
    .expect("plan");
    let left = a.link();
    let right = b.link();
    let old = left
        .peer_token(b.identity().device_id())
        .expect("old token");
    let (send, recv) = tokio::join!(
        left.open_transfer(b.identity().device_id(), "resume-job", 4),
        right.accept_transfer()
    );
    let send_progress = Progress::new();
    let receive_progress = Progress::new();
    let cancel = Cancel::new();
    let cut = async {
        let mut poll = tokio::time::interval(Duration::from_millis(1));
        tokio::time::timeout(Duration::from_secs(10), async {
            while receive_progress.snapshot().bytes_done < 2 * 1024 * 1024 {
                poll.tick().await;
            }
        })
        .await
        .expect("verified chunks before cut");
        left.close(b.identity().device_id())
            .await
            .expect("kill session");
    };
    let (sent, received, ()) = tokio::join!(
        sender.send_native(
            plan.clone(),
            send.expect("send streams"),
            &cancel,
            &send_progress
        ),
        receiver.receive_native(
            recv.expect("recv streams"),
            Consent::Automatic,
            &cancel,
            &receive_progress
        ),
        cut
    );
    assert!(matches!(sent, Err(glide_xfer::Error::Io(_))));
    assert!(
        matches!(received, Err(glide_xfer::Error::Io(_))),
        "transport interruption retains resume: {:?}",
        received.err()
    );
    assert!(receive_progress.snapshot().bytes_done >= 2 * 1024 * 1024);
    wait_connections(&a, &b).await;
    assert_ne!(
        left.peer_token(b.identity().device_id())
            .expect("replacement"),
        old
    );
    let (send, recv) = tokio::join!(
        left.open_transfer(b.identity().device_id(), "resume-job", 4),
        right.accept_transfer()
    );
    let resumed_send = Progress::new();
    let resumed_receive = Progress::new();
    let (sent, received) = tokio::join!(
        sender.send_native(plan, send.expect("fresh streams"), &cancel, &resumed_send),
        receiver.receive_native(
            recv.expect("fresh receive"),
            Consent::Automatic,
            &cancel,
            &resumed_receive
        )
    );
    sent.expect("resumed send");
    let received = received.expect("resumed receive");
    assert_eq!(hash(&input), hash(&received.paths[0]));
    assert_eq!(resumed_receive.snapshot().bytes_done, 32 * 1024 * 1024);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_native_codec_closes_the_authenticated_session() {
    use tokio::io::AsyncWriteExt;
    let (_a_dir, b_dir, a, b) = paired().await;
    let left = a.link();
    let right = b.link();
    let (send, recv) = tokio::join!(
        left.open_transfer(b.identity().device_id(), "malformed-job", 1),
        right.accept_transfer()
    );
    let mut send = send.expect("streams");
    let recv = recv.expect("authenticated receive");
    let handle = recv.handle.clone();
    send.control
        .write_all(&[0, 0, 0, 2, 3, 128])
        .await
        .expect("malformed frame");
    let receiver = FileEngine::new(b_dir.path(), Config::default())
        .await
        .expect("receiver");
    let result = receiver
        .receive_native(recv, Consent::Automatic, &Cancel::new(), &Progress::new())
        .await;
    assert!(
        matches!(result, Err(glide_xfer::Error::Codec(_))),
        "unexpected malformed result: {:?}",
        result.err()
    );
    assert!(
        handle.is_closed(),
        "codec failures revoke the authenticated session"
    );
}

fn bounded_runtime() -> tokio::runtime::Runtime {
    let cores = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(2);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(cores.clamp(2, 4))
        .max_blocking_threads(cores.saturating_sub(1).clamp(1, 4))
        .enable_all()
        .build()
        .expect("bounded benchmark runtime")
}

#[test]
#[ignore = "explicit real 512 MiB QUIC/FileEngine latency acceptance benchmark"]
fn bulk_quic_transfer_keeps_input_p99_under_three_ms() {
    bounded_runtime().block_on(run_bulk_quic_transfer());
}

async fn run_bulk_quic_transfer() {
    const BYTES: usize = 512 * 1024 * 1024;
    let (a_dir, b_dir, a, b) = paired().await;
    admit(&a, &b, 1).await;
    let left = a.link();
    let right = b.link();
    let mut seq = 0;
    samples(
        &left,
        &right,
        b.identity().device_id(),
        a.identity().device_id(),
        &mut seq,
        100,
        "warmup",
        None,
    )
    .await;
    let idle = samples(
        &left,
        &right,
        b.identity().device_id(),
        a.identity().device_id(),
        &mut seq,
        1000,
        "idle",
        None,
    )
    .await;
    let input = a_dir.path().join("large.mp4");
    incompressible_source(&input, BYTES);
    let config = Config {
        chunk_size: 1024 * 1024,
        parallel_streams: 2,
        max_concurrent_transfers: 2,
        ..Config::default()
    };
    let sender = FileEngine::new(a_dir.path(), config.clone())
        .await
        .expect("sender");
    let receiver = FileEngine::new(b_dir.path(), config.clone())
        .await
        .expect("receiver");
    let plan = build_manifest(
        vec![input.clone()],
        "latency-job".into(),
        "clip".into(),
        config,
        Cancel::new(),
    )
    .await
    .expect("plan");
    let (send, recv) = tokio::join!(
        left.open_transfer(b.identity().device_id(), "latency-job", 2),
        right.accept_transfer()
    );
    let sent_progress = Progress::new();
    let received_progress = Progress::new();
    let cancel = Cancel::new();
    let measure = async {
        let mut poll = tokio::time::interval(Duration::from_millis(1));
        tokio::time::timeout(Duration::from_secs(10), async {
            while received_progress.snapshot().bytes_done < 4 * 1024 * 1024 {
                poll.tick().await;
            }
        })
        .await
        .expect("bulk active");
        let before = received_progress.snapshot().bytes_done;
        let result = samples(
            &left,
            &right,
            b.identity().device_id(),
            a.identity().device_id(),
            &mut seq,
            1000,
            "saturated-transfer",
            Some(&received_progress),
        )
        .await;
        let after = received_progress.snapshot().bytes_done;
        assert!(
            before < after && after < BYTES as u64,
            "traffic persists throughout sampling: {before}..{after}"
        );
        result
    };
    let start = Instant::now();
    let send_cancel = cancel.clone();
    let sent = tokio::spawn(async move {
        sender
            .send_native(
                plan,
                send.expect("send streams"),
                &send_cancel,
                &sent_progress,
            )
            .await
    });
    let receive_cancel = cancel.clone();
    let receive_progress = received_progress.clone();
    let received = tokio::spawn(async move {
        receiver
            .receive_native(
                recv.expect("recv streams"),
                Consent::Automatic,
                &receive_cancel,
                &receive_progress,
            )
            .await
    });
    let loaded = measure.await;
    sent.await.expect("sender actor").expect("512 MiB send");
    let received = received
        .await
        .expect("receiver actor")
        .expect("512 MiB receive");
    let elapsed = start.elapsed();
    assert_eq!(hash(&input), hash(&received.paths[0]));
    println!("512 MiB real QUIC/FileEngine: idle input p99={:?}, mouse p99={:?}; loaded input p99={:?}, mouse p99={:?}; throughput={:.2} MiB/s", idle.0, idle.1, loaded.0, loaded.1, 512.0 / elapsed.as_secs_f64());
    assert!(
        loaded.0 < Duration::from_millis(3),
        "reliable input p99 exceeded 3 ms: {:?}",
        loaded.0
    );
    assert!(
        loaded.1 < Duration::from_millis(3),
        "mouse datagram p99 exceeded 3 ms: {:?}",
        loaded.1
    );
}
