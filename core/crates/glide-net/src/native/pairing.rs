//! Exporter-bound SPAKE2 pairing exchange for the native QUIC transport.
//!
//! The caller must first complete the pairing ALPN hello on a separate stream,
//! enforce the host code's expiry/attempt budget, and pass the device IDs
//! computed from each TLS certificate's SubjectPublicKeyInfo. This module
//! leaves all candidates unpinned; the manager owns the confirmation barrier.
//!
//! `words.txt` is an offline-derived and manually scrubbed set of 2048 English
//! words. It is intentionally not claimed to be the canonical BIP-39 list. The
//! checked-in table is validated for count, sorting, uniqueness, unique
//! four-letter prefixes, and absence of edit-distance-one pairs.

use glide_platform::Os;
use glide_proto::ipc::{ErrorCode, IpcError};
use hkdf::Hkdf;
use hmac::{Hmac, KeyInit, Mac};
use quinn::{Connection, RecvStream, SendStream};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity, Password, Spake2};
use subtle::ConstantTimeEq;
use tokio::io::AsyncWriteExt;
use zeroize::Zeroizing;

const EXPORTER_LABEL: &[u8] = b"EXPORTER-Glide-Pairing-v1";
const EXPORTER_CONTEXT: &[u8] = glide_proto::wire::PAIR_ALPN;
const MAX_FRAME_BYTES: usize = 1024;
const MAX_NAME_BYTES: usize = 128;
const MSG_HELLO: u8 = 1;
const MSG_SPAKE: u8 = 2;
const MSG_AUTH: u8 = 3;
const MSG_DECISION: u8 = 4;
const ROLE_JOINER: u8 = 1;
const ROLE_HOST: u8 = 2;

type HmacSha256 = Hmac<Sha256>;

/// Non-secret metadata exchanged during pairing and shown to the user.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PairHello {
    pub(crate) device_id: String,
    pub(crate) name: String,
    pub(crate) os: Os,
}

/// A cryptographically authenticated, still-unpinned pairing candidate.
pub(crate) struct AuthenticatedPairing {
    pub(crate) send: SendStream,
    pub(crate) recv: RecvStream,
    /// Session key for the authenticated confirmation exchange.
    pub(crate) key: Zeroizing<[u8; 32]>,
    pub(crate) phrase: [String; 3],
    pub(crate) remote: PairHello,
}

/// Authenticated state-machine message used before a candidate is pinned.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum DecisionStage {
    Confirm = 1,
    Ready = 2,
    Commit = 3,
    Complete = 4,
}

/// Run the second, PAKE-protected pairing stream on an established pairing
/// connection. The host accepts it; the joiner opens it.
pub(crate) async fn pair_exchange(
    connection: &Connection,
    is_host: bool,
    code: Zeroizing<String>,
    local: PairHello,
    local_device_id: &str,
    remote_cert_device_id: &str,
) -> Result<AuthenticatedPairing, IpcError> {
    pair_exchange_inner(
        connection,
        is_host,
        code,
        local,
        local_device_id,
        remote_cert_device_id,
    )
    .await
}

async fn pair_exchange_inner(
    connection: &Connection,
    is_host: bool,
    code: Zeroizing<String>,
    local: PairHello,
    local_device_id: &str,
    remote_cert_device_id: &str,
) -> Result<AuthenticatedPairing, IpcError> {
    validate_code(&code)?;
    validate_device_id(local_device_id)?;
    validate_device_id(remote_cert_device_id)?;
    if local.device_id != local_device_id {
        return Err(invalid_pairing());
    }
    validate_hello(&local)?;

    let (mut send, mut recv) = if is_host {
        connection.accept_bi().await.map_err(stream_error)?
    } else {
        connection.open_bi().await.map_err(stream_error)?
    };

    let mut exporter = Zeroizing::new([0u8; 32]);
    connection
        .export_keying_material(&mut exporter[..], EXPORTER_LABEL, EXPORTER_CONTEXT)
        .map_err(|_| internal_pairing("could not derive TLS channel binding"))?;

    let local_hello = serde_json::to_vec(&local)
        .map_err(|_| internal_pairing("could not encode pairing metadata"))?;
    send_frame(&mut send, tagged(MSG_HELLO, &local_hello)?).await?;
    let remote_frame = recv_frame(&mut recv).await?;
    let remote_payload = tagged_payload(&remote_frame, MSG_HELLO)?;
    let remote: PairHello =
        serde_json::from_slice(remote_payload).map_err(|_| invalid_pairing())?;
    validate_hello(&remote)?;
    if remote.device_id != remote_cert_device_id {
        return Err(invalid_pairing());
    }

    let (id_a, id_b) = if is_host {
        (remote_cert_device_id, local_device_id)
    } else {
        (local_device_id, remote_cert_device_id)
    };
    let (local_spake, local_spake_message) =
        start_spake(&code, &exporter[..], id_a, id_b, is_host)?;
    send_frame(&mut send, tagged(MSG_SPAKE, &local_spake_message)?).await?;
    let remote_spake_frame = recv_frame(&mut recv).await?;
    let remote_spake_message = tagged_payload(&remote_spake_frame, MSG_SPAKE)?;
    if remote_spake_message.len() != 33 {
        return Err(invalid_pairing());
    }
    let pake_key = Zeroizing::new(
        local_spake
            .finish(remote_spake_message)
            .map_err(|_| invalid_pairing())?,
    );

    let local_spake_message = local_spake_message.as_slice();
    let (hello_a, hello_b, spake_a, spake_b) = if is_host {
        (
            &remote_frame[1..],
            local_hello.as_slice(),
            remote_spake_message,
            local_spake_message,
        )
    } else {
        (
            local_hello.as_slice(),
            &remote_frame[1..],
            local_spake_message,
            remote_spake_message,
        )
    };
    let transcript = transcript_hash(
        &exporter[..],
        id_a,
        id_b,
        hello_a,
        hello_b,
        spake_a,
        spake_b,
    );
    let session_key = derive_session_key(&pake_key, &exporter[..], &transcript)?;
    let phrase = derive_phrase(&pake_key, &exporter[..], id_a, id_b, &transcript)?;

    let local_role = role(is_host);
    let local_id = if is_host { id_b } else { id_a };
    let mut auth_frame = Vec::with_capacity(34);
    auth_frame.push(MSG_AUTH);
    auth_frame.push(local_role);
    auth_frame.extend_from_slice(&pair_auth_mac(
        &session_key,
        &transcript,
        local_role,
        local_id,
    )?);
    send_frame(&mut send, auth_frame).await?;
    let remote_auth = recv_frame(&mut recv).await?;
    if remote_auth.len() != 34 || remote_auth[0] != MSG_AUTH || remote_auth[1] != role(!is_host) {
        return Err(invalid_pairing());
    }
    let remote_id = if is_host { id_a } else { id_b };
    let expected = pair_auth_mac(&session_key, &transcript, role(!is_host), remote_id)?;
    if !bool::from(expected.as_slice().ct_eq(&remote_auth[2..])) {
        // Deliver our own key-confirmation proof before dropping the stream.
        // Otherwise a close can mask BadCode as an unrelated read error.
        let _ = send.finish();
        let _ = tokio::time::timeout(super::IO_TIMEOUT, send.stopped()).await;
        return Err(bad_code());
    }

    Ok(AuthenticatedPairing {
        send,
        recv,
        key: session_key,
        phrase,
        remote,
    })
}

/// Send one authenticated decision for the specified pairing barrier stage.
/// Both peers must exchange each stage before proceeding to the next.
pub(crate) async fn send_decision(
    send: &mut SendStream,
    key: &[u8; 32],
    phrase: &[String; 3],
    is_host: bool,
    stage: DecisionStage,
    accepted: bool,
) -> Result<(), IpcError> {
    let role = role(is_host);
    let accepted = u8::from(accepted);
    let mac = decision_mac(key, phrase, stage, role, accepted)?;
    let mut frame = Vec::with_capacity(36);
    frame.extend_from_slice(&[MSG_DECISION, stage as u8, role, accepted]);
    frame.extend_from_slice(&mac);
    send_frame(send, frame).await
}

/// Read and authenticate the other peer's decision for a single barrier stage.
pub(crate) async fn read_decision(
    recv: &mut RecvStream,
    key: &[u8; 32],
    phrase: &[String; 3],
    peer_is_host: bool,
    stage: DecisionStage,
) -> Result<bool, IpcError> {
    let frame = recv_frame(recv).await?;
    if frame.len() != 36
        || frame[0] != MSG_DECISION
        || frame[1] != stage as u8
        || frame[2] != role(peer_is_host)
        || frame[3] > 1
    {
        return Err(invalid_pairing());
    }
    let expected = decision_mac(key, phrase, stage, frame[2], frame[3])?;
    if !bool::from(expected.as_slice().ct_eq(&frame[4..])) {
        return Err(invalid_pairing());
    }
    Ok(frame[3] == 1)
}

fn start_spake(
    code: &str,
    exporter: &[u8],
    id_a: &str,
    id_b: &str,
    is_host: bool,
) -> Result<(Spake2<Ed25519Group>, Vec<u8>), IpcError> {
    // SPAKE2's convenience constructors call infallible OsRng methods. Draw
    // its scalar entropy from the OS CSPRNG up front so an entropy failure is
    // returned as an error instead of panicking inside the library.
    let rng = OsEntropy::new()?;
    start_spake_with_rng(code, exporter, id_a, id_b, is_host, rng)
}

/// Bytes for one SPAKE2 start: Ed25519 scalar generation consumes 64 bytes.
const SPAKE_ENTROPY_BYTES: usize = 64;

/// OS-CSPRNG-backed RNG for spake2 0.4, which is written against the
/// `rand_core` 0.6 traits. Every byte comes from `ring::rand::SystemRandom`:
/// a prefilled zeroizing buffer serves the expected request, and any further
/// request draws directly from the OS (failing closed rather than ever
/// returning non-random bytes).
struct OsEntropy {
    bytes: Zeroizing<[u8; SPAKE_ENTROPY_BYTES]>,
    used: usize,
}

impl OsEntropy {
    fn new() -> Result<Self, IpcError> {
        let mut bytes = Zeroizing::new([0u8; SPAKE_ENTROPY_BYTES]);
        SystemRandom::new()
            .fill(&mut bytes[..])
            .map_err(|_| internal_pairing("secure pairing randomness unavailable"))?;
        Ok(Self { bytes, used: 0 })
    }
}

impl rand_core_06::RngCore for OsEntropy {
    fn next_u32(&mut self) -> u32 {
        rand_core_06::impls::next_u32_via_fill(self)
    }

    fn next_u64(&mut self) -> u64 {
        rand_core_06::impls::next_u64_via_fill(self)
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        if self.try_fill_bytes(dest).is_err() {
            panic!("OS CSPRNG failed while generating pairing entropy");
        }
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core_06::Error> {
        let start = self.used;
        if let Some(available) = self.bytes.get_mut(start..start + dest.len()) {
            dest.copy_from_slice(available);
            available.fill(0);
            self.used += dest.len();
            return Ok(());
        }
        SystemRandom::new().fill(dest).map_err(|_| {
            let code = core::num::NonZeroU32::new(rand_core_06::Error::CUSTOM_START)
                .unwrap_or(core::num::NonZeroU32::MIN);
            rand_core_06::Error::from(code)
        })
    }
}

impl rand_core_06::CryptoRng for OsEntropy {}

fn start_spake_with_rng(
    code: &str,
    exporter: &[u8],
    id_a: &str,
    id_b: &str,
    is_host: bool,
    rng: impl rand_core_06::CryptoRng + rand_core_06::RngCore,
) -> Result<(Spake2<Ed25519Group>, Vec<u8>), IpcError> {
    let password = pairing_password(code, exporter, id_a, id_b)?;
    let password = Password::new(&password[..]);
    let id_a_identity = pake_identity(exporter, id_a, id_b, b"joiner")?;
    let id_b_identity = pake_identity(exporter, id_a, id_b, b"host")?;
    let (context, message) = if is_host {
        Spake2::<Ed25519Group>::start_b_with_rng(&password, &id_a_identity, &id_b_identity, rng)
    } else {
        Spake2::<Ed25519Group>::start_a_with_rng(&password, &id_a_identity, &id_b_identity, rng)
    };
    Ok((context, message))
}

/// HKDF-SHA256(salt = TLS exporter, IKM = code) expanded over both device IDs.
fn pairing_password(
    code: &str,
    exporter: &[u8],
    id_a: &str,
    id_b: &str,
) -> Result<Zeroizing<[u8; 32]>, IpcError> {
    let mut password = Zeroizing::new([0u8; 32]);
    let mut password_info = Vec::with_capacity(80);
    password_info.extend_from_slice(b"glide/pair/password/v1");
    append_field(&mut password_info, id_a.as_bytes())?;
    append_field(&mut password_info, id_b.as_bytes())?;
    Hkdf::<Sha256>::new(Some(exporter), code.as_bytes())
        .expand(&password_info, &mut password[..])
        .map_err(|_| internal_pairing("could not derive pairing password"))?;
    Ok(password)
}

fn pake_identity(
    exporter: &[u8],
    id_a: &str,
    id_b: &str,
    role_name: &[u8],
) -> Result<Identity, IpcError> {
    Ok(Identity::new(&pake_identity_digest(
        exporter, id_a, id_b, role_name,
    )))
}

fn pake_identity_digest(exporter: &[u8], id_a: &str, id_b: &str, role_name: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"glide/pair/spake-identity/v1");
    hash.update(exporter);
    hash.update([role_name.len() as u8]);
    hash.update(role_name);
    hash.update(id_a.as_bytes());
    hash.update(id_b.as_bytes());
    hash.finalize().into()
}

fn transcript_hash(
    exporter: &[u8],
    id_a: &str,
    id_b: &str,
    hello_a: &[u8],
    hello_b: &[u8],
    spake_a: &[u8],
    spake_b: &[u8],
) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"glide/pair/transcript/v1");
    hash.update(exporter);
    for field in [
        id_a.as_bytes(),
        id_b.as_bytes(),
        hello_a,
        hello_b,
        spake_a,
        spake_b,
    ] {
        hash.update((field.len() as u32).to_be_bytes());
        hash.update(field);
    }
    hash.finalize().into()
}

fn derive_session_key(
    pake_key: &[u8],
    exporter: &[u8],
    transcript: &[u8; 32],
) -> Result<Zeroizing<[u8; 32]>, IpcError> {
    let mut key = Zeroizing::new([0u8; 32]);
    let mut info = Vec::with_capacity(64);
    info.extend_from_slice(b"glide/pair/session/v1");
    info.extend_from_slice(transcript);
    Hkdf::<Sha256>::new(Some(exporter), pake_key)
        .expand(&info, &mut key[..])
        .map_err(|_| internal_pairing("could not derive pairing session key"))?;
    Ok(key)
}

fn derive_phrase(
    pake_key: &[u8],
    exporter: &[u8],
    id_a: &str,
    id_b: &str,
    transcript: &[u8; 32],
) -> Result<[String; 3], IpcError> {
    let mut binding = Vec::with_capacity(128);
    binding.extend_from_slice(exporter);
    append_field(&mut binding, id_a.as_bytes())?;
    append_field(&mut binding, id_b.as_bytes())?;
    binding.extend_from_slice(transcript);
    let mut entropy = Zeroizing::new([0u8; 5]);
    Hkdf::<Sha256>::new(Some(pake_key), &binding)
        .expand(b"glide/pair/three-word-sas/v1", &mut entropy[..])
        .map_err(|_| internal_pairing("could not derive verification phrase"))?;
    let indexes = [
        ((usize::from(entropy[0])) << 3) | (usize::from(entropy[1]) >> 5),
        ((usize::from(entropy[1]) & 0x1f) << 6) | (usize::from(entropy[2]) >> 2),
        ((usize::from(entropy[2]) & 0x03) << 9)
            | (usize::from(entropy[3]) << 1)
            | (usize::from(entropy[4]) >> 7),
    ];
    let words = word_list();
    if words.len() != 2048 {
        return Err(internal_pairing("verification word list is invalid"));
    }
    let get_word = |index: usize| {
        words
            .get(index)
            .map(|word| (*word).to_owned())
            .ok_or_else(|| internal_pairing("verification word list is invalid"))
    };
    Ok([
        get_word(indexes[0])?,
        get_word(indexes[1])?,
        get_word(indexes[2])?,
    ])
}

fn word_list() -> Vec<&'static str> {
    include_str!("words.txt").split_ascii_whitespace().collect()
}

fn pair_auth_mac(
    key: &[u8; 32],
    transcript: &[u8; 32],
    sender_role: u8,
    sender_id: &str,
) -> Result<[u8; 32], IpcError> {
    let mut mac = HmacSha256::new_from_slice(key)
        .map_err(|_| internal_pairing("could not authenticate pairing transcript"))?;
    mac.update(b"glide/pair/fingerprint-auth/v1");
    mac.update(transcript);
    mac.update(&[sender_role]);
    mac.update(sender_id.as_bytes());
    Ok(mac.finalize().into_bytes().into())
}

fn decision_mac(
    key: &[u8; 32],
    phrase: &[String; 3],
    stage: DecisionStage,
    sender_role: u8,
    accepted: u8,
) -> Result<[u8; 32], IpcError> {
    let mut mac = HmacSha256::new_from_slice(key)
        .map_err(|_| internal_pairing("could not authenticate pairing decision"))?;
    mac.update(b"glide/pair/decision/v1");
    mac.update(&[stage as u8, sender_role, accepted]);
    for word in phrase {
        append_hmac_field(&mut mac, word.as_bytes())?;
    }
    Ok(mac.finalize().into_bytes().into())
}

fn append_hmac_field(mac: &mut HmacSha256, field: &[u8]) -> Result<(), IpcError> {
    let len = u16::try_from(field.len()).map_err(|_| invalid_pairing())?;
    mac.update(&len.to_be_bytes());
    mac.update(field);
    Ok(())
}

fn tagged(tag: u8, payload: &[u8]) -> Result<Vec<u8>, IpcError> {
    let length = payload.len().checked_add(1).ok_or_else(invalid_pairing)?;
    if length > MAX_FRAME_BYTES {
        return Err(invalid_pairing());
    }
    let mut frame = Vec::with_capacity(length);
    frame.push(tag);
    frame.extend_from_slice(payload);
    Ok(frame)
}

fn tagged_payload(frame: &[u8], expected_tag: u8) -> Result<&[u8], IpcError> {
    if frame.first().copied() != Some(expected_tag) {
        return Err(invalid_pairing());
    }
    Ok(&frame[1..])
}

async fn send_frame(send: &mut SendStream, frame: Vec<u8>) -> Result<(), IpcError> {
    if frame.is_empty() || frame.len() > MAX_FRAME_BYTES {
        return Err(invalid_pairing());
    }
    let length = u16::try_from(frame.len()).map_err(|_| invalid_pairing())?;
    send.write_all(&length.to_be_bytes())
        .await
        .map_err(stream_error)?;
    send.write_all(&frame).await.map_err(stream_error)?;
    send.flush().await.map_err(stream_error)
}

async fn recv_frame(recv: &mut RecvStream) -> Result<Vec<u8>, IpcError> {
    let mut length = [0u8; 2];
    recv.read_exact(&mut length).await.map_err(stream_error)?;
    let length = usize::from(u16::from_be_bytes(length));
    if length == 0 || length > MAX_FRAME_BYTES {
        return Err(invalid_pairing());
    }
    let mut frame = vec![0u8; length];
    recv.read_exact(&mut frame).await.map_err(stream_error)?;
    Ok(frame)
}

fn append_field(output: &mut Vec<u8>, field: &[u8]) -> Result<(), IpcError> {
    let length = u16::try_from(field.len()).map_err(|_| invalid_pairing())?;
    output.extend_from_slice(&length.to_be_bytes());
    output.extend_from_slice(field);
    Ok(())
}

fn validate_code(code: &str) -> Result<(), IpcError> {
    if code.len() == 6 && code.bytes().all(|byte| byte.is_ascii_digit()) {
        Ok(())
    } else {
        Err(invalid_pairing())
    }
}

fn validate_device_id(device_id: &str) -> Result<(), IpcError> {
    if device_id.len() == 64
        && device_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(invalid_pairing())
    }
}

fn validate_hello(hello: &PairHello) -> Result<(), IpcError> {
    validate_device_id(&hello.device_id)?;
    if hello.name.is_empty()
        || hello.name.len() > MAX_NAME_BYTES
        || hello.name.trim().is_empty()
        || hello.name.chars().any(char::is_control)
    {
        return Err(invalid_pairing());
    }
    Ok(())
}

fn role(is_host: bool) -> u8 {
    if is_host {
        ROLE_HOST
    } else {
        ROLE_JOINER
    }
}

fn stream_error(_error: impl std::fmt::Debug) -> IpcError {
    internal_pairing("pairing stream ended or failed")
}

fn bad_code() -> IpcError {
    IpcError::new(ErrorCode::BadCode, "pairing code is incorrect")
}

fn invalid_pairing() -> IpcError {
    IpcError::new(ErrorCode::InvalidParams, "pairing exchange is invalid")
}

fn internal_pairing(message: &str) -> IpcError {
    IpcError::new(ErrorCode::Internal, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVICE_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const DEVICE_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    /// Deterministic byte source used ONLY by the known-answer test so that the
    /// SPAKE2 scalars (and therefore every pairing output) are reproducible.
    struct FixedRng {
        bytes: Vec<u8>,
        position: usize,
    }

    impl FixedRng {
        fn new(start: u8) -> Self {
            Self {
                bytes: (0..128u8).map(|i| start.wrapping_add(i)).collect(),
                position: 0,
            }
        }
    }

    impl rand_core_06::RngCore for FixedRng {
        fn next_u32(&mut self) -> u32 {
            let mut bytes = [0u8; 4];
            self.fill_bytes(&mut bytes);
            u32::from_le_bytes(bytes)
        }
        fn next_u64(&mut self) -> u64 {
            let mut bytes = [0u8; 8];
            self.fill_bytes(&mut bytes);
            u64::from_le_bytes(bytes)
        }
        fn fill_bytes(&mut self, dest: &mut [u8]) {
            let end = self.position + dest.len();
            dest.copy_from_slice(&self.bytes[self.position..end]);
            self.position = end;
        }
        fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core_06::Error> {
            self.fill_bytes(dest);
            Ok(())
        }
    }

    impl rand_core_06::CryptoRng for FixedRng {}

    #[test]
    fn os_entropy_serves_wiped_os_bytes_and_falls_back_to_the_os() {
        use rand_core_06::RngCore as _;

        let mut first = OsEntropy::new().unwrap();
        let mut second = OsEntropy::new().unwrap();
        let mut a = [0u8; SPAKE_ENTROPY_BYTES];
        let mut b = [0u8; SPAKE_ENTROPY_BYTES];
        first.fill_bytes(&mut a);
        second.fill_bytes(&mut b);
        assert_ne!(a, b);
        assert!(
            first.bytes.iter().all(|byte| *byte == 0),
            "served bytes wiped"
        );
        let mut extra = [0u8; 32];
        first.try_fill_bytes(&mut extra).unwrap();
        assert_ne!(extra, [0u8; 32]);
    }

    /// Pins every pairing derivation (HKDF/HMAC/SHA-256, SPAKE2 messages and
    /// key, the three-word SAS). Expected values were recorded with the
    /// pre-upgrade dependency set (sha2 0.10.9, hkdf 0.12.4, hmac 0.12.1,
    /// spake2 0.4.0) and must never change: peers on older releases derive
    /// these exact bytes.
    #[test]
    fn pairing_derivations_known_answer() {
        let exporter: [u8; 32] = core::array::from_fn(|i| i as u8);
        let code = "394817";
        let password = pairing_password(code, &exporter, DEVICE_A, DEVICE_B).unwrap();
        let joiner_identity = pake_identity_digest(&exporter, DEVICE_A, DEVICE_B, b"joiner");
        let host_identity = pake_identity_digest(&exporter, DEVICE_A, DEVICE_B, b"host");
        let (joiner, joiner_message) = start_spake_with_rng(
            code,
            &exporter,
            DEVICE_A,
            DEVICE_B,
            false,
            FixedRng::new(0x11),
        )
        .unwrap();
        let (host, host_message) = start_spake_with_rng(
            code,
            &exporter,
            DEVICE_A,
            DEVICE_B,
            true,
            FixedRng::new(0x77),
        )
        .unwrap();
        let joiner_key = Zeroizing::new(joiner.finish(&host_message).unwrap());
        let host_key = Zeroizing::new(host.finish(&joiner_message).unwrap());
        assert_eq!(joiner_key.as_slice(), host_key.as_slice());
        let transcript = transcript_hash(
            &exporter,
            DEVICE_A,
            DEVICE_B,
            br#"{"joiner":1}"#,
            br#"{"host":2}"#,
            &joiner_message,
            &host_message,
        );
        let session = derive_session_key(&joiner_key, &exporter, &transcript).unwrap();
        let phrase =
            derive_phrase(&joiner_key, &exporter, DEVICE_A, DEVICE_B, &transcript).unwrap();
        let joiner_auth = pair_auth_mac(&session, &transcript, ROLE_JOINER, DEVICE_A).unwrap();
        let host_auth = pair_auth_mac(&session, &transcript, ROLE_HOST, DEVICE_B).unwrap();
        let decision =
            decision_mac(&session, &phrase, DecisionStage::Commit, ROLE_HOST, 1).unwrap();

        let actual = [
            ("password", hex::encode(&password[..])),
            ("joiner_identity", hex::encode(joiner_identity)),
            ("host_identity", hex::encode(host_identity)),
            ("joiner_message", hex::encode(&joiner_message)),
            ("host_message", hex::encode(&host_message)),
            ("pake_key", hex::encode(joiner_key.as_slice())),
            ("transcript", hex::encode(transcript)),
            ("session_key", hex::encode(&session[..])),
            ("phrase", phrase.join(" ")),
            ("joiner_auth", hex::encode(joiner_auth)),
            ("host_auth", hex::encode(host_auth)),
            ("decision", hex::encode(decision)),
        ];
        let expected = [
            (
                "password",
                "11d07e4536b9e8de6bd934f1dde4dbccdc5b4ef71af053fcebd823d36a4b3df6",
            ),
            (
                "joiner_identity",
                "612cc9f9eca9bbe5de8d9544f2e60e7dc7aedb054a68c7896384e81797f1e7e4",
            ),
            (
                "host_identity",
                "e992fb7d2e3c3027196d47c7d20ab32c0740fa1b6ebb2e118636db7918d3957f",
            ),
            (
                "joiner_message",
                "410cc6c8a682a1618ec5a0f7a2d2fcdd2f6539045b09d71b3418ca15cf2b7ff4ec",
            ),
            (
                "host_message",
                "42ecc528541f9c931c8dce17f1b38d465441c091d5427363f1de74d08f32a42054",
            ),
            (
                "pake_key",
                "a475e6d9d37614515c7b643e7e62ca4d5a1072782a6baf2c2d41022a042a959f",
            ),
            (
                "transcript",
                "f5285d73454d4c3ad5711c723d5be3c839dcf3db389e7849b1f4b2f11f634206",
            ),
            (
                "session_key",
                "a07a3059806cc6258e5a2783628ce0626b6e653222f184c9a3ae2afc5a9ffcc5",
            ),
            ("phrase", "recall king walls"),
            (
                "joiner_auth",
                "4afed6c772fd2b263ab3a880b325ce7c2eb069465636f97001bdb69dc1698cf5",
            ),
            (
                "host_auth",
                "82215302175c8163f98ec72c21aee319abd452b35eac95065f50151a07b5aceb",
            ),
            (
                "decision",
                "0560904ad3aa6235c58f7bd806dd298a8be23a0c1bf21cf38c6ed375eece78ff",
            ),
        ];
        assert_eq!(expected.len(), actual.len(), "KAT table incomplete");
        for ((name, value), (expected_name, expected_value)) in actual.iter().zip(expected) {
            assert_eq!(*name, expected_name);
            assert_eq!(value, expected_value, "{name} changed");
        }
    }

    #[test]
    fn exporter_bound_spake_derives_same_key_and_phrase_on_both_sides() {
        let exporter = [0x55; 32];
        let (joiner, joiner_message) =
            start_spake("123456", &exporter, DEVICE_A, DEVICE_B, false).unwrap();
        let (host, host_message) =
            start_spake("123456", &exporter, DEVICE_A, DEVICE_B, true).unwrap();
        let joiner_key = Zeroizing::new(joiner.finish(&host_message).unwrap());
        let host_key = Zeroizing::new(host.finish(&joiner_message).unwrap());
        assert!(bool::from(joiner_key.as_slice().ct_eq(host_key.as_slice())));
        let transcript = transcript_hash(
            &exporter,
            DEVICE_A,
            DEVICE_B,
            b"joiner hello",
            b"host hello",
            &joiner_message,
            &host_message,
        );
        let joiner_phrase =
            derive_phrase(&joiner_key, &exporter, DEVICE_A, DEVICE_B, &transcript).unwrap();
        let host_phrase =
            derive_phrase(&host_key, &exporter, DEVICE_A, DEVICE_B, &transcript).unwrap();
        assert_eq!(joiner_phrase, host_phrase);
    }

    #[test]
    fn wrong_code_and_changed_exporter_do_not_authenticate() {
        let exporter = [0x21; 32];
        let (joiner, joiner_message) =
            start_spake("123456", &exporter, DEVICE_A, DEVICE_B, false).unwrap();
        let (wrong_code_host, wrong_code_message) =
            start_spake("654321", &exporter, DEVICE_A, DEVICE_B, true).unwrap();
        let joiner_key = Zeroizing::new(joiner.finish(&wrong_code_message).unwrap());
        let wrong_code_key = Zeroizing::new(wrong_code_host.finish(&joiner_message).unwrap());
        let transcript = [7; 32];
        let joiner_mac = pair_auth_mac(&[9; 32], &transcript, ROLE_JOINER, DEVICE_A).unwrap();
        let wrong_mac = pair_auth_mac(&[8; 32], &transcript, ROLE_JOINER, DEVICE_A).unwrap();
        assert!(!bool::from(joiner_mac.ct_eq(&wrong_mac)));
        assert!(!bool::from(
            joiner_key.as_slice().ct_eq(wrong_code_key.as_slice())
        ));

        let other_exporter = [0x22; 32];
        let (other_host, other_host_message) =
            start_spake("123456", &other_exporter, DEVICE_A, DEVICE_B, true).unwrap();
        let (same_exporter_joiner, same_exporter_message) =
            start_spake("123456", &exporter, DEVICE_A, DEVICE_B, false).unwrap();
        let other_key = Zeroizing::new(other_host.finish(&same_exporter_message).unwrap());
        let expected_key =
            Zeroizing::new(same_exporter_joiner.finish(&other_host_message).unwrap());
        assert!(!bool::from(
            other_key.as_slice().ct_eq(expected_key.as_slice())
        ));
    }

    #[test]
    fn wordlist_has_2048_distinct_plain_words() {
        let words = word_list();
        assert_eq!(words.len(), 2048);
        let mut unique = std::collections::HashSet::with_capacity(words.len());
        let mut prefixes = std::collections::HashSet::with_capacity(words.len());
        assert!(words.iter().all(|word| {
            !word.is_empty()
                && word.bytes().all(|byte| byte.is_ascii_lowercase())
                && word.len() >= 4
                && unique.insert(*word)
                && prefixes.insert(&word[..4])
        }));
        assert!(words.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn wordlist_has_no_edit_distance_one_pairs_or_sensitive_entries() {
        let words = word_list();
        let mut close_pairs = Vec::new();
        'outer: for left in 0..words.len() {
            for right in left + 1..words.len() {
                if edit_distance_one(words[left].as_bytes(), words[right].as_bytes()) {
                    close_pairs.push((words[left], words[right]));
                    if close_pairs.len() == 100 {
                        break 'outer;
                    }
                }
            }
        }
        assert!(
            close_pairs.is_empty(),
            "word list contains edit-distance-one pairs: {close_pairs:?}"
        );

        // Keep pairing prompts clear of common profanity, slurs, explicit
        // content, drug terms, and the distressing terms found during audit.
        let excluded = [
            "abortion",
            "abuse",
            "assault",
            "bomb",
            "cocaine",
            "killed",
            "marijuana",
            "murder",
            "slave",
            "slavery",
            "terror",
            "homosexual",
            "sexuality",
            "penetrate",
            "pregnancy",
            "weapon",
            "warfare",
            "ammo",
            "armed",
            "arms",
            "blood",
            "knife",
            "skull",
            "sword",
            "anarchy",
            "jihad",
            "narc",
            "dumb",
            "insane",
            "homeless",
            "obesity",
            "cancer",
            "death",
            "deadly",
            "demon",
            "evil",
            "drug",
            "enemy",
            "gang",
            "fatal",
            "fraud",
            "malicious",
            "misconduct",
            "misleading",
            "nasty",
            "outrage",
            "prejudice",
            "prison",
            "trauma",
            "unemployed",
            "unfair",
            "unlawful",
            "virus",
            "vulnerable",
            "alcohol",
            "cruel",
            "hell",
            "asshole",
            "bastard",
            "bitch",
            "bollocks",
            "bugger",
            "bullshit",
            "cunt",
            "damn",
            "dick",
            "dildo",
            "fag",
            "faggot",
            "fuck",
            "fucker",
            "fucking",
            "motherfucker",
            "nigger",
            "nigga",
            "prick",
            "pussy",
            "retard",
            "shit",
            "shitty",
            "slut",
            "twat",
            "whore",
            "wanker",
        ];
        assert!(excluded.iter().all(|word| !words.contains(word)));
    }

    fn edit_distance_one(left: &[u8], right: &[u8]) -> bool {
        if left.len().abs_diff(right.len()) > 1 {
            return false;
        }
        if left.len() == right.len() {
            return left
                .iter()
                .zip(right)
                .filter(|(left_byte, right_byte)| left_byte != right_byte)
                .take(2)
                .count()
                == 1;
        }

        let (shorter, longer) = if left.len() < right.len() {
            (left, right)
        } else {
            (right, left)
        };
        let (mut short_index, mut long_index, mut skipped) = (0, 0, false);
        while short_index < shorter.len() && long_index < longer.len() {
            if shorter[short_index] == longer[long_index] {
                short_index += 1;
                long_index += 1;
            } else if !skipped {
                skipped = true;
                long_index += 1;
            } else {
                return false;
            }
        }
        true
    }

    #[test]
    fn decision_mac_binds_stage_role_result_and_sas() {
        let key = [0x31; 32];
        let phrase = ["amber".to_owned(), "breeze".to_owned(), "cabin".to_owned()];
        let confirm = decision_mac(&key, &phrase, DecisionStage::Confirm, ROLE_JOINER, 1).unwrap();

        for altered in [
            decision_mac(&key, &phrase, DecisionStage::Ready, ROLE_JOINER, 1).unwrap(),
            decision_mac(&key, &phrase, DecisionStage::Complete, ROLE_JOINER, 1).unwrap(),
            decision_mac(&key, &phrase, DecisionStage::Confirm, ROLE_HOST, 1).unwrap(),
            decision_mac(&key, &phrase, DecisionStage::Confirm, ROLE_JOINER, 0).unwrap(),
        ] {
            assert!(!bool::from(confirm.ct_eq(&altered)));
        }

        let mismatched_phrase = ["amber".to_owned(), "breeze".to_owned(), "cactus".to_owned()];
        let mismatched_sas = decision_mac(
            &key,
            &mismatched_phrase,
            DecisionStage::Confirm,
            ROLE_JOINER,
            1,
        )
        .unwrap();
        assert!(!bool::from(confirm.ct_eq(&mismatched_sas)));
    }
}
