use glide_xfer::*;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};

pub fn block(size: usize, compressible: bool) -> Vec<u8> {
    let mut data = vec![0; size];
    if !compressible {
        let mut seed = 0x9e3779b9u32;
        for byte in &mut data {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            *byte = seed as u8;
        }
    }
    data
}

/// Protocol/CPU benchmark: synthetic source and verified sink, no filesystem, TLS or QUIC.
/// Fixed source, encoder and decompression buffers; length does not change live storage.
pub async fn framed_stream<W, R>(
    mut writer: W,
    mut reader: R,
    bytes: u64,
    compressible: bool,
) -> Result<f64>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    let config = Config::default();
    let block = block(config.chunk_size, compressible);
    let mut expected = blake3::Hasher::new();
    let chunks = bytes.div_ceil(config.chunk_size as u64);
    if chunks > u32::MAX as u64 {
        return Err(Error::Limit("synthetic chunks"));
    }
    for index in 0..chunks {
        let size =
            (bytes - index * config.chunk_size as u64).min(config.chunk_size as u64) as usize;
        expected.update(&block[..size]);
    }
    let expected = *expected.finalize().as_bytes();
    let cancel = Cancel::new();
    let mut encoder = ChunkEncoder::new(&config)?;
    let capacity = encoder.buffer_capacity();
    let mut decoder = zstd::bulk::Decompressor::new()?;
    decoder.window_log_max(22)?;
    let mut output = vec![0; config.chunk_size];
    let started = Instant::now();
    let sending = async {
        for index in 0..chunks {
            let size =
                (bytes - index * config.chunk_size as u64).min(config.chunk_size as u64) as usize;
            encoder.buffer_mut(size)?.copy_from_slice(&block[..size]);
            let prepared = encoder.prepare("data.bin")?;
            let chunk = encoder.view("synthetic", 0, index as u32, prepared)?;
            write_file_chunk(&mut writer, &chunk, Duration::from_secs(30), &cancel).await?;
            if encoder.buffer_capacity() != capacity {
                return Err(Error::Limit("encoder storage grew"));
            }
        }
        Ok::<_, Error>(())
    };
    let receiving = async {
        let mut hash = blake3::Hasher::new();
        let mut received = 0u64;
        let mut chunk_reader = ChunkReader::new(config.chunk_size)?;
        for index in 0..chunks {
            let ChunkMessage::Chunk(chunk) = chunk_reader
                .read(&mut reader, Duration::from_secs(30), &cancel)
                .await?
            else {
                return Err(Error::Invalid("synthetic message"));
            };
            let size = (bytes - received).min(config.chunk_size as u64) as usize;
            if chunk.transfer_id != "synthetic"
                || chunk.file_id != 0
                || chunk.chunk_index != index as u32
                || chunk.offset != received
                || chunk.uncompressed_size as usize != size
            {
                return Err(Error::Invalid("synthetic geometry"));
            }
            let bytes: &[u8] = if chunk.compressed {
                if decoder.decompress_to_buffer(chunk.data, &mut output[..size])? != size {
                    return Err(Error::Integrity);
                }
                &output[..size]
            } else {
                chunk.data
            };
            if bytes.len() != size || *blake3::hash(bytes).as_bytes() != chunk.blake3_hash {
                return Err(Error::Integrity);
            }
            hash.update(bytes);
            received += size as u64;
        }
        if received != bytes || *hash.finalize().as_bytes() != expected {
            return Err(Error::Integrity);
        }
        Ok::<_, Error>(())
    };
    let (sent, received) = tokio::join!(sending, receiving);
    sent?;
    received?;
    Ok(bytes as f64 / started.elapsed().as_secs_f64() / 1e9)
}

/// Full engine benchmark: disk-backed source, parallel lanes, journal, whole-file
/// verification, sync and atomic publication. Source generation/prehash are untimed.
pub async fn engine_stream<C, W, R>(
    mut send_control: C,
    mut receive_control: C,
    sends: Vec<W>,
    receives: Vec<R>,
    bytes: u64,
    compressible: bool,
) -> Result<f64>
where
    C: AsyncRead + AsyncWrite + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
    R: AsyncRead + Unpin + Send + 'static,
{
    use std::io::Write;
    let target =
        std::env::var_os("CARGO_TARGET_DIR").ok_or(Error::Invalid("external target directory"))?;
    let source = tempfile::tempdir_in(&target)?;
    let data = tempfile::tempdir_in(&target)?;
    let config = Config::default();
    let block = block(config.chunk_size, compressible);
    let path = source.path().join("benchmark.bin");
    let mut file = std::fs::File::create(&path)?;
    let mut remaining = bytes;
    while remaining > 0 {
        let count = remaining.min(block.len() as u64) as usize;
        file.write_all(&block[..count])?;
        remaining -= count as u64;
    }
    file.sync_all()?;
    drop(file);
    drop(block);
    let cancel = Cancel::new();
    let plan = build_manifest(
        vec![path],
        "benchmark".into(),
        "clip".into(),
        config.clone(),
        cancel.clone(),
    )
    .await?;
    let engine = FileEngine::new(data.path(), config).await?;
    let sent = Progress::new();
    let received = Progress::new();
    let started = Instant::now();
    let (send, receive) = tokio::join!(
        engine.send(plan, &mut send_control, sends, &cancel, &sent),
        engine.receive(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            &mut receive_control,
            receives,
            Consent::Automatic,
            &cancel,
            &received
        ),
    );
    send?;
    let _published = receive?;
    if sent.snapshot().bytes_done != bytes || received.snapshot().bytes_done != bytes {
        return Err(Error::Integrity);
    }
    Ok(bytes as f64 / started.elapsed().as_secs_f64() / 1e9)
}
