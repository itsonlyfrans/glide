//! Foundation baseline; not a native input, QUIC, or file-transfer acceptance benchmark.
use glide_daemon::layout::Desktop;
use glide_platform::{Monitor, Point};
use glide_proto::{
    codec::{decode_move, encode_move_into},
    ipc::{Layout, LayoutDevice},
    wire::{Move, MAX_MOVE_FRAME_BYTES},
};
use std::{collections::HashMap, hint::black_box, time::Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let layout = Layout {
        devices: vec![LayoutDevice {
            device_id: "bench".into(),
            x: 0.0,
            y: 0.0,
        }],
    };
    let monitors = HashMap::from([(
        "bench".into(),
        vec![Monitor {
            id: "display".into(),
            x: 0.0,
            y: 0.0,
            w: 1920.0,
            h: 1080.0,
            scale: 1.0,
            primary: true,
        }],
    )]);
    let desktop = Desktop::from_layout(&layout, &monitors)?;
    let mut samples = Vec::with_capacity(20_000);
    let mut buffer = [0; MAX_MOVE_FRAME_BYTES];
    let mut cursor = Point { x: 500.0, y: 500.0 };
    for seq in 0..20_000 {
        let start = Instant::now();
        cursor = desktop
            .move_cursor(
                cursor,
                Point {
                    x: if seq % 2 == 0 { 1.0 } else { -1.0 },
                    y: 0.0,
                },
            )
            .position;
        let bytes = encode_move_into(
            &Move {
                seq,
                x: cursor.x,
                y: cursor.y,
            },
            &mut buffer,
        )?;
        black_box(decode_move(bytes)?);
        samples.push(start.elapsed().as_nanos());
    }
    samples.sort_unstable();
    println!(
        "geometry + mouse codec p99: {} ns (native capture/injection and network excluded)",
        samples[samples.len() * 99 / 100]
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    const CHUNK: usize = 4 * 1024 * 1024;
    const CHUNKS: usize = 64;
    let sender = tokio::spawn(async move {
        let mut socket = tokio::net::TcpStream::connect(address).await?;
        let chunk = vec![0x5a; CHUNK];
        for _ in 0..CHUNKS {
            socket.write_all(&chunk).await?;
        }
        socket.shutdown().await
    });
    let (mut socket, _) = listener.accept().await?;
    let mut buffer = vec![0; CHUNK];
    let start = Instant::now();
    for _ in 0..CHUNKS {
        socket.read_exact(&mut buffer).await?;
        anyhow::ensure!(
            buffer.iter().all(|byte| *byte == 0x5a),
            "loopback data mismatch"
        );
    }
    sender.await??;
    println!(
        "verified raw TCP loopback: {:.2} GB/s (QUIC, compression and disk excluded)",
        (CHUNK * CHUNKS) as f64 / start.elapsed().as_secs_f64() / 1e9
    );
    Ok(())
}
