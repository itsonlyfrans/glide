use super::*;

fn directory() -> tempfile::TempDir {
    tempfile::tempdir_in(std::env::var_os("CARGO_TARGET_DIR").expect("external target"))
        .expect("directory")
}
fn peer(letter: char) -> Peer {
    let id = letter.to_string().repeat(64);
    Peer {
        fingerprint: tls::human_fingerprint(&id),
        device_id: id,
        name: "Test".into(),
        os: glide_platform::Os::Windows,
        online: false,
        connection: PeerState::Offline,
        address: None,
        latency_ms: None,
        monitors: Vec::new(),
        clipboard_enabled: true,
        wake_mac: None,
        last_monitors: Vec::new(),
        app_version: None,
        model: None,
    }
}
fn seed(path: &Path) {
    write_peers(&path.join("network-peers.json"), &[peer('a'), peer('b')]).expect("seed");
    write_json_store(&path.join("revoked-peers.json"), &vec![peer('c').device_id]).expect("deny");
}

#[test]
fn repair_discards_only_identified_unfinished_trust_and_preserves_denials() {
    let data = directory();
    seed(data.path());
    write_json_store(
        &data.path().join("pairing-pending.json"),
        &peer('b').device_id,
    )
    .expect("staged");
    let report = NativePeerManager::repair_unfinished_pairing(data.path()).expect("repair");
    assert_eq!(report.removed_peer_ids, vec![peer('b').device_id]);
    assert!(report
        .removed_files
        .contains(&"pairing-pending.json".into()));
    let peers = read_peers(&data.path().join("network-peers.json")).expect("pins");
    assert_eq!(peers.len(), 1);
    assert!(peers.contains_key(&peer('a').device_id));
    assert!(read_revocations(&data.path().join("revoked-peers.json"))
        .expect("denied")
        .contains(&peer('c').device_id));
    assert_eq!(
        NativePeerManager::repair_unfinished_pairing(data.path()).expect("idempotent"),
        PairingRepair::default()
    );
}

#[test]
fn every_transaction_write_and_rename_boundary_can_be_repaired_without_activation() {
    for name in [
        "pairing-pending.json",
        "network-peers.json",
        "revoked-peers.json",
    ] {
        for boundary in ["create", "write", "sync", "rename", "directory_sync"] {
            let data = directory();
            seed(data.path());
            if name != "pairing-pending.json" {
                write_json_store(
                    &data.path().join("pairing-pending.json"),
                    &peer('b').device_id,
                )
                .expect("pending");
            }
            STORAGE_FAULT.with(|fault| fault.set(Some((name, boundary))));
            let result = match name {
                "pairing-pending.json" => {
                    write_json_store(&data.path().join(name), &peer('b').device_id)
                }
                "network-peers.json" => {
                    write_peers(&data.path().join(name), &[peer('a'), peer('b')])
                }
                _ => write_json_store(
                    &data.path().join(name),
                    &vec![peer('b').device_id, peer('c').device_id],
                ),
            };
            STORAGE_FAULT.with(|fault| fault.set(None));
            assert!(result.is_err(), "{name}:{boundary}");
            NativePeerManager::repair_unfinished_pairing(data.path()).expect("explicit repair");
            let pins = read_peers(&data.path().join("network-peers.json")).expect("pins");
            assert!(
                !pins.contains_key(&peer('b').device_id),
                "never activate {name}:{boundary}"
            );
        }
    }
}

#[test]
fn interrupted_repairs_remain_denied_and_are_idempotent() {
    for (name, boundary) in [
        ("pairing-repair.json", "create"),
        ("pairing-repair.json", "write"),
        ("pairing-repair.json", "sync"),
        ("pairing-repair.json", "directory_sync"),
        ("network-peers.json", "create"),
        ("network-peers.json", "write"),
        ("network-peers.json", "sync"),
        ("network-peers.json", "rename"),
        ("network-peers.json", "directory_sync"),
        ("network-peers.json.tmp", "remove"),
        ("pairing-pending.json", "remove"),
        ("pairing-pending.json.tmp", "remove"),
        ("revoked-peers.json.tmp", "remove"),
        ("pairing-repair.json.tmp", "remove"),
        ("pairing-repair.json", "before_remove"),
        ("pairing-repair.json", "remove"),
        ("pairing-repair.json", "final_directory_sync"),
    ] {
        let data = directory();
        seed(data.path());
        write_json_store(
            &data.path().join("pairing-pending.json"),
            &peer('b').device_id,
        )
        .expect("staged");
        // Exercise every cleanup boundary, including partial evidence. Ambiguity
        // deliberately discards all pins; committed deny records remain intact.
        for temporary in [
            "network-peers.json.tmp",
            "pairing-pending.json.tmp",
            "revoked-peers.json.tmp",
            "pairing-repair.json.tmp",
        ] {
            std::fs::write(data.path().join(temporary), b"{").expect("partial journal");
        }
        STORAGE_FAULT.with(|fault| fault.set(Some((name, boundary))));
        let result = NativePeerManager::repair_unfinished_pairing(data.path());
        STORAGE_FAULT.with(|fault| fault.set(None));
        assert!(result.is_err(), "{name}:{boundary}");
        let cleanup_complete =
            name == "pairing-repair.json" && matches!(boundary, "remove" | "final_directory_sync");
        assert_eq!(
            data.path().join("pairing-repair.json").exists(),
            !cleanup_complete,
            "startup remains denied until all cleanup is durable"
        );
        NativePeerManager::repair_unfinished_pairing(data.path()).expect("retry");
        assert!(!read_peers(&data.path().join("network-peers.json"))
            .expect("pins")
            .contains_key(&peer('b').device_id));
        assert!(read_revocations(&data.path().join("revoked-peers.json"))
            .expect("denials")
            .contains(&peer('c').device_id));
        assert_eq!(
            NativePeerManager::repair_unfinished_pairing(data.path()).expect("repeat"),
            PairingRepair::default()
        );
    }
}

#[test]
fn ambiguous_journals_discard_pins_and_unsafe_paths_are_preserved() {
    for name in [
        "pairing-pending.json.tmp",
        "revoked-peers.json.tmp",
        "network-peers.json.tmp",
        "pairing-repair.json.tmp",
    ] {
        let data = directory();
        seed(data.path());
        std::fs::write(data.path().join(name), b"{").expect("partial write");
        let report = NativePeerManager::repair_unfinished_pairing(data.path()).expect("discard");
        assert_eq!(report.removed_peer_ids.len(), 2);
        assert!(read_peers(&data.path().join("network-peers.json"))
            .expect("pins")
            .is_empty());
    }
    let data = directory();
    seed(data.path());
    std::fs::create_dir(data.path().join("pairing-pending.json")).expect("obstacle");
    assert!(NativePeerManager::repair_unfinished_pairing(data.path()).is_err());
    assert!(data.path().join("pairing-pending.json").is_dir());
}
