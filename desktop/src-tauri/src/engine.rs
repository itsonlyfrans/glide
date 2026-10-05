//! The link between this window and the Glide engine (`glided`), speaking the JSON-Lines protocol from SPEC §7.
//!
//! * Windows: the engine is a separate, always-on background process with its own tray. This app attaches to it over
//!   a local, same-user-only named pipe with a token (`<data dir>/ipc.json`). Closing the window leaves it running.
//! * macOS (and the mock engine used for development): this app starts the engine as a child and talks over stdio.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

/// The only methods the window may call. Anything else is refused here, before it reaches the engine.
pub const ALLOWED_METHODS: &[&str] = &[
    "get_state",
    "set_settings",
    "set_sharing",
    "set_layout",
    "pairing.start_host",
    "pairing.cancel_host",
    "pairing.join",
    "pairing.confirm",
    "peer.add_manual",
    "peer.unpair",
    "peer.configure",
    "peer.wake",
    "peer.arrange",
    "diag.cursor",
    "return_home",
    "transfer.cancel",
    "transfer.confirm",
    "permissions.open_settings",
    "permissions.request",
];

const CALL_TIMEOUT: Duration = Duration::from_secs(20);

pub type EventSink = Arc<dyn Fn(&str, Value) + Send + Sync>;
pub type LogSink = Arc<dyn Fn(&str) + Send + Sync>;

fn failure(message: &str) -> Value {
    json!({ "ok": false, "error": { "code": "internal", "message": message } })
}

/// One live JSON-Lines connection: a writer queue and the calls waiting for an answer.
struct Conn {
    lines: mpsc::UnboundedSender<String>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    generation: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Attach to the always-on engine (Windows).
    Attach,
    /// Own the engine as a child process over stdio (macOS, mock).
    Stdio,
}

pub struct EngineOptions {
    pub mode: Mode,
    pub data_dir: PathBuf,
    /// The engine command and its extra arguments.
    pub command: Option<(PathBuf, Vec<String>)>,
    /// This window's executable, handed to the engine so its tray can open the window (attach mode).
    pub ui_path: Option<PathBuf>,
    /// Replace an engine left running by an older install when its version differs (packaged builds, Windows).
    #[cfg_attr(not(windows), allow(dead_code))]
    pub expected_version: Option<String>,
}

pub struct Engine {
    opts: EngineOptions,
    events: EventSink,
    log: LogSink,
    conn: Mutex<Option<Conn>>,
    next_id: AtomicU64,
    generation: AtomicU64,
    closing: AtomicBool,
    replacing: AtomicBool,
    restarts: AtomicU32,
    child: tokio::sync::Mutex<Option<tokio::process::Child>>,
    /// The engine's last few lines of error output, to say why it stopped.
    recent: Mutex<std::collections::VecDeque<String>>,
    /// When the running engine said it was ready; a crash soon after still counts as "keeps stopping".
    ready_at: Mutex<Option<std::time::Instant>>,
}

impl Engine {
    pub fn new(opts: EngineOptions, events: EventSink, log: LogSink) -> Arc<Self> {
        Arc::new(Self {
            opts,
            events,
            log,
            conn: Mutex::new(None),
            next_id: AtomicU64::new(1),
            generation: AtomicU64::new(0),
            closing: AtomicBool::new(false),
            replacing: AtomicBool::new(false),
            restarts: AtomicU32::new(0),
            child: tokio::sync::Mutex::new(None),
            recent: Mutex::new(std::collections::VecDeque::new()),
            ready_at: Mutex::new(None),
        })
    }

    pub fn attached(&self) -> bool {
        self.opts.mode == Mode::Attach
    }

    /// Start (or attach to) the engine. Problems are reported as an `engine.fatal` event.
    pub async fn start(self: &Arc<Self>) {
        self.closing.store(false, Ordering::Release);
        let result = match self.opts.mode {
            Mode::Attach => self.attach().await,
            Mode::Stdio => self.spawn_child().await,
        };
        if let Err(message) = result {
            (self.log)(&format!("engine start failed: {message}"));
            (self.events)("engine.fatal", json!({ "message": message }));
        }
    }

    /// Ask the engine something. Unknown methods never leave this process.
    pub async fn call(self: &Arc<Self>, method: &str, params: Value) -> Value {
        if !ALLOWED_METHODS.contains(&method) {
            return json!({ "ok": false, "error": { "code": "invalid_params", "message": format!("Unknown method {method}") } });
        }
        self.raw(method, params, CALL_TIMEOUT).await
    }

    async fn raw(&self, method: &str, params: Value, timeout: Duration) -> Value {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let guard = self.conn.lock().unwrap_or_else(|p| p.into_inner());
            let Some(conn) = guard.as_ref() else {
                return failure("The Glide engine is not running.");
            };
            conn.pending.lock().unwrap_or_else(|p| p.into_inner()).insert(id, tx);
            let line = json!({ "id": id, "method": method, "params": params }).to_string();
            if conn.lines.send(line).is_err() {
                conn.pending.lock().unwrap_or_else(|p| p.into_inner()).remove(&id);
                return failure("The Glide engine is not running.");
            }
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => failure("The Glide engine stopped."),
            Err(_) => {
                if let Some(conn) = self.conn.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
                    conn.pending.lock().unwrap_or_else(|p| p.into_inner()).remove(&id);
                }
                failure("The engine did not answer in time.")
            }
        }
    }

    /// Wire a connected reader/writer pair into the call/event machinery.
    fn adopt<R, W>(self: &Arc<Self>, reader: R, mut writer: W)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let (lines_tx, mut lines_rx) = mpsc::unbounded_channel::<String>();
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> = Arc::default();
        *self.conn.lock().unwrap_or_else(|p| p.into_inner()) = Some(Conn {
            lines: lines_tx,
            pending: pending.clone(),
            generation,
        });
        tauri::async_runtime::spawn(async move {
            while let Some(line) = lines_rx.recv().await {
                if writer.write_all(line.as_bytes()).await.is_err()
                    || writer.write_all(b"\n").await.is_err()
                    || writer.flush().await.is_err()
                {
                    break;
                }
            }
        });
        let me = self.clone();
        tauri::async_runtime::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if let Some(id) = message.get("id").and_then(Value::as_u64) {
                    let waiter = pending.lock().unwrap_or_else(|p| p.into_inner()).remove(&id);
                    if let Some(waiter) = waiter {
                        let answer = if message.get("ok").and_then(Value::as_bool) == Some(true) {
                            json!({ "ok": true, "result": message.get("result").cloned().unwrap_or(Value::Null) })
                        } else {
                            json!({ "ok": false, "error": message.get("error").cloned()
                                .unwrap_or_else(|| json!({ "code": "internal", "message": "Unknown error" })) })
                        };
                        let _ = waiter.send(answer);
                    } else if let Some(text) = message.pointer("/error/message").and_then(Value::as_str) {
                        // Nobody asked: the engine is explaining why it could not start (id 0). Keep it.
                        (me.log)(&format!("Error: {text}"));
                        let mut recent = me.recent.lock().unwrap_or_else(|p| p.into_inner());
                        recent.push_back(format!("Error: {text}"));
                    }
                } else if let Some(event) = message.get("event").and_then(Value::as_str) {
                    if event == "ready" {
                        *me.ready_at.lock().unwrap_or_else(|p| p.into_inner()) = Some(std::time::Instant::now());
                    }
                    (me.events)(event, message.get("data").cloned().unwrap_or_else(|| json!({})));
                }
            }
            me.connection_lost(generation, pending).await;
        });
    }

    async fn connection_lost(
        self: &Arc<Self>,
        generation: u64,
        pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    ) {
        {
            let mut guard = self.conn.lock().unwrap_or_else(|p| p.into_inner());
            if guard.as_ref().is_some_and(|c| c.generation == generation) {
                *guard = None;
            } else {
                return; // an older connection; a newer one already took over
            }
        }
        for (_, waiter) in pending.lock().unwrap_or_else(|p| p.into_inner()).drain() {
            let _ = waiter.send(failure("The Glide engine stopped."));
        }
        if self.closing.load(Ordering::Acquire) || self.replacing.load(Ordering::Acquire) {
            return;
        }
        (self.events)("engine.down", json!({}));
        if self.opts.mode == Mode::Stdio {
            // Say how it ended (exit code or signal) in the log next to its own output.
            if let Some(child) = self.child.lock().await.as_mut() {
                if let Ok(Ok(status)) = tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
                    (self.log)(&format!("engine exited: {status}"));
                }
            }
            // Only an engine that ran for a while starts the count again; one that dies right after starting keeps
            // counting, so the window stops retrying and says why instead of "Restarting…" forever.
            let ran = self.ready_at.lock().unwrap_or_else(|p| p.into_inner()).take();
            if ran.is_some_and(|t| t.elapsed() > Duration::from_secs(30)) {
                self.restarts.store(0, Ordering::Release);
            }
            let attempt = self.restarts.fetch_add(1, Ordering::AcqRel) + 1;
            if attempt <= 5 {
                tokio::time::sleep(Duration::from_millis((250u64 << attempt).min(4000))).await;
                if !self.closing.load(Ordering::Acquire) {
                    let me = self.clone();
                    tauri::async_runtime::spawn(async move { me.start().await });
                }
            } else {
                if attempt == 6 {
                    let reason = self.last_reason();
                    let message = match reason {
                        Some(reason) => format!("The Glide engine keeps stopping. Its last message was: {reason}"),
                        None => "The Glide engine keeps stopping.".to_string(),
                    };
                    (self.events)("engine.fatal", json!({ "message": message }));
                }
                // Whatever stopped it (a locked or sleeping Mac, a permission being re-granted) often clears up by
                // itself, so keep trying quietly; a start that works clears the message.
                tokio::time::sleep(Duration::from_secs(30)).await;
                let idle = self.conn.lock().unwrap_or_else(|p| p.into_inner()).is_none();
                if idle && !self.closing.load(Ordering::Acquire) && !self.replacing.load(Ordering::Acquire) {
                    let me = self.clone();
                    tauri::async_runtime::spawn(async move { me.start().await });
                }
            }
        }
    }

    /// The most telling recent line of the engine's error output: the last error, else the last warning.
    fn last_reason(&self) -> Option<String> {
        let recent = self.recent.lock().unwrap_or_else(|p| p.into_inner());
        let pick = |needle: &str| recent.iter().rev().find(|l| l.contains(needle)).cloned();
        pick("Error").or_else(|| pick("ERROR")).or_else(|| pick("WARN")).map(|line| {
            let line = line.trim();
            // Drop the timestamp and level prefix the engine's log lines start with.
            let text = line.split_once(": ").map_or(line, |(_, rest)| rest);
            text.chars().take(240).collect()
        })
    }

    /// Start again after the engine kept stopping (the window's "Try again").
    pub fn reset_restarts(&self) {
        self.restarts.store(0, Ordering::Release);
    }

    // ---------------------------------------------------------------- stdio (macOS, mock)

    async fn spawn_child(self: &Arc<Self>) -> Result<(), String> {
        let (cmd, extra) = self.opts.command.clone().ok_or_else(|| {
            "The Glide engine (glided) was not found. Reinstall Glide.".to_string()
        })?;
        let mut command = tokio::process::Command::new(&cmd);
        command
            .args(&extra)
            .arg("--data-dir")
            .arg(&self.opts.data_dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        let mut child = command
            .spawn()
            .map_err(|e| format!("Could not start the Glide engine: {e}"))?;
        let stdin = child.stdin.take().ok_or("engine stdin unavailable")?;
        let stdout = child.stdout.take().ok_or("engine stdout unavailable")?;
        if let Some(stderr) = child.stderr.take() {
            let me = self.clone();
            tauri::async_runtime::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    (me.log)(&line);
                    let mut recent = me.recent.lock().unwrap_or_else(|p| p.into_inner());
                    if recent.len() == 20 {
                        recent.pop_front();
                    }
                    recent.push_back(line);
                }
            });
        }
        *self.child.lock().await = Some(child);
        self.adopt(stdout, stdin);
        Ok(())
    }

    // ---------------------------------------------------------------- attach (Windows)

    fn read_info(&self) -> Option<Value> {
        let text = std::fs::read_to_string(self.opts.data_dir.join("ipc.json")).ok()?;
        let info: Value = serde_json::from_str(&text).ok()?;
        (info.get("endpoint")?.is_string() && info.get("token")?.is_string()).then_some(info)
    }

    fn spawn_detached(&self) -> Result<(), String> {
        let (cmd, extra) = self.opts.command.clone().ok_or_else(|| {
            "The Glide engine (glided) was not found. Reinstall Glide.".to_string()
        })?;
        let mut command = std::process::Command::new(&cmd);
        command.arg("--headless").arg("--data-dir").arg(&self.opts.data_dir);
        if let Some(ui) = &self.opts.ui_path {
            command.arg("--ui").arg(ui);
        }
        command
            .args(extra.iter().filter(|a| a.as_str() != "--headless"))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW: it must outlive this window.
            command.creation_flags(0x0000_0008 | 0x0000_0200 | 0x0800_0000);
        }
        command
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("Could not start the Glide engine: {e}"))
    }

    /// Connect and authenticate. Returns the split stream on success.
    #[cfg(windows)]
    async fn connect(
        &self,
        info: &Value,
    ) -> Result<
        (
            BufReader<tokio::io::ReadHalf<tokio::net::windows::named_pipe::NamedPipeClient>>,
            tokio::io::WriteHalf<tokio::net::windows::named_pipe::NamedPipeClient>,
        ),
        String,
    > {
        use tokio::net::windows::named_pipe::ClientOptions;
        let endpoint = info["endpoint"].as_str().unwrap_or_default().to_string();
        let token = info["token"].as_str().unwrap_or_default().to_string();
        let mut tries = 0;
        let pipe = loop {
            match ClientOptions::new().open(&endpoint) {
                Ok(pipe) => break pipe,
                // ERROR_PIPE_BUSY: every instance is in use for a moment; try again shortly.
                Err(e) if e.raw_os_error() == Some(231) && tries < 40 => {
                    tries += 1;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => return Err(format!("Could not reach the Glide engine ({e}).")),
            }
        };
        let (reader, mut writer) = tokio::io::split(pipe);
        let mut reader = BufReader::new(reader);
        writer
            .write_all(format!("{}\n", json!({ "auth": token })).as_bytes())
            .await
            .map_err(|e| format!("Could not reach the Glide engine ({e})."))?;
        let mut first = String::new();
        tokio::time::timeout(Duration::from_secs(6), reader.read_line(&mut first))
            .await
            .map_err(|_| "The Glide engine did not answer.".to_string())?
            .map_err(|e| format!("Could not reach the Glide engine ({e})."))?;
        let answer: Value = serde_json::from_str(first.trim()).unwrap_or(Value::Null);
        if answer.get("auth").and_then(Value::as_str) != Some("ok") {
            return Err("The Glide engine refused the connection.".into());
        }
        // Keep the buffered reader: the engine may already have sent its first state right after "auth ok".
        Ok((reader, writer))
    }

    #[cfg(windows)]
    async fn wait_and_connect(
        &self,
        total: Duration,
    ) -> Result<(Value, (
        BufReader<tokio::io::ReadHalf<tokio::net::windows::named_pipe::NamedPipeClient>>,
        tokio::io::WriteHalf<tokio::net::windows::named_pipe::NamedPipeClient>,
    )), String> {
        let deadline = tokio::time::Instant::now() + total;
        let mut last = "The Glide engine did not start in time. Try opening Glide again.".to_string();
        while tokio::time::Instant::now() < deadline {
            if let Some(info) = self.read_info() {
                match self.connect(&info).await {
                    Ok(stream) => return Ok((info, stream)),
                    Err(e) => last = e,
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Err(last)
    }

    #[cfg(windows)]
    async fn attach(self: &Arc<Self>) -> Result<(), String> {
        let (info, (reader, writer)) = match self.wait_and_connect(Duration::from_millis(300)).await {
            Ok(ok) => ok,
            Err(_) => {
                self.spawn_detached()?;
                self.wait_and_connect(Duration::from_secs(15)).await?
            }
        };
        self.adopt(reader, writer);
        // An engine left running by an older install is replaced so the window and the engine always match.
        let version = info.get("version").and_then(Value::as_str);
        if let (Some(want), Some(have)) = (self.opts.expected_version.as_deref(), version) {
            if want != have {
                (self.log)(&format!("replacing engine {have} with {want}"));
                self.replacing.store(true, Ordering::Release);
                let _ = self.raw("app.shutdown", json!({}), Duration::from_secs(3)).await;
                for _ in 0..24 {
                    if self.read_info().is_none() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                *self.conn.lock().unwrap_or_else(|p| p.into_inner()) = None;
                let result = async {
                    self.spawn_detached()?;
                    self.wait_and_connect(Duration::from_secs(15)).await
                }
                .await;
                self.replacing.store(false, Ordering::Release);
                let (_, (reader, writer)) = result?;
                self.adopt(reader, writer);
            }
        }
        Ok(())
    }

    #[cfg(not(windows))]
    async fn attach(self: &Arc<Self>) -> Result<(), String> {
        let _ = (Self::read_info, Self::spawn_detached);
        Err("Attach mode is only used on Windows.".into())
    }

    /// Closing the window / app. Attach: just disconnect, the engine keeps sharing. Stdio: stop the child engine.
    pub async fn stop(self: &Arc<Self>) {
        self.closing.store(true, Ordering::Release);
        if self.opts.mode == Mode::Stdio {
            let _ = self.raw("app.shutdown", json!({}), Duration::from_millis(1500)).await;
            if let Some(mut child) = self.child.lock().await.take() {
                if tokio::time::timeout(Duration::from_millis(1500), child.wait()).await.is_err() {
                    let _ = child.kill().await;
                }
            }
        }
        *self.conn.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    /// "Quit Glide completely": ask the engine itself to exit, then disconnect.
    pub async fn shutdown_engine(self: &Arc<Self>) {
        self.closing.store(true, Ordering::Release);
        let _ = self.raw("app.shutdown", json!({}), Duration::from_secs(3)).await;
        self.stop().await;
    }
}

/// Where the engine binary lives: next to this app when installed, otherwise the developer's build.
pub fn resolve_engine(dev_root: &Path) -> Option<PathBuf> {
    let exe = if cfg!(windows) { "glided.exe" } else { "glided" };
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("GLIDE_DAEMON") {
        candidates.push(PathBuf::from(p));
    }
    if let Ok(me) = std::env::current_exe() {
        if let Some(dir) = me.parent() {
            candidates.push(dir.join(exe));
        }
    }
    candidates.push(dev_root.join("core").join("target").join("release").join(exe));
    candidates.into_iter().find(|p| p.is_file())
}
