//! Short rolling logs: `<name>.log` up to 1 MB, then `<name>.1.log` .. `<name>.3.log`; files older than 7 days are deleted.
//! Never logs keystrokes, clipboard contents or files; only what the engine writes to its own error output.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MAX_BYTES: u64 = 1024 * 1024;
const KEEP: u32 = 3;
const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);

pub struct RotatingLog {
    dir: PathBuf,
    name: String,
    lock: Mutex<()>,
}

impl RotatingLog {
    pub fn new(dir: PathBuf, name: &str) -> Self {
        let log = Self { dir, name: name.to_string(), lock: Mutex::new(()) };
        log.prune();
        log
    }

    fn path(&self, index: u32) -> PathBuf {
        if index == 0 {
            self.dir.join(format!("{}.log", self.name))
        } else {
            self.dir.join(format!("{}.{index}.log", self.name))
        }
    }

    pub fn write(&self, text: &str) {
        let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        if fs::create_dir_all(&self.dir).is_err() {
            return;
        }
        let current = self.path(0);
        if fs::metadata(&current).map(|m| m.len()).unwrap_or(0) > MAX_BYTES {
            for index in (1..KEEP).rev() {
                let _ = fs::rename(self.path(index), self.path(index + 1));
            }
            let _ = fs::rename(&current, self.path(1));
        }
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&current) {
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                let _ = writeln!(file, "{stamp} {line}");
            }
        }
    }

    pub fn prune(&self) {
        let Ok(entries) = fs::read_dir(&self.dir) else { return };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with(&self.name) || !name.ends_with(".log") {
                continue;
            }
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > MAX_AGE);
            if old {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}
