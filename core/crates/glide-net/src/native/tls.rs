use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use rustls::{
    client::danger::{ServerCertVerified, ServerCertVerifier},
    crypto::{ring, verify_tls12_signature, verify_tls13_signature, CryptoProvider},
    pki_types::{CertificateDer, ServerName, UnixTime},
    server::danger::{ClientCertVerified, ClientCertVerifier},
    sign::{CertifiedKey, SigningKey},
    ClientConfig as RustlsClientConfig, DigitallySignedStruct, Error as TlsError,
    ServerConfig as RustlsServerConfig, SignatureScheme,
};
use zeroize::Zeroize;

pub use super::identity::PinSet;
use super::{
    identity::{device_id_from_certificate, grouped_fingerprint},
    NativeError, NativeIdentity,
};

const NORMAL_ALPN: &[u8] = glide_proto::wire::ALPN;
const PAIRING_ALPN: &[u8] = glide_proto::wire::PAIR_ALPN;

mod dispatch;

/// Select an exact ALPN before feeding ClientHello to either certificate verifier.
/// Normal mutual authentication remains strict while the pairing window is open.
pub fn dispatch_server(
    identity: &NativeIdentity,
    pins: PinSet,
    pairing_open: Arc<AtomicBool>,
) -> Result<quinn::ServerConfig, NativeError> {
    let normal = normal_server(identity, pins)?;
    let pairing = pairing_server(identity, pairing_open)?;
    Ok(quinn::ServerConfig::with_crypto(Arc::new(
        dispatch::Dispatch {
            normal: normal.crypto,
            pairing: pairing.crypto,
        },
    )))
}

/// Build a TLS 1.3 QUIC client for a pinned, already-paired peer.
pub fn normal_client(
    identity: &NativeIdentity,
    pins: PinSet,
) -> Result<quinn::ClientConfig, NativeError> {
    client_config(identity, Some(pins), NORMAL_ALPN)
}

/// Build a TLS 1.3 QUIC client for the PAKE-authenticated pairing handshake.
///
/// This accepts any certificate with a parseable SubjectPublicKeyInfo, but TLS
/// still verifies the peer's handshake signature. Pairing code and PAKE must
/// authenticate this key before the peer can be pinned or send normal data.
pub fn pairing_client(identity: &NativeIdentity) -> Result<quinn::ClientConfig, NativeError> {
    client_config(identity, None, PAIRING_ALPN)
}

/// Build the strict normal-session server configuration.
pub fn normal_server(
    identity: &NativeIdentity,
    pins: PinSet,
) -> Result<quinn::ServerConfig, NativeError> {
    server_config(
        identity,
        Arc::new(PinnedClientVerifier::new(pins)),
        NORMAL_ALPN,
    )
}

/// Build a pairing-only server configuration for the same QUIC endpoint.
///
/// Used by `dispatch_server` alongside the strict normal configuration. Its live
/// atomic closes pairing admission without changing normal-session admission.
pub fn pairing_server(
    identity: &NativeIdentity,
    pairing_open: Arc<AtomicBool>,
) -> Result<quinn::ServerConfig, NativeError> {
    server_config(
        identity,
        Arc::new(PairingClientVerifier::new(pairing_open)),
        PAIRING_ALPN,
    )
}

pub(crate) fn peer_device_id(connection: &quinn::Connection) -> Result<String, NativeError> {
    let identity = connection
        .peer_identity()
        .ok_or_else(|| NativeError::Security("peer certificate is unavailable".into()))?;
    let certificates = identity
        .downcast::<Vec<CertificateDer<'static>>>()
        .map_err(|_| NativeError::Security("peer certificate type is invalid".into()))?;
    certificates
        .first()
        .and_then(device_id_from_certificate)
        .ok_or_else(|| NativeError::Security("peer certificate is malformed".into()))
}

pub(crate) fn human_fingerprint(device_id: &str) -> String {
    grouped_fingerprint(device_id)
}

fn client_config(
    identity: &NativeIdentity,
    pins: Option<PinSet>,
    alpn: &[u8],
) -> Result<quinn::ClientConfig, NativeError> {
    client_config_alpns(identity, pins, vec![alpn.to_vec()])
}

pub(super) fn client_config_alpns(
    identity: &NativeIdentity,
    pins: Option<PinSet>,
    alpns: Vec<Vec<u8>>,
) -> Result<quinn::ClientConfig, NativeError> {
    let provider = Arc::new(ring::default_provider());
    let verifier = Arc::new(PeerServerVerifier { provider, pins });
    let mut config = RustlsClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(tls_config_error)?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_cert_resolver(Arc::new(FixedClientCert {
            key: certified_key(identity)?,
        }));
    config.alpn_protocols = alpns;
    config.enable_early_data = false;
    config.resumption = rustls::client::Resumption::disabled();
    let quic = quinn::crypto::rustls::QuicClientConfig::try_from(config).map_err(|error| {
        NativeError::Internal(format!("could not configure QUIC client TLS: {error}"))
    })?;
    Ok(quinn::ClientConfig::new(Arc::new(quic)))
}

fn server_config(
    identity: &NativeIdentity,
    verifier: Arc<dyn ClientCertVerifier>,
    alpn: &[u8],
) -> Result<quinn::ServerConfig, NativeError> {
    let provider = Arc::new(ring::default_provider());
    let mut config = RustlsServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(tls_config_error)?
        .with_client_cert_verifier(verifier)
        .with_cert_resolver(Arc::new(FixedServerCert {
            key: certified_key(identity)?,
        }));
    config.alpn_protocols = vec![alpn.to_vec()];
    config.max_early_data_size = 0;
    config.send_tls13_tickets = 0;
    config.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    let quic = quinn::crypto::rustls::QuicServerConfig::try_from(config).map_err(|error| {
        NativeError::Internal(format!("could not configure QUIC server TLS: {error}"))
    })?;
    Ok(quinn::ServerConfig::with_crypto(Arc::new(quic)))
}

fn certified_key(identity: &NativeIdentity) -> Result<Arc<CertifiedKey>, NativeError> {
    let mut private_key = identity.private_key()?;
    let signing_key = rustls::crypto::ring::sign::any_supported_type(&private_key).map_err(|_| {
        NativeError::Security("device identity key is not supported by rustls".into())
    });
    private_key.zeroize();
    let key: Arc<dyn SigningKey> = signing_key?;
    let certified_key = Arc::new(CertifiedKey::new(vec![identity.cert()], key));
    certified_key.keys_match().map_err(|_| {
        NativeError::Security("device certificate does not match its private key".into())
    })?;
    Ok(certified_key)
}

#[derive(Debug)]
struct FixedClientCert {
    key: Arc<CertifiedKey>,
}

impl rustls::client::ResolvesClientCert for FixedClientCert {
    fn resolve(
        &self,
        _root_hint_subjects: &[&[u8]],
        _sigschemes: &[SignatureScheme],
    ) -> Option<Arc<CertifiedKey>> {
        Some(self.key.clone())
    }

    fn has_certs(&self) -> bool {
        true
    }
}

#[derive(Debug)]
struct FixedServerCert {
    key: Arc<CertifiedKey>,
}

impl rustls::server::ResolvesServerCert for FixedServerCert {
    fn resolve(&self, _client_hello: rustls::server::ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.key.clone())
    }
}

#[derive(Debug)]
struct PeerServerVerifier {
    provider: Arc<CryptoProvider>,
    pins: Option<PinSet>,
}

impl ServerCertVerifier for PeerServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let Some(device_id) = device_id_from_certificate(end_entity) else {
            return Err(bad_certificate());
        };
        if intermediates.is_empty()
            && (self
                .pins
                .as_ref()
                .is_some_and(|pins| pins.contains(&device_id))
                || self.pins.is_none())
        {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(untrusted_certificate())
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[derive(Debug)]
struct PinnedClientVerifier {
    provider: Arc<CryptoProvider>,
    pins: PinSet,
}

impl PinnedClientVerifier {
    fn new(pins: PinSet) -> Self {
        Self {
            provider: Arc::new(ring::default_provider()),
            pins,
        }
    }
}

impl ClientCertVerifier for PinnedClientVerifier {
    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, TlsError> {
        let Some(device_id) = device_id_from_certificate(end_entity) else {
            return Err(bad_certificate());
        };
        if intermediates.is_empty() && self.pins.contains(&device_id) {
            Ok(ClientCertVerified::assertion())
        } else {
            Err(untrusted_certificate())
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[derive(Debug)]
struct PairingClientVerifier {
    provider: Arc<CryptoProvider>,
    pairing_open: Arc<AtomicBool>,
}

impl PairingClientVerifier {
    fn new(pairing_open: Arc<AtomicBool>) -> Self {
        Self {
            provider: Arc::new(ring::default_provider()),
            pairing_open,
        }
    }
}

impl ClientCertVerifier for PairingClientVerifier {
    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, TlsError> {
        if !self.pairing_open.load(Ordering::Acquire) {
            return Err(untrusted_certificate());
        }
        if intermediates.is_empty() && device_id_from_certificate(end_entity).is_some() {
            Ok(ClientCertVerified::assertion())
        } else {
            Err(bad_certificate())
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn bad_certificate() -> TlsError {
    TlsError::InvalidCertificate(rustls::CertificateError::BadEncoding)
}

fn untrusted_certificate() -> TlsError {
    TlsError::InvalidCertificate(rustls::CertificateError::UnknownIssuer)
}

fn tls_config_error(error: TlsError) -> NativeError {
    NativeError::Internal(format!("could not configure TLS 1.3: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configs_build_with_exact_alpns_and_no_resumption() {
        let identity = NativeIdentity::ephemeral().expect("test identity");
        let pins = PinSet::new();
        pins.insert(identity.device_id()).expect("pin own test key");
        assert!(normal_client(&identity, pins.clone()).is_ok());
        assert!(normal_server(&identity, pins).is_ok());
        assert!(pairing_client(&identity).is_ok());
        assert!(pairing_server(&identity, Arc::new(AtomicBool::new(true))).is_ok());

        {
            assert_eq!(super::PAIRING_ALPN, b"glide/pair/3");
        }
    }

    #[test]
    fn unknown_peer_is_not_accepted_by_normal_verifier() {
        let identity = NativeIdentity::ephemeral().expect("test identity");
        let pins = PinSet::new();
        let verifier = PeerServerVerifier {
            provider: Arc::new(ring::default_provider()),
            pins: Some(pins.clone()),
        };
        let cert = identity.cert();
        assert!(verifier
            .verify_server_cert(
                &cert,
                &[],
                &ServerName::try_from("glide.invalid").expect("test name"),
                &[],
                UnixTime::now(),
            )
            .is_err());
        pins.insert(identity.device_id()).expect("pin test peer");
        assert!(verifier
            .verify_server_cert(
                &cert,
                &[],
                &ServerName::try_from("glide.invalid").expect("test name"),
                &[],
                UnixTime::now(),
            )
            .is_ok());
        pins.remove(identity.device_id()).expect("revoke test peer");
        assert!(verifier
            .verify_server_cert(
                &cert,
                &[],
                &ServerName::try_from("glide.invalid").expect("test name"),
                &[],
                UnixTime::now(),
            )
            .is_err());
    }

    #[test]
    fn pairing_verifier_fails_closed_after_window_closes() {
        let identity = NativeIdentity::ephemeral().expect("test identity");
        let pairing_open = Arc::new(AtomicBool::new(true));
        let verifier = PairingClientVerifier::new(pairing_open.clone());
        let cert = identity.cert();
        assert!(verifier
            .verify_client_cert(&cert, &[], UnixTime::now())
            .is_ok());
        pairing_open.store(false, Ordering::Release);
        assert!(verifier
            .verify_client_cert(&cert, &[], UnixTime::now())
            .is_err());
    }
}
