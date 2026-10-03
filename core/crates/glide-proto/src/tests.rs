use crate::codec::*;
use crate::ipc::*;
use crate::wire::*;
use glide_platform::{Key, Monitor, Os, PermissionStatus, Permissions};
use proptest::prelude::*;

fn bounded<T, const LIMIT: usize>(items: Vec<T>) -> BoundedVec<T, LIMIT> {
    BoundedVec::try_from_vec(items).unwrap_or_else(|_| panic!("test fixture exceeds limit {LIMIT}"))
}

fn sample_monitor() -> Monitor {
    Monitor {
        id: "display-1".to_owned(),
        x: 0.0,
        y: 0.0,
        w: 1920.0,
        h: 1080.0,
        scale: 1.0,
        primary: true,
    }
}

fn sample_state() -> State {
    State {
        self_info: SelfInfo {
            device_id: "self-id".to_owned(),
            name: "Desk".to_owned(),
            os: Os::Windows,
            fingerprint: "fingerprint".to_owned(),
            listen_port: 24800,
            version: "0.1.0".to_owned(),
            monitors: vec![sample_monitor()],
        },
        sharing_enabled: true,
        active_device_id: "self-id".to_owned(),
        permissions: Permissions {
            accessibility: PermissionStatus::Granted,
            input_monitoring: PermissionStatus::NotApplicable,
            injection: PermissionStatus::Granted,
        }
        .into(),
        peers: vec![Peer {
            device_id: "peer-id".to_owned(),
            name: "Laptop".to_owned(),
            os: Os::Macos,
            fingerprint: "peer-fingerprint".to_owned(),
            online: true,
            connection: Connection::Connected,
            address: Some("192.0.2.1:24800".to_owned()),
            latency_ms: Some(1.25),
            monitors: vec![sample_monitor()],
            clipboard_enabled: true,
        }],
        discovered: vec![DiscoveredPeer {
            device_id: "nearby-id".to_owned(),
            name: "Nearby".to_owned(),
            os: Os::Macos,
            address: "192.0.2.2:24800".to_owned(),
        }],
        layout: Layout {
            devices: vec![LayoutDevice {
                device_id: "self-id".to_owned(),
                x: 0.5,
                y: -10.25,
            }],
        },
        settings: Settings::default(),
        transfers: vec![Transfer {
            id: "transfer-1".to_owned(),
            direction: TransferDirection::Receive,
            peer_id: "peer-id".to_owned(),
            name: "photo.png".to_owned(),
            items: 1,
            bytes_total: 512,
            bytes_done: 256,
            rate_bps: 1024,
            state: TransferState::Active,
            error: None,
        }],
    }
}

#[test]
fn ipc_state_and_envelopes_match_the_json_contract() {
    // Default-sensitive filtering and the escape hotkey are part of the UI contract.
    let cases = [
        (ErrorCode::BadCode, "bad_code"),
        (ErrorCode::CodeExpired, "code_expired"),
        (ErrorCode::LockedOut, "locked_out"),
        (ErrorCode::Unreachable, "unreachable"),
        (ErrorCode::NotPaired, "not_paired"),
        (ErrorCode::InvalidParams, "invalid_params"),
        (ErrorCode::PermissionDenied, "permission_denied"),
        (ErrorCode::Internal, "internal"),
    ];
    for (code, expected) in cases {
        assert_eq!(serde_json::to_value(code).unwrap(), expected);
    }
    for (status, expected) in [
        (PermissionStatus::Granted, "granted"),
        (PermissionStatus::Denied, "denied"),
        (PermissionStatus::Unknown, "unknown"),
        (PermissionStatus::NotApplicable, "n/a"),
    ] {
        assert_eq!(serde_json::to_value(status).unwrap(), expected);
    }
    let defaults = Settings::default();
    assert!(defaults.clipboard.exclude_sensitive);
    assert_eq!(defaults.hotkeys.return_home, "Ctrl+Alt+Shift+Home");
    assert_eq!(defaults.hotkeys.toggle_sharing, "Ctrl+Alt+Shift+S");
    assert_eq!(defaults.clipboard.max_auto_mb, 2048);
    assert_eq!(defaults.keyboard.swap_ctrl_cmd, SwapCtrlCmd::Auto);
    assert_eq!(serde_json::to_value(Os::Windows).unwrap(), "windows");
    assert_eq!(serde_json::to_value(Os::Macos).unwrap(), "macos");
    let state = sample_state();
    let value = serde_json::to_value(&state).expect("state serializes");
    assert!(value.get("self").is_some());
    assert!(value.get("self_info").is_none());
    assert_eq!(serde_json::from_value::<State>(value).unwrap(), state);

    let ready = Event::Ready {
        version: "0.1.0".to_owned(),
    };
    assert_eq!(
        serde_json::to_value(ready).unwrap(),
        serde_json::json!({
            "event": "ready",
            "data": {"version": "0.1.0"}
        })
    );
    assert_eq!(
        serde_json::to_value(Event::State(Box::new(state.clone()))).unwrap()["event"],
        "state"
    );

    let success = Response::success(7, serde_json::json!({"value": 1}));
    assert_eq!(
        serde_json::to_value(success).unwrap(),
        serde_json::json!({
            "id": 7,
            "ok": true,
            "result": {"value": 1}
        })
    );
    let failure = Response::failure(8, IpcError::new(ErrorCode::BadCode, "invalid pairing code"));
    assert_eq!(
        serde_json::to_value(failure).unwrap(),
        serde_json::json!({
            "id": 8,
            "ok": false,
            "error": {"code": "bad_code", "message": "invalid pairing code"}
        })
    );
}

#[test]
fn permission_request_and_restart_state_round_trip() {
    let request = Request {
        id: 42,
        method: METHOD_PERMISSIONS_REQUEST.into(),
        params: serde_json::json!({}),
    };
    let decoded =
        decode_jsonl_request(&encode_jsonl(&request).expect("request JSONL")).expect("request");
    assert_eq!(decoded.method, METHOD_PERMISSIONS_REQUEST);
    assert_eq!(decoded.params, serde_json::json!({}));
    let _: EmptyParams = serde_json::from_value(decoded.params.clone()).expect("empty params");
    let reply = Response::success(decoded.id, serde_json::json!({}));
    assert_eq!(
        serde_json::from_slice::<Response>(&encode_jsonl(&reply).expect("reply"))
            .expect("round trip"),
        reply
    );
    let mut state = sample_state();
    for restart in [false, true] {
        state.permissions.restart_required = restart;
        let json = serde_json::to_value(&state).expect("state");
        assert_eq!(json["permissions"]["restart_required"], restart);
        assert_eq!(
            serde_json::from_value::<State>(json).expect("state round trip"),
            state
        );
    }
    let mut old = serde_json::to_value(&state).expect("state");
    old["permissions"]
        .as_object_mut()
        .expect("permissions")
        .remove("restart_required");
    assert!(
        !serde_json::from_value::<State>(old)
            .expect("older state")
            .permissions
            .restart_required
    );
}

#[test]
fn every_ipc_event_variant_uses_its_contract_name() {
    let state = sample_state();
    let events = [
        Event::Ready {
            version: "0.1".to_owned(),
        },
        Event::State(Box::new(state)),
        Event::PairingIncoming(PairingIncoming {
            name: "Mac".to_owned(),
            os: Os::Macos,
            address: "host:1".to_owned(),
        }),
        Event::PairingResult(PairingResult {
            ok: false,
            device_id: None,
            error: Some(IpcError::new(ErrorCode::BadCode, "bad code")),
        }),
        Event::PairingVerify(PairingVerify {
            phrase: ["amber".to_owned(), "birch".to_owned(), "cobalt".to_owned()],
            peer: VerificationPeer {
                name: "Desk".to_owned(),
                os: Os::Windows,
            },
            expires_at_ms: 1234,
        }),
        Event::PeerStats(PeerStats {
            device_id: "peer".to_owned(),
            latency_ms: 2.0,
            rx_bps: 10,
            tx_bps: 20,
        }),
        Event::TransferProgress(TransferProgress {
            id: "transfer".to_owned(),
            bytes_done: 4,
            rate_bps: 8,
        }),
        Event::Notification(Notification {
            level: "info".to_owned(),
            title: "Ready".to_owned(),
            body: "Connected".to_owned(),
            action: None,
        }),
        Event::ActiveChanged(ActiveChanged {
            device_id: "peer".to_owned(),
            reason: "edge".to_owned(),
        }),
    ];
    let expected = [
        "ready",
        "state",
        "pairing.incoming",
        "pairing.result",
        "pairing.verify",
        "peer.stats",
        "transfer.progress",
        "notification",
        "active_changed",
    ];
    for (event, name) in events.iter().zip(expected) {
        let value = serde_json::to_value(event).expect("event serializes");
        assert_eq!(value["event"], name);
        assert!(value.get("data").is_some());
        assert_eq!(serde_json::from_value::<Event>(value).unwrap(), *event);
    }
}

#[test]
fn ipc_methods_and_unknown_fields_are_accepted() {
    let request: Request = serde_json::from_str(
        r#"{"id":1,"method":"set_sharing","params":{"enabled":true,"future":1},"future":2}"#,
    )
    .expect("unknown fields are ignored");
    let params: SetSharingParams = serde_json::from_value(request.params.clone()).unwrap();
    assert!(params.enabled);
    assert_eq!(
        [
            METHOD_GET_STATE,
            METHOD_SET_SETTINGS,
            METHOD_SET_SHARING,
            METHOD_SET_LAYOUT,
            METHOD_PAIRING_START_HOST,
            METHOD_PAIRING_CANCEL_HOST,
            METHOD_PAIRING_JOIN,
            METHOD_PAIRING_CONFIRM,
            METHOD_PEER_ADD_MANUAL,
            METHOD_PEER_UNPAIR,
            METHOD_PEER_CONFIGURE,
            METHOD_RETURN_HOME,
            METHOD_TRANSFER_CANCEL,
            METHOD_TRANSFER_CONFIRM,
            METHOD_PERMISSIONS_OPEN_SETTINGS,
            METHOD_PERMISSIONS_REQUEST,
            METHOD_APP_SHUTDOWN,
        ],
        [
            "get_state",
            "set_settings",
            "set_sharing",
            "set_layout",
            "pairing.start_host",
            "pairing.cancel_host",
            "pairing.join",
            "pairing.confirm",
            "peer.add_manual",
            "peer.unpair",
            "peer.configure",
            "return_home",
            "transfer.cancel",
            "transfer.confirm",
            "permissions.open_settings",
            "permissions.request",
            "app.shutdown",
        ]
    );
    assert!(PairingJoinParams {
        address: Some("host:1".to_owned()),
        device_id: None,
        code: "123456".into(),
    }
    .has_single_target());

    let confirm = PairingConfirmParams { accepted: true };
    assert_eq!(
        serde_json::from_value::<PairingConfirmParams>(
            serde_json::to_value(&confirm).expect("confirm serializes")
        )
        .expect("confirm deserializes"),
        confirm
    );
    let configure = PeerConfigureParams {
        device_id: "peer".to_owned(),
        clipboard_enabled: Some(false),
    };
    assert_eq!(
        serde_json::from_value::<PeerConfigureParams>(
            serde_json::to_value(&configure).expect("configure serializes")
        )
        .expect("configure deserializes"),
        configure
    );
}

#[test]
fn jsonl_codec_enforces_line_boundaries_and_limits() {
    let request = Request {
        id: 1,
        method: METHOD_GET_STATE.to_owned(),
        params: serde_json::json!({}),
    };
    let encoded = encode_jsonl(&request).unwrap();
    assert_eq!(encoded.last(), Some(&b'\n'));
    assert_eq!(decode_jsonl_request(&encoded).unwrap(), request);
    assert_eq!(
        decode_jsonl_request(b"{\"id\":1}\n{\"id\":2}\n"),
        Err(CodecError::InvalidJsonLine)
    );
    assert!(matches!(
        decode_jsonl_request(&vec![b' '; MAX_IPC_LINE_BYTES + 1]),
        Err(CodecError::FrameTooLarge { .. })
    ));
    let oversized = serde_json::json!({"data": "x".repeat(MAX_IPC_LINE_BYTES)});
    assert!(matches!(encode_jsonl(&oversized), Err(CodecError::Json(_))));
}

#[test]
fn declared_oversized_collection_is_rejected_before_items_are_read() {
    // Control enum Hello, version 1, empty device/name, Windows, then monitor count 65.
    let malicious = [0, 1, 0, 0, 0, (MAX_MONITORS as u8) + 1];
    assert!(decode_control(&malicious).is_err());
}

#[test]
fn oversized_frames_are_rejected_before_decode() {
    let oversized = vec![0; MAX_INPUT_FRAME_BYTES + 1];
    assert!(matches!(
        decode_input(&oversized),
        Err(CodecError::FrameTooLarge { .. })
    ));
    let oversized_move = vec![0; MAX_MOVE_FRAME_BYTES + 1];
    assert!(matches!(
        decode_move(&oversized_move),
        Err(CodecError::FrameTooLarge { .. })
    ));
}

#[test]
fn aggregate_codec_applies_clipboard_limit_from_message_tag() {
    // The protocol identifier and literal lane tag are interoperability boundaries.
    assert_eq!(ALPN, b"glide/3");
    let mut oversized = vec![2, 0];
    oversized.resize(MAX_CLIP_ANNOUNCE_FRAME_BYTES + 1, 0);
    assert_eq!(
        decode_wire(&oversized),
        Err(CodecError::FrameTooLarge {
            limit: MAX_CLIP_ANNOUNCE_FRAME_BYTES,
            actual: MAX_CLIP_ANNOUNCE_FRAME_BYTES + 1,
        })
    );
}

#[test]
fn non_finite_coordinates_are_rejected() {
    let movement = Move {
        seq: 1,
        x: f64::NAN,
        y: 0.0,
    };
    assert!(matches!(
        encode_move(&movement),
        Err(CodecError::InvalidValue(_))
    ));
    let malformed = postcard::to_allocvec(&movement).unwrap();
    assert!(matches!(
        decode_move(&malformed),
        Err(CodecError::InvalidValue(_))
    ));
}

#[test]
fn every_wire_family_rejects_trailing_and_overlong_encodings() {
    fn reject<T>(frame: Vec<u8>, decode: impl Fn(&[u8]) -> Result<T, CodecError>) {
        assert!(decode(&frame).is_ok());
        let mut trailing = frame.clone();
        trailing.push(0);
        assert!(decode(&trailing).is_err());
        let mut overlong = frame;
        overlong[0] |= 0x80;
        overlong.insert(1, 0);
        assert!(decode(&overlong).is_err());
    }
    let control = ControlMessage::Heartbeat(Heartbeat { seq: 1, ts: 2 });
    let input = InputMessage::Key(InputKey {
        epoch: 1,
        seq: 1,
        hid_usage: Key(4),
        down: true,
    });
    reject(encode_control(&control).unwrap(), decode_control);
    reject(encode_input(&input).unwrap(), decode_input);
    reject(
        encode_move(&Move {
            seq: 1,
            x: 0.0,
            y: 0.0,
        })
        .unwrap(),
        decode_move,
    );
    reject(
        encode_clipboard(&ClipboardMessage::ClipFetch(ClipFetch {
            clip_id: "c".into(),
            format: "text".into(),
        }))
        .unwrap(),
        decode_clipboard,
    );
    reject(
        encode_transfer(&TransferMessage::FileCancel(FileCancel {
            transfer_id: "t".into(),
        }))
        .unwrap(),
        decode_transfer,
    );
    reject(
        encode_wire(&WireMessage::Control(control.clone())).unwrap(),
        decode_wire,
    );
    let mut buffer = [0; MAX_INPUT_FRAME_BYTES];
    assert_eq!(
        encode_input_into(&input, &mut buffer).unwrap(),
        encode_input(&input).unwrap()
    );
    assert_eq!(
        encode_control_into(&control, &mut buffer).unwrap(),
        encode_control(&control).unwrap()
    );
    assert!(encode_input_into(&input, &mut []).is_err());
    assert!(encode_control_into(&control, &mut []).is_err());
}

#[test]
fn borrowed_chunk_is_golden_equivalent_and_incremental() {
    for compressed in [false, true] {
        let chunk = FileChunk {
            transfer_id: "job".into(),
            file_id: 128,
            chunk_index: 16384,
            offset: 1 << 40,
            data: bounded(vec![0, 127, 128, 255]),
            uncompressed_size: 4,
            compressed,
            blake3_hash: [255; 32],
        };
        let view = FileChunkView {
            transfer_id: &chunk.transfer_id,
            file_id: chunk.file_id,
            chunk_index: chunk.chunk_index,
            offset: chunk.offset,
            data: &chunk.data,
            uncompressed_size: chunk.uncompressed_size,
            compressed,
            blake3_hash: chunk.blake3_hash,
        };
        let golden = encode_transfer(&TransferMessage::FileChunk(chunk.clone())).unwrap();
        assert_eq!(
            decode_transfer(&golden).unwrap(),
            TransferMessage::FileChunk(chunk.clone())
        );
        let wire_frame = [&[3][..], &golden].concat();
        assert_eq!(
            decode_wire(&wire_frame).unwrap(),
            WireMessage::Transfer(TransferMessage::FileChunk(chunk.clone()))
        );
        let mut overlong_outer = wire_frame.clone();
        overlong_outer[0] = 131;
        overlong_outer.insert(1, 0);
        assert!(decode_wire(&overlong_outer).is_err());
        let mut overlong_inner = wire_frame.clone();
        overlong_inner[1] = 129;
        overlong_inner.insert(2, 0);
        assert!(decode_wire(&overlong_inner).is_err());
        let mut out = [0; 512];
        assert_eq!(encode_file_chunk_into(&view, &mut out).unwrap(), golden);
        let mut prefix = [0; 256];
        let prefix = encode_file_chunk_prefix_into(&view, &mut prefix).unwrap();
        let mut trailer = [0; 38];
        let trailer = encode_file_chunk_trailer_into(&view, &mut trailer).unwrap();
        assert_eq!([prefix, view.data, trailer].concat(), golden);
        for length in 0..prefix.len() {
            assert!(decode_file_chunk_header(&prefix[..length])
                .unwrap()
                .is_none());
        }
        assert_eq!(
            decode_file_chunk_header(prefix)
                .unwrap()
                .unwrap()
                .0
                .data_len,
            4
        );
        let borrowed = decode_file_chunk_view(&golden).unwrap();
        assert_eq!(borrowed.data, view.data);
        assert!(borrowed.data.as_ptr() >= golden.as_ptr());
        let mut trailing = golden.clone();
        trailing.push(0);
        assert!(decode_file_chunk_view(&trailing).is_err());
        assert!(decode_transfer(&trailing).is_err());
        assert!(decode_wire(&[&[3][..], &trailing].concat()).is_err());
        let mut overlong = golden.clone();
        overlong[0] = 129;
        overlong.insert(1, 0);
        assert!(decode_file_chunk_view(&overlong).is_err());
        for length in 0..golden.len() {
            assert!(decode_file_chunk_view(&golden[..length]).is_err());
        }
    }
    // Bounds are rejected from an incomplete header, before any payload arrives.
    assert!(decode_file_chunk_header(&[1, 129, 1]).is_err());
    let mut header =
        postcard::to_allocvec(&(1u32, "t", 0u32, 0u32, 0u64, MAX_FILE_CHUNK_BYTES + 1)).unwrap();
    assert!(decode_file_chunk_header(&header).is_err());
    header = postcard::to_allocvec(&(1u32, "t", u64::MAX)).unwrap();
    assert!(decode_file_chunk_header(&header).is_err());
}

#[test]
fn manifest_pages_are_contiguous_bounded_and_final() {
    use crate::manifest::ManifestValidator;
    let mut page = FileManifest {
        transfer_id: "job".into(),
        clip_id: "clip".into(),
        chunk_size: 4096,
        page: 0,
        final_page: false,
        files: bounded(vec![FileManifestEntry {
            file_id: 0,
            relative_path: "a".into(),
            size: 1,
            is_dir: false,
            blake3_hash: Some([0; 32]),
        }]),
    };
    let mut validator = ManifestValidator::default();
    validator.push(&page).unwrap();
    assert!(validator.finish().is_err());
    assert!(validator.push(&page).is_err());
    page.page = 1;
    page.files[0].file_id = 1;
    page.final_page = true;
    for field in 0..3 {
        let mut invalid = page.clone();
        match field {
            0 => invalid.chunk_size += 1,
            1 => invalid.clip_id.push('x'),
            _ => invalid.transfer_id.push('x'),
        }
        assert!(validator.push(&invalid).is_err());
    }
    let mut invalid = page.clone();
    invalid.files[0].file_id = 2;
    assert!(validator.push(&invalid).is_err());
    invalid = page.clone();
    invalid.files[0].size = MAX_TRANSFER_BYTES;
    assert!(validator.push(&invalid).is_err());
    validator.push(&page).unwrap();
    assert_eq!(validator.finish().unwrap().items, 2);
    assert!(validator.push(&page).is_err());
    for size in [0, MAX_FILE_CHUNK_BYTES as u32 + 1] {
        page.chunk_size = size;
        assert!(encode_transfer(&TransferMessage::FileManifest(page.clone())).is_err());
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn arbitrary_input_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..4096)) {
        let _ = decode_jsonl_request(&bytes);
        let _ = decode_control(&bytes);
        let _ = decode_input(&bytes);
        let _ = decode_move(&bytes);
        let _ = decode_clipboard(&bytes);
        let _ = decode_transfer(&bytes);
        let _ = decode_wire(&bytes);
    }
}
