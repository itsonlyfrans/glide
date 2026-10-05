//! Same-user authenticated local JSONL control. No network listener is used.
use crate::{ipc, tray, Core};
use glide_proto::ipc::{Event, Request, Response};
use rand::TryRng;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::{mpsc, oneshot, watch, Semaphore},
};

#[cfg(unix)]
use crate::control_unix as native;
#[cfg(windows)]
use crate::control_win as native;

pub(super) const CLIENTS: usize = 4;
pub(super) const UNAUTHENTICATED: usize = 16;
const REJECT_DELAY: Duration = Duration::from_millis(250);
const AUTH_TIMEOUT: Duration = Duration::from_secs(2);
const WRITE_TIMEOUT: Duration = Duration::from_millis(500);
const OUTBOUND_BYTES: usize = 2 * ipc::MAX_IPC_LINE_BYTES;
const UPDATE_CHECK_FIRST: Duration = Duration::from_secs(120);
const UPDATE_CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);

#[derive(Serialize, Deserialize)]
pub struct Metadata {
    pub endpoint: String,
    pub token: String,
    pub pid: u32,
    pub version: String,
    pub protocol: u32,
    pub started_at_ms: u64,
}
impl Drop for Metadata {
    fn drop(&mut self) {
        // SAFETY: exclusive access; replacing bytes by zero preserves UTF-8.
        glide_proto::ipc::wipe_bytes(unsafe { self.token.as_bytes_mut() });
    }
}

#[derive(Debug, thiserror::Error)]
#[error("Glide is already running")]
pub struct AlreadyRunning;

/// Held before loading configuration or opening any input/network backend.
pub struct Instance {
    _lock: native::InstanceLock,
    data_dir: PathBuf,
    #[cfg_attr(not(windows), allow(dead_code))] // names the Windows pipe
    id: String,
    published: bool,
}
impl Instance {
    pub fn acquire(data_dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(data_dir)?;
        let data_dir = data_dir.canonicalize()?;
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        data_dir.hash(&mut hash);
        let key = format!("{:016x}", hash.finish());
        let lock = match native::InstanceLock::acquire(&data_dir, &key) {
            Ok(lock) => lock,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // The owner acquires its lock before publishing the ready control endpoint.
                let deadline = Instant::now() + Duration::from_secs(2);
                while Instant::now() < deadline {
                    if read_metadata(&data_dir).is_ok_and(|m| native::process_is_engine(m.pid)) {
                        return Err(AlreadyRunning.into());
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                anyhow::bail!(
                    "Glide engine lock is held; owner has not published live control metadata"
                );
            }
            Err(e) => return Err(e.into()),
        };
        let metadata_path = data_dir.join("ipc.json");
        if metadata_path.exists() {
            if read_metadata(&data_dir).is_ok_and(|m| native::process_is_engine(m.pid)) {
                return Err(AlreadyRunning.into());
            }
            std::fs::remove_file(&metadata_path)?;
        }
        let id_path = data_dir.join("control.id");
        let id = match std::fs::read_to_string(&id_path) {
            Ok(id) => {
                anyhow::ensure!(
                    id.len() == 16
                        && id
                            .bytes()
                            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
                    "invalid control directory id"
                );
                id
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let mut random = [0u8; 8];
                rand::rngs::SysRng.try_fill_bytes(&mut random)?;
                let id = hex::encode(random);
                native::secure_write(&id_path, id.as_bytes())?;
                id
            }
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            _lock: lock,
            data_dir,
            id,
            published: false,
        })
    }

    fn publish(&mut self, endpoint: String, token: &Token) -> anyhow::Result<()> {
        let metadata = Metadata {
            endpoint,
            token: hex::encode(token.0),
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").into(),
            protocol: 1,
            started_at_ms: crate::state::now_ms(),
        };
        let bytes = SecretBytes(serde_json::to_vec(&metadata)?);
        native::secure_write(&self.data_dir.join("ipc.json"), &bytes.0)?;
        self.published = true;
        Ok(())
    }

    /// Publish ownership for a stdio engine without opening an attachable endpoint.
    pub fn publish_stdio_owner(&mut self) -> anyhow::Result<()> {
        self.publish("stdio".into(), &Token::generate()?)
    }
}
impl Drop for Instance {
    fn drop(&mut self) {
        if self.published {
            let _ = std::fs::remove_file(self.data_dir.join("ipc.json"));
            #[cfg(unix)]
            let _ = std::fs::remove_file(self.data_dir.join("ipc/ctl.sock"));
        }
    }
}

pub fn read_metadata(data_dir: &Path) -> io::Result<Metadata> {
    use std::io::Read;
    let file = std::fs::File::open(data_dir.join("ipc.json"))?;
    if file.metadata()?.len() > 4096 {
        return Err(io::Error::other("invalid control metadata"));
    }
    let mut bytes = SecretBytes(Vec::new());
    file.take(4097).read_to_end(&mut bytes.0)?;
    if bytes.0.len() > 4096 {
        return Err(io::Error::other("invalid control metadata"));
    }
    serde_json::from_slice(&bytes.0).map_err(|_| io::Error::other("invalid control metadata"))
}

struct Token([u8; 32]);
impl Token {
    fn generate() -> anyhow::Result<Self> {
        let mut token = Self([0; 32]);
        rand::rngs::SysRng.try_fill_bytes(&mut token.0)?;
        Ok(token)
    }
    fn matches(&self, text: &str) -> bool {
        let mut decoded = [0u8; 32];
        if hex::decode_to_slice(text, &mut decoded).is_err() {
            return false;
        }
        let mut difference = 0u8;
        for (expected, supplied) in self.0.iter().zip(decoded.iter()) {
            // Volatile loads keep every fixed-width comparison in the generated loop.
            // SAFETY: both references point to initialized live bytes.
            difference |=
                unsafe { std::ptr::read_volatile(expected) ^ std::ptr::read_volatile(supplied) };
        }
        glide_proto::ipc::wipe_bytes(&mut decoded);
        std::hint::black_box(difference) == 0
    }
}
impl Drop for Token {
    fn drop(&mut self) {
        glide_proto::ipc::wipe_bytes(&mut self.0);
    }
}
struct SecretBytes(Vec<u8>);
impl Drop for SecretBytes {
    fn drop(&mut self) {
        glide_proto::ipc::wipe_bytes(&mut self.0);
    }
}

// Failures are telemetry only: same-user junk must never lock out a valid token.
struct Limits {
    failures: AtomicU64,
    pending: Arc<Semaphore>,
    clients: Arc<Semaphore>,
}
impl Limits {
    fn new() -> Self {
        Self {
            failures: AtomicU64::new(0),
            pending: Arc::new(Semaphore::new(UNAUTHENTICATED)),
            clients: Arc::new(Semaphore::new(CLIENTS)),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ClientKind {
    Ui,
    #[default]
    Tool,
}

async fn authenticate<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    token: &Token,
    failures: &AtomicU64,
) -> io::Result<Option<ClientKind>> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Auth {
        auth: String,
        #[serde(default)]
        client: Option<ClientKind>,
    }
    impl Drop for Auth {
        fn drop(&mut self) {
            glide_proto::ipc::wipe_bytes(unsafe { self.auth.as_bytes_mut() });
        }
    }
    let attempt = tokio::time::timeout(AUTH_TIMEOUT, async {
        let line = SecretBytes(
            ipc::read_line_limited(reader, 1024)
                .await?
                .ok_or_else(|| io::Error::other("missing auth"))?,
        );
        if line.0.last() != Some(&b'\n') {
            return Err(io::Error::other("incomplete auth"));
        }
        let auth: Auth =
            serde_json::from_slice(&line.0).map_err(|_| io::Error::other("invalid auth"))?;
        Ok(auth)
    })
    .await
    .unwrap_or_else(|_| Err(io::Error::new(io::ErrorKind::TimedOut, "auth timeout")));
    match attempt {
        Ok(auth) if token.matches(&auth.auth) => Ok(auth.client),
        attempt => {
            let count = failures.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
            tracing::debug!(count, "local control authentication rejected");
            // Complete malformed lines and wrong tokens have identical silent close behavior.
            // A stalled first line already consumed its two-second budget.
            if !attempt
                .as_ref()
                .is_err_and(|e| e.kind() == io::ErrorKind::TimedOut)
            {
                tokio::time::sleep(REJECT_DELAY).await;
            }
            Err(io::Error::other("authentication rejected"))
        }
    }
}

async fn acknowledge<W: AsyncWrite + Unpin>(writer: &mut W) -> io::Result<()> {
    tokio::time::timeout(WRITE_TIMEOUT, async {
        writer.write_all(b"{\"auth\":\"ok\"}\n").await?;
        writer.flush().await
    })
    .await
    .map_err(|_| io::Error::other("auth writer timeout"))?
}

struct Frame {
    bytes: Arc<SecretBytes>,
    _budget: tokio::sync::OwnedSemaphorePermit,
    flushed: Option<oneshot::Sender<()>>,
}
struct Client {
    kind: ClientKind,
    output: mpsc::Sender<Frame>,
    budget: Arc<Semaphore>,
    cancel: watch::Sender<bool>,
}
impl Client {
    fn send(&self, bytes: Arc<SecretBytes>, flushed: Option<oneshot::Sender<()>>) -> bool {
        let Ok(budget) = self
            .budget
            .clone()
            .try_acquire_many_owned(bytes.0.len() as u32)
        else {
            return false;
        };
        self.output
            .try_send(Frame {
                bytes,
                _budget: budget,
                flushed,
            })
            .is_ok()
    }
}
/// Start the Glide app detached and without a console window.
fn launch_ui(ui: &Path, args: &[&str]) -> io::Result<()> {
    let mut command = std::process::Command::new(ui);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    command.spawn().map(drop)
}

fn ui_clients(clients: &HashMap<u64, Client>) -> usize {
    clients
        .values()
        .filter(|client| matches!(client.kind, ClientKind::Ui))
        .count()
}
enum Message {
    Joined(u64, Client),
    Request(u64, Request),
    Gone(u64),
    Output(SecretBytes),
    End,
}

async fn connection<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    stream: S,
    id: u64,
    token: Arc<Token>,
    limits: Arc<Limits>,
    messages: mpsc::Sender<Message>,
    pending_slot: tokio::sync::OwnedSemaphorePermit,
    identified_ui: bool,
) {
    let (read, mut write) = tokio::io::split(stream);
    let mut read = BufReader::new(read);
    let Ok(kind) = authenticate(&mut read, &token, &limits.failures).await else {
        return;
    };
    let Ok(_client_slot) = limits.clients.clone().try_acquire_owned() else {
        return;
    };
    drop(pending_slot);
    if acknowledge(&mut write).await.is_err() {
        return;
    }
    let kind = kind.unwrap_or(if identified_ui {
        ClientKind::Ui
    } else {
        ClientKind::Tool
    });
    let (output, mut queued) = mpsc::channel::<Frame>(16);
    let (cancel, mut cancelled) = watch::channel(false);
    let client = Client {
        kind,
        output,
        budget: Arc::new(Semaphore::new(OUTBOUND_BYTES)),
        cancel,
    };
    if messages.send(Message::Joined(id, client)).await.is_err() {
        return;
    }
    let writing = async {
        while let Some(frame) = queued.recv().await {
            tokio::time::timeout(WRITE_TIMEOUT, async {
                write.write_all(&frame.bytes.0).await?;
                write.flush().await
            })
            .await
            .map_err(|_| io::Error::other("slow client"))??;
            if let Some(flushed) = frame.flushed {
                let _ = flushed.send(());
            }
        }
        Ok::<(), io::Error>(())
    };
    let reading = async {
        while let Some(line) = ipc::read_line(&mut read).await? {
            let line = SecretBytes(line);
            let request = glide_proto::codec::decode_jsonl_request(&line.0)
                .map_err(|_| io::Error::other("invalid request"))?;
            messages
                .send(Message::Request(id, request))
                .await
                .map_err(|_| io::Error::other("engine stopped"))?;
        }
        Ok::<(), io::Error>(())
    };
    tokio::select! { _ = writing => {}, _ = reading => {}, _ = cancelled.changed() => {} }
    let _ = messages.send(Message::Gone(id)).await;
}

#[cfg(windows)]
async fn accept_loop(
    mut pipe: tokio::net::windows::named_pipe::NamedPipeServer,
    endpoint: String,
    token: Arc<Token>,
    limits: Arc<Limits>,
    messages: mpsc::Sender<Message>,
    ui: Option<PathBuf>,
) -> io::Result<()> {
    let mut next_id = 1u64;
    loop {
        let slot = limits
            .pending
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| io::Error::other("control admission stopped"))?;
        pipe.connect().await?;
        // Keep one server handle alive at all times; never open a squatting gap.
        let next = native::create_pipe(&endpoint, false)?;
        let accepted = std::mem::replace(&mut pipe, next);
        let Ok(identified_ui) = native::verify_peer(&accepted, ui.as_deref()) else {
            tracing::debug!("local control peer rejected");
            continue;
        };
        tokio::spawn(connection(
            accepted,
            next_id,
            token.clone(),
            limits.clone(),
            messages.clone(),
            slot,
            identified_ui,
        ));
        next_id = next_id.wrapping_add(1);
    }
}
#[cfg(unix)]
async fn accept_loop(
    listener: tokio::net::UnixListener,
    _endpoint: String,
    token: Arc<Token>,
    limits: Arc<Limits>,
    messages: mpsc::Sender<Message>,
    _ui: Option<PathBuf>,
) -> io::Result<()> {
    let mut next_id = 1u64;
    loop {
        let slot = limits
            .pending
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| io::Error::other("control admission stopped"))?;
        let (stream, _) = listener.accept().await?;
        if native::verify_peer(&stream).is_err() {
            tracing::debug!("local control peer rejected");
            continue;
        }
        tokio::spawn(connection(
            stream,
            next_id,
            token.clone(),
            limits.clone(),
            messages.clone(),
            slot,
            false,
        ));
        next_id = next_id.wrapping_add(1);
    }
}

/// Run the existing engine protocol with bounded client fan-out and response routing.
pub async fn serve(core: Core, mut instance: Instance) -> anyhow::Result<()> {
    let token = Arc::new(Token::generate()?);
    #[cfg(windows)]
    let (listener, endpoint) = {
        let endpoint = format!("\\\\.\\pipe\\glide-ctl-{}", instance.id);
        (native::create_pipe(&endpoint, true)?, endpoint)
    };
    #[cfg(unix)]
    let (listener, endpoint) = {
        let path = native::prepare_socket(&instance.data_dir)?;
        native::remove_stale_socket(&path)?;
        let listener = tokio::net::UnixListener::bind(&path)?;
        native::secure_socket(&path)?;
        (listener, path.to_string_lossy().into_owned())
    };
    let mut state = core.snapshot();
    let mut snapshot = Arc::new(SecretBytes(glide_proto::codec::encode_jsonl(
        &Event::State(Box::new(state.clone())),
    )?));
    let ready = Arc::new(SecretBytes(glide_proto::codec::encode_jsonl(
        &Event::Ready {
            version: env!("CARGO_PKG_VERSION").into(),
        },
    )?));
    let (messages, mut incoming) = mpsc::channel(128);
    let (actions, mut tray_actions) = mpsc::channel(16);
    let ui_path = core.ui_path();
    let tray = match tray::Tray::start(ui_path.clone(), actions) {
        Ok(tray) => Some(tray),
        Err(_) => {
            tracing::warn!("native tray unavailable; engine continues running");
            None
        }
    };
    if let Some(tray) = &tray {
        tray.update(&Event::State(Box::new(state.clone())), 0);
    }
    instance.publish(endpoint.clone(), &token)?;
    let mut accepting = tokio::spawn(accept_loop(
        listener,
        endpoint,
        token.clone(),
        Arc::new(Limits::new()),
        messages.clone(),
        ui_path.clone(),
    ));
    let (engine, broker) = tokio::io::duplex(64 * 1024);
    let (engine_read, engine_write) = tokio::io::split(engine);
    let engine = tokio::spawn(ipc::serve_with_mode(engine_read, engine_write, core, true));
    let (broker_read, mut broker_write) = tokio::io::split(broker);
    let (requests, mut requested) = mpsc::channel::<SecretBytes>(128);
    let request_writer = tokio::spawn(async move {
        while let Some(bytes) = requested.recv().await {
            broker_write.write_all(&bytes.0).await?;
        }
        Ok::<(), io::Error>(())
    });
    let outbox = messages.clone();
    let output_reader = tokio::spawn(async move {
        let mut reader = BufReader::new(broker_read);
        while let Ok(Some(bytes)) = ipc::read_line(&mut reader).await {
            if outbox
                .send(Message::Output(SecretBytes(bytes)))
                .await
                .is_err()
            {
                return;
            }
        }
        let _ = outbox.send(Message::End).await;
    });
    let mut clients: HashMap<u64, Client> = HashMap::new();
    let mut pending: HashMap<u64, (u64, u64, bool)> = HashMap::new();
    let mut next_request = 1u64;
    let mut tray_active = tray.is_some();
    let mut update_checks = tokio::time::interval_at(
        tokio::time::Instant::now() + UPDATE_CHECK_FIRST,
        UPDATE_CHECK_EVERY,
    );
    update_checks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let result = async {
        loop {
            let message = tokio::select! {
                message = incoming.recv() => message,
                failure = &mut accepting => {
                    match failure { Ok(Err(error)) => return Err(error.into()), Err(error) => return Err(error.into()), Ok(Ok(())) => return Err(io::Error::other("control listener stopped").into()) }
                }
                // Windows: the window usually is not open, so start Glide now and then just to look for updates.
                _ = update_checks.tick(), if tray_active && ui_path.is_some() => {
                    if let Some(ui) = &ui_path {
                        if launch_ui(ui, &["--check-update"]).is_err() { tracing::warn!("could not check for updates"); }
                    }
                    continue;
                }
                action = tray_actions.recv(), if tray_active => {
                    let Some(action) = action else { tray_active = false; continue; };
                    let (method, params) = match action {
                        tray::TrayAction::Open => {
                            if let Some(ui) = &ui_path {
                                if launch_ui(ui, &[]).is_err() { tracing::warn!("could not open Glide window"); }
                            }
                            continue;
                        },
                        tray::TrayAction::OpenLogs => {
                            #[cfg(windows)]
                            {
                                let directory = instance.data_dir.join("logs");
                                // Shell operations stay off the input/engine thread.
                                let _ = std::thread::Builder::new().name("glide-open-logs".into()).spawn(move || {
                                    use std::os::windows::process::CommandExt;
                                    let result = std::process::Command::new("explorer.exe")
                                        .arg(directory).creation_flags(0x0800_0000)
                                        .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn();
                                    if result.is_err() { tracing::warn!("could not open logs folder"); }
                                });
                            }
                            continue;
                        },
                        tray::TrayAction::ToggleSharing => ("set_sharing", serde_json::json!({"enabled":!state.sharing_enabled})),
                        tray::TrayAction::ReturnHome => ("return_home", serde_json::json!({})),
                        tray::TrayAction::Quit => {
                            #[cfg(windows)]
                            let _ = glide_platform_win::restore_system_cursors();
                            ("app.shutdown", serde_json::json!({}))
                        },
                    };
                    Some(Message::Request(0, Request { id:0, method:method.into(), params }))
                }
            };
            match message {
                Some(Message::Joined(id, client)) => {
                    if client.send(ready.clone(), None) && client.send(snapshot.clone(),None) { clients.insert(id,client); }
                    else { let _ = client.cancel.send(true); }
                    if let Some(tray) = &tray { tray.update(&Event::State(Box::new(state.clone())), ui_clients(&clients)); }
                }
                Some(Message::Gone(id)) => {
                    clients.remove(&id);
                    pending.retain(|_,route| route.0 != id);
                    if let Some(tray) = &tray { tray.update(&Event::State(Box::new(state.clone())), ui_clients(&clients)); }
                }
                Some(Message::Request(id,mut request)) => {
                    if id != 0 && !clients.contains_key(&id) { continue; }
                    if pending.values().filter(|route|route.0 == id).count() >= 128 {
                        if let Some(client) = clients.remove(&id) { let _ = client.cancel.send(true); }
                        pending.retain(|_,route|route.0 != id);
                        continue;
                    }
                    let route = (id,request.id,request.method == "app.shutdown");
                    request.id = next_request;
                    next_request = next_request.checked_add(1).ok_or_else(|| io::Error::other("request ids exhausted"))?;
                    let bytes = SecretBytes(glide_proto::codec::encode_jsonl(&request)?);
                    if requests.try_send(bytes).is_err() {
                        if let Some(client) = clients.remove(&id) { let _ = client.cancel.send(true); }
                        pending.retain(|_,route|route.0 != id);
                    } else { pending.insert(request.id,route); }
                }
                Some(Message::Output(bytes)) => {
                    if let Ok(mut response) = serde_json::from_slice::<Response>(&bytes.0) {
                        let Some((id,original,shutdown)) = pending.remove(&response.id) else { continue; };
                        response.id = original;
                        if let Some(client) = clients.get(&id) {
                            let bytes = Arc::new(SecretBytes(glide_proto::codec::encode_jsonl(&response)?));
                            let (flushed, received) = oneshot::channel();
                            if !client.send(bytes,shutdown.then_some(flushed)) {
                                if let Some(client) = clients.remove(&id) { let _ = client.cancel.send(true); }
                                pending.retain(|_,route|route.0 != id);
                            } else if shutdown {
                                // The update handshake is flushed before dropping any connection.
                                let _ = tokio::time::timeout(WRITE_TIMEOUT,received).await;
                            }
                        }
                    } else {
                        let event:Event = serde_json::from_slice(&bytes.0)?;
                        let bytes = Arc::new(bytes);
                        if let Event::State(next) = &event { state = *next.clone(); snapshot = bytes.clone(); }
                        if let Some(tray) = &tray { tray.update(&event, ui_clients(&clients)); }
                        clients.retain(|_,client| {
                            if client.send(bytes.clone(),None) { true } else { let _ = client.cancel.send(true); false }
                        });
                        pending.retain(|_,route|route.0 == 0 || clients.contains_key(&route.0));
                    }
                }
                Some(Message::End) | None => break,
            }
        }
        Ok::<(),anyhow::Error>(())
    }.await;
    accepting.abort();
    request_writer.abort();
    output_reader.abort();
    for client in clients.values() {
        let _ = client.cancel.send(true);
    }
    drop(clients);
    #[cfg_attr(not(windows), allow(clippy::drop_non_drop))]
    // the tray only has a destructor on Windows
    drop(tray);
    // Core's drop guard restores input if the broker itself fails.
    if result.is_err() {
        engine.abort();
    }
    let engine_result = engine.await;
    result?;
    engine_result??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    // Prevents same-user junk from locking the UI out, or leaking token validity on rejection.
    #[tokio::test]
    async fn wrong_auth_burst_cannot_lock_out_valid_token_and_rejections_are_identical() {
        let token = Token([0x42; 32]);
        let failures = Arc::new(AtomicU64::new(0));
        let mut tasks = tokio::task::JoinSet::new();
        for n in 0..100 {
            let failures = failures.clone();
            tasks.spawn(async move {
                let input = if n % 2 == 0 {
                    "{\"auth\":\"bad\"}\n"
                } else {
                    "{\"auth\":true}\n"
                };
                let now = Instant::now();
                let error = authenticate(
                    &mut BufReader::new(input.as_bytes()),
                    &Token([0x42; 32]),
                    &failures,
                )
                .await
                .unwrap_err();
                assert!(now.elapsed() >= REJECT_DELAY);
                assert_eq!(error.to_string(), "authentication rejected");
            });
        }
        while let Some(result) = tasks.join_next().await {
            result.unwrap();
        }
        assert_eq!(failures.load(Ordering::Relaxed), 100);
        for kind in [None, Some("ui"), Some("tool")] {
            let mut auth = serde_json::json!({"auth":hex::encode(token.0)});
            if let Some(kind) = kind {
                auth["client"] = kind.into();
            }
            let input = format!("{auth}\n");
            let started = Instant::now();
            let result = authenticate(&mut BufReader::new(input.as_bytes()), &token, &failures)
                .await
                .unwrap();
            assert!(started.elapsed() < Duration::from_millis(500));
            assert_eq!(matches!(result, Some(ClientKind::Ui)), kind == Some("ui"));
        }
        // Every position must participate in the existing volatile fixed-width comparison.
        for index in 0..32 {
            let mut supplied = token.0;
            supplied[index] ^= 1;
            assert!(!token.matches(&hex::encode(supplied)));
        }
        let source = include_str!("control.rs");
        let compare = source
            .split("fn matches")
            .nth(1)
            .unwrap()
            .split("impl Drop for Token")
            .next()
            .unwrap();
        assert!(compare.contains("read_volatile(expected) ^ std::ptr::read_volatile(supplied)"));
        assert!(compare.contains("black_box(difference) == 0"));
    }

    // Prevents pending handshakes from occupying the UI's four authenticated slots indefinitely.
    #[tokio::test]
    async fn held_junk_is_capped_and_valid_login_waits_at_most_first_line_timeout() {
        let limits = Arc::new(Limits::new());
        let token = Arc::new(Token([0x42; 32]));
        let (messages, mut incoming) = mpsc::channel(128);
        let mut held = Vec::new();
        let mut tasks = tokio::task::JoinSet::new();
        for id in 0..UNAUTHENTICATED {
            let slot = limits.pending.clone().try_acquire_owned().unwrap();
            let (client, server) = tokio::io::duplex(2048);
            held.push(client);
            tasks.spawn(connection(
                server,
                id as u64,
                token.clone(),
                limits.clone(),
                messages.clone(),
                slot,
                false,
            ));
        }
        assert!(limits.pending.clone().try_acquire_owned().is_err());
        assert_eq!(limits.clients.available_permits(), CLIENTS);
        let started = Instant::now();
        let slot = tokio::time::timeout(
            AUTH_TIMEOUT + Duration::from_millis(300),
            limits.pending.clone().acquire_owned(),
        )
        .await
        .expect("junk did not release handshake slot")
        .unwrap();
        let (mut client, server) = tokio::io::duplex(2048);
        tasks.spawn(connection(
            server,
            100,
            token.clone(),
            limits.clone(),
            messages,
            slot,
            false,
        ));
        client
            .write_all(format!("{{\"auth\":\"{}\"}}\n", hex::encode(token.0)).as_bytes())
            .await
            .unwrap();
        let line = tokio::time::timeout(
            Duration::from_millis(500),
            ipc::read_line(&mut BufReader::new(client)),
        )
        .await
        .expect("valid login delayed after junk expiry")
        .unwrap()
        .unwrap();
        assert_eq!(line, b"{\"auth\":\"ok\"}\n");
        assert!(started.elapsed() < AUTH_TIMEOUT + Duration::from_millis(500));
        assert!(matches!(
            incoming.recv().await,
            Some(Message::Joined(100, _))
        ));
        tasks.abort_all();
    }

    // Prevents oversized/unterminated IPC auth from allocating without bound or entering the API.
    #[tokio::test]
    async fn malformed_oversized_incomplete_auth_closes_without_output() {
        for input in [
            "{\"auth\":true}\n".into(),
            "x".repeat(1025),
            "{\"auth\":\"bad\"}".into(),
        ] {
            let (mut client, server) = tokio::io::duplex(2048);
            let limits = Arc::new(Limits::new());
            let slot = limits.pending.clone().try_acquire_owned().unwrap();
            let (messages, _) = mpsc::channel(1);
            let task = tokio::spawn(connection(
                server,
                1,
                Arc::new(Token([0x42; 32])),
                limits,
                messages,
                slot,
                false,
            ));
            client.write_all(input.as_bytes()).await.unwrap();
            client.shutdown().await.unwrap();
            let mut reader = BufReader::new(client);
            assert!(
                tokio::time::timeout(Duration::from_secs(1), ipc::read_line(&mut reader))
                    .await
                    .unwrap()
                    .unwrap()
                    .is_none()
            );
            task.await.unwrap();
        }
    }
    #[test]
    fn slow_outbound_client_does_not_stall_healthy_client() {
        fn client() -> (Client, mpsc::Receiver<Frame>) {
            let (output, rx) = mpsc::channel(1);
            let (cancel, _) = watch::channel(false);
            (
                Client {
                    kind: ClientKind::Tool,
                    output,
                    budget: Arc::new(Semaphore::new(OUTBOUND_BYTES)),
                    cancel,
                },
                rx,
            )
        }
        let (slow, _slow_rx) = client();
        let (healthy, mut healthy_rx) = client();
        let bytes = Arc::new(SecretBytes(b"{}\n".to_vec()));
        assert!(slow.send(bytes.clone(), None));
        assert!(!slow.send(bytes.clone(), None));
        for _ in 0..100 {
            assert!(healthy.send(bytes.clone(), None));
            assert!(healthy_rx.try_recv().is_ok());
        }
        assert!(!healthy.send(Arc::new(SecretBytes(vec![0; OUTBOUND_BYTES + 1])), None));
    }
}
