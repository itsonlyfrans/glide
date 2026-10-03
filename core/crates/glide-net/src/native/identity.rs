use std::{
    collections::HashSet,
    fmt,
    path::Path,
    sync::{Arc, RwLock},
};

use rcgen::{
    CertificateParams, DnType, PublicKeyData, SigningKey as RcgenSigningKey, PKCS_ED25519,
};
use ring::{
    aead::{self, Aad, LessSafeKey, UnboundKey, AES_256_GCM},
    rand::{SecureRandom, SystemRandom},
    signature::Ed25519KeyPair,
};
use sha2::{Digest, Sha256};
use std::io::Read;
use zeroize::{Zeroize, Zeroizing};

use super::NativeError;

const KEYRING_SERVICE: &str = "com.glide.identity.v1";
const DIRECT_KEY_NAME: &str = "device-pkcs8";
const WRAPPING_KEY_NAME: &str = "device-wrap-key";
const ENCRYPTED_KEY_FILE: &str = "identity.key.enc";
const ENCRYPTED_FILE_MAGIC: &[u8; 8] = b"GLIDKEY1";
const NONCE_LEN: usize = 12;
const MAX_ENCRYPTED_KEY_FILE: usize = 4096;
const MAX_CERTIFICATE_BYTES: usize = 4096;
const MAX_KEYRING_SECRET_HEX: usize = 4096;
const MAX_KEYRING_SECRET_BYTES: usize = MAX_KEYRING_SECRET_HEX / 2;

/// A device identity whose private key is held in zeroizing memory and an OS keystore.
pub struct NativeIdentity {
    certificate: rustls::pki_types::CertificateDer<'static>,
    private_key: Zeroizing<Vec<u8>>,
    device_id: String,
    fingerprint: String,
}

impl fmt::Debug for NativeIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeIdentity")
            .field("device_id", &self.device_id)
            .field("fingerprint", &self.fingerprint)
            .field("private_key", &"[redacted]")
            .finish()
    }
}

impl NativeIdentity {
    /// Load the keystore identity, or create one on first use.
    ///
    /// The private key is stored directly in the OS keystore when possible. If
    /// that credential is too large for the provider, the file fallback is
    /// authenticated-encrypted with a random key held in the same keystore.
    pub fn load_or_create(data_dir: &Path) -> Result<Self, NativeError> {
        std::fs::create_dir_all(data_dir)?;
        let namespace = identity_namespace(data_dir)?;
        Self::load_with_entries(
            data_dir,
            IdentityEntry::Native(keystore_entry(&format!("{namespace}-{DIRECT_KEY_NAME}"))?),
            IdentityEntry::Native(keystore_entry(&format!("{namespace}-{WRAPPING_KEY_NAME}"))?),
        )
    }

    fn load_with_entries(
        data_dir: &Path,
        direct: IdentityEntry,
        wrapping: IdentityEntry,
    ) -> Result<Self, NativeError> {
        match direct.get_password() {
            Ok(encoded) => {
                let encoded = Zeroizing::new(encoded);
                let key = load_keystore_secret(&encoded)?;
                return Self::from_private_key(key);
            }
            Err(keyring::Error::NoEntry) => {}
            Err(error) => return Err(keystore_error(error)),
        }

        match wrapping.get_password() {
            Ok(encoded) => {
                let encoded = Zeroizing::new(encoded);
                let key = load_keystore_secret(&encoded)?;
                let wrapping_key = decode_wrapping_key(key)?;
                let file = data_dir.join(ENCRYPTED_KEY_FILE);
                let metadata = std::fs::symlink_metadata(&file).map_err(|error| {
                    if error.kind() == std::io::ErrorKind::NotFound {
                        NativeError::Security(
                            "the keystore wrapping key exists but the encrypted identity file is missing".into(),
                        )
                    } else {
                        NativeError::Io(error)
                    }
                })?;
                if !metadata.is_file() || metadata.len() > MAX_ENCRYPTED_KEY_FILE as u64 {
                    return Err(NativeError::Security(
                        "encrypted identity file has an invalid type or size".into(),
                    ));
                }
                let mut file_handle = std::fs::File::open(&file)?;
                if !file_handle.metadata()?.is_file() {
                    return Err(NativeError::Security(
                        "encrypted identity file is not regular".into(),
                    ));
                }
                let mut encrypted = Vec::with_capacity(metadata.len() as usize);
                file_handle
                    .by_ref()
                    .take((MAX_ENCRYPTED_KEY_FILE + 1) as u64)
                    .read_to_end(&mut encrypted)?;
                let key = decrypt_private_key(&encrypted, &wrapping_key)?;
                return Self::from_private_key(key);
            }
            Err(keyring::Error::NoEntry) => {}
            Err(error) => return Err(keystore_error(error)),
        }

        match std::fs::symlink_metadata(data_dir.join(ENCRYPTED_KEY_FILE)) {
            Ok(_) => {
                return Err(NativeError::Security(
                    "encrypted identity file has no keystore-held wrapping key".into(),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(NativeError::Io(error)),
        }

        let key = generate_private_key()?;
        let encoded_key = store_keystore_secret(key.as_slice())?;
        match direct.set_password(encoded_key.as_str()) {
            Ok(()) => Self::from_private_key(key),
            Err(_direct_error) => {
                let wrapping_key = random_wrapping_key()?;
                let wrapping_secret = store_keystore_secret(wrapping_key.as_slice())?;
                wrapping
                    .set_password(wrapping_secret.as_str())
                    .map_err(keystore_error)?;
                let encrypted = encrypt_private_key(&key, &wrapping_key)?;
                if let Err(error) = write_encrypted_key_file(data_dir, &encrypted) {
                    let _ = wrapping.delete_credential();
                    return Err(NativeError::Io(error));
                }
                Self::from_private_key(key)
            }
        }
    }

    /// Return the DER certificate presented during TLS authentication.
    pub(crate) fn cert(&self) -> rustls::pki_types::CertificateDer<'static> {
        self.certificate.clone()
    }

    /// Return a zeroizable rustls private-key value for immediate config setup.
    pub(crate) fn private_key(
        &self,
    ) -> Result<rustls::pki_types::PrivateKeyDer<'static>, NativeError> {
        Ok(rustls::pki_types::PrivatePkcs8KeyDer::from(self.private_key.to_vec()).into())
    }

    /// Lowercase hexadecimal SHA-256 of the certificate SubjectPublicKeyInfo.
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// Stable uppercase hexadecimal fingerprint grouped in blocks of four.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    fn from_private_key(private_key: Zeroizing<Vec<u8>>) -> Result<Self, NativeError> {
        if private_key.len() != 83 {
            return Err(NativeError::Security(
                "stored device identity key has an invalid size".into(),
            ));
        }
        let signing_key = Ed25519KeyPair::from_pkcs8(&private_key)
            .map_err(|_| NativeError::Security("stored device identity key is invalid".into()))?;
        let signing_key = RcgenKey(signing_key);
        let params = certificate_params();
        let certificate = params.self_signed(&signing_key).map_err(|error| {
            NativeError::Internal(format!("could not issue device certificate: {error}"))
        })?;
        let certificate = certificate.der().clone();
        let computed = device_id_from_certificate(&certificate).ok_or_else(|| {
            NativeError::Security("generated device certificate is invalid".into())
        })?;
        Ok(Self::with_certificate(private_key, certificate, computed))
    }

    fn with_certificate(
        private_key: Zeroizing<Vec<u8>>,
        certificate: rustls::pki_types::CertificateDer<'static>,
        device_id: String,
    ) -> Self {
        let fingerprint = grouped_fingerprint(&device_id);
        Self {
            certificate,
            private_key,
            device_id,
            fingerprint,
        }
    }

    #[cfg(test)]
    pub(crate) fn ephemeral() -> Result<Self, NativeError> {
        Self::from_private_key(generate_private_key()?)
    }
}

/// Persistent public namespace only: no key material is written here. The caller
/// must exclusively own the protected data directory, just as for trust storage.
fn identity_namespace(data_dir: &Path) -> Result<String, NativeError> {
    use std::io::Write;
    let path = data_dir.join("identity.namespace");
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if !metadata.is_file() || metadata.len() != 64 {
                return Err(NativeError::Security(
                    "invalid identity namespace file".into(),
                ));
            }
            let mut value = String::new();
            std::fs::File::open(path)?
                .take(65)
                .read_to_string(&mut value)?;
            if !valid_device_id(&value) {
                return Err(NativeError::Security("invalid identity namespace".into()));
            }
            Ok(value)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut random = [0; 32];
            SystemRandom::new()
                .fill(&mut random)
                .map_err(|_| NativeError::Security("namespace randomness unavailable".into()))?;
            let value = hex::encode(random);
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)?;
            file.write_all(value.as_bytes())?;
            file.sync_all()?;
            #[cfg(unix)]
            std::fs::File::open(data_dir)?.sync_all()?;
            Ok(value)
        }
        Err(error) => Err(error.into()),
    }
}

enum IdentityEntry {
    Native(keyring::Entry),
    #[cfg(feature = "test-support")]
    Test(TestKeyStore, String),
}

impl IdentityEntry {
    fn get_password(&self) -> Result<String, keyring::Error> {
        match self {
            Self::Native(entry) => entry.get_password(),
            #[cfg(feature = "test-support")]
            Self::Test(store, name) => store
                .0
                .lock()
                .map_err(|_| keyring::Error::NoEntry)?
                .get(name)
                .map(|s| s.to_string())
                .ok_or(keyring::Error::NoEntry),
        }
    }
    fn set_password(&self, value: &str) -> Result<(), keyring::Error> {
        match self {
            Self::Native(entry) => entry.set_password(value),
            #[cfg(feature = "test-support")]
            Self::Test(store, name) => {
                store
                    .0
                    .lock()
                    .map_err(|_| keyring::Error::NoEntry)?
                    .insert(name.clone(), Zeroizing::new(value.to_owned()));
                Ok(())
            }
        }
    }
    fn delete_credential(&self) -> Result<(), keyring::Error> {
        match self {
            Self::Native(entry) => entry.delete_credential(),
            #[cfg(feature = "test-support")]
            Self::Test(store, name) => {
                store
                    .0
                    .lock()
                    .map_err(|_| keyring::Error::NoEntry)?
                    .remove(name);
                Ok(())
            }
        }
    }
}

/// Isolated in-process credential provider for downstream authenticated tests.
/// Shares only within clones; drop erases secrets. Never bypasses TLS or pairing.
#[cfg(feature = "test-support")]
#[derive(Clone, Default)]
pub struct TestKeyStore(
    Arc<std::sync::Mutex<std::collections::HashMap<String, Zeroizing<String>>>>,
);

#[cfg(feature = "test-support")]
impl TestKeyStore {
    pub fn load_or_create(&self, data_dir: &Path) -> Result<NativeIdentity, NativeError> {
        std::fs::create_dir_all(data_dir)?;
        let namespace = identity_namespace(data_dir)?;
        NativeIdentity::load_with_entries(
            data_dir,
            IdentityEntry::Test(self.clone(), format!("{namespace}-{DIRECT_KEY_NAME}")),
            IdentityEntry::Test(self.clone(), format!("{namespace}-{WRAPPING_KEY_NAME}")),
        )
    }
}

struct RcgenKey(Ed25519KeyPair);

impl PublicKeyData for RcgenKey {
    fn der_bytes(&self) -> &[u8] {
        use ring::signature::KeyPair as _;
        self.0.public_key().as_ref()
    }

    fn algorithm(&self) -> &'static rcgen::SignatureAlgorithm {
        &PKCS_ED25519
    }
}

impl RcgenSigningKey for RcgenKey {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rcgen::Error> {
        Ok(self.0.sign(message).as_ref().to_vec())
    }
}

/// Mutable, shared set of trusted device IDs. A poisoned lock always denies trust.
#[derive(Clone, Default)]
pub struct PinSet(Arc<RwLock<HashSet<String>>>);

impl fmt::Debug for PinSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PinSet").finish_non_exhaustive()
    }
}

impl PinSet {
    pub(crate) fn try_contains(&self, device_id: &str) -> Result<bool, crate::LinkError> {
        if !valid_device_id(device_id) {
            return Ok(false);
        }
        match self.0.try_read() {
            Ok(pins) => Ok(pins.contains(device_id)),
            Err(std::sync::TryLockError::WouldBlock) => Err(crate::LinkError::Busy),
            Err(std::sync::TryLockError::Poisoned(_)) => Err(crate::LinkError::Closed),
        }
    }
    pub fn new() -> Self {
        Self::default()
    }

    /// Return false on invalid IDs or a poisoned lock.
    pub fn contains(&self, device_id: &str) -> bool {
        if !valid_device_id(device_id) {
            return false;
        }
        self.0
            .read()
            .map(|pins| pins.contains(device_id))
            .unwrap_or(false)
    }

    pub fn insert(&self, device_id: &str) -> Result<(), NativeError> {
        if !valid_device_id(device_id) {
            return Err(NativeError::Security("invalid device ID pin".into()));
        }
        self.0
            .write()
            .map_err(|_| NativeError::Internal("device pin set lock is poisoned".into()))?
            .insert(device_id.to_owned());
        Ok(())
    }

    pub fn remove(&self, device_id: &str) -> Result<bool, NativeError> {
        if !valid_device_id(device_id) {
            return Err(NativeError::Security("invalid device ID pin".into()));
        }
        Ok(self
            .0
            .write()
            .map_err(|_| NativeError::Internal("device pin set lock is poisoned".into()))?
            .remove(device_id))
    }

    pub fn list(&self) -> Result<Vec<String>, NativeError> {
        let mut pins = self
            .0
            .read()
            .map_err(|_| NativeError::Internal("device pin set lock is poisoned".into()))?
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        pins.sort_unstable();
        Ok(pins)
    }
}

pub(crate) fn device_id_from_certificate(
    certificate: &rustls::pki_types::CertificateDer<'_>,
) -> Option<String> {
    let spki = subject_public_key_info(certificate.as_ref())?;
    Some(device_id_for_spki(spki))
}

pub(crate) fn valid_device_id(device_id: &str) -> bool {
    device_id.len() == 64
        && device_id
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

fn device_id_for_spki(spki: &[u8]) -> String {
    hex::encode(Sha256::digest(spki))
}

pub(crate) fn grouped_fingerprint(device_id: &str) -> String {
    device_id
        .to_ascii_uppercase()
        .as_bytes()
        .chunks(4)
        .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

fn certificate_params() -> CertificateParams {
    let mut params = CertificateParams::default();
    params
        .distinguished_name
        .push(DnType::CommonName, "Glide device");
    params
}

fn generate_private_key() -> Result<Zeroizing<Vec<u8>>, NativeError> {
    let mut seed = Zeroizing::new([0u8; 32]);
    SystemRandom::new()
        .fill(&mut *seed)
        .map_err(|_| NativeError::Internal("device identity key generation failed".into()))?;
    pkcs8_from_seed(&seed)
}

fn pkcs8_from_seed(seed: &[u8; 32]) -> Result<Zeroizing<Vec<u8>>, NativeError> {
    use ring::signature::KeyPair as _;

    let key = Ed25519KeyPair::from_seed_unchecked(seed)
        .map_err(|_| NativeError::Internal("device identity key generation failed".into()))?;
    // Ed25519 PKCS#8 v2 has fixed-width fields; build it directly so every
    // private DER byte stays in a zeroizing buffer instead of ring's Document.
    let mut private_key = Zeroizing::new(Vec::with_capacity(83));
    private_key.extend_from_slice(&[
        0x30, 0x51, 0x02, 0x01, 0x01, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20,
    ]);
    private_key.extend_from_slice(seed);
    private_key.extend_from_slice(&[0x81, 0x21, 0x00]);
    private_key.extend_from_slice(key.public_key().as_ref());
    Ok(private_key)
}

fn random_wrapping_key() -> Result<Zeroizing<[u8; 32]>, NativeError> {
    let mut key = Zeroizing::new([0u8; 32]);
    SystemRandom::new()
        .fill(&mut *key)
        .map_err(|_| NativeError::Internal("identity wrapping key generation failed".into()))?;
    Ok(key)
}

fn decode_wrapping_key(encoded: Zeroizing<Vec<u8>>) -> Result<Zeroizing<[u8; 32]>, NativeError> {
    if encoded.len() != 32 {
        return Err(NativeError::Security(
            "stored identity wrapping key has an invalid length".into(),
        ));
    }
    let mut key = Zeroizing::new([0u8; 32]);
    key.copy_from_slice(&encoded);
    Ok(key)
}

fn decode_hex_secret(encoded: &str, max_bytes: usize) -> Result<Zeroizing<Vec<u8>>, NativeError> {
    if !encoded.len().is_multiple_of(2) || encoded.len() / 2 > max_bytes {
        return Err(NativeError::Security(
            "stored secret has an invalid size".into(),
        ));
    }
    let mut decoded = Zeroizing::new(vec![0u8; encoded.len() / 2]);
    hex::decode_to_slice(encoded, &mut decoded)
        .map_err(|_| NativeError::Security("stored secret is malformed".into()))?;
    Ok(decoded)
}

#[cfg(windows)]
fn store_keystore_secret(secret: &[u8]) -> Result<Zeroizing<String>, NativeError> {
    let protected = dpapi_protect(secret)?;
    Ok(Zeroizing::new(hex::encode(protected.as_slice())))
}

#[cfg(not(windows))]
fn store_keystore_secret(secret: &[u8]) -> Result<Zeroizing<String>, NativeError> {
    Ok(Zeroizing::new(hex::encode(secret)))
}

#[cfg(windows)]
fn load_keystore_secret(encoded: &str) -> Result<Zeroizing<Vec<u8>>, NativeError> {
    if encoded.len() > MAX_KEYRING_SECRET_HEX {
        return Err(NativeError::Security(
            "stored keystore secret exceeds its size limit".into(),
        ));
    }
    let stored = decode_hex_secret(encoded, MAX_KEYRING_SECRET_BYTES)?;
    // Never migrate an old raw key on Windows: a roaming Credential Manager
    // entry may already have cloned that identity onto another device.
    dpapi_unprotect(&stored)
}

#[cfg(not(windows))]
fn load_keystore_secret(encoded: &str) -> Result<Zeroizing<Vec<u8>>, NativeError> {
    if encoded.len() > MAX_KEYRING_SECRET_HEX {
        return Err(NativeError::Security(
            "stored keystore secret exceeds its size limit".into(),
        ));
    }
    decode_hex_secret(encoded, MAX_KEYRING_SECRET_BYTES)
}

#[cfg(windows)]
const MAX_DPAPI_BLOB_BYTES: usize = 2048;

#[cfg(windows)]
fn dpapi_protect(secret: &[u8]) -> Result<Zeroizing<Vec<u8>>, NativeError> {
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CRYPTPROTECT_LOCAL_MACHINE, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    if secret.is_empty() || secret.len() > MAX_KEYRING_SECRET_BYTES {
        return Err(NativeError::Security(
            "secret exceeds the Windows keystore protection limit".into(),
        ));
    }
    let input_len = u32::try_from(secret.len())
        .map_err(|_| NativeError::Security("secret exceeds the DPAPI size limit".into()))?;
    let input = CRYPT_INTEGER_BLOB {
        cbData: input_len,
        pbData: secret.as_ptr().cast_mut(),
    };
    let mut output = DpapiOutput(CRYPT_INTEGER_BLOB::default());
    // SAFETY: `input.pbData` references the bounded `secret` slice for the
    // synchronous call; all optional pointers are null as permitted, and the
    // output blob is initialized and owned by `DpapiOutput` for LocalFree.
    let succeeded = unsafe {
        CryptProtectData(
            &input,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_LOCAL_MACHINE | CRYPTPROTECT_UI_FORBIDDEN,
            &mut output.0,
        )
    };
    if succeeded == 0 {
        return Err(dpapi_error("protection"));
    }
    output.copy_to_zeroizing(MAX_DPAPI_BLOB_BYTES)
}

#[cfg(windows)]
fn dpapi_unprotect(ciphertext: &[u8]) -> Result<Zeroizing<Vec<u8>>, NativeError> {
    use windows_sys::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    if ciphertext.is_empty() || ciphertext.len() > MAX_DPAPI_BLOB_BYTES {
        return Err(NativeError::Security(
            "stored Windows keystore blob has an invalid size".into(),
        ));
    }
    let input_len = u32::try_from(ciphertext.len())
        .map_err(|_| NativeError::Security("DPAPI blob exceeds its size limit".into()))?;
    let input = CRYPT_INTEGER_BLOB {
        cbData: input_len,
        pbData: ciphertext.as_ptr().cast_mut(),
    };
    let mut output = DpapiOutput(CRYPT_INTEGER_BLOB::default());
    // SAFETY: `input.pbData` references the bounded `ciphertext` slice for the
    // synchronous call; optional pointers are null and output ownership stays
    // with the RAII wrapper, which wipes and frees any returned buffer.
    let succeeded = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output.0,
        )
    };
    if succeeded == 0 {
        return Err(dpapi_error("unprotection"));
    }
    output.copy_to_zeroizing(MAX_KEYRING_SECRET_BYTES)
}

#[cfg(windows)]
struct DpapiOutput(windows_sys::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB);

#[cfg(windows)]
impl DpapiOutput {
    fn copy_to_zeroizing(&self, max_bytes: usize) -> Result<Zeroizing<Vec<u8>>, NativeError> {
        let length = self.0.cbData as usize;
        if self.0.pbData.is_null() || length == 0 || length > max_bytes {
            return Err(NativeError::Security(
                "Windows DPAPI returned an invalid buffer".into(),
            ));
        }
        let mut copy = Zeroizing::new(Vec::with_capacity(length));
        // SAFETY: a successful DPAPI call allocated `cbData` readable bytes at
        // `pbData`; the length was checked above and the RAII wrapper retains
        // ownership until its Drop implementation wipes and frees the buffer.
        let source = unsafe { std::slice::from_raw_parts(self.0.pbData, length) };
        copy.extend_from_slice(source);
        Ok(copy)
    }
}

#[cfg(windows)]
impl Drop for DpapiOutput {
    fn drop(&mut self) {
        use windows_sys::Win32::{
            Foundation::{LocalFree, HLOCAL},
            Security::Cryptography::CRYPT_INTEGER_BLOB,
        };

        if self.0.pbData.is_null() {
            return;
        }
        if (self.0.cbData as usize) <= MAX_DPAPI_BLOB_BYTES {
            // SAFETY: CryptProtectData/CryptUnprotectData allocated this buffer
            // and set `cbData`; the explicit cap prevents trusting an invalid
            // length while wiping bytes before LocalFree.
            unsafe {
                std::slice::from_raw_parts_mut(self.0.pbData, self.0.cbData as usize).zeroize();
            }
        }
        // SAFETY: DPAPI documents LocalFree as the deallocator for its output
        // DATA_BLOB. The pointer is checked non-null and freed exactly once.
        let _ = unsafe { LocalFree(self.0.pbData.cast::<core::ffi::c_void>() as HLOCAL) };
        self.0 = CRYPT_INTEGER_BLOB::default();
    }
}

#[cfg(windows)]
fn dpapi_error(operation: &str) -> NativeError {
    // SAFETY: GetLastError has no pointer arguments and returns this thread's
    // last Win32 status immediately following the failed DPAPI call.
    let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
    NativeError::Security(format!("Windows DPAPI {operation} failed (error {code})"))
}

fn encrypt_private_key(
    private_key: &[u8],
    wrapping_key: &[u8; 32],
) -> Result<Vec<u8>, NativeError> {
    let unbound = UnboundKey::new(&AES_256_GCM, wrapping_key)
        .map_err(|_| NativeError::Internal("identity encryption setup failed".into()))?;
    let key = LessSafeKey::new(unbound);
    let mut nonce_bytes = [0u8; NONCE_LEN];
    SystemRandom::new()
        .fill(&mut nonce_bytes)
        .map_err(|_| NativeError::Internal("identity encryption nonce generation failed".into()))?;
    let nonce = aead::Nonce::assume_unique_for_key(nonce_bytes);
    let mut encrypted = Zeroizing::new(private_key.to_vec());
    key.seal_in_place_append_tag(
        nonce,
        Aad::from(ENCRYPTED_FILE_MAGIC.as_slice()),
        &mut *encrypted,
    )
    .map_err(|_| NativeError::Internal("device identity encryption failed".into()))?;
    let mut output =
        Vec::with_capacity(ENCRYPTED_FILE_MAGIC.len() + nonce_bytes.len() + encrypted.len());
    output.extend_from_slice(ENCRYPTED_FILE_MAGIC);
    output.extend_from_slice(&nonce_bytes);
    output.extend_from_slice(encrypted.as_slice());
    Ok(output)
}

fn decrypt_private_key(
    encrypted: &[u8],
    wrapping_key: &[u8; 32],
) -> Result<Zeroizing<Vec<u8>>, NativeError> {
    if encrypted.len() > MAX_ENCRYPTED_KEY_FILE
        || encrypted.len() < ENCRYPTED_FILE_MAGIC.len() + NONCE_LEN + 16
        || !encrypted.starts_with(ENCRYPTED_FILE_MAGIC)
    {
        return Err(NativeError::Security(
            "encrypted identity file is malformed".into(),
        ));
    }
    let nonce_start = ENCRYPTED_FILE_MAGIC.len();
    let mut nonce_bytes = [0u8; NONCE_LEN];
    nonce_bytes.copy_from_slice(&encrypted[nonce_start..nonce_start + NONCE_LEN]);
    let nonce = aead::Nonce::assume_unique_for_key(nonce_bytes);
    let unbound = UnboundKey::new(&AES_256_GCM, wrapping_key)
        .map_err(|_| NativeError::Internal("identity decryption setup failed".into()))?;
    let key = LessSafeKey::new(unbound);
    let mut plaintext = Zeroizing::new(encrypted[nonce_start + NONCE_LEN..].to_vec());
    let plain = key
        .open_in_place(
            nonce,
            Aad::from(ENCRYPTED_FILE_MAGIC.as_slice()),
            &mut plaintext,
        )
        .map_err(|_| NativeError::Security("encrypted identity authentication failed".into()))?;
    let plain_len = plain.len();
    plaintext.truncate(plain_len);
    Ok(plaintext)
}

#[cfg(any(windows, target_os = "macos"))]
fn keystore_entry(name: &str) -> Result<keyring::Entry, NativeError> {
    keyring::Entry::new(KEYRING_SERVICE, name).map_err(keystore_error)
}

#[cfg(any(windows, target_os = "macos"))]
fn keystore_error(error: keyring::Error) -> NativeError {
    let reason = match error {
        keyring::Error::BadEncoding(mut secret) => {
            secret.zeroize();
            "stored credential is not UTF-8"
        }
        keyring::Error::BadDataFormat(mut secret, _) => {
            secret.zeroize();
            "stored credential has an unexpected format"
        }
        keyring::Error::NoStorageAccess(_) => "secure storage is locked or access is denied",
        keyring::Error::NoEntry => "required secure credential is missing",
        keyring::Error::TooLong(_, _) => "credential exceeds the secure storage limit",
        keyring::Error::Ambiguous(_) => "multiple matching secure credentials exist",
        _ => "secure storage backend failed",
    };
    NativeError::Security(format!("OS keystore unavailable: {reason}"))
}

#[cfg(windows)]
fn write_encrypted_key_file(data_dir: &Path, encrypted: &[u8]) -> Result<(), std::io::Error> {
    use std::os::windows::fs::OpenOptionsExt;

    let target = data_dir.join(ENCRYPTED_KEY_FILE);
    let temp = data_dir.join(format!("{ENCRYPTED_KEY_FILE}.tmp"));
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .share_mode(0)
        .open(&temp)?;
    use std::io::Write;
    file.write_all(encrypted)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(temp, target)
}

#[cfg(target_os = "macos")]
fn write_encrypted_key_file(data_dir: &Path, encrypted: &[u8]) -> Result<(), std::io::Error> {
    use std::{fs::OpenOptions, io::Write, os::unix::fs::OpenOptionsExt};

    let target = data_dir.join(ENCRYPTED_KEY_FILE);
    let temp = data_dir.join(format!("{ENCRYPTED_KEY_FILE}.tmp"));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temp)?;
    file.write_all(encrypted)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(temp, target)
}

fn subject_public_key_info(certificate: &[u8]) -> Option<&[u8]> {
    if certificate.len() > MAX_CERTIFICATE_BYTES {
        return None;
    }
    let mut outer = DerReader::new(certificate);
    let certificate_sequence = outer.read(0x30)?;
    if !outer.is_empty() {
        return None;
    }
    let mut fields = DerReader::new(certificate_sequence.value);
    let tbs = fields.read(0x30)?;
    fields.read(0x30)?;
    fields.read(0x03)?;
    if !fields.is_empty() {
        return None;
    }
    let mut tbs_fields = DerReader::new(tbs.value);
    if tbs_fields.peek_tag()? == 0xa0 {
        tbs_fields.read(0xa0)?;
    }
    tbs_fields.read(0x02)?;
    tbs_fields.read(0x30)?;
    tbs_fields.read(0x30)?;
    tbs_fields.read(0x30)?;
    tbs_fields.read(0x30)?;
    let spki = tbs_fields.read(0x30)?;
    if !supported_spki(spki.encoded) {
        return None;
    }
    Some(spki.encoded)
}

fn supported_spki(spki: &[u8]) -> bool {
    let mut outer = DerReader::new(spki);
    let Some(sequence) = outer.read(0x30) else {
        return false;
    };
    if !outer.is_empty() {
        return false;
    }
    let mut fields = DerReader::new(sequence.value);
    let Some(algorithm) = fields.read(0x30) else {
        return false;
    };
    let Some(public_key) = fields.read(0x03) else {
        return false;
    };
    if !fields.is_empty() || public_key.value.first() != Some(&0) {
        return false;
    }
    let mut algorithm_fields = DerReader::new(algorithm.value);
    let Some(oid) = algorithm_fields.read(0x06) else {
        return false;
    };
    match oid.value {
        [0x2b, 0x65, 0x70] => algorithm_fields.is_empty() && public_key.value.len() == 33,
        [0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01] => {
            let Some(curve_oid) = algorithm_fields.read(0x06) else {
                return false;
            };
            algorithm_fields.is_empty()
                && curve_oid.value == [0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07]
                && public_key.value.len() == 66
        }
        _ => false,
    }
}

struct DerValue<'a> {
    value: &'a [u8],
    encoded: &'a [u8],
}

struct DerReader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> DerReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn is_empty(&self) -> bool {
        self.position == self.bytes.len()
    }

    fn peek_tag(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }

    fn read(&mut self, expected_tag: u8) -> Option<DerValue<'a>> {
        let start = self.position;
        let tag = *self.bytes.get(self.position)?;
        if tag != expected_tag {
            return None;
        }
        self.position += 1;
        let first_length = *self.bytes.get(self.position)?;
        self.position += 1;
        let length = if first_length & 0x80 == 0 {
            usize::from(first_length)
        } else {
            let count = usize::from(first_length & 0x7f);
            if count == 0 || count > std::mem::size_of::<usize>() {
                return None;
            }
            let length_bytes = self.bytes.get(self.position..self.position + count)?;
            if length_bytes.first()? == &0 {
                return None;
            }
            let mut value = 0usize;
            for byte in length_bytes {
                value = value.checked_mul(256)?.checked_add(usize::from(*byte))?;
            }
            if value < 128 {
                return None;
            }
            self.position += count;
            value
        };
        let end = self.position.checked_add(length)?;
        let value = self.bytes.get(self.position..end)?;
        let encoded = self.bytes.get(start..end)?;
        self.position = end;
        Some(DerValue { value, encoded })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_id_is_spki_hash_and_fingerprint_is_grouped() {
        let identity = NativeIdentity::ephemeral().expect("test identity generation");
        assert_eq!(
            device_id_from_certificate(&identity.cert()).as_deref(),
            Some(identity.device_id())
        );
        assert_eq!(
            identity.fingerprint().replace(' ', "").to_lowercase(),
            identity.device_id()
        );
        assert!(identity
            .fingerprint()
            .split(' ')
            .all(|group| group.len() == 4));
        assert_eq!(identity.fingerprint().split(' ').count(), 16);
    }

    /// Pins the device ID / fingerprint derivation (SHA-256 of the SPKI) for a
    /// fixed key. Expected values were recorded with the pre-upgrade dependency
    /// set (sha2 0.10.9) and must never change: existing pins depend on them.
    #[test]
    fn device_id_and_fingerprint_known_answer() {
        let private_key = pkcs8_from_seed(&[0x42; 32]).expect("fixed test key");
        let pkcs8_hex = hex::encode(private_key.as_slice());
        let identity = NativeIdentity::from_private_key(private_key).expect("fixed identity");
        let spki_hash = device_id_for_spki(b"glide sha-256 known answer");
        assert_eq!(
            pkcs8_hex,
            "3051020101300506032b657004220420424242424242424242424242424242424242424242424242\
             42424242424242428121002152f8d19b791d24453242e15f2eab6cb7cffa7b6a5ed30097960e0698\
             81db12"
        );
        assert_eq!(
            identity.device_id(),
            "9a82517f9af19416d98fdbcf193726b3a95c0b6fec1d51884bf3e1b739ba2ef4"
        );
        assert_eq!(
            identity.fingerprint(),
            "9A82 517F 9AF1 9416 D98F DBCF 1937 26B3 A95C 0B6F EC1D 5188 4BF3 E1B7 39BA 2EF4"
        );
        // Independently cross-checked with Python's hashlib.sha256.
        assert_eq!(
            spki_hash,
            "38180515d3983bb8afde6d0f6057824a2686fd9dbaecb2e2c378f3c8ae71b435"
        );
    }

    #[test]
    fn peer_certificate_parser_accepts_p256_identity_keys() {
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
            .expect("test ECDSA key generation");
        let mut params = CertificateParams::default();
        params
            .distinguished_name
            .push(DnType::CommonName, "Glide test device");
        let certificate = params.self_signed(&key).expect("test ECDSA certificate");
        assert!(device_id_from_certificate(certificate.der()).is_some());
    }

    #[test]
    fn pin_set_rejects_noncanonical_ids_and_sorts_results() {
        let pins = PinSet::new();
        let b = "b".repeat(64);
        let a = "a".repeat(64);
        assert!(pins.insert(&b).is_ok());
        assert!(pins.insert(&a).is_ok());
        assert!(pins.contains(&a));
        assert_eq!(pins.list().expect("pin list"), vec![a.clone(), b.clone()]);
        assert!(pins.remove(&a).expect("pin removal"));
        assert!(!pins.contains(&a));
        assert!(pins.insert(&"A".repeat(64)).is_err());
    }

    #[test]
    fn poisoned_pin_set_never_grants_trust() {
        let pins = PinSet::new();
        let poisoned = pins.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poisoned.0.write().expect("unpoisoned test lock");
            panic!("poison the test lock");
        })
        .join();
        assert!(!pins.contains(&"a".repeat(64)));
        assert!(pins.list().is_err());
        assert!(pins.insert(&"a".repeat(64)).is_err());
    }

    #[test]
    fn encrypted_file_round_trip_and_tamper_rejection() {
        let wrapping_key = random_wrapping_key().expect("wrapping key");
        let private_key = generate_private_key().expect("private key");
        let encrypted = encrypt_private_key(&private_key, &wrapping_key).expect("encrypt");
        let decrypted = decrypt_private_key(&encrypted, &wrapping_key).expect("decrypt");
        assert_eq!(decrypted.as_slice(), private_key.as_slice());
        let mut tampered = encrypted;
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        assert!(decrypt_private_key(&tampered, &wrapping_key).is_err());
    }

    #[test]
    fn keystore_hex_decoding_is_bounded() {
        let secret = decode_hex_secret(&"ab".repeat(32), 32).expect("valid wrapped key");
        assert_eq!(secret.as_slice(), [0xab; 32]);
        assert!(decode_hex_secret("xyz", 32).is_err());
        assert!(decode_hex_secret(&"aa".repeat(33), 32).is_err());
    }

    #[test]
    fn malformed_certificate_der_is_rejected() {
        assert!(subject_public_key_info(&[0x30, 0x80, 0x00, 0x00]).is_none());
        assert!(subject_public_key_info(&[0x30, 0x81, 0x01, 0x00]).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_machine_scope_round_trip_and_wrong_ciphertext_rejection() {
        let secret = Zeroizing::new(vec![0x5a; 83]);
        let protected = dpapi_protect(&secret).expect("DPAPI protection");
        assert_ne!(protected.as_slice(), secret.as_slice());
        let recovered = dpapi_unprotect(&protected).expect("DPAPI unprotection");
        assert_eq!(recovered.as_slice(), secret.as_slice());

        let mut modified = protected.to_vec();
        let middle = modified.len() / 2;
        modified[middle] ^= 1;
        assert!(dpapi_unprotect(&modified).is_err());
    }

    #[cfg(windows)]
    struct TestCredential(keyring::Entry);

    #[cfg(windows)]
    impl Drop for TestCredential {
        fn drop(&mut self) {
            let _ = self.0.delete_credential();
        }
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "writes and removes a uniquely named temporary Credential Manager entry"]
    fn live_credential_store_round_trip_uses_machine_bound_ciphertext() {
        let mut suffix = [0u8; 16];
        SystemRandom::new()
            .fill(&mut suffix)
            .expect("test-only unique credential name randomness");
        let service = format!(
            "com.glide.identity.test.{}.{}",
            std::process::id(),
            hex::encode(suffix)
        );
        let entry =
            keyring::Entry::new(&service, "round-trip").expect("temporary credential entry");
        let credential = TestCredential(entry);
        let secret = Zeroizing::new(vec![0x93; 83]);
        let encoded = store_keystore_secret(&secret).expect("machine-bound encoding");
        credential
            .0
            .set_password(encoded.as_str())
            .expect("temporary credential write");
        let stored = Zeroizing::new(
            credential
                .0
                .get_password()
                .expect("temporary credential read"),
        );
        let recovered = load_keystore_secret(&stored).expect("machine-bound credential decode");
        assert_eq!(recovered.as_slice(), secret.as_slice());
    }

    /// Raw Win32 access to generic credentials, used to reproduce exactly what
    /// keyring 3.6.3 (the release before the keyring 4 upgrade) stored.
    #[cfg(windows)]
    mod legacy_credential {
        use windows_sys::Win32::{
            Foundation::FILETIME,
            Security::Credentials::{
                CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_ENTERPRISE,
                CRED_TYPE_GENERIC,
            },
        };
        use zeroize::{Zeroize, Zeroizing};

        /// keyring 3.6.3 stamped this comment on every credential it created.
        pub(super) const KEYRING3_COMMENT: &str = "keyring v3.6.3";

        /// keyring 3.6.3 `WinCredential::new_with_target(None, service, user)`.
        pub(super) fn keyring3_target(service: &str, user: &str) -> String {
            format!("{user}.{service}")
        }

        pub(super) fn utf16_le(value: &str) -> Vec<u8> {
            value.encode_utf16().flat_map(u16::to_le_bytes).collect()
        }

        fn wide(value: &str) -> Vec<u16> {
            value.encode_utf16().chain(std::iter::once(0)).collect()
        }

        /// # Safety
        /// `value` must be null or point to a NUL-terminated UTF-16 string.
        unsafe fn from_wide(value: *const u16) -> String {
            if value.is_null() {
                return String::new();
            }
            let mut length = 0;
            // SAFETY: guaranteed NUL-terminated by the caller.
            while unsafe { *value.add(length) } != 0 {
                length += 1;
            }
            // SAFETY: `length` UTF-16 units precede the terminator.
            String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(value, length) })
        }

        /// Write a generic credential with every field keyring 3.6.3's
        /// `WinCredential::save_credential` set: target `{user}.{service}`,
        /// user name, its version comment, empty alias, no attributes,
        /// enterprise persistence and a UTF-16LE password blob.
        pub(super) fn write_keyring3(service: &str, user: &str, password: &str) {
            let mut target = wide(&keyring3_target(service, user));
            let mut username = wide(user);
            let mut alias = wide("");
            let mut comment = wide(KEYRING3_COMMENT);
            let mut blob = utf16_le(password);
            let credential = CREDENTIALW {
                Flags: 0,
                Type: CRED_TYPE_GENERIC,
                TargetName: target.as_mut_ptr(),
                Comment: comment.as_mut_ptr(),
                LastWritten: FILETIME {
                    dwLowDateTime: 0,
                    dwHighDateTime: 0,
                },
                CredentialBlobSize: u32::try_from(blob.len()).expect("test blob size"),
                CredentialBlob: blob.as_mut_ptr(),
                Persist: CRED_PERSIST_ENTERPRISE,
                AttributeCount: 0,
                Attributes: std::ptr::null_mut(),
                TargetAlias: alias.as_mut_ptr(),
                UserName: username.as_mut_ptr(),
            };
            // SAFETY: every pointer references a live, NUL-terminated local
            // buffer (or the sized blob) for the duration of the call.
            let written = unsafe { CredWriteW(&credential, 0) };
            blob.zeroize();
            assert_ne!(written, 0, "temporary legacy credential write failed");
        }

        pub(super) struct RawCredential {
            pub(super) kind: u32,
            pub(super) persist: u32,
            pub(super) user: String,
            pub(super) comment: String,
            pub(super) blob: Zeroizing<Vec<u8>>,
        }

        pub(super) fn read(target: &str) -> Option<RawCredential> {
            let target = wide(target);
            let mut credential: *mut CREDENTIALW = std::ptr::null_mut();
            // SAFETY: `target` is NUL-terminated; on success the OS allocates
            // the credential, which is released with CredFree below.
            if unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) } == 0 {
                return None;
            }
            // SAFETY: CredReadW succeeded, so `credential` points to a valid
            // CREDENTIALW whose strings and blob stay valid until CredFree.
            let raw = unsafe {
                let value = &*credential;
                let blob = if value.CredentialBlob.is_null() {
                    Vec::new()
                } else {
                    std::slice::from_raw_parts(
                        value.CredentialBlob,
                        value.CredentialBlobSize as usize,
                    )
                    .to_vec()
                };
                RawCredential {
                    kind: value.Type,
                    persist: value.Persist,
                    user: from_wide(value.UserName),
                    comment: from_wide(value.Comment),
                    blob: Zeroizing::new(blob),
                }
            };
            // SAFETY: allocated by the successful CredReadW above; freed once.
            unsafe { CredFree(credential.cast()) };
            Some(raw)
        }

        pub(super) fn delete(target: &str) -> bool {
            let target = wide(target);
            // SAFETY: `target` is a NUL-terminated UTF-16 buffer.
            unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) != 0 }
        }

        /// Deletes a test-only credential when dropped, including on panic.
        pub(super) struct Cleanup(pub(super) String);

        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = delete(&self.0);
            }
        }

        pub(super) const GENERIC: u32 = CRED_TYPE_GENERIC;
        pub(super) const ENTERPRISE: u32 = CRED_PERSIST_ENTERPRISE;
    }

    /// Existing installs keep their identity across the keyring 3 -> 4 upgrade.
    /// A credential is written exactly as keyring 3.6.3 wrote it (raw Win32
    /// API, same target-name format and fields), and the upgraded keystore
    /// path must load that identity rather than generate a new one. The
    /// reverse direction (keyring 4 writes the layout keyring 3 reads) keeps a
    /// downgrade working too. Only uniquely named test credentials are touched.
    #[cfg(windows)]
    #[test]
    #[ignore = "writes and removes uniquely named temporary Credential Manager entries"]
    fn keyring3_credentials_are_read_by_upgraded_keystore() {
        use legacy_credential::*;

        let mut random = [0u8; 48];
        SystemRandom::new()
            .fill(&mut random)
            .expect("test-only unique credential name randomness");
        let service = format!(
            "com.glide.identity.test.{}.{}",
            std::process::id(),
            hex::encode(&random[..16])
        );
        let namespace = hex::encode(&random[16..]);
        let direct_user = format!("{namespace}-{DIRECT_KEY_NAME}");
        let wrapping_user = format!("{namespace}-{WRAPPING_KEY_NAME}");
        let reverse_user = "keyring4-write";
        let direct_target = keyring3_target(&service, &direct_user);
        let wrapping_target = keyring3_target(&service, &wrapping_user);
        let reverse_target = keyring3_target(&service, reverse_user);
        let _cleanup = [
            Cleanup(direct_target.clone()),
            Cleanup(wrapping_target.clone()),
            Cleanup(reverse_target.clone()),
        ];

        // The value the current release stores: DPAPI-wrapped PKCS#8, hex.
        let private_key = pkcs8_from_seed(&[0x42; 32]).expect("fixed test key");
        let stored = store_keystore_secret(&private_key).expect("machine-bound encoding");
        write_keyring3(&service, &direct_user, &stored);

        let data_dir = tempfile::tempdir().expect("test data directory");
        let identity = NativeIdentity::load_with_entries(
            data_dir.path(),
            IdentityEntry::Native(keyring::Entry::new(&service, &direct_user).expect("entry")),
            IdentityEntry::Native(keyring::Entry::new(&service, &wrapping_user).expect("entry")),
        )
        .expect("legacy identity loads");
        // Same identity as the known-answer key: found, not regenerated.
        assert_eq!(
            identity.device_id(),
            "9a82517f9af19416d98fdbcf193726b3a95c0b6fec1d51884bf3e1b739ba2ef4"
        );
        let legacy = read(&direct_target).expect("legacy credential is kept");
        assert_eq!(legacy.comment, KEYRING3_COMMENT);
        assert_eq!(legacy.blob.as_slice(), utf16_le(&stored).as_slice());
        assert!(read(&wrapping_target).is_none(), "no fallback key created");
        assert!(!data_dir.path().join(ENCRYPTED_KEY_FILE).exists());

        // keyring 4 writes the same layout, so a keyring 3 build reads it.
        let entry = keyring::Entry::new(&service, reverse_user).expect("entry");
        entry.set_password("ab01").expect("keyring 4 write");
        let written = read(&reverse_target).expect("keyring 4 credential target");
        assert_eq!(written.kind, GENERIC);
        assert_eq!(written.persist, ENTERPRISE);
        assert_eq!(written.user, reverse_user);
        assert_eq!(written.blob.as_slice(), utf16_le("ab01").as_slice());
        entry.delete_credential().expect("keyring 4 delete");
        assert!(read(&reverse_target).is_none());
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "creates independent real Credential Manager identities and removes their entries"]
    fn two_data_directories_have_independent_native_identities() {
        let a = tempfile::tempdir().expect("directory A");
        let b = tempfile::tempdir().expect("directory B");
        let mut credentials = Vec::new();
        for dir in [a.path(), b.path()] {
            let namespace = identity_namespace(dir).expect("public namespace");
            for name in [DIRECT_KEY_NAME, WRAPPING_KEY_NAME] {
                credentials.push(TestCredential(
                    keystore_entry(&format!("{namespace}-{name}")).expect("credential"),
                ));
            }
        }
        let first = NativeIdentity::load_or_create(a.path()).expect("real identity A");
        let second = NativeIdentity::load_or_create(b.path()).expect("real identity B");
        assert_ne!(first.device_id(), second.device_id());
        assert_eq!(
            first.device_id(),
            NativeIdentity::load_or_create(a.path())
                .expect("reload A")
                .device_id()
        );
        assert_eq!(
            second.device_id(),
            NativeIdentity::load_or_create(b.path())
                .expect("reload B")
                .device_id()
        );
        assert_eq!(
            std::fs::read(a.path().join("identity.namespace"))
                .expect("public file")
                .len(),
            64
        );
        assert!(
            !a.path().join(ENCRYPTED_KEY_FILE).exists(),
            "direct keystore path"
        );
    }

    #[cfg(feature = "test-support")]
    #[test]
    fn isolated_keystores_reload_only_their_own_namespaces() {
        let a = tempfile::tempdir().expect("directory A");
        let b = tempfile::tempdir().expect("directory B");
        let store = TestKeyStore::default();
        let first = store.load_or_create(a.path()).expect("identity A");
        let second = store.load_or_create(b.path()).expect("identity B");
        assert_ne!(first.device_id(), second.device_id());
        assert_eq!(
            first.device_id(),
            store
                .clone()
                .load_or_create(a.path())
                .expect("reload")
                .device_id()
        );
        assert_ne!(
            first.device_id(),
            TestKeyStore::default()
                .load_or_create(a.path())
                .expect("isolated provider")
                .device_id()
        );
    }
}
