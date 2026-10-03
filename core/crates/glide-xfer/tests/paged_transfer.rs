#![cfg(feature = "file-engine")]

use glide_proto::wire::*;
use glide_xfer::*;
use std::{fs, time::Duration};
use tokio::io::{AsyncWriteExt, DuplexStream};

const PEER: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn temporary() -> tempfile::TempDir {
    tempfile::tempdir_in(std::env::var_os("CARGO_TARGET_DIR").expect("external target"))
        .expect("temp directory")
}

fn pipes(
    lanes: usize,
) -> (
    DuplexStream,
    DuplexStream,
    Vec<DuplexStream>,
    Vec<DuplexStream>,
) {
    let (left, right) = tokio::io::duplex(64 * 1024);
    let (send, receive) = (0..lanes).map(|_| tokio::io::duplex(64 * 1024)).unzip();
    (left, right, send, receive)
}

#[tokio::test]
async fn negotiated_chunk_size_is_used_and_bound_to_resume() {
    let source = temporary();
    let target = temporary();
    let bytes = vec![42; 12_321];
    let path = source.path().join("payload.bin");
    fs::write(&path, &bytes).expect("source");
    let sender_config = Config {
        chunk_size: 2048,
        parallel_streams: 1,
        ..Config::default()
    };
    let sender = FileEngine::new(target.path(), sender_config.clone())
        .await
        .expect("sender");
    let receiver = FileEngine::new(target.path(), Config::default())
        .await
        .expect("receiver")
        .with_chunk_size_bounds(ChunkSizeBounds {
            min: 1024,
            max: 4096,
        })
        .expect("bounds");
    let cancel = Cancel::new();
    let plan = build_manifest(
        vec![path],
        "negotiated".into(),
        "clip".into(),
        sender_config,
        cancel.clone(),
    )
    .await
    .expect("plan");
    let (mut left, mut right, send, receive) = pipes(1);
    let sent = Progress::new();
    let received = Progress::new();
    let (a, b) = tokio::join!(
        sender.send(plan, &mut left, send, &cancel, &sent),
        receiver.receive(
            PEER,
            &mut right,
            receive,
            Consent::Automatic,
            &cancel,
            &received
        )
    );
    a.expect("send");
    let b = b.expect("receive");
    assert_eq!(b.manifest.chunk_size, 2048);
    assert_eq!(fs::read(&b.paths[0]).expect("published"), bytes);
}

#[tokio::test]
async fn negotiated_chunk_out_of_range_fails_before_staging() {
    for size in [256, 8192] {
        let source = temporary();
        let target = temporary();
        let path = source.path().join("payload.bin");
        fs::write(&path, [1]).expect("source");
        let plan = build_manifest(
            vec![path],
            "out-of-range".into(),
            "clip".into(),
            Config {
                chunk_size: size,
                ..Config::default()
            },
            Cancel::new(),
        )
        .await
        .expect("plan");
        let receiver = FileEngine::new(target.path(), Config::default())
            .await
            .expect("receiver")
            .with_chunk_size_bounds(ChunkSizeBounds {
                min: 1024,
                max: 4096,
            })
            .expect("bounds");
        let (mut left, mut right, _, receive) = pipes(1);
        let cancel = Cancel::new();
        let progress = Progress::new();
        let message = TransferMessage::FileManifest(plan.manifest);
        let (a, b) = tokio::join!(
            write_message(&mut left, &message, Duration::from_secs(10), &cancel),
            receiver.receive(
                PEER,
                &mut right,
                receive,
                Consent::Automatic,
                &cancel,
                &progress
            )
        );
        a.expect("write");
        assert!(matches!(b, Err(Error::Limit("manifest chunk size"))));
        assert_eq!(
            fs::read_dir(target.path().join("glide-xfer"))
                .expect("staging")
                .count(),
            0
        );
    }
}

#[tokio::test]
async fn paged_walker_transfers_more_than_4096_entries_and_prompts_for_complete_digest() {
    let source = temporary();
    let target = temporary();
    let root = source.path().join("tree");
    fs::create_dir(&root).expect("root");
    for index in 0u32..4100 {
        fs::write(
            root.join(format!("{index}.txt")),
            if index % 1000 == 0 {
                &b"payload"[..]
            } else {
                &[]
            },
        )
        .expect("source");
    }
    let config = Config {
        chunk_size: 1024,
        parallel_streams: 2,
        max_auto_bytes: 0,
        ..Config::default()
    };
    let paging = PagingConfig {
        max_entries: 5000,
        page_entries: 128,
        page_bytes: 64 * 1024,
        max_roots: 1,
        ..PagingConfig::default()
    };
    let cancel = Cancel::new();
    let plan = build_manifest_paged(
        vec![root],
        "paged-tree".into(),
        "clip".into(),
        config.clone(),
        paging.clone(),
        source.path(),
        cancel.clone(),
    )
    .await
    .expect("paged plan");
    assert_eq!(plan.manifest.summary().items, 4101);
    assert!(plan.manifest.summary().pages > 1);
    let engine = FileEngine::new(target.path(), config)
        .await
        .expect("engine");
    let (mut left, mut right, _, receive) = pipes(2);
    let progress = Progress::new();
    let send_pages = async {
        for index in 0..plan.manifest.summary().pages {
            write_message(
                &mut left,
                &TransferMessage::FileManifest(plan.manifest.read_page(index).expect("page")),
                Duration::from_secs(30),
                &cancel,
            )
            .await
            .expect("write page");
        }
    };
    let (_, prompt) = tokio::join!(
        send_pages,
        engine.receive_paged(
            PEER,
            &mut right,
            receive,
            paging.clone(),
            Consent::Automatic,
            &cancel,
            &progress
        )
    );
    let PagedReceive::NeedsApproval(review) = prompt.expect("prompt") else {
        panic!("missing approval prompt")
    };
    assert_eq!(review.summary().items, 4101);
    assert_eq!(review.approve(), plan.manifest.approve());
    let consent = review.approve();
    drop(review);
    let pages: Vec<_> = (0..plan.manifest.summary().pages)
        .map(|index| plan.manifest.read_page(index).expect("page"))
        .collect();
    assert_eq!(
        consent,
        Consent::approve_pages(pages.iter()).expect("canonical approval")
    );
    drop(pages);
    let (mut left, mut right, send, receive) = pipes(2);
    let sent = Progress::new();
    let received = Progress::new();
    let (a, b) = tokio::join!(
        engine.send_paged(plan, &mut left, send, &cancel, &sent),
        engine.receive_paged(PEER, &mut right, receive, paging, consent, &cancel, &received)
    );
    a.expect("paged sender");
    let PagedReceive::Received(b) = b.expect("paged receiver") else {
        panic!("second approval prompt")
    };
    assert_eq!(b.manifest.summary().items, 4101);
    assert_eq!(b.paths.len(), 1);
    assert_eq!(
        fs::read_dir(&b.paths[0]).expect("published tree").count(),
        4100
    );
    assert_eq!(
        fs::read(b.paths[0].join("4000.txt")).expect("payload"),
        b"payload"
    );
    assert_eq!(received.snapshot().bytes_done, 35);
}

#[tokio::test]
async fn paged_approval_rejects_changed_digest_and_removes_private_spool() {
    let source = temporary();
    let target = temporary();
    let path = source.path().join("payload.bin");
    fs::write(&path, [1]).expect("source");
    let config = Config::default();
    let paging = PagingConfig::default();
    let cancel = Cancel::new();
    let plan = build_manifest_paged(
        vec![path],
        "digest".into(),
        "clip".into(),
        config.clone(),
        paging.clone(),
        source.path(),
        cancel.clone(),
    )
    .await
    .expect("plan");
    let consent = plan.manifest.approve();
    let mut page = plan.manifest.read_page(0).expect("page");
    page.files[0].blake3_hash = Some([7; 32]);
    let engine = FileEngine::new(target.path(), config)
        .await
        .expect("engine");
    let (mut left, mut right, _, receive) = pipes(1);
    let message = TransferMessage::FileManifest(page);
    let progress = Progress::new();
    let (a, b) = tokio::join!(
        write_message(&mut left, &message, Duration::from_secs(30), &cancel),
        engine.receive_paged(PEER, &mut right, receive, paging, consent, &cancel, &progress)
    );
    a.expect("send page");
    assert!(matches!(
        b,
        Err(Error::Invalid("approval manifest mismatch"))
    ));
    assert_eq!(
        fs::read_dir(target.path().join("glide-xfer"))
            .expect("staging")
            .count(),
        0
    );
}

#[tokio::test]
async fn resume_across_cut_preserves_verified_prefix_with_more_than_4096_gaps() {
    let source = temporary();
    let target = temporary();
    let bytes = vec![42; 10_000];
    let path = source.path().join("fragmented.bin");
    fs::write(&path, &bytes).expect("source");
    let config = Config {
        chunk_size: 1,
        parallel_streams: 1,
        ..Config::default()
    };
    let cancel = Cancel::new();
    let plan = build_manifest(
        vec![path],
        "many-gaps".into(),
        "clip".into(),
        config.clone(),
        cancel.clone(),
    )
    .await
    .expect("plan");
    let engine = FileEngine::new(target.path(), config.clone())
        .await
        .expect("engine")
        .with_resume_coalescing();
    let (mut left, mut right, mut send, receive) = pipes(1);
    let mut lane = send.remove(0);
    let progress = Progress::new();
    let cut = async {
        write_message(
            &mut left,
            &TransferMessage::FileManifest(plan.manifest.clone()),
            Duration::from_secs(30),
            &cancel,
        )
        .await
        .expect("manifest");
        read_message(&mut left, Duration::from_secs(30), &cancel)
            .await
            .expect("resume");
        let mut encoder = ChunkEncoder::new(&config).expect("encoder");
        for index in (0..10_000).step_by(2) {
            encoder.buffer_mut(1).expect("buffer")[0] = 42;
            let prepared = encoder.prepare("fragmented.bin").expect("prepare");
            write_file_chunk(
                &mut lane,
                &encoder.view("many-gaps", 0, index, prepared).expect("view"),
                Duration::from_secs(30),
                &cancel,
            )
            .await
            .expect("chunk");
        }
        tokio::time::timeout(Duration::from_secs(30), async {
            while progress.snapshot().bytes_done < 5000 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("all alternating chunks applied");
        lane.shutdown().await.expect("cut");
    };
    let (_, first) = tokio::join!(
        cut,
        engine.receive(
            PEER,
            &mut right,
            receive,
            Consent::Automatic,
            &cancel,
            &progress
        )
    );
    assert!(matches!(first, Err(Error::Io(_))));
    let (mut left, mut right, send, receive) = pipes(1);
    let sent = Progress::new();
    let received = Progress::new();
    let (a, b) = tokio::join!(
        engine.send(plan, &mut left, send, &cancel, &sent),
        engine.receive(
            PEER,
            &mut right,
            receive,
            Consent::Automatic,
            &cancel,
            &received
        )
    );
    a.expect("resume sender");
    let b = b.expect("resume receiver");
    assert_eq!(fs::read(&b.paths[0]).expect("published"), bytes);
    assert_eq!(received.snapshot().bytes_done, 10_000);
}

#[tokio::test]
async fn approval_spools_obey_quota_live_lease_and_crash_purge() {
    let target = temporary();
    let config = Config {
        max_staging_sessions: 1,
        stale_after: Duration::from_millis(1),
        ..Config::default()
    };
    let engine = FileEngine::new(target.path(), config.clone())
        .await
        .expect("engine");
    let staging = fs::canonicalize(target.path())
        .expect("canonical")
        .join("glide-xfer");
    let paging = PagingConfig {
        max_entries: 8,
        max_roots: 8,
        ..PagingConfig::default()
    };
    let mut spool = PageSpool::new(&staging, config.clone(), paging.clone()).expect("spool");
    spool
        .push(
            &FileManifest {
                transfer_id: "approval".into(),
                clip_id: "clip".into(),
                chunk_size: MAX_FILE_CHUNK_BYTES as u32,
                page: 0,
                final_page: true,
                files: ManifestFiles::try_from_vec(vec![FileManifestEntry {
                    file_id: 0,
                    relative_path: "tree".into(),
                    size: 0,
                    is_dir: true,
                    blake3_hash: None,
                }])
                .expect("entry"),
            },
            None,
            &Cancel::new(),
        )
        .expect("page");
    let held = spool.finish().expect("held review");
    assert!(matches!(
        PageSpool::new(&staging, config, paging),
        Err(Error::Limit("staging session count"))
    ));
    tokio::time::sleep(Duration::from_millis(3)).await;
    assert_eq!(engine.purge_stale().await.expect("live spool skipped"), 0);
    drop(held);
    assert_eq!(
        fs::read_dir(&staging).expect("cleanup").count(),
        0,
        "final Arc closes handles before TempDir cleanup"
    );
    let orphan = staging.join("glide-manifest-orphan123");
    fs::create_dir(&orphan).expect("orphan");
    fs::write(orphan.join("reservation"), 8u64.to_le_bytes()).expect("reservation");
    let lease = fs::File::create(orphan.join("lease")).expect("lease");
    lease
        .set_modified(std::time::SystemTime::UNIX_EPOCH)
        .expect("age");
    drop(lease);
    assert_eq!(engine.purge_stale().await.expect("crash purge"), 1);
    assert!(!orphan.exists());
}

#[tokio::test]
async fn paged_send_rechecks_stricter_engine_limits_before_wire_traffic() {
    let source = temporary();
    let target = temporary();
    let path = source.path().join("payload.bin");
    fs::write(&path, [1]).expect("source");
    let cancel = Cancel::new();
    let plan = build_manifest_paged(
        vec![path],
        "sender-policy".into(),
        "clip".into(),
        Config::default(),
        PagingConfig::default(),
        source.path(),
        cancel.clone(),
    )
    .await
    .expect("plan");
    let engine = FileEngine::new(
        target.path(),
        Config {
            max_file_bytes: 0,
            ..Config::default()
        },
    )
    .await
    .expect("stricter sender");
    let (mut left, _right, send, _receive) = pipes(1);
    assert!(matches!(
        engine
            .send_paged(plan, &mut left, send, &cancel, &Progress::new())
            .await,
        Err(Error::Limit("file size"))
    ));
}
