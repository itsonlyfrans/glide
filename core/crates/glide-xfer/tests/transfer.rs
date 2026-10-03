#![cfg(feature = "file-engine")]

use glide_proto::wire::*;
use glide_xfer::*;
use std::fs;
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{duplex, AsyncWriteExt, DuplexStream};

const PEER: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn temporary() -> TempDir {
    // Keep test data under the externally supplied target, not in the repository.
    let path = std::env::var_os("CARGO_TARGET_DIR").expect("external target dir");
    tempfile::tempdir_in(path).expect("temp directory")
}

fn pipes(
    count: usize,
) -> (
    DuplexStream,
    DuplexStream,
    Vec<DuplexStream>,
    Vec<DuplexStream>,
) {
    let (left, right) = duplex(64 * 1024);
    let mut sends = Vec::new();
    let mut receives = Vec::new();
    for _ in 0..count {
        let (send, receive) = duplex(64 * 1024);
        sends.push(send);
        receives.push(receive);
    }
    (left, right, sends, receives)
}

async fn round_trip(
    roots: Vec<std::path::PathBuf>,
    config: Config,
) -> (TempDir, Received, Progress, Progress) {
    let data = temporary();
    let engine = FileEngine::new(data.path(), config.clone())
        .await
        .expect("engine");
    let plan = build_manifest(
        roots,
        "test-job".into(),
        "clip".into(),
        config.clone(),
        Cancel::new(),
    )
    .await
    .expect("manifest");
    let (mut left, mut right, sends, receives) = pipes(config.parallel_streams);
    let sent = Progress::new();
    let received = Progress::new();
    let cancel = Cancel::new();
    let (sender, receiver) = tokio::join!(
        engine.send(plan, &mut left, sends, &cancel, &sent),
        engine.receive(
            PEER,
            &mut right,
            receives,
            Consent::Automatic,
            &cancel,
            &received
        ),
    );
    sender.expect("send");
    (data, receiver.expect("receive"), sent, received)
}

fn data(size: usize) -> Vec<u8> {
    let mut seed = 0x9e3779b9u32;
    (0..size)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as u8
        })
        .collect()
}

#[tokio::test]
async fn many_sizes_and_default_chunk_boundaries() {
    let source = temporary();
    let sizes = [
        0,
        1,
        4095,
        4096,
        4097,
        MAX_FILE_CHUNK_BYTES - 1,
        MAX_FILE_CHUNK_BYTES,
        MAX_FILE_CHUNK_BYTES + 1,
    ];
    let mut roots = Vec::new();
    for size in sizes {
        let path = source.path().join(format!("size-{size}.bin"));
        fs::write(&path, data(size)).expect("source");
        roots.push(path);
    }
    let (target, received, sent, progress) = round_trip(roots, Config::default()).await;
    assert_eq!(received.paths.len(), sizes.len());
    for (path, size) in received.paths.iter().zip(sizes) {
        assert_eq!(fs::read(path).expect("received"), data(size));
    }
    assert_eq!(
        sent.snapshot().bytes_done,
        sizes.iter().sum::<usize>() as u64
    );
    assert_eq!(progress.snapshot().bytes_done, sent.snapshot().bytes_done);
    assert!(progress.snapshot().rate_bps > 0);
    assert!(received
        .paths
        .iter()
        .all(|path| path.starts_with(fs::canonicalize(target.path()).expect("canonical data"))));
}

#[tokio::test]
async fn trees_unicode_empty_directories_and_many_files() {
    let source = temporary();
    let root = source.path().join("資料-🌊");
    fs::create_dir(&root).expect("root");
    let mut leaf = root.clone();
    for _ in 0..20 {
        leaf = leaf.join("d");
        fs::create_dir(&leaf).expect("deep directory");
    }
    fs::create_dir(root.join("empty")).expect("empty");
    fs::write(leaf.join("Résumé.txt"), b"unicode payload").expect("file");
    for n in 0..200 {
        fs::write(root.join(format!("{n}.txt")), vec![n as u8; n]).expect("many files");
    }
    let (_target, received, _, _) = round_trip(
        vec![root.clone()],
        Config {
            chunk_size: 4096,
            ..Config::default()
        },
    )
    .await;
    let target = &received.paths[0];
    assert!(target.join("empty").is_dir());
    assert_eq!(
        fs::read(
            target
                .join(leaf.strip_prefix(&root).expect("relative"))
                .join("Résumé.txt")
        )
        .expect("leaf"),
        b"unicode payload"
    );
    for n in 0..200 {
        assert_eq!(
            fs::read(target.join(format!("{n}.txt"))).expect("file"),
            vec![n as u8; n]
        );
    }
}

fn manifest(name: &str, bytes: &[u8]) -> FileManifest {
    FileManifest {
        transfer_id: "attack-test".into(),
        clip_id: "clip".into(),
        chunk_size: MAX_FILE_CHUNK_BYTES as u32,
        page: 0,
        final_page: true,
        files: ManifestFiles::try_from_vec(vec![FileManifestEntry {
            file_id: 0,
            relative_path: name.into(),
            size: bytes.len() as u64,
            is_dir: false,
            blake3_hash: Some(*blake3::hash(bytes).as_bytes()),
        }])
        .expect("bounded"),
    }
}

#[tokio::test]
async fn paged_manifests_are_rejected_and_approval_binds_complete_geometry() {
    let mut first = manifest("a", &[1]);
    first.final_page = false;
    assert!(matches!(
        rejected_manifest(first.clone(), Config::default()).await,
        Error::Invalid("paged manifests unsupported")
    ));
    let mut second = manifest("b", &[2]);
    second.page = 1;
    second.files[0].file_id = 1;
    let consent = Consent::approve_pages([&first, &second]).expect("complete pages");
    assert!(Consent::approve(&first).is_err());
    assert!(Consent::approve_pages([&second, &first]).is_err());
    for changed in 0..4 {
        let mut left = first.clone();
        let mut right = second.clone();
        match changed {
            0 => {
                left.chunk_size /= 2;
                right.chunk_size /= 2;
            }
            1 => {
                left.clip_id.push('x');
                right.clip_id.push('x');
            }
            2 => {
                left.transfer_id.push('x');
                right.transfer_id.push('x');
            }
            _ => right.files[0].blake3_hash = Some([7; 32]),
        }
        assert_ne!(
            consent,
            Consent::approve_pages([&left, &right]).expect("changed complete manifest")
        );
    }
}

async fn rejected_manifest(mut manifest: FileManifest, config: Config) -> Error {
    manifest.chunk_size = config.chunk_size as u32;
    let data = temporary();
    let engine = FileEngine::new(data.path(), config).await.expect("engine");
    let (mut left, mut right, _sends, receives) = pipes(1);
    let cancel = Cancel::new();
    let progress = Progress::new();
    let message = TransferMessage::FileManifest(manifest);
    let (sent, result) = tokio::join!(
        write_message(&mut left, &message, Duration::from_secs(10), &cancel),
        engine.receive(
            PEER,
            &mut right,
            receives,
            Consent::Automatic,
            &cancel,
            &progress
        ),
    );
    sent.expect("send manifest");
    assert_eq!(
        fs::read_dir(data.path().join("glide-xfer"))
            .expect("staging")
            .count(),
        0
    );
    match result {
        Err(error) => error,
        Ok(_) => panic!("accepted malicious manifest"),
    }
}

#[tokio::test]
async fn every_malicious_name_and_traversal_is_rejected() {
    assert!(
        glide_proto::codec::encode_transfer(&TransferMessage::FileManifest(manifest("", &[])))
            .is_err()
    );
    let names = [
        "",
        ".",
        "..",
        "../escape",
        "a/../../b",
        "/absolute",
        "a//b",
        "a/",
        "\\absolute",
        "a\\b",
        "C:foo",
        "foo:bar",
        "con",
        "NUL.txt",
        "PrN",
        "aux.png",
        "COM1",
        "COM9.txt",
        "LPT1",
        "LPT9",
        "COM¹",
        "LPT².txt",
        "CON .txt",
        "COM1 .txt",
        "CONIN$",
        "CONOUT$",
        "CLOCK$",
        "trailing.",
        "trailing ",
        "a\0b",
        "a\nb",
        "a\tb",
        "a<b",
        "a>b",
        "a|b",
        "a?b",
        "a*b",
        "a\"b",
        "foo..bar",
    ];
    for name in names {
        // Empty relative paths are rejected by the existing encoder as well.
        if name.is_empty() {
            continue;
        }
        assert!(
            matches!(
                rejected_manifest(manifest(name, &[]), Config::default()).await,
                Error::Invalid(_) | Error::Limit(_)
            ),
            "{name:?}"
        );
    }
    assert!(matches!(
        rejected_manifest(manifest(&"x".repeat(256), &[]), Config::default()).await,
        Error::Invalid(_)
    ));
}

#[tokio::test]
async fn staging_byte_quota_and_trailing_compressed_data_reject_before_publication() {
    assert!(matches!(
        rejected_manifest(
            manifest("a", &[1]),
            Config {
                max_staging_bytes: 8 << 20,
                max_transfer_bytes: 4 << 20,
                ..Config::default()
            }
        )
        .await,
        Error::Limit("staging byte quota")
    ));
    let config = Config {
        chunk_size: 4096,
        ..Config::default()
    };
    let bytes = vec![0; 4096];
    let mut encoded = zstd::bulk::compress(&bytes, 1).expect("compress");
    encoded.extend_from_slice(b"invalid trailing frame");
    let mut bad = chunk(&bytes, 0, &config);
    bad.data = ChunkBytes::try_from_vec(encoded).expect("bounded");
    bad.compressed = true;
    assert!(matches!(
        malicious_chunk(bad, manifest("a", &bytes), config, false).await,
        Error::Invalid(_)
    ));
}

#[tokio::test]
async fn manifest_limits_collisions_and_consent_fail_before_staging() {
    let mut collision = manifest("a", b"x");
    let mut entry = collision.files[0].clone();
    entry.file_id = 1;
    entry.relative_path = "A".into();
    collision.files.push(entry).expect("push");
    assert!(matches!(
        rejected_manifest(collision, Config::default()).await,
        Error::Invalid(_)
    ));
    assert!(matches!(
        rejected_manifest(manifest("parent/child", b"x"), Config::default()).await,
        Error::Invalid(_)
    ));
    let mut unknown = manifest("a", b"x");
    unknown.files[0].file_id = 8;
    assert!(glide_proto::codec::encode_transfer(&TransferMessage::FileManifest(unknown)).is_err());
    let mut oversize = manifest("a", b"x");
    oversize.files[0].size = u64::MAX;
    assert!(glide_proto::codec::encode_transfer(&TransferMessage::FileManifest(oversize)).is_err());
    let config = Config {
        max_auto_bytes: 0,
        ..Config::default()
    };
    assert!(matches!(
        rejected_manifest(manifest("a", b"x"), config).await,
        Error::ConfirmationRequired { bytes: 1, .. }
    ));
    let config = Config {
        max_transfer_bytes: 0,
        ..Config::default()
    };
    assert!(matches!(
        rejected_manifest(manifest("a", b"x"), config).await,
        Error::Limit(_)
    ));
    let config = Config {
        max_depth: 1,
        ..Config::default()
    };
    assert!(matches!(
        rejected_manifest(manifest("a/b", b"x"), config).await,
        Error::Limit(_)
    ));
    let mut many = manifest("a", &[]);
    let mut entry = many.files[0].clone();
    entry.file_id = 1;
    entry.relative_path = "b".into();
    many.files.push(entry).expect("push");
    assert!(matches!(
        rejected_manifest(
            many,
            Config {
                max_entries: 1,
                ..Config::default()
            }
        )
        .await,
        Error::Limit(_)
    ));
}

fn chunk(bytes: &[u8], index: u32, config: &Config) -> FileChunk {
    let mut encoder = ChunkEncoder::new(config).expect("encoder");
    encoder
        .buffer_mut(bytes.len())
        .expect("buffer")
        .copy_from_slice(bytes);
    encoder
        .encode("attack-test", 0, index, "test.bin")
        .expect("chunk")
}

async fn malicious_chunk(
    chunk: FileChunk,
    mut expected: FileManifest,
    config: Config,
    complete: bool,
) -> Error {
    expected.chunk_size = config.chunk_size as u32;
    let data = temporary();
    let engine = FileEngine::new(data.path(), config.clone())
        .await
        .expect("engine");
    let (mut left, mut right, mut sends, receives) = pipes(1);
    let mut lane = sends.remove(0);
    let cancel = Cancel::new();
    let progress = Progress::new();
    let sending = async {
        write_message(
            &mut left,
            &TransferMessage::FileManifest(expected),
            config.operation_timeout,
            &cancel,
        )
        .await
        .expect("manifest");
        read_message(&mut left, config.operation_timeout, &cancel)
            .await
            .expect("resume");
        write_message(
            &mut lane,
            &TransferMessage::FileChunk(chunk),
            config.operation_timeout,
            &cancel,
        )
        .await
        .expect("chunk");
        if complete {
            let done = TransferMessage::FileComplete(FileComplete {
                transfer_id: "attack-test".into(),
            });
            write_message(&mut lane, &done, config.operation_timeout, &cancel)
                .await
                .expect("lane complete");
            write_message(&mut left, &done, config.operation_timeout, &cancel)
                .await
                .expect("control complete");
        }
    };
    let (_, result) = tokio::join!(
        sending,
        engine.receive(
            PEER,
            &mut right,
            receives,
            Consent::Automatic,
            &cancel,
            &progress
        )
    );
    assert_eq!(
        fs::read_dir(data.path().join("glide-xfer"))
            .expect("staging")
            .count(),
        0,
        "bad peer data must be purged"
    );
    match result {
        Err(error) => error,
        Ok(_) => panic!("accepted invalid chunk"),
    }
}

#[tokio::test]
async fn corruption_whole_file_hash_and_chunk_geometry_reject_and_cleanup() {
    let config = Config {
        chunk_size: 4096,
        ..Config::default()
    };
    let bytes = data(4096);
    let mut corrupt = chunk(&bytes, 0, &config);
    corrupt.data[13] ^= 1;
    assert!(matches!(
        malicious_chunk(corrupt, manifest("a", &bytes), config.clone(), false).await,
        Error::Integrity
    ));
    let mut corrupt = chunk(&bytes, 0, &config);
    corrupt.blake3_hash[0] ^= 1;
    assert!(matches!(
        malicious_chunk(corrupt, manifest("a", &bytes), config.clone(), false).await,
        Error::Integrity
    ));
    let mut wrong_whole = manifest("a", &bytes);
    wrong_whole.files[0].blake3_hash = Some([0; 32]);
    assert!(matches!(
        malicious_chunk(chunk(&bytes, 0, &config), wrong_whole, config.clone(), true).await,
        Error::Integrity
    ));
    let mut bad = chunk(&bytes, 0, &config);
    bad.offset = 1;
    assert!(matches!(
        malicious_chunk(bad, manifest("a", &bytes), config.clone(), false).await,
        Error::Invalid(_)
    ));
    let mut bad = chunk(&bytes, 0, &config);
    bad.chunk_index = 100;
    assert!(matches!(
        malicious_chunk(bad, manifest("a", &bytes), config.clone(), false).await,
        Error::Invalid(_)
    ));
    let mut bad = chunk(&bytes, 0, &config);
    bad.file_id = 100;
    assert!(matches!(
        malicious_chunk(bad, manifest("a", &bytes), config, false).await,
        Error::Invalid(_)
    ));
}

#[tokio::test]
async fn decompression_bomb_and_wrong_size_rejected() {
    let config = Config {
        chunk_size: 4096,
        ..Config::default()
    };
    let bytes = vec![0; 8192];
    let encoded = zstd::bulk::compress(&bytes, 1).expect("compress");
    let bomb = FileChunk {
        transfer_id: "attack-test".into(),
        file_id: 0,
        chunk_index: 0,
        offset: 0,
        data: ChunkBytes::try_from_vec(encoded).expect("bounded"),
        uncompressed_size: 4096,
        compressed: true,
        blake3_hash: *blake3::hash(&bytes[..4096]).as_bytes(),
    };
    assert!(matches!(
        malicious_chunk(bomb, manifest("a", &bytes[..4096]), config.clone(), false).await,
        Error::Invalid(_)
    ));
    let mut short = chunk(&vec![0; 1024], 0, &config);
    short.uncompressed_size = 4096;
    assert!(matches!(
        malicious_chunk(short, manifest("a", &vec![0; 4096]), config, false).await,
        Error::Invalid(_)
    ));
}

#[tokio::test]
async fn compression_decision_and_storage_reuse() {
    let config = Config::default();
    let mut encoder = ChunkEncoder::new(&config).expect("encoder");
    let initial = encoder.buffer_capacity();
    encoder.buffer_mut(64 * 1024).expect("buffer").fill(0);
    let compressed = encoder.encode("id", 0, 0, "data.txt").expect("encode");
    assert!(compressed.compressed);
    assert!(compressed.data.len() < 1024);
    encoder.recycle(compressed);
    assert_eq!(encoder.buffer_capacity(), initial);
    encoder.buffer_mut(64 * 1024).expect("buffer").fill(0);
    let media = encoder.encode("id", 0, 0, "VIDEO.MP4").expect("encode");
    assert!(!media.compressed);
    encoder.recycle(media);
    encoder
        .buffer_mut(64 * 1024)
        .expect("buffer")
        .copy_from_slice(&data(64 * 1024));
    let random = encoder.encode("id", 0, 0, "data.txt").expect("encode");
    assert!(!random.compressed);
    encoder.recycle(random);
    assert_eq!(encoder.buffer_capacity(), initial);
    let (_a, mut b) = duplex(1);
    let cancel = Cancel::new();
    cancel.cancel();
    assert!(matches!(
        read_message(&mut b, Duration::from_secs(1), &cancel).await,
        Err(Error::Cancelled)
    ));
}

#[tokio::test]
async fn timeout_and_oversize_frames_do_not_allocate_payloads() {
    let (mut left, mut right) = duplex(64);
    let cancel = Cancel::new();
    left.write_u32((MAX_FILE_CONTROL_FRAME_BYTES + 1) as u32)
        .await
        .expect("prefix");
    assert!(matches!(
        read_message(&mut right, Duration::from_secs(1), &cancel).await,
        Err(Error::Limit(_))
    ));
    left.write_u32((MAX_FILE_CHUNK_FRAME_BYTES + 1) as u32)
        .await
        .expect("prefix");
    left.write_u8(1).await.expect("variant");
    assert!(matches!(
        read_message(&mut right, Duration::from_secs(1), &cancel).await,
        Err(Error::Invalid(_))
    ));
    left.write_u32(0).await.expect("prefix");
    assert!(matches!(
        read_message(&mut right, Duration::from_secs(1), &cancel).await,
        Err(Error::Limit(_))
    ));
    left.write_u32(1).await.expect("prefix");
    left.write_u8(5).await.expect("variant");
    assert!(matches!(
        read_message(&mut right, Duration::from_secs(1), &cancel).await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        read_message(&mut right, Duration::from_millis(20), &cancel).await,
        Err(Error::Timeout)
    ));
    let trickle = async {
        for _ in 0..4 {
            left.write_u8(0).await.expect("trickle");
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    };
    let (_, result) = tokio::join!(
        trickle,
        read_message(&mut right, Duration::from_millis(20), &cancel)
    );
    assert!(matches!(result, Err(Error::Timeout)));
}

#[tokio::test]
async fn resume_after_random_disconnect_points_and_reverify_modified_disk() {
    let config = Config {
        chunk_size: 4096,
        parallel_streams: 1,
        ..Config::default()
    };
    let bytes = data(config.chunk_size * 8 + 13);
    for cut in [0, 1, 4095, 4096, 4097, 8200, 16031, 27019, 33000] {
        let target = temporary();
        let engine = FileEngine::new(target.path(), config.clone())
            .await
            .expect("engine");
        let mut expected = manifest("resume.bin", &bytes);
        expected.chunk_size = config.chunk_size as u32;
        let (mut left, mut right, mut sends, receives) = pipes(1);
        let mut lane = sends.remove(0);
        let cancel = Cancel::new();
        let progress = Progress::new();
        let sending = async {
            write_message(
                &mut left,
                &TransferMessage::FileManifest(expected.clone()),
                config.operation_timeout,
                &cancel,
            )
            .await
            .expect("manifest");
            read_message(&mut left, config.operation_timeout, &cancel)
                .await
                .expect("resume");
            let mut sent = 0usize;
            for (index, block) in bytes.chunks(config.chunk_size).enumerate() {
                let encoded = glide_proto::codec::encode_transfer(&TransferMessage::FileChunk(
                    chunk(block, index as u32, &config),
                ))
                .expect("codec");
                let mut frame = (encoded.len() as u32).to_be_bytes().to_vec();
                frame.extend(encoded);
                let take = frame.len().min(cut - sent);
                if take > 0 {
                    lane.write_all(&frame[..take]).await.expect("cut write");
                }
                sent += take;
                if take < frame.len() || sent >= cut {
                    break;
                }
            }
            lane.shutdown().await.expect("disconnect");
        };
        let (_, result) = tokio::join!(
            sending,
            engine.receive(
                PEER,
                &mut right,
                receives,
                Consent::Automatic,
                &cancel,
                &progress
            )
        );
        assert!(
            matches!(result, Err(Error::Io(_))),
            "{cut}: {:?}",
            result.err()
        );
        assert_eq!(
            fs::read_dir(target.path().join("glide-xfer"))
                .expect("staging")
                .count(),
            1
        );
        // A retained journal must not hide altered on-disk chunks.
        if cut > 8200 {
            let root = fs::read_dir(target.path().join("glide-xfer"))
                .expect("staging")
                .next()
                .expect("job")
                .expect("entry")
                .path();
            let path = root.join("parts/0.part");
            let mut file = fs::OpenOptions::new().write(true).open(path).expect("part");
            std::io::Write::write_all(&mut file, &[0xff]).expect("modify");
        }
        let source = temporary();
        let path = source.path().join("resume.bin");
        fs::write(&path, &bytes).expect("source");
        let plan = build_manifest(
            vec![path],
            "attack-test".into(),
            "clip".into(),
            config.clone(),
            cancel.clone(),
        )
        .await
        .expect("manifest");
        let (mut left, mut right, sends, receives) = pipes(1);
        let sent = Progress::new();
        let received = Progress::new();
        let (a, b) = tokio::join!(
            engine.send(plan, &mut left, sends, &cancel, &sent),
            engine.receive(
                PEER,
                &mut right,
                receives,
                Consent::Automatic,
                &cancel,
                &received
            )
        );
        a.expect("resumed sender");
        let b = b.expect("resumed receiver");
        assert_eq!(fs::read(&b.paths[0]).expect("result"), bytes);
        assert_eq!(received.snapshot().bytes_done, bytes.len() as u64);
    }
}

#[tokio::test]
async fn cancellation_cleanup_concurrency_and_rate_limit() {
    let config = Config {
        chunk_size: 4096,
        parallel_streams: 1,
        max_concurrent_transfers: 1,
        ..Config::default()
    };
    let target = temporary();
    let engine = FileEngine::new(target.path(), config.clone())
        .await
        .expect("engine");
    let (mut left, mut right, _sends, receives) = pipes(1);
    let cancel = Cancel::new();
    let progress = Progress::new();
    let receiving = engine.receive(
        PEER,
        &mut right,
        receives,
        Consent::Automatic,
        &cancel,
        &progress,
    );
    let orchestration = async {
        let mut announced = manifest("a", &data(4096));
        announced.chunk_size = config.chunk_size as u32;
        write_message(
            &mut left,
            &TransferMessage::FileManifest(announced),
            config.operation_timeout,
            &cancel,
        )
        .await
        .expect("manifest");
        read_message(&mut left, config.operation_timeout, &cancel)
            .await
            .expect("resume");
        let (mut other_left, mut other_right, _other_sends, other_receives) = pipes(1);
        assert!(matches!(
            engine
                .receive(
                    PEER,
                    &mut other_right,
                    other_receives,
                    Consent::Automatic,
                    &Cancel::new(),
                    &Progress::new()
                )
                .await,
            Err(Error::Busy)
        ));
        other_left.shutdown().await.expect("shutdown");
        cancel.cancel();
    };
    let (_, result) = tokio::join!(orchestration, receiving);
    assert!(matches!(result, Err(Error::Cancelled)));
    assert_eq!(
        fs::read_dir(target.path().join("glide-xfer"))
            .expect("staging")
            .count(),
        0
    );
    let source = temporary();
    let path = source.path().join("rate.bin");
    fs::write(&path, data(4096)).expect("source");
    let started = std::time::Instant::now();
    let (_target, _received, _, _) = round_trip(
        vec![path],
        Config {
            rate_limit_bps: Some(8192),
            parallel_streams: 2,
            chunk_size: 1024,
            ..Config::default()
        },
    )
    .await;
    assert!(started.elapsed() >= Duration::from_millis(450));
}

#[tokio::test]
async fn source_limits_and_disk_space_fail_closed() {
    let source = temporary();
    let root = source.path().join("root");
    fs::create_dir(&root).expect("root");
    for i in 0..5 {
        fs::write(root.join(format!("{i}")), [0]).expect("source");
    }
    assert!(matches!(
        build_manifest(
            vec![root.clone()],
            "a".into(),
            "b".into(),
            Config {
                max_entries: 3,
                ..Config::default()
            },
            Cancel::new()
        )
        .await,
        Err(Error::Limit(_))
    ));
    fs::create_dir(root.join("d")).expect("directory");
    fs::write(root.join("d/x"), [0]).expect("file");
    assert!(matches!(
        build_manifest(
            vec![root],
            "a".into(),
            "b".into(),
            Config {
                max_depth: 1,
                ..Config::default()
            },
            Cancel::new()
        )
        .await,
        Err(Error::Limit(_))
    ));
    assert!(matches!(
        rejected_manifest(
            manifest("a", &[0]),
            Config {
                disk_reserve_bytes: u64::MAX,
                ..Config::default()
            }
        )
        .await,
        Error::DiskSpace
    ));
    assert!(Config {
        chunk_size: 0,
        ..Config::default()
    }
    .validate()
    .is_err());
    assert!(Config {
        chunk_size: MAX_FILE_CHUNK_BYTES + 1,
        ..Config::default()
    }
    .validate()
    .is_err());
    assert!(Config {
        parallel_streams: 5,
        ..Config::default()
    }
    .validate()
    .is_err());
    assert!(Config {
        rate_limit_bps: Some(0),
        ..Config::default()
    }
    .validate()
    .is_err());
    assert!(Config {
        rate_limit_bps: Some(1),
        ..Config::default()
    }
    .validate()
    .is_err());
    assert!(Config {
        rate_limit_bps: Some(1),
        chunk_size: 1,
        ..Config::default()
    }
    .validate()
    .is_ok());
    assert!(Config::default().payload_memory_ceiling() < 192 * 1024 * 1024);
}

#[tokio::test]
async fn approval_is_bound_to_exact_manifest_and_can_resume_after_prompt() {
    let config = Config {
        max_auto_bytes: 0,
        parallel_streams: 1,
        ..Config::default()
    };
    let pending = rejected_manifest(manifest("a", &[1]), config.clone()).await;
    let pending = match pending {
        Error::ConfirmationRequired { bytes: 1, manifest } => *manifest,
        other => panic!("unexpected prompt: {other}"),
    };
    let consent = Consent::approve(&pending).expect("bind approval");
    let target = temporary();
    let source = temporary();
    let path = source.path().join("a");
    fs::write(&path, [1]).expect("source");
    let engine = FileEngine::new(target.path(), config.clone())
        .await
        .expect("engine");
    let plan = build_manifest(
        vec![path],
        "attack-test".into(),
        "clip".into(),
        config.clone(),
        Cancel::new(),
    )
    .await
    .expect("plan");
    let (mut left, mut right, sends, receives) = pipes(1);
    let cancel = Cancel::new();
    let sent = Progress::new();
    let progress = Progress::new();
    let (send, receive) = tokio::join!(
        engine.send(plan, &mut left, sends, &cancel, &sent),
        engine.receive(PEER, &mut right, receives, consent, &cancel, &progress)
    );
    send.expect("approved send");
    let receive = receive.expect("approved receive");
    assert_eq!(fs::read(&receive.paths[0]).expect("data"), [1]);
    let new_target = temporary();
    let engine = FileEngine::new(new_target.path(), config)
        .await
        .expect("engine");
    let (mut left, mut right, _sends, receives) = pipes(1);
    let changed = TransferMessage::FileManifest(manifest("renamed", &[1]));
    let (send, receive) = tokio::join!(
        write_message(&mut left, &changed, Duration::from_secs(1), &cancel),
        engine.receive(PEER, &mut right, receives, consent, &cancel, &progress)
    );
    send.expect("changed manifest");
    assert!(matches!(
        receive,
        Err(Error::Invalid("approval manifest mismatch"))
    ));
    assert_eq!(
        fs::read_dir(new_target.path().join("glide-xfer"))
            .expect("staging")
            .count(),
        0
    );
}

#[tokio::test]
async fn dropping_receiver_future_cleans_up_and_peer_cancel_is_honoured() {
    let target = temporary();
    let engine = FileEngine::new(target.path(), Config::default())
        .await
        .expect("engine");
    let (mut left, mut right, sends, receives) = pipes(1);
    let running = engine.clone();
    let task = tokio::spawn(async move {
        running
            .receive(
                PEER,
                &mut right,
                receives,
                Consent::Automatic,
                &Cancel::new(),
                &Progress::new(),
            )
            .await
    });
    let cancel = Cancel::new();
    write_message(
        &mut left,
        &TransferMessage::FileManifest(manifest("a", &[1])),
        Duration::from_secs(1),
        &cancel,
    )
    .await
    .expect("manifest");
    read_message(&mut left, Duration::from_secs(1), &cancel)
        .await
        .expect("resume");
    task.abort();
    let _ = task.await;
    drop(sends);
    assert_eq!(
        fs::read_dir(target.path().join("glide-xfer"))
            .expect("staging")
            .count(),
        0
    );
    let (mut left, mut right, sends, receives) = pipes(1);
    let progress = Progress::new();
    let send = async {
        write_message(
            &mut left,
            &TransferMessage::FileManifest(manifest("a", &[1])),
            Duration::from_secs(1),
            &cancel,
        )
        .await
        .expect("manifest");
        read_message(&mut left, Duration::from_secs(1), &cancel)
            .await
            .expect("resume");
        write_message(
            &mut left,
            &TransferMessage::FileCancel(FileCancel {
                transfer_id: "attack-test".into(),
            }),
            Duration::from_secs(1),
            &cancel,
        )
        .await
        .expect("cancel");
    };
    let (_, result) = tokio::join!(
        send,
        engine.receive(
            PEER,
            &mut right,
            receives,
            Consent::Automatic,
            &cancel,
            &progress
        )
    );
    assert!(matches!(result, Err(Error::Cancelled)));
    assert_eq!(
        fs::read_dir(target.path().join("glide-xfer"))
            .expect("staging")
            .count(),
        0
    );
    drop(sends);
}

#[cfg(windows)]
#[tokio::test]
async fn source_junctions_and_staging_reparse_points_are_rejected() {
    let source = temporary();
    let original = source.path().join("original");
    fs::create_dir(&original).expect("original");
    fs::write(original.join("secret"), b"secret").expect("file");
    let junction = source.path().join("junction");
    let result = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        // mklink interprets '/' in otherwise valid Cargo target paths as switches.
        .arg(
            junction
                .to_str()
                .expect("temporary junction path")
                .replace('/', "\\"),
        )
        .arg(
            original
                .to_str()
                .expect("temporary source path")
                .replace('/', "\\"),
        )
        .output()
        .expect("mklink");
    assert!(
        result.status.success(),
        "junction setup: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(matches!(
        build_manifest(
            vec![junction.clone()],
            "a".into(),
            "b".into(),
            Config::default(),
            Cancel::new()
        )
        .await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        build_manifest(
            vec![junction.join("secret")],
            "a".into(),
            "b".into(),
            Config::default(),
            Cancel::new()
        )
        .await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        FileEngine::new(&junction, Config::default()).await,
        Err(Error::Invalid(_))
    ));
    // Remove the junction itself, preserving its target; never recursively delete it.
    fs::remove_dir(&junction).expect("junction removal");
    assert_eq!(
        fs::read(original.join("secret")).expect("preserved"),
        b"secret"
    );
}
