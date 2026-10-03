use super::{NORMAL_ALPN, PAIRING_ALPN};
use quinn_proto::{
    crypto::{self, Session},
    transport_parameters::TransportParameters,
    ConnectionId, Side, TransportError, TransportErrorCode,
};
use std::{any::Any, sync::Arc};

const MAX_CLIENT_HELLO: usize = 16 * 1024;

pub(super) struct Dispatch {
    pub normal: Arc<dyn crypto::ServerConfig>,
    pub pairing: Arc<dyn crypto::ServerConfig>,
}

impl crypto::ServerConfig for Dispatch {
    fn initial_keys(
        &self,
        version: u32,
        cid: &ConnectionId,
    ) -> Result<crypto::Keys, crypto::UnsupportedVersion> {
        self.normal.initial_keys(version, cid)
    }
    fn retry_tag(&self, version: u32, cid: &ConnectionId, packet: &[u8]) -> [u8; 16] {
        self.normal.retry_tag(version, cid, packet)
    }
    fn start_session(
        self: Arc<Self>,
        version: u32,
        params: &TransportParameters,
    ) -> Box<dyn Session> {
        Box::new(DispatchSession {
            inner: self.normal.clone().start_session(version, params),
            pairing: Some(self.pairing.clone().start_session(version, params)),
            hello: Vec::new(),
            selected: false,
        })
    }
}

struct DispatchSession {
    inner: Box<dyn Session>,
    pairing: Option<Box<dyn Session>>,
    hello: Vec<u8>,
    selected: bool,
}

impl Session for DispatchSession {
    fn initial_keys(&self, cid: &ConnectionId, side: Side) -> crypto::Keys {
        self.inner.initial_keys(cid, side)
    }
    fn handshake_data(&self) -> Option<Box<dyn Any>> {
        self.inner.handshake_data()
    }
    fn peer_identity(&self) -> Option<Box<dyn Any>> {
        self.inner.peer_identity()
    }
    fn early_crypto(&self) -> Option<(Box<dyn crypto::HeaderKey>, Box<dyn crypto::PacketKey>)> {
        None
    }
    fn early_data_accepted(&self) -> Option<bool> {
        Some(false)
    }
    fn is_handshaking(&self) -> bool {
        self.inner.is_handshaking()
    }
    fn read_handshake(&mut self, bytes: &[u8]) -> Result<bool, TransportError> {
        if self.selected {
            return self.inner.read_handshake(bytes);
        }
        if bytes.len() > MAX_CLIENT_HELLO.saturating_sub(self.hello.len()) {
            return Err(invalid_hello());
        }
        self.hello.extend_from_slice(bytes);
        let Some(pairing) = select_alpn(&self.hello)? else {
            return Ok(false);
        };
        if pairing {
            self.inner = self.pairing.take().ok_or_else(invalid_hello)?;
        }
        self.pairing = None;
        self.selected = true;
        let result = self.inner.read_handshake(&self.hello);
        self.hello = Vec::new();
        result
    }
    fn transport_parameters(&self) -> Result<Option<TransportParameters>, TransportError> {
        self.inner.transport_parameters()
    }
    fn write_handshake(&mut self, bytes: &mut Vec<u8>) -> Option<crypto::Keys> {
        if self.selected {
            self.inner.write_handshake(bytes)
        } else {
            None
        }
    }
    fn next_1rtt_keys(&mut self) -> Option<crypto::KeyPair<Box<dyn crypto::PacketKey>>> {
        self.inner.next_1rtt_keys()
    }
    fn is_valid_retry(&self, cid: &ConnectionId, header: &[u8], payload: &[u8]) -> bool {
        self.inner.is_valid_retry(cid, header, payload)
    }
    fn export_keying_material(
        &self,
        output: &mut [u8],
        label: &[u8],
        context: &[u8],
    ) -> Result<(), crypto::ExportKeyingMaterialError> {
        self.inner.export_keying_material(output, label, context)
    }
}

fn invalid_hello() -> TransportError {
    TransportError {
        code: TransportErrorCode::crypto(47),
        frame: None,
        reason: "invalid or ambiguous ClientHello ALPN".into(),
    }
}

fn take<'a>(bytes: &mut &'a [u8], length: usize) -> Result<&'a [u8], TransportError> {
    let value = bytes.get(..length).ok_or_else(invalid_hello)?;
    *bytes = &bytes[length..];
    Ok(value)
}
fn length16(bytes: &mut &[u8]) -> Result<usize, TransportError> {
    let value = take(bytes, 2)?;
    Ok(usize::from(u16::from_be_bytes([value[0], value[1]])))
}
fn vector16<'a>(bytes: &mut &'a [u8]) -> Result<&'a [u8], TransportError> {
    let length = length16(bytes)?;
    take(bytes, length)
}

/// QUIC CRYPTO carries handshake bytes without TLS record headers. Only the
/// bounded first ClientHello is inspected; rustls validates the complete TLS
/// structure and authenticates the transcript after selection.
fn select_alpn(bytes: &[u8]) -> Result<Option<bool>, TransportError> {
    if bytes.first().is_some_and(|tag| *tag != 1) {
        return Err(invalid_hello());
    }
    if bytes.len() < 4 {
        return Ok(None);
    }
    let length =
        (usize::from(bytes[1]) << 16) | (usize::from(bytes[2]) << 8) | usize::from(bytes[3]);
    if length + 4 > MAX_CLIENT_HELLO {
        return Err(invalid_hello());
    }
    let Some(mut hello) = bytes.get(4..4 + length) else {
        return Ok(None);
    };
    if take(&mut hello, 2)? != [3, 3] {
        return Err(invalid_hello());
    }
    take(&mut hello, 32)?; // random
    let session_id = usize::from(take(&mut hello, 1)?[0]);
    if session_id > 32 {
        return Err(invalid_hello());
    }
    take(&mut hello, session_id)?;
    let ciphers = vector16(&mut hello)?;
    if ciphers.is_empty() || ciphers.len() % 2 != 0 {
        return Err(invalid_hello());
    }
    let compression_len = usize::from(take(&mut hello, 1)?[0]);
    if take(&mut hello, compression_len)? != [0] {
        return Err(invalid_hello());
    }
    let mut extensions = vector16(&mut hello)?;
    if !hello.is_empty() {
        return Err(invalid_hello());
    }
    let mut selected = None;
    while !extensions.is_empty() {
        let tag = length16(&mut extensions)?;
        let extension = vector16(&mut extensions)?;
        if tag != 16 {
            continue;
        }
        if selected.is_some() {
            return Err(invalid_hello());
        }
        let mut extension = extension;
        let mut protocols = vector16(&mut extension)?;
        if !extension.is_empty() {
            return Err(invalid_hello());
        }
        let protocol_len = usize::from(take(&mut protocols, 1)?[0]);
        let protocol = take(&mut protocols, protocol_len)?;
        // Refuse mixed lists, duplicates and unknown protocols. A permissive
        // pairing verifier can never negotiate the normal protocol.
        if !protocols.is_empty() {
            return Err(invalid_hello());
        }
        selected = Some(match protocol {
            NORMAL_ALPN => false,
            PAIRING_ALPN => true,
            _ => return Err(invalid_hello()),
        });
    }
    selected.map(Some).ok_or_else(invalid_hello)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn hello(protocols: &[&[u8]]) -> Vec<u8> {
        let mut body = vec![3, 3];
        body.extend_from_slice(&[0; 32]);
        body.extend_from_slice(&[0, 0, 2, 0x13, 1, 1, 0]);
        let mut extensions = Vec::new();
        if !protocols.is_empty() {
            let mut list = Vec::new();
            for protocol in protocols {
                list.push(protocol.len() as u8);
                list.extend_from_slice(protocol);
            }
            extensions.extend_from_slice(&[0, 16]);
            extensions.extend_from_slice(&((list.len() + 2) as u16).to_be_bytes());
            extensions.extend_from_slice(&(list.len() as u16).to_be_bytes());
            extensions.extend_from_slice(&list);
        }
        body.extend_from_slice(&(extensions.len() as u16).to_be_bytes());
        body.extend_from_slice(&extensions);
        let mut bytes = vec![1, 0, (body.len() >> 8) as u8, body.len() as u8];
        bytes.extend_from_slice(&body);
        bytes
    }
    #[test]
    fn exact_alpn_selection_is_incremental_and_fails_closed() {
        for (protocol, pairing) in [(NORMAL_ALPN, false), (PAIRING_ALPN, true)] {
            let bytes = hello(&[protocol]);
            for count in 0..bytes.len() {
                assert_eq!(select_alpn(&bytes[..count]).expect("partial"), None);
            }
            assert_eq!(select_alpn(&bytes).expect("hello"), Some(pairing));
        }
        for protocols in [
            vec![],
            vec![b"unknown".as_slice()],
            vec![NORMAL_ALPN, PAIRING_ALPN],
            vec![PAIRING_ALPN, NORMAL_ALPN],
            vec![NORMAL_ALPN, NORMAL_ALPN],
            vec![b"".as_slice()],
        ] {
            assert!(select_alpn(&hello(&protocols)).is_err());
        }
        let mut malformed = hello(&[NORMAL_ALPN]);
        malformed[4] = 0;
        assert!(select_alpn(&malformed).is_err());
        assert!(select_alpn(&[1, 255, 255, 255]).is_err());
        assert!(select_alpn(&[2]).is_err());
        let mut malformed = hello(&[PAIRING_ALPN]);
        *malformed.last_mut().expect("last") = 255;
        assert!(select_alpn(&malformed).is_err());
    }
}
