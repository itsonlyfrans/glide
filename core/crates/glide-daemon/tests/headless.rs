#![cfg(windows)]
use glide_daemon::control::{read_metadata, Metadata};
use serde_json::{json, Value};
use std::{
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncWriteExt, BufReader},
    net::windows::named_pipe::{ClientOptions, NamedPipeClient},
};

struct Daemon(Child);
impl Daemon {
    fn start(dir: &Path) -> Self {
        Self(
            Command::new(std::fs::canonicalize(env!("CARGO_BIN_EXE_glided")).unwrap())
                .args(["--headless", "--mock-backends", "--data-dir"])
                .arg(dir)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }
    async fn metadata(&mut self, dir: &Path) -> Metadata {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(metadata) = read_metadata(dir) {
                if metadata.pid == self.0.id() {
                    return metadata;
                }
            }
            assert!(
                self.0.try_wait().unwrap().is_none(),
                "headless startup failed"
            );
            assert!(Instant::now() < deadline, "ipc.json not published");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    async fn stopped(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                assert!(status.success());
                return;
            }
            assert!(Instant::now() < deadline, "shutdown exceeded 2 seconds");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}
type Connection = BufReader<NamedPipeClient>;
async fn connect(metadata: &Metadata) -> Connection {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match ClientOptions::new().open(&metadata.endpoint) {
            Ok(pipe) => return BufReader::new(pipe),
            Err(_) => {
                assert!(Instant::now() < deadline, "pipe unavailable");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
}
async fn frame(connection: &mut Connection) -> Option<Value> {
    let bytes = tokio::time::timeout(
        Duration::from_secs(3),
        glide_daemon::ipc::read_line(connection),
    )
    .await
    .unwrap()
    .ok()
    .flatten()?;
    Some(serde_json::from_slice(&bytes).unwrap())
}
async fn authenticate(metadata: &Metadata) -> Connection {
    let mut connection = connect(metadata).await;
    connection
        .get_mut()
        .write_all(format!("{{\"auth\":\"{}\"}}\n", metadata.token).as_bytes())
        .await
        .unwrap();
    assert_eq!(frame(&mut connection).await.unwrap(), json!({"auth":"ok"}));
    assert_eq!(frame(&mut connection).await.unwrap()["event"], "ready");
    assert_eq!(frame(&mut connection).await.unwrap()["event"], "state");
    connection
}
async fn request(connection: &mut Connection, id: u64, method: &str, params: Value) -> Value {
    connection
        .get_mut()
        .write_all(format!("{}\n", json!({"id":id,"method":method,"params":params})).as_bytes())
        .await
        .unwrap();
    loop {
        let frame = frame(connection).await.expect("response before EOF");
        if frame["id"] == id {
            return frame;
        }
    }
}

#[tokio::test]
async fn headless_stdin_eof_four_clients_auth_state_routing_single_instance_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::start(dir.path());
    let metadata = daemon.metadata(dir.path()).await;
    assert_eq!(metadata.protocol, 1);
    assert_eq!(metadata.token.len(), 64);
    assert!(metadata.endpoint.starts_with(r"\\.\pipe\glide-ctl-"));
    assert_eq!(metadata.endpoint.len(), r"\\.\pipe\glide-ctl-".len() + 16);
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        daemon.0.try_wait().unwrap().is_none(),
        "stdin EOF must not stop engine"
    );
    let duplicate = Command::new(std::fs::canonicalize(env!("CARGO_BIN_EXE_glided")).unwrap())
        .args(["--headless", "--mock-backends", "--data-dir"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert_eq!(duplicate.status.code(), Some(75));
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("Glide is already running"));
    let mut clients = Vec::new();
    for _ in 0..4 {
        clients.push(authenticate(&metadata).await);
    }
    let mut fifth = connect(&metadata).await;
    let _ = fifth
        .get_mut()
        .write_all(format!("{{\"auth\":\"{}\"}}\n", metadata.token).as_bytes())
        .await;
    assert!(
        frame(&mut fifth).await.is_none(),
        "fifth client must be refused"
    );
    for client in &mut clients {
        let response = request(client, 42, "get_state", json!({})).await;
        assert_eq!(response["ok"], true);
        assert_eq!(response["result"]["self"]["version"], metadata.version);
    }
    assert_eq!(
        request(&mut clients[0], 43, "set_sharing", json!({"enabled":false})).await["ok"],
        true
    );
    for client in clients.iter_mut().skip(1) {
        loop {
            let event = frame(client).await.unwrap();
            if event["event"] == "state" && event["data"]["sharing_enabled"] == false {
                break;
            }
        }
    }
    let started = Instant::now();
    assert_eq!(
        request(&mut clients[3], 44, "app.shutdown", json!({})).await["ok"],
        true
    );
    daemon.stopped().await;
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(!dir.path().join("ipc.json").exists());
}

#[tokio::test]
async fn stale_metadata_and_crash_lock_are_cleaned_and_token_rotates() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("ipc.json"),json!({"endpoint":"stale","token":"00","pid":std::process::id(),"version":"old","protocol":1,"started_at_ms":0}).to_string()).unwrap();
    std::fs::write(dir.path().join("engine.lock"), b"stale").unwrap();
    let mut first = Daemon::start(dir.path());
    let old = first.metadata(dir.path()).await;
    let mut client = authenticate(&old).await;
    assert_eq!(
        request(&mut client, 1, "get_state", json!({})).await["ok"],
        true
    );
    first.0.kill().unwrap();
    first.0.wait().unwrap();
    drop(client);
    let mut second = Daemon::start(dir.path());
    // Old ipc.json can remain until the new process acquires the OS lock.
    let deadline = Instant::now() + Duration::from_secs(5);
    let fresh = loop {
        if let Ok(fresh) = read_metadata(dir.path()) {
            if fresh.pid == second.0.id() {
                break fresh;
            }
        }
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(old.endpoint, fresh.endpoint);
    assert!(old.token != fresh.token, "authentication token must rotate");
    let mut client = authenticate(&fresh).await;
    assert_eq!(
        request(&mut client, 2, "app.shutdown", json!({})).await["ok"],
        true
    );
    second.stopped().await;
}

// Prevents a local junk-login flood from locking the real window out of its engine.
#[tokio::test]
async fn process_wrong_auth_burst_still_accepts_valid_login_immediately() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::start(dir.path());
    let metadata = daemon.metadata(dir.path()).await;
    // Seven bounded waves submit 100 wrong tokens, plus malformed/oversized first lines.
    for wave in 0..7 {
        let mut clients = Vec::new();
        for n in 0..16 {
            if wave * 16 + n >= 100 {
                break;
            }
            let mut client = connect(&metadata).await;
            client
                .get_mut()
                .write_all(b"{\"auth\":\"bad\"}\n")
                .await
                .unwrap();
            clients.push(client);
        }
        for client in &mut clients {
            assert!(frame(client).await.is_none());
        }
    }
    for bytes in [
        b"{\"id\":1,\"method\":\"app.shutdown\"}\n".to_vec(),
        vec![b'x'; 1025],
    ] {
        let mut client = connect(&metadata).await;
        client.get_mut().write_all(&bytes).await.unwrap();
        assert!(frame(&mut client).await.is_none());
    }
    let started = Instant::now();
    let mut valid = authenticate(&metadata).await;
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "valid token delayed by junk history"
    );
    assert_eq!(
        request(&mut valid, 1, "app.shutdown", json!({})).await["ok"],
        true
    );
    daemon.stopped().await;
}

// Prevents slow first-line clients from monopolizing admission indefinitely.
#[tokio::test]
async fn process_sixteen_held_handshakes_release_admission_within_two_seconds() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::start(dir.path());
    let metadata = daemon.metadata(dir.path()).await;
    let mut held = Vec::new();
    for _ in 0..16 {
        held.push(connect(&metadata).await);
    }
    let started = Instant::now();
    let mut valid = tokio::time::timeout(Duration::from_millis(2500), authenticate(&metadata))
        .await
        .expect("held junk starved a valid login past its timeout");
    assert!(started.elapsed() < Duration::from_millis(2500));
    assert_eq!(
        request(&mut valid, 1, "app.shutdown", json!({})).await["ok"],
        true
    );
    daemon.stopped().await;
}

#[tokio::test]
async fn process_auth_timeout_rejects_without_stopping_engine() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::start(dir.path());
    let metadata = daemon.metadata(dir.path()).await;
    let mut timed_out = connect(&metadata).await;
    let started = Instant::now();
    assert!(frame(&mut timed_out).await.is_none());
    assert!(started.elapsed() >= Duration::from_millis(1900));
    let mut valid = authenticate(&metadata).await;
    assert_eq!(
        request(&mut valid, 1, "app.shutdown", json!({})).await["ok"],
        true
    );
    daemon.stopped().await;
}

#[tokio::test]
async fn slow_process_client_is_disconnected_while_another_client_keeps_working() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::start(dir.path());
    let metadata = daemon.metadata(dir.path()).await;
    let slow = authenticate(&metadata).await;
    let mut healthy = authenticate(&metadata).await;
    let (slow_read, mut slow_write) = tokio::io::split(slow.into_inner());
    let flood = b"{\"id\":1,\"method\":\"get_state\",\"params\":{}}\n".repeat(5000);
    let flooding = tokio::spawn(async move {
        let _ = slow_write.write_all(&flood).await;
        slow_write
    });
    for id in 0..10 {
        assert_eq!(
            request(&mut healthy, id, "get_state", json!({})).await["ok"],
            true
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
    let mut slow_read = BufReader::new(slow_read);
    tokio::time::timeout(Duration::from_secs(2), async {
        while let Ok(Some(_)) = glide_daemon::ipc::read_line(&mut slow_read).await {}
    })
    .await
    .expect("slow client was not dropped");
    flooding.abort();
    assert_eq!(
        request(&mut healthy, 11, "app.shutdown", json!({})).await["ok"],
        true
    );
    daemon.stopped().await;
}

#[test]
fn cli_rejects_invalid_ui_paths_before_starting_engine() {
    let dir = tempfile::tempdir().unwrap();
    for path in [
        std::path::PathBuf::from("relative.exe"),
        dir.path().join("missing.exe"),
        dir.path().join("bad\nui.exe"),
    ] {
        let output = Command::new(std::fs::canonicalize(env!("CARGO_BIN_EXE_glided")).unwrap())
            .args(["--headless", "--mock-backends", "--data-dir"])
            .arg(dir.path())
            .arg("--ui")
            .arg(path)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!dir.path().join("ipc.json").exists());
    }
}
