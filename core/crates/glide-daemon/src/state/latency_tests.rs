use super::{native_tests, Core};
use glide_net::TestKeyStore;
use glide_platform::{InputEvent, InputEventKind, Key, Os, Point};
use glide_proto::ipc::{LayoutDevice, Request};
use serde_json::json;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{sync::Mutex, task::JoinHandle};

fn bounded_runtime() -> tokio::runtime::Runtime {
    let cores = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(2);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(cores.clamp(2, 4))
        .max_blocking_threads(cores.saturating_sub(1).clamp(1, 4))
        .enable_all()
        .build()
        .expect("bounded latency-test runtime")
}

async fn wait_injected(core: &Arc<Mutex<Core>>, matches: impl Fn(&InputEvent) -> bool) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let injected = {
                let core = core.lock().await;
                core.mock_platform()
                    .expect("mock platform")
                    .input
                    .take_injected_events()
                    .expect("injected events")
            };
            if injected
                .iter()
                .any(|event| event.injected && matches(event))
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("captured event injected over QUIC");
}

#[test]
fn native_core_capture_to_inject_loopback_p50_p99() {
    bounded_runtime().block_on(async {
        // Exclude the native crypto/file fixtures during absolute latency measurement.
        let _timing = native_tests::NATIVE_TIMING.write().await;
        let temp = tempfile::tempdir().expect("latency fixture");
        let a_dir = temp.path().join("latency-a");
        let b_dir = temp.path().join("latency-b");
        std::fs::create_dir_all(&a_dir).expect("A data directory");
        std::fs::create_dir_all(&b_dir).expect("B data directory");
        let store = TestKeyStore::default();
        let mut a = native_tests::native_core(&a_dir, 0, Os::Windows, &store).await;
        let mut b = native_tests::native_core(&b_dir, 0, Os::Macos, &store).await;
        native_tests::pair_native_cores(&mut a, &mut b).await;

        let a_id = a.snapshot().self_info.device_id;
        let b_id = b.snapshot().self_info.device_id;
        let layout = a
            .handle(Request {
                id: 10,
                method: "set_layout".into(),
                params: json!({
                    "devices": [
                        LayoutDevice {
                            device_id: a_id.clone(),
                            x: 0.0,
                            y: 0.0,
                        },
                        LayoutDevice {
                            device_id: b_id.clone(),
                            x: 1920.0,
                            y: 0.0,
                        }
                    ]
                }),
            })
            .await
            .expect("layout response");
        assert!(layout.ok, "native loopback layout accepted");
        let settings = a
            .handle(Request {
                id: 11,
                method: "set_settings".into(),
                params: json!({
                    "patch": {"switching":{"edge_delay_ms":0,"corner_dead_zone_px":0}}
                }),
            })
            .await
            .expect("settings response");
        assert!(settings.ok, "native loopback edge switching enabled");

        let a = Arc::new(Mutex::new(a));
        let b = Arc::new(Mutex::new(b));
        let mut pumps: Vec<JoinHandle<()>> = native_tests::spawn_native_pumps(a.clone()).await;
        pumps.extend(native_tests::spawn_native_pumps(b.clone()).await);
        native_tests::wait_native_layout(&a, &b).await;
        {
            let mut host = a.lock().await;
            host.capture_input(InputEvent {
                injected: false,
                kind: InputEventKind::PointerMoved {
                    position: Point { x: 0.0, y: 200.0 },
                    delta_x: 0.0,
                    delta_y: 200.0,
                },
            })
            .await
            .expect("position at edge");
            host.capture_input(InputEvent {
                injected: false,
                kind: InputEventKind::PointerMoved {
                    position: Point { x: 1919.0, y: 200.0 },
                    delta_x: 2000.0,
                    delta_y: 0.0,
                },
            })
            .await
            .expect("enter peer screen");
        }
        assert_eq!(a.lock().await.snapshot().active_device_id, b_id);
        b.lock()
            .await
            .mock_platform()
            .expect("mock receiver")
            .input
            .take_injected_events()
            .expect("clear Enter injection");

        let mut reliable = Vec::with_capacity(1000);
        for _ in 0..1000 {
            let start = Instant::now();
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
                .expect("capture key down");
            wait_injected(&b, |event| {
                matches!(event.kind, InputEventKind::Key { key: Key(4), down: true })
            })
            .await;
            reliable.push(start.elapsed());
            a.lock()
                .await
                .capture_input(InputEvent {
                    injected: false,
                    kind: InputEventKind::Key {
                        key: Key(4),
                        down: false,
                    },
                })
                .await
                .expect("capture key up");
            wait_injected(&b, |event| {
                matches!(event.kind, InputEventKind::Key { key: Key(4), down: false })
            })
            .await;

        }
        reliable.sort_unstable();
        println!("real QUIC Core capture-to-inject, 1000 reliable key samples: p50={:?} p99={:?}", reliable[500], reliable[990]);
        assert!(reliable[990] <= Duration::from_millis(50), "reliable p99 exceeds 50 ms: {:?}", reliable[990]);

        // Datagram moves are latest-wins: intermediate samples may disappear. A burst
        // must still converge to the final captured position without another event.
        let final_position = {
            let mut host = a.lock().await;
            for _ in 0..1000 {
                host.capture_input(InputEvent {
                    injected: false,
                    kind: InputEventKind::PointerMoved {
                        position: Point { x: 0.0, y: 200.0 }, delta_x: 0.1, delta_y: 0.0,
                    },
                }).await.expect("capture mouse burst");
            }
            host.engine.forwarded_position().expect("burst preserves forwarding").1
        };
        wait_injected(&b, |event| matches!(event.kind, InputEventKind::PointerMoved { position, .. } if position == final_position)).await;
        assert_eq!(a.lock().await.snapshot().active_device_id, b_id, "mouse burst does not disconnect");

        a.lock().await.shutdown().await.expect("host shutdown");
        b.lock().await.shutdown().await.expect("receiver shutdown");
        for pump in pumps {
            pump.abort();
        }
    });
}
