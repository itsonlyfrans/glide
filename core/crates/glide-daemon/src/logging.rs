//! Bounded, best-effort engine logging. Only audited static messages reach disk.
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::mpsc::{self, SyncSender},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tracing::{
    field::{Field, Visit},
    Event, Subscriber,
};
use tracing_subscriber::{layer::Context, Layer};

const ROTATE_BYTES: u64 = 1_000_000;
const RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const MAINTENANCE: Duration = Duration::from_secs(24 * 60 * 60);
const QUEUE: usize = 64;
// Do not add payloads, paths, OS error strings or Debug implementations here.
const MESSAGES: &[&str] = &[
    "engine starting",
    "peer shown offline while connected; resyncing",
    "engine stopped",
    "native tray unavailable; engine continues running",
    "could not open Glide window",
    "could not open logs folder",
    "configuration replacement failed",
    "could not read live pairing state",
    "could not reconcile pairing state",
    "peer registration pending or rejected",
    "received clipboard dropped: this computer's clipboard changed first",
    "received clipboard dropped: no longer allowed",
    "received clipboard could not be written by the system",
    "received clipboard was rejected by the system",
    "clipboard offer ignored: invalid or turned off here",
    "clipboard files not fetched: transfer link unavailable",
    "clipboard file transfer failed",
    "clipboard not shared: marked sensitive",
    "clipboard files not shared: unsupported or duplicate names",
    "cursor returned: this computer's own keyboard or mouse was used",
    "cursor returned: this computer's own keyboard was used",
    "cursor returned: this computer's own mouse moved",
    "cursor returned: a button on this computer's own mouse was used",
    "cursor returned: this computer's own scroll wheel was used",
    "cursor returned: return-home hotkey",
    "cursor returned: connection to the other computer was lost",
    "cursor returned: the other computer handed it back",
    "cursor returned: the other computer stopped answering",
    "cursor returned: this computer stopped capturing the mouse and keyboard",
    "cursor returned: screens, layout or settings changed",
    "cursor returned: the other computer could not take the cursor",
    "cursor returned: the other computer's mouse took over",
    "cursor returned: other reason",
    "clipboard transfer storage failed: permission denied",
    "clipboard transfer storage failed: folder or file missing",
    "clipboard transfer storage failed: already exists",
    "clipboard transfer storage failed: other",
    "clipboard transfer failed: size limit",
    "clipboard transfer failed: too many earlier copies kept",
    "clipboard transfer failed: path too long or too deep",
    "clipboard transfer failed: too many files",
    "clipboard transfer failed: too large",
    "clipboard transfer storage failed: name not allowed here",
    "clipboard transfer storage failed: disk full",
    "clipboard transfer storage failed: file in use",
    "clipboard transfer storage failed: invalid data",
    "clipboard transfer failed: disk space",
    "clipboard transfer failed: timed out",
    "clipboard transfer failed: verification",
    "clipboard transfer failed: cancelled",
    "clipboard transfer failed: connection or file error",
    "clipboard transfer failed: other",
];

enum Command {
    Line(String),
    Flush(mpsc::Sender<()>),
}
#[derive(Clone)]
pub struct LogLayer(SyncSender<Command>);

pub struct RollingLog {
    sender: SyncSender<Command>,
}
impl RollingLog {
    /// Directory or thread failures disable file logging, never daemon startup.
    pub fn start(data_dir: &Path) -> (Self, LogLayer) {
        let (sender, receiver) = mpsc::sync_channel(QUEUE);
        let directory = data_dir.join("logs");
        let _ = thread::Builder::new()
            .name("glide-log".into())
            .spawn(move || {
                let Ok(mut log) = Files::open(directory, SystemTime::now()) else {
                    return;
                };
                let mut maintenance = std::time::Instant::now();
                loop {
                    let remaining = MAINTENANCE.saturating_sub(maintenance.elapsed());
                    match receiver.recv_timeout(remaining) {
                        Ok(Command::Line(line)) => {
                            let _ = log.write(line.as_bytes());
                        }
                        Ok(Command::Flush(done)) => {
                            let _ = log.file.flush();
                            let _ = done.send(());
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            let _ = log.file.flush();
                            break;
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    if maintenance.elapsed() >= MAINTENANCE {
                        let _ = log.retain(SystemTime::now());
                        maintenance = std::time::Instant::now();
                    }
                }
            });
        (
            Self {
                sender: sender.clone(),
            },
            LogLayer(sender),
        )
    }

    /// Flush the accepted queue on shutdown, without allowing a hung disk to delay input safety.
    pub fn flush(&self) -> bool {
        let (done, completed) = mpsc::channel();
        let deadline = std::time::Instant::now() + Duration::from_millis(200);
        let mut command = Command::Flush(done);
        loop {
            match self.sender.try_send(command) {
                Ok(()) => {
                    return completed
                        .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                        .is_ok()
                }
                Err(mpsc::TrySendError::Disconnected(_)) => return false,
                Err(mpsc::TrySendError::Full(returned)) => command = returned,
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(1));
        }
    }
}
impl Drop for RollingLog {
    fn drop(&mut self) {
        self.flush();
    }
}

struct Message(String);
impl std::fmt::Write for Message {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        if self.0.len() + text.len() > 256 {
            return Err(std::fmt::Error);
        }
        self.0.push_str(text);
        Ok(())
    }
}
impl Visit for Message {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            use std::fmt::Write;
            if write!(self, "{value:?}").is_err() {
                self.0.clear();
            }
        }
    }
}
impl<S: Subscriber> Layer<S> for LogLayer {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let level = *event.metadata().level();
        if level > tracing::Level::INFO {
            return;
        }
        let mut message = Message(String::new());
        event.record(&mut message);
        let safe = if MESSAGES.contains(&message.0.as_str()) {
            message.0.as_str()
        } else {
            "event details redacted"
        };
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // Never include target/field values: they may contain tokens, SAS, paths or payloads.
        let _ = self
            .0
            .try_send(Command::Line(format!("{timestamp} {level} {safe}\n")));
    }
}

struct Files {
    directory: PathBuf,
    file: File,
    bytes: u64,
}
impl Files {
    fn path(directory: &Path, index: usize) -> PathBuf {
        directory.join(if index == 0 {
            "glided.log".to_owned()
        } else {
            format!("glided.{index}.log")
        })
    }
    fn open(directory: PathBuf, now: SystemTime) -> io::Result<Self> {
        fs::create_dir_all(&directory)?;
        if !fs::symlink_metadata(&directory)?.file_type().is_dir() {
            return Err(io::Error::other("invalid logs directory"));
        }
        Self::purge(&directory, now)?;
        let file = Self::open_current(&directory)?;
        let bytes = file.metadata()?.len();
        Ok(Self {
            directory,
            file,
            bytes,
        })
    }
    fn open_current(directory: &Path) -> io::Result<File> {
        let path = Self::path(directory, 0);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if !metadata.file_type().is_file() => {
                return Err(io::Error::other("invalid log file"))
            }
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        OpenOptions::new().create(true).append(true).open(path)
    }
    fn purge(directory: &Path, now: SystemTime) -> io::Result<bool> {
        let mut current_removed = false;
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            // Only engine logs; preserve unrelated files and never follow a symlink.
            if name != "glided.log" && !(name.starts_with("glided.") && name.ends_with(".log")) {
                continue;
            }
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_file()
                && now.duration_since(metadata.modified()?).unwrap_or_default() > RETENTION
            {
                fs::remove_file(entry.path())?;
                current_removed |= name == "glided.log";
            }
        }
        Ok(current_removed)
    }
    fn retain(&mut self, now: SystemTime) -> io::Result<()> {
        if Self::purge(&self.directory, now)? {
            self.file = Self::open_current(&self.directory)?;
            self.bytes = self.file.metadata()?.len();
        }
        Ok(())
    }
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.bytes + bytes.len() as u64 > ROTATE_BYTES {
            self.file.flush()?;
            // Rust file handles permit delete-sharing on Windows. No fourth archive is created.
            let oldest = Self::path(&self.directory, 3);
            match fs::remove_file(oldest) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            for index in (0..3).rev() {
                match fs::rename(
                    Self::path(&self.directory, index),
                    Self::path(&self.directory, index + 1),
                ) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            self.file = Self::open_current(&self.directory)?;
            self.bytes = 0;
        }
        self.file.write_all(bytes)?;
        self.bytes += bytes.len() as u64;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::prelude::*;

    // Prevents unattended engines from filling the user's disk, retaining only three archives.
    #[test]
    fn rotation_preserves_order_and_caps_three_archives() {
        let temp = tempfile::tempdir().unwrap();
        let mut log = Files::open(temp.path().join("logs"), SystemTime::now()).unwrap();
        for byte in b'a'..=b'e' {
            log.write(&vec![byte; ROTATE_BYTES as usize]).unwrap();
        }
        log.file.flush().unwrap();
        for (index, &expected) in b"edcb".iter().enumerate() {
            let content = fs::read(Files::path(&log.directory, index)).unwrap();
            assert_eq!(content.len(), ROTATE_BYTES as usize);
            assert!(content.iter().all(|byte| *byte == expected));
        }
        assert_eq!(fs::read_dir(&log.directory).unwrap().count(), 4);
    }

    // Prevents old diagnostics from outliving the promised retention, including the open current file.
    #[test]
    fn retention_runs_at_startup_and_reopens_expired_current_log() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("logs");
        fs::create_dir(&directory).unwrap();
        let now = SystemTime::now();
        let old = now - RETENTION - Duration::from_secs(1);
        for index in 0..4 {
            let file = File::create(Files::path(&directory, index)).unwrap();
            file.set_times(fs::FileTimes::new().set_modified(old))
                .unwrap();
        }
        fs::write(directory.join("unrelated.txt"), b"preserve").unwrap();
        let mut log = Files::open(directory, now).unwrap();
        assert_eq!(fs::read_dir(&log.directory).unwrap().count(), 2);
        log.write(b"old current").unwrap();
        log.file
            .set_times(fs::FileTimes::new().set_modified(old))
            .unwrap();
        log.retain(now).unwrap();
        log.write(b"new current").unwrap();
        log.file.flush().unwrap();
        assert_eq!(
            fs::read(Files::path(&log.directory, 0)).unwrap(),
            b"new current"
        );
    }

    // A read-only/broken logs path must not prevent the KVM from starting or shutting down.
    #[test]
    fn bad_logs_directory_is_nonfatal() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("logs"), b"not a directory").unwrap();
        let (log, layer) = RollingLog::start(temp.path());
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || tracing::warn!("engine starting"));
        let _ = log.flush();
        assert_eq!(
            fs::read(temp.path().join("logs")).unwrap(),
            b"not a directory"
        );
    }

    // Prevents logs from exposing authentication, human verification, clipboard, filenames or keystrokes.
    #[test]
    fn info_warn_error_log_only_audited_messages_and_never_payloads() {
        let temp = tempfile::tempdir().unwrap();
        let (log, layer) = RollingLog::start(temp.path());
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
            for level in [
                tracing::Level::INFO,
                tracing::Level::WARN,
                tracing::Level::ERROR,
            ] {
                let secret = "control-token pairing-code sas-words clipboard-text typed-key file-name private-key";
                match level {
                    tracing::Level::INFO => tracing::info!(
                        token = secret,
                        code = secret,
                        phrase = secret,
                        clipboard = secret,
                        key = secret,
                        file = secret,
                        material = secret,
                        "engine starting"
                    ),
                    tracing::Level::WARN => tracing::warn!(token = secret, "{secret}"),
                    _ => tracing::error!(failure = secret, "configuration replacement failed"),
                }
            }
            tracing::debug!("private-key");
            tracing::info!("unreviewed static message");
        });
        assert!(log.flush());
        let text = fs::read_to_string(temp.path().join("logs/glided.log")).unwrap();
        assert!(text.contains("INFO engine starting"));
        assert!(text.contains("ERROR configuration replacement failed"));
        assert!(text.contains("WARN event details redacted"));
        for secret in [
            "control-token",
            "pairing-code",
            "sas-words",
            "clipboard-text",
            "typed-key",
            "file-name",
            "private-key",
            "unreviewed static message",
        ] {
            assert!(!text.contains(secret));
        }
    }

    // Prevents a stalled disk/full queue from stalling input capture; shutdown flush is also bounded.
    #[test]
    fn full_log_queue_drops_events_and_flush_has_deadline() {
        let (sender, _held_receiver) = mpsc::sync_channel(1);
        sender
            .try_send(Command::Line("occupied".into()))
            .unwrap_or_else(|_| panic!("empty queue"));
        let log = RollingLog {
            sender: sender.clone(),
        };
        let now = std::time::Instant::now();
        tracing::subscriber::with_default(
            tracing_subscriber::registry().with(LogLayer(sender)),
            || {
                for _ in 0..1000 {
                    tracing::info!("engine starting");
                }
            },
        );
        assert!(now.elapsed() < Duration::from_millis(100));
        assert!(!log.flush());
        assert!(now.elapsed() < Duration::from_millis(500));
    }
}
