use super::tests::paired_core;
use super::*;
use glide_net::LinkEvent;
use glide_platform::{
    BackendError, InputBackend, InputEvent, InputEventKind, InputOperation, Key, Monitor, Point,
};
use glide_proto::wire::{self, ControlMessage, InputMessage, WireMessage};

fn monitor(id: &str, x: f64, y: f64, w: f64, h: f64, primary: bool) -> Monitor {
    Monitor {
        id: id.into(),
        x,
        y,
        w,
        h,
        scale: 1.0,
        primary,
    }
}

async fn control(core: &mut Core, message: ControlMessage) -> Result<(), IpcError> {
    core.receive_link(LinkEvent::Reliable {
        peer_id: "e".repeat(64),
        peer_token: None,
        message: WireMessage::Control(message),
    })
    .await
}
async fn enter(core: &mut Core, epoch: u64) {
    control(
        core,
        ControlMessage::Enter(wire::Enter {
            epoch,
            pos: Point { x: 50.0, y: 50.0 },
            modifiers_down: wire::ModifierKeys::new(),
        }),
    )
    .await
    .expect("enter");
}
async fn key(core: &mut Core, seq: u64, down: bool) -> Result<(), IpcError> {
    core.receive_link(LinkEvent::Reliable {
        peer_id: "e".repeat(64),
        peer_token: None,
        message: WireMessage::Input(InputMessage::Key(wire::InputKey {
            epoch: 1,
            seq,
            hid_usage: Key(4),
            down,
        })),
    })
    .await
}

#[tokio::test]
async fn l_shaped_desks_allow_bbox_overlap_but_only_real_monitor_edges_cross() {
    // Prevents the Desk rejecting usable L-shaped layouts or switching across an empty bbox edge.
    let dir = tempfile::tempdir().expect("dir");
    let core = paired_core(dir.path()).await;
    let mut state = core.snapshot();
    state.self_info.monitors = vec![
        monitor("wide", 0.0, 0.0, 200.0, 100.0, true),
        monitor("leg", 0.0, 100.0, 100.0, 100.0, false),
    ];
    state.peers[0].monitors = vec![monitor("peer", 0.0, 0.0, 100.0, 100.0, true)];
    state.layout.devices[1].x = 100.0;
    state.layout.devices[1].y = 100.0;
    validate_layout(&state.layout.devices, &state).expect("actual screens do not overlap");
    assert_eq!(
        reconcile_layout(&state.layout.devices, &state).expect("reconcile"),
        state.layout
    );
    let home = state.self_info.device_id.clone();
    let peer = state.peers[0].device_id.clone();
    let monitors = HashMap::from([
        (home.clone(), state.self_info.monitors.clone()),
        (peer.clone(), state.peers[0].monitors.clone()),
    ]);
    let desktop = crate::layout::Desktop::from_layout(&state.layout, &monitors).expect("desktop");
    assert_eq!(
        desktop
            .move_cursor_from(
                Point { x: 90.0, y: 150.0 },
                Point { x: 20.0, y: 0.0 },
                Some(&home)
            )
            .crossing
            .expect("real edge")
            .target_device_id,
        peer
    );
    state.layout.devices[1].x = 200.0;
    let desktop =
        crate::layout::Desktop::from_layout(&state.layout, &monitors).expect("gap desktop");
    let moved = desktop.move_cursor_from(
        Point { x: 90.0, y: 150.0 },
        Point { x: 150.0, y: 0.0 },
        Some(&home),
    );
    assert!(moved.crossing.is_none());
    assert_eq!(moved.position.x, 100.0);
    state.layout.devices[1].x = 50.0;
    assert!(validate_layout(&state.layout.devices, &state).is_err());
}

#[tokio::test]
async fn newly_paired_screen_touches_primary_monitor_in_user_stacked_desk() {
    // Prevents a new Mac floating beside the empty upper-right of the Windows bounding box.
    let dir = tempfile::tempdir().expect("dir");
    let core = paired_core(dir.path()).await;
    let mut state = core.snapshot();
    state.self_info.monitors = vec![
        monitor("top", 870.0, 0.0, 5120.0 / 1.5, 960.0, false),
        monitor("primary", 0.0, 960.0, 5120.0, 1440.0, true),
    ];
    let proposed = &state.layout.devices[..1];
    let result = reconcile_layout(proposed, &state).expect("auto placement");
    assert_eq!((result.devices[1].x, result.devices[1].y), (5120.0, 960.0));
    let home = &state.self_info.device_id;
    let peer = &state.peers[0].device_id;
    let desktop = crate::layout::Desktop::from_layout(
        &result,
        &HashMap::from([
            (home.clone(), state.self_info.monitors.clone()),
            (peer.clone(), state.peers[0].monitors.clone()),
        ]),
    )
    .expect("desktop");
    assert_eq!(
        desktop
            .move_cursor_from(
                Point {
                    x: 7679.0 / 1.5,
                    y: 1000.0
                },
                Point {
                    x: 2.0 / 1.5,
                    y: 0.0
                },
                Some(home)
            )
            .crossing
            .expect("flush edge")
            .target_device_id,
        *peer
    );
}

// Feature: arrange this computer's screens differently from the operating system. Through the real settings call the
// engine reports the arranged screens, keeps them where they were dropped on the desk, and when a paired computer
// moves the cursor onto the arranged top monitor, the cursor lands on that same pixel of the REAL top monitor.
#[tokio::test]
async fn arranged_screens_are_reported_and_cursor_moves_land_on_the_real_screen() {
    let dir = tempfile::tempdir().expect("dir");
    let mut core = paired_core(dir.path()).await;
    let input = core.mock_platform().expect("mock").input.clone();
    input
        .set_monitors(vec![
            monitor("top", 870.0, 0.0, 5120.0 / 1.5, 960.0, false),
            monitor("bottom", 0.0, 960.0, 5120.0, 1440.0, true),
        ])
        .expect("screens");
    core.tick().await.expect("hot-plug handled");
    let response = core
        .handle(Request {
            id: 1,
            method: "set_settings".into(),
            params: serde_json::json!({ "patch": { "display": { "arrangement": [
                { "monitor_id": "bottom", "x": 0.0, "y": 0.0 },
                { "monitor_id": "top", "x": 5120.0, "y": 0.0 }
            ] } } }),
        })
        .await
        .expect("response");
    assert!(response.ok, "arrangement accepted: {:?}", response.error);
    let state = core.snapshot();
    let top = state
        .self_info
        .monitors
        .iter()
        .find(|m| m.id == "top")
        .expect("top");
    assert_eq!(
        (top.x, top.y),
        (5120.0, 0.0),
        "the arranged top monitor is reported"
    );

    enter(&mut core, 1).await;
    input.take_injected_events().expect("clear");
    core.receive_link(LinkEvent::Move {
        peer_id: "e".repeat(64),
        movement: wire::Move {
            seq: 1,
            x: 5200.0,
            y: 100.0,
        },
    })
    .await
    .expect("move");
    let landed = input
        .take_injected_events()
        .expect("events")
        .into_iter()
        .rev()
        .find_map(|e| match e.kind {
            InputEventKind::PointerMoved { position, .. } => Some(position),
            _ => None,
        })
        .expect("a cursor move was injected");
    assert_eq!(
        landed,
        Point { x: 950.0, y: 100.0 },
        "same pixel of the real top monitor"
    );

    // An empty arrangement goes back to the operating system's own layout.
    let reset = core
        .handle(Request {
            id: 2,
            method: "set_settings".into(),
            params: serde_json::json!({ "patch": { "display": { "arrangement": [] } } }),
        })
        .await
        .expect("response");
    assert!(reset.ok);
    let top = core
        .snapshot()
        .self_info
        .monitors
        .into_iter()
        .find(|m| m.id == "top")
        .expect("top");
    assert_eq!((top.x, top.y), (870.0, 0.0));
}

// Bug: when the Mac's screen had gone dark, moving the cursor onto it left the screen off.
#[tokio::test]
async fn arriving_cursor_wakes_the_screen_once_per_crossing() {
    let dir = tempfile::tempdir().expect("dir");
    let mut core = paired_core(dir.path()).await;
    let input = core.mock_platform().expect("mock").input.clone();
    assert_eq!(input.display_wakes().expect("wakes"), 0);
    enter(&mut core, 1).await;
    assert_eq!(input.display_wakes().expect("wakes"), 1);
    // A repeated Enter for the same crossing is ignored and does not wake again.
    enter(&mut core, 1).await;
    assert_eq!(input.display_wakes().expect("wakes"), 1);
}

#[tokio::test]
async fn bad_coordinates_transients_and_elevated_apps_do_not_disconnect_or_spam() {
    // Prevents one bad coordinate/UIPI rejection tearing down sharing and claiming missing permissions.
    let dir = tempfile::tempdir().expect("dir");
    let mut core = paired_core(dir.path()).await;
    enter(&mut core, 1).await;
    let input = core.mock_platform().expect("mock").input.clone();
    key(&mut core, 1, true).await.expect("hold");
    input
        .fail_next(InputOperation::Inject, BackendError::InvalidPosition)
        .expect("script");
    core.receive_link(LinkEvent::Move {
        peer_id: "e".repeat(64),
        movement: wire::Move {
            seq: 1,
            x: -100.0,
            y: 5000.0,
        },
    })
    .await
    .expect("clamp retry");
    let pos = input.local_cursor_pos().expect("cursor");
    assert!(pos.x >= 0.0 && pos.y < 1080.0);
    for seq in 2..62 {
        // Both preflight and fresh-topology retry can race a display reconfiguration.
        for _ in 0..2 {
            input
                .fail_next(InputOperation::Inject, BackendError::InvalidPosition)
                .expect("script");
        }
        core.receive_link(LinkEvent::Move {
            peer_id: "e".repeat(64),
            movement: wire::Move {
                seq,
                x: 2000.0,
                y: 100.0,
            },
        })
        .await
        .expect("invalid position never tears down");
        assert!(core.receiving_from.is_some());
    }
    core.take_events();
    for seq in 2..122 {
        input
            .fail_next(
                InputOperation::Inject,
                if seq == 2 {
                    BackendError::Transient
                } else {
                    BackendError::TargetElevated
                },
            )
            .expect("script");
        key(&mut core, seq, false)
            .await
            .expect("single event dropped");
        assert!(core.receiving_from.is_some());
    }
    let notices: Vec<_> = core
        .take_events()
        .into_iter()
        .filter_map(|e| {
            if let Event::Notification(n) = e {
                Some(n)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].body.contains("administrator"));
    assert!(!notices[0].title.contains("permission"));
    key(&mut core, 122, false).await.expect("recovery");
    assert!(input.held_keys().expect("release").is_empty());
    core.end_forwarding("return_home").await.expect("home");
}

#[tokio::test]
async fn permission_and_sustained_injection_failure_release_holds_and_restore_cursor() {
    // Prevents stuck keys/invisible cursors after revocation or sustained OS injection rejection.
    for (failure, count, code) in [
        (
            BackendError::PermissionDenied,
            1,
            ErrorCode::PermissionDenied,
        ),
        (BackendError::Transient, 50, ErrorCode::Internal),
    ] {
        let dir = tempfile::tempdir().expect("dir");
        let mut core = paired_core(dir.path()).await;
        enter(&mut core, 1).await;
        let input = core.mock_platform().expect("mock").input.clone();
        key(&mut core, 1, true).await.expect("hold");
        input
            .inject(InputEvent {
                kind: InputEventKind::Button {
                    button: glide_platform::Button::Left,
                    down: true,
                },
                injected: true,
            })
            .expect("button held");
        input.set_cursor_visible(false).expect("hide");
        core.take_events();
        for index in 0..count {
            input
                .fail_next(InputOperation::Inject, failure.clone())
                .expect("script");
            let result = key(&mut core, index + 2, false).await;
            if index + 1 < count {
                result.expect("keep session");
                assert!(core.receiving_from.is_some());
            } else {
                assert_eq!(result.expect_err("teardown").code, code);
            }
        }
        assert!(core.receiving_from.is_none());
        assert!(input.held_keys().expect("keys released").is_empty());
        assert!(input.held_buttons().expect("buttons released").is_empty());
        assert!(input.cursor_visible().expect("visible"));
        let notices: Vec<_> = core
            .take_events()
            .into_iter()
            .filter_map(|e| {
                if let Event::Notification(n) = e {
                    Some(n)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(notices.len(), 1);
        assert_eq!(
            notices[0].title == "Glide needs permission",
            failure == BackendError::PermissionDenied
        );
    }
}

#[tokio::test]
async fn inactive_receiver_cursor_balances_leave_enter_liveness_and_failure_paths() {
    // Prevents the abandoned Mac cursor staying visible, or remaining invisible after recovery/exit.
    let dir = tempfile::tempdir().expect("dir");
    let mut core = paired_core(dir.path()).await;
    let input = core.mock_platform().expect("mock").input.clone();
    assert!(input.cursor_visible().expect("startup"));
    for epoch in 1..=3 {
        enter(&mut core, epoch).await;
        assert!(input.cursor_visible().expect("enter shows"));
        control(&mut core, ControlMessage::Leave(wire::Leave { epoch }))
            .await
            .expect("leave");
        assert!(!input.cursor_visible().expect("leave hides"));
    }
    let start = Instant::now();
    core.cursor_hidden_peer = Some(("e".repeat(64), start));
    core.cursor_watchdog(start + Duration::from_millis(1499));
    assert!(!input.cursor_visible().expect("live hidden"));
    core.cursor_watchdog(start + Duration::from_millis(1500));
    assert!(input.cursor_visible().expect("timeout escape"));
    enter(&mut core, 4).await;
    control(&mut core, ControlMessage::Leave(wire::Leave { epoch: 4 }))
        .await
        .expect("leave");
    core.receive_link(LinkEvent::Disconnected {
        peer_id: "e".repeat(64),
        reason: None,
    })
    .await
    .expect("disconnect");
    assert!(input.cursor_visible().expect("link loss"));
    enter(&mut core, 5).await;
    control(&mut core, ControlMessage::Leave(wire::Leave { epoch: 5 }))
        .await
        .expect("leave");
    core.end_forwarding("return_home")
        .await
        .expect("return home");
    assert!(input.cursor_visible().expect("home"));
    input.set_cursor_visible(false).expect("hide");
    input
        .fail_next(InputOperation::SetCursorVisible, BackendError::Unavailable)
        .expect("show failure");
    core.end_forwarding("return_home")
        .await
        .expect("cosmetic failure keeps link");
    assert!(!input.cursor_visible().expect("first show failed"));
    core.cursor_watchdog(Instant::now());
    assert!(input
        .cursor_visible()
        .expect("watchdog retries restoration"));
    input.set_cursor_visible(false).expect("hide");
    core.shutdown().await.expect("shutdown");
    assert!(input.cursor_visible().expect("shutdown restore"));
    input.set_cursor_visible(false).expect("hide before drop");
    drop(core);
    assert!(input.cursor_visible().expect("drop restore"));

    let dir = tempfile::tempdir().expect("dir");
    let core = paired_core(dir.path()).await;
    let input = core.mock_platform().expect("mock").input.clone();
    input.set_cursor_visible(false).expect("hide before panic");
    let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _core = core;
        panic!("scripted engine unwind");
    }));
    assert!(unwind.is_err());
    assert!(input.cursor_visible().expect("unwind restores"));
}

#[tokio::test]
async fn source_cursor_hides_only_after_remote_enter_and_restores_on_escape() {
    // Prevents a forwarding brain leaving its cursor visible or hiding it on a failed Enter.
    let dir = tempfile::tempdir().expect("dir");
    let mut core = paired_core(dir.path()).await;
    let input = core.mock_platform().expect("mock").input.clone();
    let peer = core.state.peers[0].device_id.clone();
    let event = crate::layout::EngineEvent::Enter {
        device_id: peer,
        position: Point { x: 50.0, y: 50.0 },
        modifiers_down: vec![],
    };
    core.process_engine_events(vec![event])
        .await
        .expect("forward");
    assert!(!input.cursor_visible().expect("hidden source"));
    core.end_forwarding("link_lost").await.expect("escape");
    assert!(input.cursor_visible().expect("restored"));
    input
        .fail_next(InputOperation::SetMode, BackendError::Unavailable)
        .expect("mode failure");
    let event = crate::layout::EngineEvent::Enter {
        device_id: "e".repeat(64),
        position: Point { x: 50.0, y: 50.0 },
        modifiers_down: vec![],
    };
    assert!(core.process_engine_events(vec![event]).await.is_err());
    assert!(input.cursor_visible().expect("failed enter stays visible"));
}

#[tokio::test]
async fn mac_on_the_bottom_ultrawide_crosses_along_its_whole_shared_edge_and_overlaps_are_repaired()
{
    // Prevents: "the cursor only crosses to the Mac at the very corner of the big monitor". Tested on a real
    // rig (3413x960 monitor centred above a 5120x1440 primary) with the MacBook (1728x1117) on the primary's right edge
    // at the top, centre and bottom, and with a stale layout where the Mac overlaps the primary.
    let dir = tempfile::tempdir().expect("dir");
    let core = paired_core(dir.path()).await;
    let mut state = core.snapshot();
    state.self_info.monitors = vec![
        monitor("top", 870.0, 0.0, 5120.0 / 1.5, 960.0, false),
        monitor("primary", 0.0, 960.0, 5120.0, 1440.0, true),
    ];
    state.peers[0].monitors = vec![monitor("mac", 0.0, 0.0, 1728.0, 1117.0, true)];
    let home = state.self_info.device_id.clone();
    let peer = state.peers[0].device_id.clone();
    let monitors = HashMap::from([
        (home.clone(), state.self_info.monitors.clone()),
        (peer.clone(), state.peers[0].monitors.clone()),
    ]);
    for top in [960.0, 1121.5, 1283.0] {
        state.layout.devices[1].x = 5120.0;
        state.layout.devices[1].y = top;
        validate_layout(&state.layout.devices, &state).expect("flush against the primary is valid");
        let desktop =
            crate::layout::Desktop::from_layout(&state.layout, &monitors).expect("desktop");
        for offset in [0.0, 1.0, 558.0, 1116.0] {
            let moved = desktop.move_cursor_from(
                Point {
                    x: 5110.0,
                    y: top + offset,
                },
                Point { x: 30.0, y: 0.0 },
                Some(&home),
            );
            assert_eq!(
                moved
                    .crossing
                    .expect("crosses anywhere along the shared edge")
                    .target_device_id,
                peer,
                "mac top {top}, offset {offset}"
            );
        }
        // Next to the Mac's span, the primary's edge is a wall, not a crossing.
        let wall_y = if top > 1000.0 { 970.0 } else { 2300.0 };
        let moved = desktop.move_cursor_from(
            Point {
                x: 5110.0,
                y: wall_y,
            },
            Point { x: 30.0, y: 0.0 },
            Some(&home),
        );
        assert!(moved.crossing.is_none() || top < 961.0, "mac top {top}");
    }
    // A stale layout (Mac overlapping the primary) is repaired to a valid, touching slot - never kept as is.
    let top_right = 870.0 + 5120.0 / 1.5;
    state.layout.devices[1].x = top_right;
    state.layout.devices[1].y = 0.0;
    assert!(validate_layout(&state.layout.devices, &state).is_err());
    let repaired = reconcile_layout(&state.layout.devices, &state).expect("repair");
    validate_layout(&repaired.devices, &state).expect("repaired layout has no overlap");
    let mac = &repaired.devices[1];
    // Dropped just above the primary, flush against the right edge of the top monitor (nearest valid slot).
    assert_eq!((mac.x, mac.y), (top_right, -157.0));
}
