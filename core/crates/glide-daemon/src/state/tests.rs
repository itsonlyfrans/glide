use super::*;

async fn replicate_layout(core: &mut Core, clock: u64, devices: Vec<LayoutDevice>) {
    let peer = core.state.peers[0].device_id.clone();
    core.receive_link(LinkEvent::Reliable {
        peer_id: peer.clone(),
        peer_token: None,
        message: WireMessage::Control(ControlMessage::LayoutUpdate(wire::LayoutUpdate {
            version: (clock, peer),
            devices: wire::LayoutDevices::try_from_vec(devices).expect("bounded layout"),
        })),
    })
    .await
    .expect("remote layout is reconciled or silently dropped");
}

#[tokio::test]
async fn pairing_places_peer_at_right_edge_and_remote_layout_missing_home_is_merged() {
    let dir = tempfile::tempdir().expect("dir");
    let mut core = paired_core(dir.path()).await;
    let home = core.state.self_info.device_id.clone();
    let peer = core.state.peers[0].device_id.clone();
    assert_eq!(core.state.layout.devices.len(), 2);
    assert_eq!(
        core.state.layout.devices[0],
        LayoutDevice {
            device_id: home.clone(),
            x: 0.0,
            y: 0.0
        }
    );
    assert_eq!(
        core.state.layout.devices[1],
        LayoutDevice {
            device_id: peer.clone(),
            x: 1920.0,
            y: 0.0
        }
    );
    core.take_events();
    replicate_layout(
        &mut core,
        10,
        vec![LayoutDevice {
            device_id: peer,
            x: 0.0,
            y: 0.0,
        }],
    )
    .await;
    let local = core
        .state
        .layout
        .devices
        .iter()
        .find(|d| d.device_id == home)
        .expect("home restored");
    assert_eq!((local.x, local.y), (1920.0, 0.0));
    assert_eq!(core.layout_version, (11, home));
    validate_layout(&core.state.layout.devices, &core.state).expect("valid merged desktop");
    assert!(!core
        .take_events()
        .iter()
        .any(|e| matches!(e, Event::Notification(_))));
    assert_eq!(
        Core::mock(dir.path(), None)
            .await
            .expect("restart")
            .state
            .layout,
        core.state.layout
    );
}

#[tokio::test]
async fn replicated_overlap_uses_nearest_free_edge_and_drops_unknown_devices() {
    let dir = tempfile::tempdir().expect("dir");
    let mut core = paired_core(dir.path()).await;
    let home = core.state.self_info.device_id.clone();
    let peer = core.state.peers[0].device_id.clone();
    core.take_events();
    replicate_layout(
        &mut core,
        10,
        vec![
            LayoutDevice {
                device_id: home.clone(),
                x: 0.0,
                y: 0.0,
            },
            LayoutDevice {
                device_id: peer,
                x: 1900.0,
                y: 0.0,
            },
            LayoutDevice {
                device_id: "f".repeat(64),
                x: -3000.0,
                y: 0.0,
            },
        ],
    )
    .await;
    assert_eq!(core.state.layout.devices.len(), 2);
    assert_eq!(core.state.layout.devices[1].x, 1920.0);
    assert_eq!(core.state.layout.devices[1].y, 0.0);
    assert_eq!(core.layout_version, (11, home));
    validate_layout(&core.state.layout.devices, &core.state).expect("no overlap");
    assert!(!core
        .take_events()
        .iter()
        .any(|e| matches!(e, Event::Notification(_))));
}

#[tokio::test]
async fn reconciliation_handles_blocked_edges_and_full_device_limit() {
    let dir = tempfile::tempdir().expect("dir");
    let core = paired_core(dir.path()).await;
    let mut state = core.snapshot();
    let mut peer = state.peers[0].clone();
    peer.device_id = "f".repeat(64);
    state.peers.push(peer);
    for monitor in state
        .self_info
        .monitors
        .iter_mut()
        .chain(state.peers.iter_mut().flat_map(|p| p.monitors.iter_mut()))
    {
        monitor.w = 100.0;
        monitor.h = 100.0;
    }
    let devices = vec![
        LayoutDevice {
            device_id: state.self_info.device_id.clone(),
            x: 0.0,
            y: 0.0,
        },
        LayoutDevice {
            device_id: state.peers[0].device_id.clone(),
            x: 100.0,
            y: 0.0,
        },
        LayoutDevice {
            device_id: state.peers[1].device_id.clone(),
            x: 90.0,
            y: 0.0,
        },
    ];
    let merged = reconcile_layout(&devices, &state).expect("blocked edges reconciled");
    assert_eq!((merged.devices[2].x, merged.devices[2].y), (90.0, -100.0));
    validate_layout(&merged.devices, &state).expect("free adjacent placement");
    let template = state.peers[0].clone();
    state.peers = (1..32)
        .map(|i| Peer {
            device_id: format!("{i:064x}"),
            ..template.clone()
        })
        .collect();
    let devices: Vec<_> = std::iter::once(&state.self_info.device_id)
        .chain(state.peers.iter().map(|p| &p.device_id))
        .map(|id| LayoutDevice {
            device_id: id.clone(),
            x: 0.0,
            y: 0.0,
        })
        .collect();
    let merged = reconcile_layout(&devices, &state).expect("maximum size reconciled");
    assert_eq!(merged.devices.len(), 32);
    validate_layout(&merged.devices, &state).expect("no overlaps at full capacity");
}

#[tokio::test]
async fn remote_lww_tie_breaks_by_device_id_and_storage_failure_is_actionable() {
    let dir = tempfile::tempdir().expect("dir");
    let mut core = paired_core(dir.path()).await;
    core.take_events();
    core.layout_version = (10, "0".repeat(64));
    let devices = core.state.layout.devices.clone();
    replicate_layout(&mut core, 10, devices.clone()).await;
    assert_eq!(core.layout_version, (10, "e".repeat(64)));
    core.layout_version = (10, "f".repeat(64));
    replicate_layout(&mut core, 10, devices.clone()).await;
    assert_eq!(core.layout_version, (10, "f".repeat(64)));
    assert!(!core
        .take_events()
        .iter()
        .any(|e| matches!(e, Event::Notification(_))));
    let original = core.state.layout.clone();
    let original_clock = core.lamport;
    let saved_dir = core.data_dir.clone();
    core.data_dir = dir.path().join("config.json");
    let peer = core.state.peers[0].device_id.clone();
    let failure = core
        .receive_link(LinkEvent::Reliable {
            peer_id: peer.clone(),
            peer_token: None,
            message: WireMessage::Control(ControlMessage::LayoutUpdate(wire::LayoutUpdate {
                version: (20, peer),
                devices: wire::LayoutDevices::try_from_vec(devices).expect("layout"),
            })),
        })
        .await
        .expect_err("local storage failed");
    core.data_dir = saved_dir;
    assert_eq!(failure.code, ErrorCode::Internal);
    assert_eq!(core.state.layout, original);
    assert_eq!(core.layout_version, (10, "f".repeat(64)));
    assert_eq!(core.lamport, original_clock);
    assert!(core
        .take_events()
        .iter()
        .any(|e| matches!(e, Event::Notification(n) if n.level == "error")));
}

#[tokio::test]
async fn newer_layout_version_with_same_positions_preserves_active_input() {
    let dir = tempfile::tempdir().expect("dir");
    let mut core = paired_core(dir.path()).await;
    let peer = core.state.peers[0].device_id.clone();
    core.receive_link(LinkEvent::Reliable {
        peer_id: peer.clone(),
        peer_token: None,
        message: WireMessage::Control(ControlMessage::Enter(wire::Enter {
            epoch: 1,
            pos: glide_platform::Point { x: 10.0, y: 10.0 },
            modifiers_down: wire::ModifierKeys::new(),
        })),
    })
    .await
    .expect("enter");
    core.receive_link(LinkEvent::Reliable {
        peer_id: peer.clone(),
        peer_token: None,
        message: WireMessage::Input(InputMessage::Key(wire::InputKey {
            epoch: 1,
            seq: 1,
            hid_usage: Key(4),
            down: true,
        })),
    })
    .await
    .expect("held key");
    let devices = core.state.layout.devices.clone();
    replicate_layout(&mut core, 10, devices).await;
    assert_eq!(core.receiving_from.as_deref(), Some(peer.as_str()));
    assert!(core
        .mock_platform()
        .expect("platform")
        .input
        .held_keys()
        .expect("keys")
        .contains(&Key(4)));
    assert_eq!(core.layout_version, (10, peer));
}

#[tokio::test]
async fn malformed_and_stale_remote_layouts_do_not_change_state_clock_or_notify() {
    let dir = tempfile::tempdir().expect("dir");
    let mut core = paired_core(dir.path()).await;
    let home = core.state.self_info.device_id.clone();
    let peer = core.state.peers[0].device_id.clone();
    let before = core.state.layout.clone();
    let version = core.layout_version.clone();
    let clock = core.lamport;
    core.take_events();
    for x in [f64::NAN, f64::INFINITY, 1_000_001.0] {
        replicate_layout(
            &mut core,
            100,
            vec![LayoutDevice {
                device_id: peer.clone(),
                x,
                y: 0.0,
            }],
        )
        .await;
    }
    replicate_layout(&mut core, u64::MAX, before.devices.clone()).await;
    replicate_layout(
        &mut core,
        100,
        vec![
            LayoutDevice {
                device_id: peer.clone(),
                x: 0.0,
                y: 0.0,
            },
            LayoutDevice {
                device_id: peer.clone(),
                x: 2000.0,
                y: 0.0,
            },
        ],
    )
    .await;
    let too_many: Vec<_> = (0..33)
        .map(|i| LayoutDevice {
            device_id: format!("{i:064x}"),
            x: 0.0,
            y: 0.0,
        })
        .collect();
    assert!(reconcile_layout(&too_many, &core.state).is_err());
    replicate_layout(&mut core, 100, too_many).await;
    replicate_layout(
        &mut core,
        0,
        vec![LayoutDevice {
            device_id: home,
            x: 9000.0,
            y: 0.0,
        }],
    )
    .await;
    assert_eq!(core.state.layout, before);
    assert_eq!(core.layout_version, version);
    assert_eq!(core.lamport, clock);
    assert!(!core
        .take_events()
        .iter()
        .any(|e| matches!(e, Event::Notification(_))));
}

#[tokio::test]
async fn manager_pairing_event_is_applied_before_early_link_message() {
    let dir = tempfile::tempdir().expect("dir");
    let mut core = paired_core(dir.path()).await;
    let peer = core.state.peers.pop().expect("paired peer");
    core.state
        .layout
        .devices
        .retain(|d| d.device_id == core.state.self_info.device_id);
    let manager = InMemoryPeerManager::new();
    core.peer_events = manager.events();
    assert!(manager.script_peer_event(PeerManagerEvent::Paired(peer.clone())));
    core.take_events();
    core.receive_link(LinkEvent::Reliable {
        peer_id: peer.device_id.clone(),
        peer_token: core.link.peer_token(&peer.device_id).ok(),
        message: WireMessage::Control(ControlMessage::Heartbeat(wire::Heartbeat { seq: 1, ts: 0 })),
    })
    .await
    .expect("early message");
    assert_eq!(core.state.peers.len(), 1);
    assert!(
        core.link.peer_token(&peer.device_id).is_ok(),
        "pairing race must not disconnect"
    );
    assert!(!core
        .take_events()
        .iter()
        .any(|e| matches!(e, Event::Notification(_))));
}

#[tokio::test]
async fn unknown_sender_flood_is_silent_and_retains_no_messages_or_peer_state() {
    let dir = tempfile::tempdir().expect("dir");
    let mut core = Core::mock(dir.path(), None).await.expect("core");
    let unpinned = Arc::new(UnpinnedLink::default());
    core.link = unpinned.clone();
    core.take_events();
    let start = Instant::now();
    tokio::time::timeout(Duration::from_secs(3), async {
        for i in 0..4096 {
            core.receive_link(LinkEvent::Reliable {
                peer_id: format!("{i:064x}"),
                peer_token: Some(glide_net::PeerToken {
                    slot: 0,
                    generation: 1,
                }),
                message: WireMessage::Clipboard(wire::ClipboardMessage::ClipData(wire::ClipData {
                    clip_id: "flood".into(),
                    format: "text".into(),
                    data: wire::ClipboardBytes::try_from_vec(vec![0; 4096]).expect("bytes"),
                })),
            })
            .await
            .expect("silent drop");
            assert!(core.events.is_empty());
            assert!(core.state.peers.is_empty());
            assert!(core.connected_tokens.is_empty());
            assert!(core.received_epochs.is_empty());
            assert!(core.clipboard.is_settled());
        }
    })
    .await
    .expect("flood handling is bounded in time");
    assert!(start.elapsed() < Duration::from_secs(3));
    assert_eq!(core.state.layout.devices.len(), 1);
    assert_eq!(
        unpinned.closes.load(std::sync::atomic::Ordering::Relaxed),
        4096
    );
}

#[derive(Default)]
struct UnpinnedLink {
    inner: InMemoryLink,
    closes: std::sync::atomic::AtomicUsize,
    stalled_close: bool,
    connected: bool,
    busy_moves: std::sync::atomic::AtomicUsize,
}

impl glide_net::MouseReceiver for UnpinnedLink {
    fn try_recv_move(&self) -> Result<Option<glide_net::ReceivedMove>, glide_net::LinkError> {
        Ok(None)
    }
}

impl Link for UnpinnedLink {
    fn peer_token(&self, id: &str) -> Result<glide_net::PeerToken, glide_net::LinkError> {
        if self.connected {
            self.inner.peer_token(id)
        } else {
            Err(glide_net::LinkError::NotConnected)
        }
    }
    fn mouse_ready(&self) -> &tokio::sync::Notify {
        self.inner.mouse_ready()
    }
    fn recv_event(&self) -> glide_net::NetFuture<'_, Result<LinkEvent, glide_net::LinkError>> {
        self.inner.recv_event()
    }
    fn connect<'a>(
        &'a self,
        address: &'a str,
        id: Option<&'a str>,
    ) -> glide_net::NetFuture<'a, Result<glide_net::PeerConnection, glide_net::LinkError>> {
        self.inner.connect(address, id)
    }
    fn accept(
        &self,
    ) -> glide_net::NetFuture<'_, Result<glide_net::PeerConnection, glide_net::LinkError>> {
        self.inner.accept()
    }
    fn send_reliable<'a>(
        &'a self,
        id: &'a str,
        message: WireMessage,
    ) -> glide_net::NetFuture<'a, Result<(), glide_net::LinkError>> {
        self.inner.send_reliable(id, message)
    }
    fn send_datagram(&self, id: &str, movement: wire::Move) -> Result<(), glide_net::LinkError> {
        if self
            .busy_moves
            .try_update(
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
                |n| n.checked_sub(1),
            )
            .is_ok()
        {
            return Err(glide_net::LinkError::Busy);
        }
        self.inner.send_datagram(id, movement)
    }
    fn recv(&self) -> glide_net::NetFuture<'_, Result<LinkEvent, glide_net::LinkError>> {
        self.inner.recv()
    }
    fn close<'a>(
        &'a self,
        id: &'a str,
    ) -> glide_net::NetFuture<'a, Result<(), glide_net::LinkError>> {
        Box::pin(async move {
            self.closes
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if self.stalled_close {
                std::future::pending::<()>().await;
            }
            self.inner.close(id).await
        })
    }
}

#[tokio::test]
async fn unknown_sender_cannot_stall_core_with_a_hanging_close() {
    let dir = tempfile::tempdir().expect("dir");
    let mut core = Core::mock(dir.path(), None).await.expect("core");
    let link = Arc::new(UnpinnedLink {
        stalled_close: true,
        ..UnpinnedLink::default()
    });
    core.link = link.clone();
    core.take_events();
    tokio::time::timeout(
        Duration::from_millis(500),
        core.receive_link(LinkEvent::Reliable {
            peer_id: "f".repeat(64),
            peer_token: None,
            message: WireMessage::Control(ControlMessage::Heartbeat(wire::Heartbeat {
                seq: 0,
                ts: 0,
            })),
        }),
    )
    .await
    .expect("100 ms close deadline")
    .expect("silent drop");
    assert_eq!(link.closes.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert!(core.take_events().is_empty());
}

#[tokio::test]
async fn busy_mouse_send_retains_last_position_without_ending_session() {
    let dir = tempfile::tempdir().expect("dir");
    let mut core = paired_core(dir.path()).await;
    let peer = core.state.peers[0].device_id.clone();
    let link = Arc::new(UnpinnedLink {
        connected: true,
        ..UnpinnedLink::default()
    });
    link.inner.script_reachable("127.0.0.1:24801", peer.clone());
    link.connect("127.0.0.1:24801", Some(&peer))
        .await
        .expect("connect");
    core.link = link.clone();
    assert!(
        request(
            &mut core,
            "set_settings",
            json!({"patch":{"switching":{"edge_delay_ms":0,"corner_dead_zone_px":0}}})
        )
        .await
        .ok
    );
    let movement = |dx| InputEvent {
        injected: false,
        kind: InputEventKind::PointerMoved {
            position: glide_platform::Point { x: 0.0, y: 200.0 },
            delta_x: dx,
            delta_y: 0.0,
        },
    };
    core.capture_input(movement(2000.0)).await.expect("enter");
    assert_eq!(core.state.active_device_id, peer);
    link.busy_moves
        .store(2, std::sync::atomic::Ordering::Relaxed);
    core.capture_input(movement(1.0))
        .await
        .expect("first busy move");
    core.capture_input(movement(2.0))
        .await
        .expect("last busy move");
    assert_eq!(core.state.active_device_id, peer, "Busy is not link loss");
    let expected = core
        .engine
        .forwarded_position()
        .expect("still forwarding")
        .1;
    core.tick()
        .await
        .expect("flush last move without new input");
    let latest = link.inner.latest_move(&peer).expect("last move retained");
    assert_eq!((latest.x, latest.y), (expected.x, expected.y));
    assert_eq!(link.closes.load(std::sync::atomic::Ordering::Relaxed), 0);
    // A retained move belongs to one admitted connection, never its replacement.
    link.busy_moves
        .store(1, std::sync::atomic::Ordering::Relaxed);
    core.capture_input(movement(1.0))
        .await
        .expect("pending move before reconnect");
    link.inner.close(&peer).await.expect("close old generation");
    link.inner
        .connect("127.0.0.1:24801", Some(&peer))
        .await
        .expect("replacement generation");
    core.flush_pending_move()
        .await
        .expect("discard old generation");
    assert!(link.inner.latest_move(&peer).is_none());
    assert_eq!(core.state.active_device_id, core.state.self_info.device_id);
}

#[tokio::test]
async fn transient_and_unsolicited_peer_failures_do_not_notify_but_local_actions_do() {
    let dir = tempfile::tempdir().expect("dir");
    let mut core = Core::mock(dir.path(), None).await.expect("core");
    core.take_events();
    for code in [
        ErrorCode::Unreachable,
        ErrorCode::NotPaired,
        ErrorCode::InvalidParams,
        ErrorCode::Internal,
    ] {
        core.notify_failure(&error(code, "transient failure"));
    }
    let manager = InMemoryPeerManager::new();
    core.peer_events = manager.events();
    for code in [
        ErrorCode::BadCode,
        ErrorCode::LockedOut,
        ErrorCode::CodeExpired,
        ErrorCode::PermissionDenied,
    ] {
        assert!(manager.script_peer_event(PeerManagerEvent::PairingResult {
            ok: false,
            device_id: None,
            error: Some(code)
        }));
    }
    assert!(
        manager.script_peer_event(PeerManagerEvent::InjectionDenied {
            device_id: "f".repeat(64)
        })
    );
    core.pump_peer_events().await;
    assert!(!core
        .take_events()
        .iter()
        .any(|e| matches!(e, Event::Notification(_))));
    for code in [
        ErrorCode::CodeExpired,
        ErrorCode::LockedOut,
        ErrorCode::PermissionDenied,
    ] {
        core.notify_failure(&error(code, "actionable local failure"));
    }
    assert_eq!(
        core.take_events()
            .iter()
            .filter(|e| matches!(e, Event::Notification(_)))
            .count(),
        3
    );
}

#[tokio::test]
async fn reliable_queue_failure_closes_the_session_instead_of_keeping_remote_holds_alive() {
    let dir = tempfile::tempdir().expect("dir");
    let mut core = paired_core(dir.path()).await;
    let peer = "e".repeat(64);
    let link = Arc::new(InMemoryLink::new());
    link.script_reachable("127.0.0.1:24801", peer.clone());
    link.connect("127.0.0.1:24801", Some(&peer))
        .await
        .expect("connect");
    let message = WireMessage::Control(ControlMessage::Heartbeat(wire::Heartbeat {
        seq: 1,
        ts: now_ms(),
    }));
    let mut full = false;
    for _ in 0..4096 {
        if link.send_reliable(&peer, message.clone()).await.is_err() {
            full = true;
            break;
        }
    }
    assert!(full, "bounded mock outbox");
    core.link = link.clone();
    assert!(core.send_reliable(&peer, message).await.is_err());
    assert!(
        link.peer_token(&peer).is_err(),
        "failed reliable delivery must kill the session"
    );
}

async fn settle_clipboard(core: &mut Core) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            core.poll_clipboard().await;
            if core.clipboard.is_settled() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("bounded clipboard workers");
}

async fn incoming_clipboard(core: &mut Core, id: &str, formats: Vec<wire::ClipFormat>) {
    core.receive_link(LinkEvent::Reliable {
        peer_token: None,
        peer_id: "e".repeat(64),
        message: WireMessage::Clipboard(wire::ClipboardMessage::ClipAnnounce(wire::ClipAnnounce {
            clip_id: id.into(),
            origin: "e".repeat(64),
            timestamp_ms: now_ms(),
            formats: wire::ClipFormats::try_from_vec(formats).expect("formats"),
            files: wire::AnnouncedFiles::new(),
        })),
    })
    .await
    .expect("announce");
}

async fn incoming_payload(
    core: &mut Core,
    id: &str,
    format: &str,
    bytes: &[u8],
) -> Result<(), IpcError> {
    core.receive_link(LinkEvent::Reliable {
        peer_token: None,
        peer_id: "e".repeat(64),
        message: WireMessage::Clipboard(wire::ClipboardMessage::ClipData(wire::ClipData {
            clip_id: id.into(),
            format: format.into(),
            data: wire::ClipboardBytes::try_from_vec(bytes.to_vec()).expect("bytes"),
        })),
    })
    .await
}

#[tokio::test]
async fn eager_clipboard_waits_for_complete_bundle_ignores_own_writes_and_replay() {
    use glide_platform::{ClipboardBackend, ClipboardData, ClipboardFormat};
    let dir = tempfile::tempdir().expect("dir");
    let mut core = paired_core(dir.path()).await;
    let png = hex::decode("89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c48900000010494441547801010500faff000000000000050001647895380000000049454e44ae426082").expect("valid one pixel PNG");
    incoming_clipboard(
        &mut core,
        "clip",
        vec![
            wire::ClipFormat {
                kind: "text".into(),
                mime: "text/plain".into(),
                size: 5,
            },
            wire::ClipFormat {
                kind: "html".into(),
                mime: "text/html".into(),
                size: 12,
            },
            wire::ClipFormat {
                kind: "png".into(),
                mime: "image/png".into(),
                size: png.len() as u64,
            },
        ],
    )
    .await;
    incoming_payload(&mut core, "clip", "text", b"hello")
        .await
        .expect("text");
    assert!(core
        .platform
        .clipboard_backend()
        .formats()
        .expect("formats")
        .is_empty());
    incoming_payload(&mut core, "clip", "html", b"<b>hello</b>")
        .await
        .expect("html");
    assert!(core
        .platform
        .clipboard_backend()
        .formats()
        .expect("formats")
        .is_empty());
    incoming_payload(&mut core, "clip", "png", &png)
        .await
        .expect("image");
    settle_clipboard(&mut core).await;
    let clipboard = &core.mock_platform().expect("mock").clipboard;
    for (format, bytes) in [
        (ClipboardFormat::Text, b"hello".as_slice()),
        (ClipboardFormat::Html, b"<b>hello</b>"),
        (ClipboardFormat::Png, png.as_slice()),
    ] {
        assert_eq!(
            clipboard
                .read(&format)
                .expect("read")
                .expect("present")
                .data,
            ClipboardData::Bytes(bytes.to_vec())
        );
    }
    let version = core.clipboard.version.clone();
    core.receive_link(LinkEvent::Reliable {
        peer_token: None,
        peer_id: "e".repeat(64),
        message: WireMessage::Clipboard(wire::ClipboardMessage::ClipAnnounce(wire::ClipAnnounce {
            clip_id: version.2.clone(),
            origin: version.1.clone(),
            timestamp_ms: version.0,
            formats: wire::ClipFormats::try_from_vec(vec![wire::ClipFormat {
                kind: "text".into(),
                mime: "text/plain".into(),
                size: 5,
            }])
            .expect("formats"),
            files: wire::AnnouncedFiles::new(),
        })),
    })
    .await
    .expect("replay");
    // Exact transaction replay is ignored even if a malicious peer changes its format list.
    assert_eq!(core.clipboard.version, version);
    core.poll_clipboard().await;
    assert!(!core.clipboard.pending && core.clipboard.read.is_none());
}

#[tokio::test]
async fn eager_clipboard_policy_size_confirmation_cancel_and_malformed_drop() {
    use glide_platform::ClipboardFormat;
    for action in ["accept", "cancel", "disable", "supersede", "disconnect"] {
        let dir = tempfile::tempdir().expect("dir");
        let mut core = paired_core(dir.path()).await;
        assert!(
            request(
                &mut core,
                "set_settings",
                json!({"patch":{"clipboard":{"max_auto_mb":0}}})
            )
            .await
            .ok
        );
        incoming_clipboard(
            &mut core,
            "approval",
            vec![wire::ClipFormat {
                kind: "text".into(),
                mime: "text/plain".into(),
                size: 5,
            }],
        )
        .await;
        incoming_payload(&mut core, "approval", "text", b"hello")
            .await
            .expect("data");
        assert!(core
            .platform
            .clipboard_backend()
            .formats()
            .expect("formats")
            .is_empty());
        settle_clipboard(&mut core).await;
        let transfer = core.state.transfers[0].clone();
        assert_eq!(transfer.state, TransferState::AwaitingConfirm);
        match action {
            "accept" => {
                assert!(
                    request(
                        &mut core,
                        "transfer.confirm",
                        json!({"id":transfer.id,"accept":true})
                    )
                    .await
                    .ok
                );
            }
            "cancel" => {
                assert!(
                    request(&mut core, "transfer.cancel", json!({"id":transfer.id}))
                        .await
                        .ok
                );
            }
            "supersede" => {
                core.clipboard_changed(glide_platform::ClipboardEvent::Changed {
                    formats: vec![ClipboardFormat::Text],
                    marker: None,
                    sensitivity: glide_platform::ClipboardSensitivity::default(),
                });
                assert_eq!(core.state.transfers[0].state, TransferState::Cancelled);
                assert!(
                    !request(
                        &mut core,
                        "transfer.confirm",
                        json!({"id":transfer.id,"accept":true})
                    )
                    .await
                    .ok
                );
            }
            "disconnect" => {
                core.receive_link(LinkEvent::Disconnected {
                    peer_id: "e".repeat(64),
                    reason: None,
                })
                .await
                .expect("disconnect");
                assert_eq!(core.state.transfers[0].state, TransferState::Cancelled);
            }
            _ => {
                assert!(
                    request(
                        &mut core,
                        "peer.configure",
                        json!({"device_id":"e".repeat(64),"clipboard_enabled":false})
                    )
                    .await
                    .ok
                );
            }
        }
        settle_clipboard(&mut core).await;
        assert_eq!(
            core.platform
                .clipboard_backend()
                .read(&ClipboardFormat::Text)
                .expect("read")
                .is_some(),
            action == "accept"
        );
        if action == "accept" {
            assert_eq!(core.state.transfers[0].state, TransferState::Done);
        }
    }
    let dir = tempfile::tempdir().expect("dir");
    let mut core = paired_core(dir.path()).await;
    incoming_clipboard(
        &mut core,
        "bad",
        vec![wire::ClipFormat {
            kind: "text".into(),
            mime: "text/plain".into(),
            size: 5,
        }],
    )
    .await;
    assert!(incoming_payload(&mut core, "bad", "text", b"wrong size")
        .await
        .is_err());
    assert!(
        core.link.peer_token(&"e".repeat(64)).is_err(),
        "bad data drops the link"
    );
    assert!(core
        .platform
        .clipboard_backend()
        .formats()
        .expect("formats")
        .is_empty());
}

#[tokio::test]
async fn deferred_native_pairing_work_does_not_pause_core_and_cancellation_aborts_it() {
    let dir = tempfile::tempdir().expect("dir");
    let mut core = Core::mock(dir.path(), None).await.expect("core");
    core.pending_join_id = Some(42);
    core.join_job = Some(tokio::spawn(std::future::pending()));
    let abort = core.join_job.as_ref().expect("job").abort_handle();
    assert!(request(&mut core, "get_state", json!({})).await.ok);
    core.tick().await.expect("pending worker never blocks tick");
    assert!(
        request(&mut core, "pairing.cancel_host", json!({}))
            .await
            .ok
    );
    assert!(core.join_job.is_none());
    tokio::task::yield_now().await;
    assert!(abort.is_finished());
    assert_eq!(core.take_responses()[0].id, 42);
    core.pending_join_id = Some(43);
    core.join_job = Some(tokio::spawn(async {
        Err(error(ErrorCode::BadCode, "The pairing code did not match."))
    }));
    tokio::task::yield_now().await;
    core.tick().await.expect("worker result");
    let responses = core.take_responses();
    assert_eq!(responses[0].id, 43);
    assert_eq!(
        responses[0].error.as_ref().expect("error").code,
        ErrorCode::BadCode
    );
    assert!(core.state.peers.is_empty());
}

#[tokio::test]
async fn unpaired_and_non_brain_input_are_ignored_and_unpair_releases() {
    let dir = tempfile::tempdir().expect("dir");
    let mut core = paired_core(dir.path()).await;
    let key = WireMessage::Input(InputMessage::Key(wire::InputKey {
        epoch: 1,
        seq: 1,
        hid_usage: Key(4),
        down: true,
    }));
    assert!(core
        .receive_link(LinkEvent::Reliable {
            peer_token: None,
            peer_id: "f".repeat(64),
            message: key.clone()
        })
        .await
        .is_ok());
    assert!(core
        .receive_link(LinkEvent::Reliable {
            peer_token: None,
            peer_id: "f".repeat(64),
            message: WireMessage::Clipboard(wire::ClipboardMessage::ClipData(wire::ClipData {
                clip_id: "unpinned".into(),
                format: "text".into(),
                data: wire::ClipboardBytes::try_from_vec(b"untrusted".to_vec()).expect("data"),
            })),
        })
        .await
        .is_ok());
    assert!(
        request(
            &mut core,
            "peer.configure",
            json!({"device_id":"e".repeat(64),"clipboard_enabled":false})
        )
        .await
        .ok
    );
    incoming_clipboard(
        &mut core,
        "disabled",
        vec![wire::ClipFormat {
            kind: "text".into(),
            mime: "text/plain".into(),
            size: 5,
        }],
    )
    .await;
    incoming_payload(&mut core, "disabled", "text", b"hello")
        .await
        .expect("disabled peer is ignored");
    settle_clipboard(&mut core).await;
    assert!(core
        .platform
        .clipboard_backend()
        .formats()
        .expect("clipboard")
        .is_empty());
    core.receive_link(LinkEvent::Reliable {
        peer_token: None,
        peer_id: "e".repeat(64),
        message: key.clone(),
    })
    .await
    .expect("ignored without Enter");
    assert!(core
        .mock_platform()
        .expect("mock")
        .input
        .held_keys()
        .expect("keys")
        .is_empty());
    core.receive_link(LinkEvent::Reliable {
        peer_token: None,
        peer_id: "e".repeat(64),
        message: WireMessage::Control(ControlMessage::Enter(wire::Enter {
            epoch: 1,
            pos: glide_platform::Point { x: 100.0, y: 100.0 },
            modifiers_down: wire::ModifierKeys::new(),
        })),
    })
    .await
    .expect("enter");
    core.receive_link(LinkEvent::Reliable {
        peer_token: None,
        peer_id: "e".repeat(64),
        message: key,
    })
    .await
    .expect("key");
    assert!(core
        .mock_platform()
        .expect("mock")
        .input
        .held_keys()
        .expect("keys")
        .contains(&Key(4)));
    assert!(
        request(
            &mut core,
            "peer.unpair",
            json!({"device_id":"e".repeat(64)})
        )
        .await
        .ok
    );
    assert!(core.receiving_from.is_none());
    assert!(core
        .mock_platform()
        .expect("mock")
        .input
        .held_keys()
        .expect("keys")
        .is_empty());
}

#[tokio::test]
async fn both_escape_hotkeys_work_without_network_and_drop_releases_holds() {
    let dir = tempfile::tempdir().expect("dir");
    let mut core = Core::mock(dir.path(), None).await.expect("core");
    for usage in [0xe0, 0xe2, 0xe1, 0x4a, 0x16] {
        core.capture_input(InputEvent {
            injected: false,
            kind: InputEventKind::Key {
                key: Key(usage),
                down: true,
            },
        })
        .await
        .expect("hotkey");
    }
    assert!(!core.state.sharing_enabled);
    assert_eq!(
        core.mock_platform()
            .expect("mock")
            .input
            .mode()
            .expect("mode"),
        CaptureMode::Local
    );
    let input = core.mock_platform().expect("mock").input.clone();
    core.platform
        .input_backend()
        .inject(InputEvent {
            injected: true,
            kind: InputEventKind::Key {
                key: Key(4),
                down: true,
            },
        })
        .expect("inject");
    drop(core);
    assert!(input.held_keys().expect("released").is_empty());
}

#[tokio::test]
async fn local_clipboard_is_coalesced_sensitive_filtered_and_peer_override_survives_link_updates() {
    use glide_platform::{ClipboardContent, ClipboardFormat, ClipboardSensitivity};
    let dir = tempfile::tempdir().expect("dir");
    let mut core = paired_core(dir.path()).await;
    let link = Arc::new(InMemoryLink::new());
    let peer = "e".repeat(64);
    link.script_reachable("127.0.0.1:24801", peer.clone());
    link.connect("127.0.0.1:24801", Some(&peer))
        .await
        .expect("connect");
    core.link = link.clone();
    for index in 0..50 {
        core.mock_platform()
            .expect("mock")
            .clipboard
            .set_external_content(
                ClipboardContent::bytes(
                    ClipboardFormat::Text,
                    index.to_string().into_bytes(),
                    ClipboardSensitivity::default(),
                )
                .expect("content"),
            )
            .expect("copy");
    }
    settle_clipboard(&mut core).await;
    let messages = link.take_sent_reliable();
    assert_eq!(
        messages.len(),
        2,
        "only one announce and its latest payload"
    );
    assert!(
        matches!(&messages[1].1, WireMessage::Clipboard(wire::ClipboardMessage::ClipData(data)) if &data.data[..] == b"49")
    );
    core.mock_platform()
        .expect("mock")
        .clipboard
        .set_external_content(
            ClipboardContent::bytes(
                ClipboardFormat::Text,
                b"concealed".to_vec(),
                ClipboardSensitivity {
                    sensitive: true,
                    ..ClipboardSensitivity::default()
                },
            )
            .expect("content"),
        )
        .expect("sensitive copy");
    settle_clipboard(&mut core).await;
    assert!(
        link.take_sent_reliable().is_empty(),
        "sensitive clipboard is never announced"
    );
    assert!(
        request(
            &mut core,
            "peer.configure",
            json!({"device_id":peer,"clipboard_enabled":false})
        )
        .await
        .ok
    );
    let manager = InMemoryPeerManager::new();
    let mut updated = core.state.peers[0].clone();
    updated.clipboard_enabled = true;
    updated.online = true;
    updated.connection = Connection::Connected;
    core.peer_events = manager.events();
    assert!(manager.script_peer_event(PeerManagerEvent::PeerUpdated(updated)));
    core.pump_peer_events().await;
    assert!(
        !core.state.peers[0].clipboard_enabled,
        "Hello cannot override local policy"
    );
}

#[tokio::test]
async fn remote_unpair_event_removes_persisted_peer_and_layout_even_without_a_wire_event() {
    let dir = tempfile::tempdir().expect("dir");
    let mut core = paired_core(dir.path()).await;
    let manager = InMemoryPeerManager::new();
    manager.seed_paired_peers(core.state.peers.clone());
    core.peer_events = manager.events();
    assert!(manager.script_peer_event(PeerManagerEvent::Unpaired("e".repeat(64))));
    core.manager = Box::new(manager);
    core.pump_peer_events().await;
    assert!(core.state.peers.is_empty());
    drop(core);
    assert!(Core::mock(dir.path(), None)
        .await
        .expect("restart")
        .state
        .peers
        .is_empty());
}

#[tokio::test]
async fn offline_overlap_is_rejected_and_layout_lamport_order_is_respected() {
    let dir = tempfile::tempdir().expect("dir");
    let mut core = paired_core(dir.path()).await;
    let home = core.state.self_info.device_id.clone();
    let peer = "e".repeat(64);
    core.state.peers[0].online = false;
    let overlap = request(
        &mut core,
        "set_layout",
        json!({"devices":[{"device_id":home,"x":0,"y":0},{"device_id":peer,"x":0,"y":0}]}),
    )
    .await;
    assert!(!overlap.ok, "offline known monitors still cannot overlap");
    let devices = wire::LayoutDevices::try_from_vec(vec![
        LayoutDevice {
            device_id: home,
            x: 0.0,
            y: 0.0,
        },
        LayoutDevice {
            device_id: peer.clone(),
            x: 1920.0,
            y: 0.0,
        },
    ])
    .expect("layout");
    for version in [(5, peer.clone()), (4, peer.clone())] {
        core.receive_link(LinkEvent::Reliable {
            peer_token: None,
            peer_id: peer.clone(),
            message: WireMessage::Control(ControlMessage::LayoutUpdate(wire::LayoutUpdate {
                version,
                devices: devices.clone(),
            })),
        })
        .await
        .expect("layout update");
    }
    assert_eq!(core.layout_version, (5, peer));
    assert_eq!(
        Core::mock(dir.path(), None)
            .await
            .expect("restart")
            .layout_version,
        core.layout_version
    );
}

#[tokio::test]
async fn receiver_teardown_notifies_source_and_rejected_enter_is_explicit() {
    let dir = tempfile::tempdir().expect("directory");
    let mut core = paired_core(dir.path()).await;
    let peer = "e".repeat(64);
    let link = Arc::new(InMemoryLink::new());
    link.script_reachable("127.0.0.1:24801", peer.clone());
    link.connect("127.0.0.1:24801", Some(&peer))
        .await
        .expect("connect");
    core.link = link.clone();
    let enter = || LinkEvent::Reliable {
        peer_token: None,
        peer_id: peer.clone(),
        message: WireMessage::Control(ControlMessage::Enter(wire::Enter {
            epoch: 1,
            pos: glide_platform::Point { x: 50.0, y: 50.0 },
            modifiers_down: wire::ModifierKeys::new(),
        })),
    };
    core.receive_link(enter()).await.expect("enter");
    assert!(
        request(&mut core, "set_sharing", json!({"enabled":false}))
            .await
            .ok
    );
    assert!(link
        .take_sent_reliable()
        .iter()
        .any(|(_, message)| matches!(message, WireMessage::Control(ControlMessage::TakeOver(_)))));
    assert!(core.receive_link(enter()).await.is_err());
    assert!(link
        .take_sent_reliable()
        .iter()
        .any(|(_, message)| matches!(message, WireMessage::Control(ControlMessage::Bye(_)))));
    assert!(core.receiving_from.is_none());
}

#[tokio::test]
async fn local_restore_failure_preserves_reported_target_and_requests_exit() {
    let dir = tempfile::tempdir().expect("directory");
    let mut core = paired_core(dir.path()).await;
    let peer = "e".repeat(64);
    core.state.active_device_id = peer.clone();
    core.platform
        .input_backend()
        .set_mode(CaptureMode::Swallow { lock_pos: true })
        .expect("swallow");
    core.mock_platform()
        .expect("mock platform")
        .input
        .fail_next(
            glide_platform::InputOperation::SetMode,
            glide_platform::BackendError::Unavailable,
        )
        .expect("failure");
    assert!(core.return_home("test").is_err());
    assert!(core.shutdown_requested());
    assert_eq!(core.state.active_device_id, peer);
    assert_eq!(
        core.mock_platform()
            .expect("mock platform")
            .input
            .mode()
            .expect("mode"),
        CaptureMode::Swallow { lock_pos: true }
    );
    core.shutdown().await.expect("release retry succeeds");
    assert_eq!(
        core.mock_platform()
            .expect("mock platform")
            .input
            .mode()
            .expect("restored"),
        CaptureMode::Local
    );
}

#[tokio::test]
async fn pending_pairing_rejection_and_expiry_pin_nothing() {
    for expires in [false, true] {
        let dir = tempfile::tempdir().expect("directory");
        let mut core = Core::mock(dir.path(), None).await.expect("core");
        assert!(core
            .handle(Request {
                id: 44,
                method: "pairing.join".into(),
                params: json!({"address":"127.0.0.1:24801","code":"123456"})
            })
            .await
            .is_none());
        assert!(core
            .take_events()
            .iter()
            .any(|event| matches!(event, Event::PairingVerify(_))));
        if expires {
            core.verification_deadline = Some(0);
            core.tick().await.expect("expiry");
        } else {
            assert!(
                !request(&mut core, "pairing.confirm", json!({"accepted":false}))
                    .await
                    .ok
            );
        }
        assert!(core.state.peers.is_empty());
        let responses = core.take_responses();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].id, 44);
        assert!(!responses[0].ok);
        assert!(Core::mock(dir.path(), None)
            .await
            .expect("restart")
            .state
            .peers
            .is_empty());
    }
}

#[tokio::test]
async fn transfer_updates_emit_notification_and_limit_progress() {
    let dir = tempfile::tempdir().expect("directory");
    let mut core = paired_core(dir.path()).await;
    core.take_events();
    let transfer = Transfer {
        id: "1".into(),
        direction: TransferDirection::Receive,
        peer_id: "e".repeat(64),
        name: "Files".into(),
        items: 1,
        bytes_total: 10,
        bytes_done: 0,
        rate_bps: 0,
        state: TransferState::AwaitingConfirm,
        error: None,
    };
    for _ in 0..20 {
        core.update_transfer(transfer.clone()).expect("snapshot");
    }
    let events = core.take_events();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::TransferProgress(_)))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::Notification(_)))
            .count(),
        1
    );
    let mut at_limit = transfer;
    at_limit.items = wire::MAX_TRANSFER_ITEMS;
    core.update_transfer(at_limit.clone())
        .expect("aggregate item cap");
    at_limit.items += 1;
    assert!(core.update_transfer(at_limit).is_err());
}

#[tokio::test]
async fn mouse_tokens_reject_replaced_connections_until_new_enter() {
    let dir = tempfile::tempdir().expect("directory");
    let mut core = paired_core(dir.path()).await;
    let peer = "e".repeat(64);
    let link = Arc::new(InMemoryLink::new());
    link.script_reachable("127.0.0.1:24801", peer.clone());
    link.connect("127.0.0.1:24801", Some(&peer))
        .await
        .expect("connect");
    core.link = link.clone();
    let enter = || LinkEvent::Reliable {
        peer_token: None,
        peer_id: peer.clone(),
        message: WireMessage::Control(ControlMessage::Enter(wire::Enter {
            epoch: 1,
            pos: glide_platform::Point { x: 50.0, y: 50.0 },
            modifiers_down: wire::ModifierKeys::new(),
        })),
    };
    core.receive_link(enter()).await.expect("enter");
    let old = link.peer_token(&peer).expect("token");
    core.mock_platform()
        .expect("mock platform")
        .input
        .take_injected_events()
        .expect("clear enter");
    let movement = wire::Move {
        seq: 1,
        x: 60.0,
        y: 70.0,
    };
    core.receive_move(glide_net::ReceivedMove {
        peer: old,
        movement,
    })
    .await
    .expect("move");
    assert_eq!(
        core.mock_platform()
            .expect("mock platform")
            .input
            .take_injected_events()
            .expect("injection")
            .len(),
        1
    );
    // A repeated sequence is suppressed even with a valid token.
    core.receive_move(glide_net::ReceivedMove {
        peer: old,
        movement,
    })
    .await
    .expect("duplicate");
    assert!(core
        .mock_platform()
        .expect("mock platform")
        .input
        .take_injected_events()
        .expect("no duplicate")
        .is_empty());
    link.close(&peer).await.expect("close");
    link.connect("127.0.0.1:24801", Some(&peer))
        .await
        .expect("reconnect");
    let new = link.peer_token(&peer).expect("replacement token");
    assert_ne!(old, new);
    for token in [old, new] {
        core.receive_move(glide_net::ReceivedMove {
            peer: token,
            movement: wire::Move { seq: 2, ..movement },
        })
        .await
        .expect("stale enter");
    }
    assert!(core
        .mock_platform()
        .expect("mock platform")
        .input
        .take_injected_events()
        .expect("no stale injection")
        .is_empty());
    core.end_forwarding("test reconnect")
        .await
        .expect("cleanup");
    core.receive_link(enter()).await.expect("new enter");
    core.mock_platform()
        .expect("mock platform")
        .input
        .take_injected_events()
        .expect("clear enter");
    core.receive_move(glide_net::ReceivedMove {
        peer: new,
        movement,
    })
    .await
    .expect("new move");
    assert_eq!(
        core.mock_platform()
            .expect("mock platform")
            .input
            .take_injected_events()
            .expect("new injection")
            .len(),
        1
    );
}

// Bug: a password field on the Mac (Secure Input) sent the cursor back to Windows mid-typing.
#[tokio::test]
async fn secure_input_on_the_receiving_computer_keeps_control_there() {
    use glide_platform::CaptureStatus;
    let dir = tempfile::tempdir().expect("directory");
    let mut core = paired_core(dir.path()).await;
    core.receiving_from = Some("e".repeat(64));
    core.last_receive = Instant::now();
    core.mock_platform()
        .expect("mock platform")
        .input
        .script_capture_status(CaptureStatus::SecureInput(true))
        .expect("status");
    core.tick().await.expect("tick");
    assert_eq!(core.receiving_from, Some("e".repeat(64)));
}

#[tokio::test]
async fn capture_status_loss_releases_input_and_recovery_never_resumes() {
    use glide_platform::CaptureStatus;
    for status in [
        CaptureStatus::PermissionLost,
        CaptureStatus::TapDisabled,
        CaptureStatus::UnsupportedInput,
        CaptureStatus::QueueOverflow,
    ] {
        let dir = tempfile::tempdir().expect("directory");
        let mut core = paired_core(dir.path()).await;
        core.receiving_from = Some("e".repeat(64));
        core.platform
            .input_backend()
            .set_mode(CaptureMode::Swallow { lock_pos: true })
            .expect("swallow");
        core.platform
            .input_backend()
            .inject(InputEvent {
                injected: true,
                kind: InputEventKind::Key {
                    key: Key(4),
                    down: true,
                },
            })
            .expect("held input");
        assert!(!core
            .mock_platform()
            .expect("mock platform")
            .input
            .held_keys()
            .expect("held keys")
            .is_empty());
        core.mock_platform()
            .expect("mock platform")
            .input
            .script_capture_status(status)
            .expect("status");
        core.tick().await.expect("escape");
        assert!(core.receiving_from.is_none());
        assert!(core
            .mock_platform()
            .expect("mock platform")
            .input
            .held_keys()
            .expect("released keys")
            .is_empty());
        assert_eq!(
            core.mock_platform()
                .expect("mock platform")
                .input
                .mode()
                .expect("local mode"),
            CaptureMode::Local
        );
        core.mock_platform()
            .expect("mock platform")
            .input
            .script_capture_status(CaptureStatus::SecureInput(false))
            .expect("recovery");
        core.tick().await.expect("recovery tick");
        assert!(core.receiving_from.is_none());
        assert_eq!(
            core.mock_platform()
                .expect("mock platform")
                .input
                .mode()
                .expect("still local"),
            CaptureMode::Local
        );
    }
}

#[tokio::test]
async fn peer_clipboard_override_survives_restart() {
    let dir = tempfile::tempdir().expect("directory");
    let mut core = paired_core(dir.path()).await;
    assert!(
        request(
            &mut core,
            "peer.configure",
            json!({"device_id":"e".repeat(64),"clipboard_enabled":false})
        )
        .await
        .ok
    );
    assert!(
        !Core::mock(dir.path(), None)
            .await
            .expect("restart")
            .state
            .peers[0]
            .clipboard_enabled
    );
}
use glide_net::LinkEvent;
use glide_platform::{InputEvent, InputEventKind, Key};
use glide_proto::wire::{self, ControlMessage, InputMessage, WireMessage};

async fn request(core: &mut Core, method: &str, params: Value) -> Response {
    core.handle(Request {
        id: 1,
        method: method.into(),
        params,
    })
    .await
    .expect("immediate response")
}

pub(super) async fn paired_core(dir: &Path) -> Core {
    let mut core = Core::mock(dir, None).await.expect("core");
    let result = core
        .handle(Request {
            id: 1,
            method: "pairing.join".into(),
            params: json!({"address":"127.0.0.1:24801","code":"123456"}),
        })
        .await;
    assert!(result.is_none());
    assert!(core.state.peers.is_empty());
    assert!(
        request(&mut core, "pairing.confirm", json!({"accepted":true}))
            .await
            .ok
    );
    assert!(core
        .take_responses()
        .iter()
        .any(|response| response.id == 1 && response.ok));
    core
}

#[tokio::test]
async fn pairing_layout_forwarding_hotkey_and_restart_unpair() {
    let dir = tempfile::tempdir().expect("directory");
    let mut core = paired_core(dir.path()).await;
    let home = core.state.self_info.device_id.clone();
    let peer = "e".repeat(64);
    let result = request(
        &mut core,
        "set_layout",
        json!({"devices":[{"device_id":home,"x":0,"y":0},{"device_id":peer,"x":1920,"y":0}]}),
    )
    .await;
    assert!(result.ok, "{result:?}");
    assert!(
        request(
            &mut core,
            "set_settings",
            json!({"patch":{"switching":{"edge_delay_ms":0,"corner_dead_zone_px":0}}})
        )
        .await
        .ok
    );
    core.capture_input(InputEvent {
        injected: false,
        kind: InputEventKind::PointerMoved {
            position: glide_platform::Point { x: 0.0, y: 200.0 },
            delta_x: 0.0,
            delta_y: 200.0,
        },
    })
    .await
    .expect("move locally");
    core.capture_input(InputEvent {
        injected: false,
        kind: InputEventKind::PointerMoved {
            position: glide_platform::Point {
                x: 1919.0,
                y: 200.0,
            },
            delta_x: 2000.0,
            delta_y: 0.0,
        },
    })
    .await
    .expect("cross edge");
    assert_eq!(core.state.active_device_id, peer);
    assert_eq!(
        core.mock_platform()
            .expect("mock platform")
            .input
            .mode()
            .expect("mode"),
        CaptureMode::Swallow { lock_pos: true }
    );
    for key in [0xe0, 0xe2, 0xe1, 0x4a] {
        core.capture_input(InputEvent {
            injected: false,
            kind: InputEventKind::Key {
                key: Key(key),
                down: true,
            },
        })
        .await
        .expect("hotkey");
    }
    assert_eq!(core.state.active_device_id, home);
    assert_eq!(
        core.mock_platform()
            .expect("mock platform")
            .input
            .mode()
            .expect("local"),
        CaptureMode::Local
    );
    core.shutdown().await.expect("shutdown");
    let mut restarted = Core::mock(dir.path(), None).await.expect("restart");
    assert_eq!(restarted.layout_version, core.layout_version);
    assert_eq!(restarted.lamport, core.lamport);
    assert_eq!(restarted.state.peers.len(), 1);
    assert!(!restarted.state.peers[0].online);
    assert!(
        request(&mut restarted, "peer.unpair", json!({"device_id":peer}))
            .await
            .ok
    );
    assert!(Core::mock(dir.path(), None)
        .await
        .expect("restart again")
        .state
        .peers
        .is_empty());
}

#[tokio::test]
async fn receiver_filters_stale_moves_and_releases_on_disconnect_and_permission_loss() {
    let dir = tempfile::tempdir().expect("directory");
    let mut core = paired_core(dir.path()).await;
    let peer = "e".repeat(64);
    core.receive_link(LinkEvent::Reliable {
        peer_token: None,
        peer_id: peer.clone(),
        message: WireMessage::Control(ControlMessage::Enter(wire::Enter {
            epoch: 1,
            pos: glide_platform::Point { x: 100.0, y: 100.0 },
            modifiers_down: wire::ModifierKeys::try_from_vec(vec![Key(0xe3)]).expect("modifiers"),
        })),
    })
    .await
    .expect("enter");
    core.receive_link(LinkEvent::Reliable {
        peer_token: None,
        peer_id: peer.clone(),
        message: WireMessage::Input(InputMessage::Key(wire::InputKey {
            epoch: 1,
            seq: 0,
            hid_usage: Key(4),
            down: true,
        })),
    })
    .await
    .expect("key");
    assert!(core
        .mock_platform()
        .expect("mock platform")
        .input
        .held_keys()
        .expect("held")
        .contains(&Key(4)));
    core.receive_link(LinkEvent::Reliable {
        peer_token: None,
        peer_id: peer.clone(),
        message: WireMessage::Input(InputMessage::ModifierSync(wire::ModifierSync {
            epoch: 1,
            seq: 1,
            modifiers_down: wire::ModifierKeys::new(),
        })),
    })
    .await
    .expect("modifier snapshot");
    let held = core
        .mock_platform()
        .expect("mock platform")
        .input
        .held_keys()
        .expect("held after sync");
    assert!(held.contains(&Key(4)));
    assert!(!held.contains(&Key(0xe3)));
    for (seq, x) in [(5, 200.0), (4, 50.0), (5, 30.0)] {
        core.receive_link(LinkEvent::Move {
            peer_id: peer.clone(),
            movement: wire::Move { seq, x, y: 100.0 },
        })
        .await
        .expect("move");
    }
    assert_eq!(
        core.platform
            .input_backend()
            .local_cursor_pos()
            .expect("cursor")
            .x,
        200.0
    );
    core.receive_link(LinkEvent::Disconnected {
        peer_id: peer.clone(),
        reason: None,
    })
    .await
    .expect("disconnect");
    assert!(core
        .mock_platform()
        .expect("mock platform")
        .input
        .held_keys()
        .expect("released")
        .is_empty());
    core.receive_link(LinkEvent::Reliable {
        peer_token: None,
        peer_id: peer.clone(),
        message: WireMessage::Control(ControlMessage::Enter(wire::Enter {
            epoch: 2,
            pos: glide_platform::Point { x: 100.0, y: 100.0 },
            modifiers_down: wire::ModifierKeys::new(),
        })),
    })
    .await
    .expect("enter again");
    core.mock_platform()
        .expect("mock platform")
        .input
        .fail_next(
            glide_platform::InputOperation::Inject,
            glide_platform::BackendError::PermissionDenied,
        )
        .expect("script failure");
    let result = core
        .receive_link(LinkEvent::Reliable {
            peer_token: None,
            peer_id: peer,
            message: WireMessage::Input(InputMessage::Key(wire::InputKey {
                epoch: 2,
                seq: 1,
                hid_usage: Key(5),
                down: true,
            })),
        })
        .await;
    assert_eq!(
        result.expect_err("denied").code,
        ErrorCode::PermissionDenied
    );
    assert!(core.receiving_from.is_none());
    assert!(core
        .mock_platform()
        .expect("mock platform")
        .input
        .held_keys()
        .expect("released")
        .is_empty());
}

#[tokio::test]
async fn rejected_settings_and_failed_atomic_save_keep_prior_state() {
    let dir = tempfile::tempdir().expect("directory");
    let mut core = Core::mock(dir.path(), None).await.expect("core");
    let before = core.snapshot();
    assert!(
        !request(
            &mut core,
            "set_settings",
            json!({"patch":{"network":{"port":0}}})
        )
        .await
        .ok
    );
    assert_eq!(core.snapshot(), before);
    std::fs::create_dir(dir.path().join("config.json.blocker")).expect("blocker");
    let saved_dir = core.data_dir.clone();
    core.data_dir = dir.path().join("config.json");
    let response = request(
        &mut core,
        "set_settings",
        json!({"patch":{"device_name":"Unsaved"}}),
    )
    .await;
    assert_eq!(
        response.error.as_ref().expect("I/O error").code,
        ErrorCode::Internal
    );
    assert_eq!(core.snapshot(), before);
    core.data_dir = saved_dir;
    assert_eq!(
        Config::load(dir.path())
            .expect("preserved")
            .expect("file")
            .settings
            .device_name,
        before.settings.device_name
    );
}

#[tokio::test]
async fn transfer_confirmation_cancel_and_permission_events() {
    let dir = tempfile::tempdir().expect("directory");
    let mut core = Core::mock(dir.path(), None).await.expect("core");
    core.state.transfers.push(Transfer {
        id: "1".into(),
        direction: TransferDirection::Receive,
        peer_id: "e".repeat(64),
        name: "Mock transfer".into(),
        items: 1,
        bytes_total: 10,
        bytes_done: 0,
        rate_bps: 0,
        state: TransferState::AwaitingConfirm,
        error: None,
    });
    assert!(
        request(
            &mut core,
            "transfer.confirm",
            json!({"id":"1","accept":true})
        )
        .await
        .ok
    );
    assert_eq!(core.state.transfers[0].state, TransferState::Queued);
    assert!(
        request(&mut core, "transfer.cancel", json!({"id":"1"}))
            .await
            .ok
    );
    assert_eq!(core.state.transfers[0].state, TransferState::Cancelled);
    assert!(
        !request(
            &mut core,
            "transfer.confirm",
            json!({"id":"1","accept":true})
        )
        .await
        .ok
    );
    core.mock_platform()
        .expect("mock platform")
        .input
        .set_permissions(Permissions {
            accessibility: PermissionStatus::Granted,
            input_monitoring: PermissionStatus::Granted,
            injection: PermissionStatus::Denied,
        })
        .expect("permissions");
    settle_permissions(&mut core, Instant::now()).await;
    core.tick().await.expect("tick");
    assert!(core.take_events().iter().any(|event|matches!(event,Event::State(state) if state.permissions.injection==PermissionStatus::Denied)));
}

async fn settle_permissions(core: &mut Core, now: Instant) {
    core.poll_permissions(now).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while core.permission_job.is_some() {
            if core.permission_job.as_ref().expect("job").1.is_finished() {
                core.poll_permissions(now).await;
            } else {
                tokio::task::yield_now().await;
            }
        }
    })
    .await
    .expect("bounded OS worker");
}

fn grants(status: PermissionStatus) -> Permissions {
    Permissions {
        accessibility: status,
        input_monitoring: status,
        injection: status,
    }
}

async fn permission_blocked_core(dir: &Path) -> Core {
    let mock = MockPlatform::new(Os::Macos, dir.to_owned());
    mock.input
        .set_permissions(grants(PermissionStatus::Denied))
        .expect("denied");
    mock.input
        .fail_next(
            glide_platform::InputOperation::StartCapture,
            glide_platform::BackendError::PermissionDenied,
        )
        .expect("initial failure");
    Core::initialize(
        dir,
        Core::load_config(dir, None, None).expect("config"),
        Arc::new(mock.clone()),
        Some(mock),
        None,
    )
    .await
    .expect("IPC survives missing grants")
}

#[tokio::test]
async fn permission_grant_retries_capture_and_notifies_once_without_polling_storm() {
    let dir = tempfile::tempdir().expect("dir");
    let mut core = permission_blocked_core(dir.path()).await;
    assert!(core.capture_pending);
    assert!(!core.state.permissions.restart_required);
    let input = core.mock_platform().expect("mock").input.clone();
    input
        .script_permissions(vec![
            grants(PermissionStatus::Denied),
            grants(PermissionStatus::Unknown),
            grants(PermissionStatus::Granted),
        ])
        .expect("sequence");
    core.take_events();
    let now = Instant::now();
    settle_permissions(&mut core, now).await;
    let counts = input.permission_call_counts().expect("counts");
    for millis in (100..2000).step_by(100) {
        core.poll_permissions(now + Duration::from_millis(millis))
            .await;
    }
    assert_eq!(input.permission_call_counts().expect("no storm"), counts);
    settle_permissions(&mut core, now + Duration::from_secs(2)).await;
    assert_eq!(
        core.state.permissions.accessibility,
        PermissionStatus::Unknown
    );
    settle_permissions(&mut core, now + Duration::from_secs(4)).await;
    assert!(!core.capture_pending);
    assert!(!core.state.permissions.restart_required);
    assert_eq!(
        input.permission_call_counts().expect("counts"),
        (counts.0 + 2, 0, 2)
    );
    input
        .emit(InputEvent {
            injected: false,
            kind: InputEventKind::Key {
                key: Key(4),
                down: true,
            },
        })
        .expect("capture installed");
    assert!(core.capture_rx.try_recv().is_ok());
    core.tick().await.expect("publish grant");
    let events = core.take_events();
    assert_eq!(events.iter().filter(|event| matches!(event, Event::Notification(n) if n.title == "Glide can now share your mouse and keyboard")).count(), 1);
    assert!(events.iter().any(|event| matches!(event, Event::State(s) if s.permissions.accessibility == PermissionStatus::Granted)));
    let counts = input.permission_call_counts().expect("counts");
    settle_permissions(&mut core, now + Duration::from_secs(13)).await;
    assert_eq!(input.permission_call_counts().expect("slow checks"), counts);
    settle_permissions(&mut core, now + Duration::from_secs(14)).await;
    assert_eq!(
        input.permission_call_counts().expect("slow check").0,
        counts.0 + 1
    );
    assert!(!core
        .take_events()
        .iter()
        .any(|event| matches!(event, Event::Notification(_))));
}

#[tokio::test]
async fn permission_grant_failed_capture_requires_restart_without_retry_storm() {
    let dir = tempfile::tempdir().expect("dir");
    let mut core = permission_blocked_core(dir.path()).await;
    let input = core.mock_platform().expect("mock").input.clone();
    input
        .set_permissions(grants(PermissionStatus::Granted))
        .expect("grant");
    input
        .fail_next(
            glide_platform::InputOperation::StartCapture,
            glide_platform::BackendError::Unavailable,
        )
        .expect("OS needs relaunch");
    core.take_events();
    let now = Instant::now();
    settle_permissions(&mut core, now).await;
    core.tick().await.expect("publish restart");
    assert!(core.state.permissions.restart_required);
    assert!(core.capture_pending);
    assert!(core
        .take_events()
        .iter()
        .any(|event| matches!(event, Event::State(s) if s.permissions.restart_required)));
    for seconds in [10, 20, 30] {
        settle_permissions(&mut core, now + Duration::from_secs(seconds)).await;
    }
    assert_eq!(
        input.permission_call_counts().expect("no capture storm").2,
        2
    );
    // A further explicit setup action can retry; success clears the restart hint.
    assert!(core
        .handle(Request {
            id: 7,
            method: METHOD_PERMISSIONS_REQUEST.into(),
            params: json!({})
        })
        .await
        .is_none());
    settle_permissions(&mut core, now + Duration::from_secs(30)).await;
    assert!(!core.state.permissions.restart_required);
    assert!(!core.capture_pending);
    assert_eq!(input.permission_call_counts().expect("explicit retry").2, 3);
}

#[tokio::test]
async fn permission_requests_run_once_per_action_and_publish_refresh() {
    let dir = tempfile::tempdir().expect("dir");
    let mut core = Core::mock(dir.path(), None).await.expect("core");
    let input = core.mock_platform().expect("mock").input.clone();
    core.take_events();
    for id in [40, 41] {
        assert!(core
            .handle(Request {
                id,
                method: METHOD_PERMISSIONS_REQUEST.into(),
                params: json!({})
            })
            .await
            .is_none());
    }
    let invalid = core
        .handle(Request {
            id: 42,
            method: METHOD_PERMISSIONS_REQUEST.into(),
            params: json!([]),
        })
        .await
        .expect("invalid response");
    assert!(!invalid.ok);
    settle_permissions(&mut core, Instant::now()).await;
    assert_eq!(input.permission_call_counts().expect("requests").1, 2);
    let responses = core.take_responses();
    assert_eq!(
        responses.iter().map(|reply| reply.id).collect::<Vec<_>>(),
        vec![40, 41]
    );
    assert!(responses
        .iter()
        .all(|reply| reply.ok && reply.result == Some(json!({}))));
    core.tick().await.expect("publish");
    assert!(core
        .take_events()
        .iter()
        .any(|event| matches!(event, Event::State(_))));
    for _ in 0..50 {
        core.tick().await.expect("idle");
    }
    assert_eq!(
        input
            .permission_call_counts()
            .expect("no repeated requests")
            .1,
        2
    );
    assert!(
        core.take_events().is_empty(),
        "unchanged permissions emit no state storm"
    );
}

#[tokio::test]
async fn permission_revocation_returns_home_releases_holds_and_resumes_fast_checks() {
    for kind in 0..3 {
        let dir = tempfile::tempdir().expect("dir");
        let mut core = paired_core(dir.path()).await;
        let peer = core.state.peers[0].device_id.clone();
        let link = Arc::new(InMemoryLink::new());
        link.script_reachable("127.0.0.1:24801", peer.clone());
        link.connect("127.0.0.1:24801", Some(&peer))
            .await
            .expect("connected peer");
        core.link = link.clone();
        request(
            &mut core,
            "set_settings",
            json!({"patch":{"switching":{"edge_delay_ms":0,"corner_dead_zone_px":0}}}),
        )
        .await;
        core.capture_input(InputEvent {
            injected: false,
            kind: InputEventKind::PointerMoved {
                position: glide_platform::Point { x: 0.0, y: 200.0 },
                delta_x: 2000.0,
                delta_y: 200.0,
            },
        })
        .await
        .expect("forward");
        let peer = core.state.peers[0].device_id.clone();
        assert_eq!(core.state.active_device_id, peer);
        core.capture_input(InputEvent {
            injected: false,
            kind: InputEventKind::Key {
                key: Key(4),
                down: true,
            },
        })
        .await
        .expect("forwarded hold");
        core.capture_input(InputEvent {
            injected: false,
            kind: InputEventKind::Button {
                button: glide_platform::Button::Left,
                down: true,
            },
        })
        .await
        .expect("forwarded button hold");
        link.take_sent_reliable();
        let input = core.mock_platform().expect("mock").input.clone();
        core.platform
            .input_backend()
            .inject(InputEvent {
                injected: true,
                kind: InputEventKind::Key {
                    key: Key(4),
                    down: true,
                },
            })
            .expect("held key");
        let mut revoked = grants(PermissionStatus::Granted);
        match kind {
            0 => revoked.accessibility = PermissionStatus::Denied,
            1 => revoked.input_monitoring = PermissionStatus::Denied,
            _ => revoked.injection = PermissionStatus::Denied,
        }
        input
            .script_permissions(vec![
                grants(PermissionStatus::Granted),
                revoked,
                grants(PermissionStatus::Granted),
            ])
            .expect("grant/revoke/grant");
        let now = Instant::now();
        settle_permissions(&mut core, now).await;
        assert_eq!(core.state.active_device_id, peer);
        settle_permissions(&mut core, now + Duration::from_secs(10)).await;
        assert_eq!(core.state.active_device_id, core.state.self_info.device_id);
        assert_eq!(input.mode().expect("local"), CaptureMode::Local);
        assert!(input.held_keys().expect("released").is_empty());
        assert!(input.release_all_count().expect("release") > 0);
        assert!(
            link.take_sent_reliable()
                .iter()
                .any(|(target, message)| target == &peer
                    && matches!(message, WireMessage::Control(ControlMessage::Leave(_)))),
            "remote receives Leave, which releases all its held keys/buttons"
        );
        core.tick().await.expect("publish denial");
        assert!(core
            .take_events()
            .iter()
            .any(|event| matches!(event, Event::State(s) if permissions::missing(s.permissions))));
        settle_permissions(&mut core, now + Duration::from_secs(12)).await;
        assert!(!permissions::missing(core.state.permissions));
        assert_eq!(
            core.state.active_device_id, core.state.self_info.device_id,
            "grant does not resume remote control"
        );
        assert!(!core.capture_pending);
    }
}

#[tokio::test]
async fn permission_not_applicable_backends_do_not_poll() {
    let dir = tempfile::tempdir().expect("dir");
    let mut core = Core::mock(dir.path(), None).await.expect("core");
    let input = core.mock_platform().expect("mock").input.clone();
    input
        .set_permissions(grants(PermissionStatus::NotApplicable))
        .expect("n/a");
    let now = Instant::now();
    settle_permissions(&mut core, now).await;
    let counts = input.permission_call_counts().expect("counts");
    settle_permissions(&mut core, now + Duration::from_secs(100)).await;
    assert_eq!(input.permission_call_counts().expect("no polling"), counts);
}

#[tokio::test]
async fn permission_capture_race_does_not_misreport_restart() {
    let dir = tempfile::tempdir().expect("dir");
    let mut core = permission_blocked_core(dir.path()).await;
    let input = core.mock_platform().expect("mock").input.clone();
    input
        .script_permissions(vec![
            grants(PermissionStatus::Granted),
            grants(PermissionStatus::Denied),
        ])
        .expect("revoke during tap creation");
    input
        .fail_next(
            glide_platform::InputOperation::StartCapture,
            glide_platform::BackendError::PermissionDenied,
        )
        .expect("race");
    settle_permissions(&mut core, Instant::now()).await;
    assert!(permissions::missing(core.state.permissions));
    assert!(core.capture_pending);
    assert!(!core.state.permissions.restart_required);
}

#[tokio::test]
async fn permission_secure_input_does_not_require_restart_and_recovery_retries() {
    let dir = tempfile::tempdir().expect("dir");
    let mut core = permission_blocked_core(dir.path()).await;
    let input = core.mock_platform().expect("mock").input.clone();
    input
        .set_permissions(grants(PermissionStatus::Granted))
        .expect("grant");
    input
        .script_capture_status(glide_platform::CaptureStatus::SecureInput(true))
        .expect("secure input");
    settle_permissions(&mut core, Instant::now()).await;
    assert!(core.capture_pending);
    assert!(!core.state.permissions.restart_required);
    assert_eq!(
        input
            .permission_call_counts()
            .expect("no retry in secure input")
            .2,
        1
    );
    input
        .script_capture_status(glide_platform::CaptureStatus::SecureInput(false))
        .expect("recovery");
    core.tick().await.expect("secure input recovery");
    settle_permissions(&mut core, Instant::now()).await;
    assert!(!core.capture_pending);
    assert!(!core.state.permissions.restart_required);
    assert_eq!(input.permission_call_counts().expect("capture retry").2, 2);
}

#[tokio::test]
async fn permission_shutdown_answers_pending_requests_instead_of_leaving_them_hanging() {
    let dir = tempfile::tempdir().expect("dir");
    let mut core = Core::mock(dir.path(), None).await.expect("core");
    assert!(core
        .handle(Request {
            id: 90,
            method: METHOD_PERMISSIONS_REQUEST.into(),
            params: json!({})
        })
        .await
        .is_none());
    request(&mut core, "app.shutdown", json!({})).await;
    let responses = core.take_responses();
    assert!(responses.iter().any(|reply| reply.id == 90 && !reply.ok));
    assert!(core.permission_job.is_none());
    assert!(core.permission_requests.is_empty());
}

#[tokio::test]
async fn a_modifier_the_system_says_is_up_is_not_pressed_on_the_other_computer() {
    let dir = tempfile::tempdir().expect("dir");
    let mut core = paired_core(dir.path()).await;
    let peer = core.state.peers[0].device_id.clone();
    let link = Arc::new(InMemoryLink::new());
    link.script_reachable("127.0.0.1:24801", peer.clone());
    link.connect("127.0.0.1:24801", Some(&peer))
        .await
        .expect("connected peer");
    core.link = link.clone();
    request(
        &mut core,
        "set_settings",
        json!({"patch":{"switching":{"edge_delay_ms":0,"corner_dead_zone_px":0}}}),
    )
    .await;
    // Ctrl went down, but its release never reached Glide (the app switcher, Secure Input).
    core.capture_input(InputEvent {
        injected: false,
        kind: InputEventKind::Key {
            key: Key(0xe0),
            down: true,
        },
    })
    .await
    .expect("ctrl down");
    core.mock_platform()
        .expect("mock")
        .input
        .set_modifiers_maybe_held(Some([false; 8]))
        .expect("system modifier state");
    link.take_sent_reliable();
    core.capture_input(InputEvent {
        injected: false,
        kind: InputEventKind::PointerMoved {
            position: glide_platform::Point { x: 0.0, y: 200.0 },
            delta_x: 2000.0,
            delta_y: 200.0,
        },
    })
    .await
    .expect("forward");
    assert_eq!(core.state.active_device_id, peer);
    let enter = link
        .take_sent_reliable()
        .into_iter()
        .find_map(|(_, message)| match message {
            WireMessage::Control(ControlMessage::Enter(enter)) => Some(enter),
            _ => None,
        })
        .expect("enter sent");
    assert_eq!(
        enter.modifiers_down.iter().count(),
        0,
        "a stale Ctrl must not be pressed on the other computer"
    );
}
