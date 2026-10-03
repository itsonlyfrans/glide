//! Process-level native acceptance. Run only on a host with the OS keystore:
//! Set `CARGO_TARGET_DIR` to a directory outside this checkout before building.
//! Headless overlapped-pipe acceptance:
//! `cargo test -p glide-daemon --test process_native --offline --locked -- --ignored --exact two_headless_native_daemons_auth_pair_clipboard_and_shutdown --nocapture`
//! Stdio acceptance:
//! `cargo test -p glide-daemon --test process_native --offline --locked -- --ignored --exact two_native_daemons_pair_clipboard_and_unpair --nocapture`

#![cfg(any(windows, target_os = "macos"))]

use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, Write},
    net::UdpSocket,
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

type ControlWrite = (Vec<u8>, mpsc::SyncSender<std::io::Result<()>>);

async fn control_io<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    mut stream: S,
    metadata: glide_daemon::control::Metadata,
    mut requested: tokio::sync::mpsc::Receiver<ControlWrite>,
    frames: mpsc::Sender<Value>,
) {
    use tokio::io::AsyncWriteExt;
    let auth = format!("{}\n", json!({"auth":metadata.token}));
    if !matches!(
        tokio::time::timeout(Duration::from_secs(2), stream.write_all(auth.as_bytes())).await,
        Ok(Ok(()))
    ) {
        return;
    }
    let (read, mut write) = tokio::io::split(stream);
    let reading = async {
        let mut reader = tokio::io::BufReader::new(read);
        while let Ok(Ok(Some(line))) = tokio::time::timeout(
            Duration::from_secs(30),
            glide_daemon::ipc::read_line(&mut reader),
        )
        .await
        {
            let Ok(frame) = serde_json::from_slice(&line) else {
                break;
            };
            if frames.send(frame).is_err() {
                break;
            }
        }
    };
    let writing = async {
        while let Ok(Some((bytes, result))) =
            tokio::time::timeout(Duration::from_secs(60), requested.recv()).await
        {
            let status = tokio::time::timeout(Duration::from_secs(2), async {
                write.write_all(&bytes).await?;
                write.flush().await
            })
            .await
            .unwrap_or_else(|_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "control write exceeded two seconds",
                ))
            });
            let failed = status.is_err();
            let _ = result.send(status);
            if failed {
                break;
            }
        }
    };
    // Both persistent futures are polled independently; a pending read never blocks a write.
    tokio::select! { _ = reading => {}, _ = writing => {} }
}

struct Daemon {
    child: Child,
    stdin: Option<Box<dyn Write + Send>>,
    control: Option<tokio::sync::mpsc::Sender<ControlWrite>>,
    frames: Receiver<Value>,
    pending: VecDeque<Value>,
    next_id: u64,
}

impl Daemon {
    fn start(data_dir: &std::path::Path, port: u16, seed: Option<&str>) -> Self {
        Self::start_mode(data_dir, port, seed, false)
    }

    fn start_mode(
        data_dir: &std::path::Path,
        port: u16,
        seed: Option<&str>,
        headless: bool,
    ) -> Self {
        let exe = std::fs::canonicalize(env!("CARGO_BIN_EXE_glided")).expect("glided binary");
        let mut command = Command::new(exe);
        command
            .args(["--mock-platform", "--no-discovery", "--port"])
            .arg(port.to_string())
            .arg("--data-dir")
            .arg(data_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(seed) = seed {
            command.arg("--mock-clipboard-text").arg(seed);
        }
        if headless {
            command.arg("--headless");
        }
        let mut child = command.spawn().expect("glided starts");
        let (sender, frames) = mpsc::channel();
        let (stdin, control) = if headless {
            drop(child.stdin.take());
            let (requests, requested) = tokio::sync::mpsc::channel(16);
            let data_dir = data_dir.to_owned();
            // ClientOptions creates an OVERLAPPED handle. Cloning a synchronous File serializes
            // ReadFile/WriteFile on Windows and deadlocks when the reader waits for a response.
            thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("control I/O runtime");
                runtime.block_on(async move {
                    let deadline = Instant::now() + Duration::from_secs(10);
                    let metadata = loop {
                        if let Ok(metadata) = glide_daemon::control::read_metadata(&data_dir) {
                            break metadata;
                        }
                        if Instant::now() >= deadline {
                            return;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    };
                    #[cfg(windows)]
                    let stream = loop {
                        match tokio::net::windows::named_pipe::ClientOptions::new()
                            .open(&metadata.endpoint)
                        {
                            Ok(stream) => break stream,
                            Err(_) if Instant::now() < deadline => {
                                tokio::time::sleep(Duration::from_millis(10)).await
                            }
                            Err(_) => return,
                        }
                    };
                    #[cfg(target_os = "macos")]
                    let stream = match tokio::time::timeout(
                        Duration::from_secs(2),
                        tokio::net::UnixStream::connect(&metadata.endpoint),
                    )
                    .await
                    {
                        Ok(Ok(stream)) => stream,
                        _ => return,
                    };
                    control_io(stream, metadata, requested, sender).await;
                });
            });
            (None, Some(requests))
        } else {
            let stdout = child.stdout.take().expect("stdout");
            thread::spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    let Ok(frame) = serde_json::from_str(&line) else {
                        break;
                    };
                    if sender.send(frame).is_err() {
                        break;
                    }
                }
            });
            (
                Some(Box::new(child.stdin.take().expect("stdin")) as Box<dyn Write + Send>),
                None,
            )
        };
        let stderr = child.stderr.take().expect("stderr");
        thread::spawn(move || {
            let mut stderr = stderr;
            let _ = std::io::copy(&mut stderr, &mut std::io::sink());
        });
        let mut daemon = Self {
            stdin,
            control,
            child,
            frames,
            pending: VecDeque::new(),
            next_id: 1,
        };
        if headless {
            let auth = daemon.wait_frame(
                "control authentication (metadata/connect/auth within 10 seconds)",
                |frame| frame["auth"].is_string(),
            );
            assert_eq!(auth, json!({"auth":"ok"}), "control authentication refused");
        }
        let ready = daemon.wait_frame("engine ready", |frame| frame["event"] == "ready");
        assert_eq!(ready["event"], "ready");
        daemon.wait_frame("initial state", |frame| frame["event"] == "state");
        daemon
    }

    fn send(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let bytes = format!("{}\n", json!({"id":id,"method":method,"params":params})).into_bytes();
        if let Some(control) = &self.control {
            let (sent, received) = mpsc::sync_channel(1);
            control
                .try_send((bytes, sent))
                .expect("control write queue closed or full");
            received
                .recv_timeout(Duration::from_secs(3))
                .unwrap_or_else(|_| panic!("control write deadline exceeded: {method}"))
                .unwrap_or_else(|error| panic!("control write failed for {method}: {error}"));
        } else {
            let stdin = self.stdin.as_mut().expect("stdio writer");
            stdin.write_all(&bytes).expect("write JSONL request");
            stdin.flush().expect("flush JSONL request");
        }
        id
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, params);
        let response = self.wait_frame(&format!("response to {method} (id {id})"), |frame| {
            frame["id"].as_u64() == Some(id)
        });
        assert_eq!(response["ok"], true, "IPC method failed: {method}");
        response["result"].clone()
    }

    fn wait_frame(&mut self, description: &str, matches: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(index) = self.pending.iter().position(&matches) {
                return self.pending.remove(index).expect("matched frame");
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let frame = self
                .frames
                .recv_timeout(remaining)
                .unwrap_or_else(|error| panic!("waiting for {description}: {error}"));
            Self::assert_no_error_notification(&frame);
            if matches(&frame) {
                return frame;
            }
            self.pending.push_back(frame);
        }
    }

    fn state(&mut self) -> Value {
        self.request("get_state", json!({}))
    }

    fn wait_state(&mut self, description: &str, matches: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            let state = self.state();
            if matches(&state) {
                return state;
            }
            assert!(
                Instant::now() < deadline,
                "state condition timed out: {description}"
            );
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn shutdown(&mut self) {
        let id = self.send("app.shutdown", json!({}));
        let response = self.wait_frame("shutdown acknowledgement", |frame| {
            frame["id"].as_u64() == Some(id)
        });
        assert_eq!(response["ok"], true, "shutdown request failed");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if self.child.try_wait().expect("process state").is_some() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "glided did not exit after shutdown"
            );
            thread::sleep(Duration::from_millis(25));
        }
        for frame in &self.pending {
            Self::assert_no_error_notification(frame);
        }
        // The stdout reader drains through EOF, including events after the shutdown response.
        let drain_deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match self
                .frames
                .recv_timeout(drain_deadline.saturating_duration_since(Instant::now()))
            {
                Ok(frame) => Self::assert_no_error_notification(&frame),
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    panic!("control reader did not reach EOF after shutdown")
                }
            }
        }
    }

    fn assert_no_error_notification(frame: &Value) {
        assert!(
            frame["event"] != "notification" || frame["data"]["level"] != "error",
            "unexpected error notification during native acceptance: {frame}"
        );
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            // Cleanup after a failed assertion must not replace the original failure with a hang.
            let deadline = Instant::now() + Duration::from_secs(2);
            while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

fn free_udp_port() -> u16 {
    UdpSocket::bind("127.0.0.1:0")
        .expect("reserve loopback port")
        .local_addr()
        .expect("port")
        .port()
}

#[test]
#[ignore = "needs the OS keystore (run locally)"]
fn two_native_daemons_pair_clipboard_and_unpair() {
    pair_clipboard_and_unpair(false);
}

#[test]
#[ignore = "needs the OS keystore (run locally)"]
fn two_headless_native_daemons_auth_pair_clipboard_and_shutdown() {
    pair_clipboard_and_unpair(true);
}

fn pair_clipboard_and_unpair(headless: bool) {
    let temp = tempfile::tempdir().expect("process test directory");
    let a_data = temp.path().join("a");
    let b_data = temp.path().join("b");
    std::fs::create_dir_all(&a_data).expect("A data directory");
    std::fs::create_dir_all(&b_data).expect("B data directory");
    let a_port = free_udp_port();
    let mut b_port = free_udp_port();
    for _ in 0..100 {
        if b_port != a_port {
            break;
        }
        b_port = free_udp_port();
    }
    assert_ne!(a_port, b_port, "could not reserve distinct UDP ports");
    let mut a = if headless {
        Daemon::start_mode(&a_data, a_port, Some("process clipboard sentinel"), true)
    } else {
        Daemon::start(&a_data, a_port, Some("process clipboard sentinel"))
    };
    let mut b = if headless {
        Daemon::start_mode(&b_data, b_port, None, true)
    } else {
        Daemon::start(&b_data, b_port, None)
    };

    b.request(
        "set_settings",
        json!({"patch":{"clipboard":{"max_auto_mb":0}}}),
    );
    let host = a.request("pairing.start_host", json!({}));
    let join_id = b.send(
        "pairing.join",
        json!({
            "address": format!("127.0.0.1:{a_port}"),
            "code": host["code"].as_str().expect("pairing code")
        }),
    );
    let a_verify = a.wait_frame("host SAS prompt", |frame| {
        frame["event"] == "pairing.verify"
    });
    let b_verify = b.wait_frame("joiner SAS prompt", |frame| {
        frame["event"] == "pairing.verify"
    });
    assert_eq!(a_verify["data"]["phrase"], b_verify["data"]["phrase"]);

    a.request("pairing.confirm", json!({"accepted":true}));
    b.request("pairing.confirm", json!({"accepted":true}));
    let join = b.wait_frame("pairing.join after both SAS confirmations", |frame| {
        frame["id"].as_u64() == Some(join_id)
    });
    assert_eq!(join["ok"], true, "pairing.join failed");
    let b_device_id = b.state()["self"]["device_id"]
        .as_str()
        .expect("B device ID")
        .to_owned();
    a.wait_state("paired peer connected", |state| {
        state["peers"].as_array().is_some_and(|peers| {
            peers
                .iter()
                .any(|peer| peer["device_id"] == b_device_id && peer["connection"] == "connected")
        })
    });
    let a_device_id = a.state()["self"]["device_id"]
        .as_str()
        .expect("A device ID")
        .to_owned();
    b.wait_state("joiner's paired peer connected", |state| {
        state["peers"].as_array().is_some_and(|peers| {
            peers
                .iter()
                .any(|peer| peer["device_id"] == a_device_id && peer["connection"] == "connected")
        })
    });

    let transfer = b.wait_state("clipboard awaiting consent", |state| {
        state["transfers"].as_array().is_some_and(|transfers| {
            transfers.iter().any(|transfer| {
                transfer["name"] == "Clipboard" && transfer["state"] == "awaiting_confirm"
            })
        })
    });
    let pending = transfer["transfers"]
        .as_array()
        .expect("transfers")
        .iter()
        .find(|transfer| transfer["name"] == "Clipboard" && transfer["state"] == "awaiting_confirm")
        .expect("clipboard confirmation");
    let transfer_id = pending["id"].as_str().expect("transfer ID").to_owned();
    assert!(pending["bytes_total"].as_u64().unwrap_or_default() > 0);
    b.request("transfer.confirm", json!({"id":transfer_id,"accept":true}));
    b.wait_state("clipboard fully transferred", |state| {
        state["transfers"].as_array().is_some_and(|transfers| {
            transfers.iter().any(|transfer| {
                transfer["id"] == transfer_id
                    && transfer["state"] == "done"
                    && transfer["bytes_done"] == transfer["bytes_total"]
            })
        })
    });

    a.request("peer.unpair", json!({"device_id":b_device_id}));
    a.wait_state("peer unpaired", |state| {
        state["peers"]
            .as_array()
            .is_some_and(|peers| peers.iter().all(|peer| peer["device_id"] != b_device_id))
    });
    a.shutdown();
    b.shutdown();
}

// Prevents the synchronous Windows pipe deadlock without requiring the real OS keystore.
#[cfg(windows)]
#[tokio::test]
async fn overlapped_control_writes_while_reader_is_waiting() {
    use tokio::io::AsyncWriteExt;
    let endpoint = format!(
        "\\\\.\\pipe\\glide-harness-{}-{:016x}",
        std::process::id(),
        rand::random::<u64>()
    );
    let server = tokio::net::windows::named_pipe::ServerOptions::new()
        .first_pipe_instance(true)
        .create(&endpoint)
        .unwrap();
    let metadata = glide_daemon::control::Metadata {
        endpoint: endpoint.clone(),
        token: "test sentinel".into(),
        pid: std::process::id(),
        version: "test".into(),
        protocol: 1,
        started_at_ms: 0,
    };
    let (requested, requests) = tokio::sync::mpsc::channel(16);
    let (sent, frames) = mpsc::channel();
    let worker = tokio::spawn(async move {
        let stream = tokio::net::windows::named_pipe::ClientOptions::new()
            .open(&endpoint)
            .unwrap();
        control_io(stream, metadata, requests, sent).await;
    });
    tokio::time::timeout(Duration::from_secs(2), server.connect())
        .await
        .expect("harness client never connected")
        .unwrap();
    let mut reader = tokio::io::BufReader::new(server);
    tokio::time::timeout(
        Duration::from_secs(2),
        glide_daemon::ipc::read_line(&mut reader),
    )
    .await
    .expect("harness auth write stalled")
    .unwrap()
    .unwrap();
    tokio::time::timeout(
        Duration::from_secs(2),
        reader.get_mut().write_all(b"{\"auth\":\"ok\"}\n"),
    )
    .await
    .expect("test server auth response stalled")
    .unwrap();
    // Let the background reader park waiting for its next frame BEFORE submitting a request.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (completed, completion) = mpsc::sync_channel(1);
    requested
        .try_send((
            b"{\"id\":1,\"method\":\"get_state\",\"params\":{}}\n".to_vec(),
            completed,
        ))
        .unwrap();
    let line = tokio::time::timeout(
        Duration::from_secs(2),
        glide_daemon::ipc::read_line(&mut reader),
    )
    .await
    .expect("request write blocked behind pending reader")
    .unwrap()
    .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&line).unwrap()["method"],
        "get_state"
    );
    assert!(completion
        .try_recv()
        .expect("write acknowledgement missing")
        .is_ok());
    tokio::time::timeout(
        Duration::from_secs(2),
        reader
            .get_mut()
            .write_all(b"{\"id\":1,\"ok\":true,\"result\":{}}\n"),
    )
    .await
    .expect("test server result response stalled")
    .unwrap();
    drop(reader);
    tokio::time::timeout(Duration::from_secs(2), worker)
        .await
        .expect("harness did not finish at EOF")
        .unwrap();
    assert_eq!(frames.try_recv().unwrap(), json!({"auth":"ok"}));
    assert_eq!(frames.try_recv().unwrap()["id"], 1);
}
