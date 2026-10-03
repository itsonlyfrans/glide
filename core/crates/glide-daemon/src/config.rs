//! Validated configuration with same-directory atomic replacement.

use glide_proto::ipc::{Layout, Peer, Settings};
use serde::{Deserialize, Serialize};
use std::{fs, io::Write, path::Path};
use thiserror::Error;

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
type Writer = std::sync::Mutex<()>;
static CONFIG_WRITERS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, std::sync::Weak<Writer>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    /// Optional executable opened from the native tray, never used for control authentication.
    #[serde(default)]
    pub ui_path: Option<std::path::PathBuf>,
    pub settings: Settings,
    pub layout: Layout,
    #[serde(default)]
    pub layout_version: (u64, String),
    pub peers: Vec<Peer>,
    pub sharing_enabled: bool,
    /// Mock identity only. Production identity must be derived from a keystore-backed certificate.
    pub mock_device_id: String,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("configuration I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("configuration is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("configuration exceeds the size limit")]
    TooLarge,
}

impl Config {
    /// Load without replacing corrupt data. A missing file is the only first-run case.
    pub fn load(data_dir: &Path) -> Result<Option<Self>, ConfigError> {
        let path = data_dir.join("config.json");
        let file = match fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if file.metadata()?.len() > MAX_CONFIG_BYTES {
            return Err(ConfigError::TooLarge);
        }
        use std::io::Read;
        let mut bytes = Vec::new();
        file.take(MAX_CONFIG_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_CONFIG_BYTES {
            return Err(ConfigError::TooLarge);
        }
        Ok(Some(serde_json::from_slice(&bytes)?))
    }

    /// Flush a complete temporary file before atomically replacing config.json.
    /// Failure preserves the previous file. Never store private keys in this document.
    pub fn save(&self, data_dir: &Path) -> Result<(), ConfigError> {
        let bytes = serde_json::to_vec_pretty(self)?;
        if bytes.len() as u64 > MAX_CONFIG_BYTES {
            return Err(ConfigError::TooLarge);
        }
        fs::create_dir_all(data_dir)?;
        // Serialize only writers to the same file. A blocked directory must not stall
        // other daemon instances. Canonicalization also handles relative/symlink aliases.
        let writer = {
            let mut writers = CONFIG_WRITERS
                .lock()
                .map_err(|_| std::io::Error::other("config writer registry poisoned"))?;
            writers.retain(|_, writer| writer.strong_count() != 0);
            let entry = writers.entry(data_dir.canonicalize()?).or_default();
            let writer = entry
                .upgrade()
                .unwrap_or_else(|| std::sync::Arc::new(Writer::new(())));
            *entry = std::sync::Arc::downgrade(&writer);
            writer
        };
        let _writer = writer
            .lock()
            .map_err(|_| std::io::Error::other("config writer poisoned"))?;
        let mut file = tempfile::NamedTempFile::new_in(data_dir)?;
        file.write_all(&bytes)?;
        file.as_file().sync_all()?;
        let mut temporary = Some(file);
        retry_replacement(
            || match temporary
                .take()
                .expect("temporary file owned until persisted")
                .persist(data_dir.join("config.json"))
            {
                Ok(_) => Ok(()),
                Err(error) => {
                    temporary = Some(error.file);
                    Err(error.error)
                }
            },
            std::thread::sleep,
        )?;
        #[cfg(unix)]
        fs::File::open(data_dir)?.sync_all()?;
        Ok(())
    }
}

fn retry_replacement(
    mut replace: impl FnMut() -> std::io::Result<()>,
    mut wait: impl FnMut(std::time::Duration),
) -> std::io::Result<()> {
    for attempt in 0..=4 {
        match replace() {
            // Windows scanners/readers can briefly deny delete/replace. Retry only
            // these native errors, preserving the complete temp and previous config.
            Err(error)
                if cfg!(windows)
                    && matches!(error.raw_os_error(), Some(5 | 32 | 33))
                    && attempt < 4 =>
            {
                tracing::debug!(attempt, code = ?error.raw_os_error(), "retrying config replacement");
                wait(std::time::Duration::from_millis(5 << attempt));
            }
            result => return result,
        }
    }
    unreachable!("last attempt returns its error")
}

/// RFC 7396 object merge: nested objects merge; null removes a property.
pub fn merge_patch(target: &mut serde_json::Value, patch: &serde_json::Value) {
    if let serde_json::Value::Object(patch) = patch {
        if !target.is_object() {
            *target = serde_json::json!({});
        }
        if let Some(target) = target.as_object_mut() {
            for (key, value) in patch {
                if value.is_null() {
                    target.remove(key);
                } else {
                    merge_patch(target.entry(key).or_insert(serde_json::Value::Null), value);
                }
            }
        }
    } else {
        *target = patch.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_retries_only_transient_windows_errors_and_is_bounded() {
        for code in [5, 32, 33, 2] {
            let mut calls = 0;
            let mut delays = Vec::new();
            let result = retry_replacement(
                || {
                    calls += 1;
                    if calls < 3 {
                        Err(std::io::Error::from_raw_os_error(code))
                    } else {
                        Ok(())
                    }
                },
                |delay| delays.push(delay),
            );
            let transient = cfg!(windows) && code != 2;
            assert_eq!(result.is_ok(), transient);
            assert_eq!(calls, if transient { 3 } else { 1 });
            assert_eq!(delays.len(), if transient { 2 } else { 0 });
        }
        let mut calls = 0;
        let mut total = std::time::Duration::ZERO;
        assert!(retry_replacement(
            || {
                calls += 1;
                Err(std::io::Error::from_raw_os_error(32))
            },
            |delay| total += delay
        )
        .is_err());
        assert_eq!(calls, if cfg!(windows) { 5 } else { 1 });
        assert_eq!(total.as_millis(), if cfg!(windows) { 75 } else { 0 });
    }

    #[cfg(windows)]
    #[test]
    fn windows_replacement_recovers_when_reader_releases_delete_lock() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("config.json");
        fs::write(&path, b"old config").expect("old config");
        let mut reader = Some(
            fs::OpenOptions::new()
                .read(true)
                .share_mode(1)
                .open(&path)
                .expect("reader denies delete"),
        );
        let mut file = tempfile::NamedTempFile::new_in(dir.path()).expect("unique temp");
        file.write_all(b"complete new config").expect("write");
        file.as_file().sync_all().expect("sync");
        let mut temporary = Some(file);
        let mut retries = 0;
        retry_replacement(
            || match temporary.take().expect("temp").persist(&path) {
                Ok(_) => Ok(()),
                Err(error) => {
                    temporary = Some(error.file);
                    Err(error.error)
                }
            },
            |_| {
                retries += 1;
                drop(reader.take());
            },
        )
        .expect("replace after reader releases lock");
        assert_eq!(retries, 1, "real Windows replacement failed while held");
        assert_eq!(fs::read(&path).expect("config"), b"complete new config");
    }

    #[test]
    fn nested_patch_keeps_siblings() {
        let mut value = serde_json::json!({"a":{"b":1,"c":2},"d":3});
        merge_patch(&mut value, &serde_json::json!({"a":{"b":4},"d":null}));
        assert_eq!(value, serde_json::json!({"a":{"b":4,"c":2}}));
    }

    #[test]
    fn corrupt_and_oversized_files_are_not_overwritten() {
        let dir = tempfile::tempdir().expect("test directory");
        let path = dir.path().join("config.json");
        fs::write(&path, b"broken").expect("test file");
        assert!(matches!(
            Config::load(dir.path()),
            Err(ConfigError::Json(_))
        ));
        assert_eq!(fs::read(&path).expect("preserved file"), b"broken");
        fs::File::create(&path)
            .expect("file")
            .set_len(MAX_CONFIG_BYTES + 1)
            .expect("length");
        assert!(matches!(
            Config::load(dir.path()),
            Err(ConfigError::TooLarge)
        ));
    }
}
