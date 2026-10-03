#[cfg(feature = "file-engine")]
mod support;

#[cfg(feature = "file-engine")]
#[tokio::main]
async fn main() -> glide_xfer::Result<()> {
    use glide_xfer::Error;
    use tokio::net::{TcpListener, TcpStream};
    let bytes = match std::env::args().nth(1) {
        Some(value) => value
            .parse::<u64>()
            .map_err(|_| Error::Invalid("benchmark byte count"))?,
        None => 1 << 30,
    };
    if std::env::var_os("GLIDE_XFER_PROFILE").is_some() {
        let block = support::block(glide_xfer::Config::default().chunk_size, false);
        let mut destination = vec![0; block.len()];
        let count = bytes.div_ceil(block.len() as u64);
        let measured = count
            .checked_mul(block.len() as u64)
            .ok_or(Error::Limit("benchmark byte count"))?;
        let started = std::time::Instant::now();
        for _ in 0..count {
            std::hint::black_box(blake3::hash(std::hint::black_box(&block)));
        }
        println!(
            "profile: BLAKE3 chunk hash {:.3} GB/s",
            measured as f64 / started.elapsed().as_secs_f64() / 1e9
        );
        let started = std::time::Instant::now();
        let mut whole = blake3::Hasher::new();
        for _ in 0..count {
            whole.update(std::hint::black_box(&block));
        }
        std::hint::black_box(whole.finalize());
        println!(
            "profile: BLAKE3 whole-file hash {:.3} GB/s",
            measured as f64 / started.elapsed().as_secs_f64() / 1e9
        );
        let started = std::time::Instant::now();
        for _ in 0..count {
            destination.copy_from_slice(std::hint::black_box(&block));
            std::hint::black_box(&destination);
        }
        println!(
            "profile: one memcpy {:.3} GB/s",
            measured as f64 / started.elapsed().as_secs_f64() / 1e9
        );
    }
    for compressible in [false, true] {
        let (left, right) = tokio::io::duplex(8 * 1024 * 1024);
        let rate = support::framed_stream(left, right, bytes, compressible).await?;
        println!("framed duplex: bytes={bytes}, compressible={compressible}, {rate:.3} GB/s");
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let (left, right) = tokio::join!(TcpStream::connect(address), listener.accept());
        let left = left?;
        let right = right?.0;
        left.set_nodelay(true)?;
        right.set_nodelay(true)?;
        let rate = support::framed_stream(left, right, bytes, compressible).await?;
        println!("framed loopback TCP: bytes={bytes}, compressible={compressible}, {rate:.3} GB/s");
        let (left, right) = tokio::io::duplex(64 * 1024);
        let mut sends = Vec::new();
        let mut receives = Vec::new();
        for _ in 0..4 {
            let (left, right) = tokio::io::duplex(1024 * 1024);
            sends.push(left);
            receives.push(right);
        }
        let rate =
            support::engine_stream(left, right, sends, receives, bytes, compressible).await?;
        println!("engine duplex (4 lanes, disk staging): bytes={bytes}, compressible={compressible}, {rate:.3} GB/s");
        async fn tcp_pair() -> glide_xfer::Result<(TcpStream, TcpStream)> {
            let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
            let (left, right) = tokio::join!(
                TcpStream::connect(listener.local_addr()?),
                listener.accept()
            );
            let left = left?;
            let right = right?.0;
            left.set_nodelay(true)?;
            right.set_nodelay(true)?;
            Ok((left, right))
        }
        let (left, right) = tcp_pair().await?;
        let mut sends = Vec::new();
        let mut receives = Vec::new();
        for _ in 0..4 {
            let (left, right) = tcp_pair().await?;
            sends.push(left);
            receives.push(right);
        }
        let rate =
            support::engine_stream(left, right, sends, receives, bytes, compressible).await?;
        println!("engine loopback TCP (4 lanes, disk staging): bytes={bytes}, compressible={compressible}, {rate:.3} GB/s");
    }
    Ok(())
}

#[cfg(not(feature = "file-engine"))]
fn main() {}
