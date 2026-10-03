//! Native transport implementation. No mock identity enters these constructors.

mod discovery;
mod identity;
mod link;
mod manager;
mod pairing;
mod tls;
mod transfer;

pub use identity::NativeIdentity;
#[cfg(feature = "test-support")]
pub use identity::TestKeyStore;
pub use link::NativeLink;
pub use manager::{NativePeerManager, PairingRepair};
pub use tls::PinSet;
pub use transfer::{TransferHandle, TransferIo, TransferStreams};

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use glide_platform::{Monitor, Os};
use glide_proto::{codec, wire};

pub(crate) const MAX_PEERS: usize = 32;
pub(crate) const IO_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(500);
pub(crate) const HEARTBEAT_TIMEOUT: Duration = Duration::from_millis(1500);
pub(crate) const PAIR_ALPN: &[u8] = wire::PAIR_ALPN;

/// Native initialization failures. These messages never contain key material.
#[derive(Debug, thiserror::Error)]
pub enum NativeError {
    #[error("network storage or socket error: {0}")]
    Io(#[from] std::io::Error),
    #[error("security initialization failed: {0}")]
    Security(String),
    #[error("network initialization failed: {0}")]
    Internal(String),
}

/// Explicit settings for the native network boundary.
///
/// `discovery` corresponds to `settings.network.discovery`. False creates no
/// mDNS service or browser. `bind_addr` port zero is useful for loopback tests.
#[derive(Clone, Debug)]
pub struct NativeConfig {
    pub data_dir: PathBuf,
    pub bind_addr: SocketAddr,
    pub name: String,
    pub os: Os,
    pub monitors: Vec<Monitor>,
    pub discovery: bool,
}

impl NativeConfig {
    fn hello(&self, identity: &NativeIdentity) -> Result<wire::Hello, NativeError> {
        if self.name.is_empty() || self.name.len() > 128 || self.name.chars().any(char::is_control)
        {
            return Err(NativeError::Internal(
                "device name must be 1..128 bytes without controls".into(),
            ));
        }
        let monitors = wire::Monitors::try_from_vec(self.monitors.clone())
            .map_err(|_| NativeError::Internal("too many monitors".into()))?;
        let hello = wire::Hello {
            proto_version: wire::PROTOCOL_VERSION,
            device_id: identity.device_id().to_owned(),
            name: self.name.clone(),
            os: self.os,
            monitors,
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
        };
        codec::encode_control(&wire::ControlMessage::Hello(hello.clone()))
            .map_err(|_| NativeError::Internal("invalid local monitor metadata".into()))?;
        Ok(hello)
    }
}

pub(crate) fn transport_config() -> std::sync::Arc<quinn::TransportConfig> {
    let mut transport = quinn::TransportConfig::default();
    transport
        .max_concurrent_bidi_streams(21u32.into())
        .max_concurrent_uni_streams(8u32.into())
        .stream_receive_window((256u32 * 1024).into())
        .receive_window((8u32 * 1024 * 1024).into())
        .send_window(8 * 1024 * 1024)
        .max_idle_timeout(Some(quinn::IdleTimeout::from(quinn::VarInt::from_u32(
            5000,
        ))))
        .keep_alive_interval(Some(HEARTBEAT_INTERVAL))
        .crypto_buffer_size(16 * 1024)
        .datagram_receive_buffer_size(Some(8 * wire::MAX_MOVE_FRAME_BYTES))
        // Quinn charges one Bytes header per queued datagram. A Move is at most
        // ten sequence bytes + two f64s, so this admits exactly one current move.
        .datagram_send_buffer_size(std::mem::size_of::<bytes::Bytes>() + 26)
        .congestion_controller_factory(std::sync::Arc::new(
            quinn::congestion::CubicConfig::default(),
        ));
    std::sync::Arc::new(transport)
}

pub(crate) fn mutex<T>(
    value: &std::sync::Mutex<T>,
) -> Result<std::sync::MutexGuard<'_, T>, crate::LinkError> {
    value
        .lock()
        .map_err(|_| crate::LinkError::Internal("transport state poisoned".into()))
}

pub(crate) fn ipc(code: glide_proto::ipc::ErrorCode, message: &str) -> glide_proto::ipc::IpcError {
    glide_proto::ipc::IpcError::new(code, message)
}

pub(crate) fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            duration.as_millis().min(u64::MAX as u128) as u64
        })
}

#[cfg(test)]
mod tests;
