use serde_json::{json, Value};

#[tokio::test]
async fn stdio_permission_request_returns_empty_result_and_refreshed_state() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let dir = tempfile::tempdir().expect("dir");
    let core = glide_daemon::Core::mock(dir.path(), None)
        .await
        .expect("core");
    let input = core.mock_platform().expect("mock").input.clone();
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server);
    let task = tokio::spawn(glide_daemon::ipc::serve(server_read, server_write, core));
    let (client_read, mut client_write) = tokio::io::split(client);
    let mut lines = BufReader::new(client_read).lines();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        let ready: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(ready["event"], "ready");
        let initial: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(initial["data"]["permissions"]["restart_required"], false);
        client_write
            .write_all(b"{\"id\":71,\"method\":\"permissions.request\",\"params\":{}}\n")
            .await
            .unwrap();
        let mut response = false;
        let mut refresh = false;
        while !response || !refresh {
            let frame: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            if frame["id"] == 71 {
                assert_eq!(frame["ok"], true);
                assert_eq!(frame["result"], json!({}));
                response = true;
            }
            if frame["event"] == "state" {
                assert_eq!(frame["data"]["permissions"]["restart_required"], false);
                refresh = true;
            }
        }
        assert_eq!(input.permission_call_counts().unwrap().1, 1);
        client_write
            .write_all(b"{\"id\":72,\"method\":\"app.shutdown\",\"params\":{}}\n")
            .await
            .unwrap();
        while let Some(line) = lines.next_line().await.unwrap() {
            let frame: Value = serde_json::from_str(&line).unwrap();
            if frame["id"] == 72 {
                assert_eq!(frame["ok"], true);
                break;
            }
        }
        task.await.expect("server task").expect("server");
    })
    .await
    .expect("request never waits for user interaction");
}

#[cfg(windows)]
#[test]
fn glided_embeds_the_per_monitor_v2_manifest_resource() {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::LibraryLoader::{
        FindResourceW, LoadLibraryExW, LoadResource, LockResource, SizeofResource,
        LOAD_LIBRARY_AS_DATAFILE, LOAD_LIBRARY_AS_IMAGE_RESOURCE,
    };
    let path: Vec<u16> = std::path::Path::new(env!("CARGO_BIN_EXE_glided"))
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: path is terminated UTF-16; loading as data/image resource executes no code.
    let module = unsafe {
        LoadLibraryExW(
            path.as_ptr(),
            std::ptr::null_mut(),
            LOAD_LIBRARY_AS_DATAFILE | LOAD_LIBRARY_AS_IMAGE_RESOURCE,
        )
    };
    assert!(!module.is_null(), "load built executable resources");
    struct Module(windows_sys::Win32::Foundation::HMODULE);
    impl Drop for Module {
        fn drop(&mut self) {
            // SAFETY: this owner balances the successful LoadLibraryExW above.
            unsafe { windows_sys::Win32::Foundation::FreeLibrary(self.0) };
        }
    }
    let owner = Module(module);
    // SAFETY: integer resource identifiers 1 and RT_MANIFEST (24) are Win32 constants.
    let resource = unsafe {
        FindResourceW(
            owner.0,
            std::ptr::without_provenance(1),
            std::ptr::without_provenance(24),
        )
    };
    assert!(!resource.is_null(), "executable manifest resource #1");
    // SAFETY: resource belongs to the loaded module retained by owner.
    let size = unsafe { SizeofResource(owner.0, resource) } as usize;
    // SAFETY: LoadResource and LockResource return borrowed storage valid while owner lives.
    let bytes = unsafe { LockResource(LoadResource(owner.0, resource)) };
    assert!(!bytes.is_null() && size > 0);
    // SAFETY: the resource pointer and exact length were returned by Win32; owner is still live.
    let text = std::str::from_utf8(unsafe { std::slice::from_raw_parts(bytes.cast::<u8>(), size) })
        .expect("UTF-8 manifest");
    assert!(text.contains("PerMonitorV2"));
    assert!(text.contains("true/pm"));
}

#[test]
fn pairing_join_response_follows_verification_and_confirmation() {
    let dir = tempfile::tempdir().expect("directory");
    let frames = run(
        dir.path(),
        &[
            json!({"id":1,"method":"pairing.join","params":{"address":"127.0.0.1:24801","code":"123456"}}),
            json!({"id":2,"method":"get_state","params":{}}),
            json!({"id":3,"method":"pairing.confirm","params":{"accepted":true}}),
            json!({"id":4,"method":"app.shutdown","params":{}}),
        ],
    );
    let verify = frames
        .iter()
        .position(|frame| frame["event"] == "pairing.verify")
        .expect("verify");
    let reply = frames
        .iter()
        .position(|frame| frame["id"] == 1)
        .expect("deferred reply");
    let confirm = frames
        .iter()
        .position(|frame| frame["id"] == 3)
        .expect("confirm reply");
    assert!(verify < confirm && confirm < reply);
    assert_eq!(
        frames.iter().find(|frame| frame["id"] == 2).expect("state")["result"]["peers"],
        json!([])
    );
    assert_eq!(frames[reply]["ok"], true);
}
use std::{
    io::Write,
    process::{Command, Stdio},
};

fn daemon() -> Command {
    // Canonicalization supplies the Windows extended-path prefix for the external target directory.
    Command::new(std::fs::canonicalize(env!("CARGO_BIN_EXE_glided")).expect("daemon binary"))
}

fn run(dir: &std::path::Path, requests: &[Value]) -> Vec<Value> {
    let mut child = daemon()
        .args(["--mock-backends", "--data-dir"])
        .arg(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("daemon starts");
    let mut stdin = child.stdin.take().expect("stdin");
    for request in requests {
        writeln!(stdin, "{request}").expect("request");
    }
    drop(stdin);
    let output = child.wait_with_output().expect("daemon exits");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("UTF-8 protocol")
        .lines()
        .map(|line| serde_json::from_str(line).expect("stdout is exclusively JSONL"))
        .collect()
}

#[test]
fn stdio_ready_first_all_methods_and_restart_persistence() {
    let dir = tempfile::tempdir().expect("test directory");
    let frames = run(
        dir.path(),
        &[
            json!({"id":1,"method":"get_state","params":{},"future":true}),
            json!({"id":2,"method":"set_settings","params":{"patch":{"device_name":"Desk","clipboard":{"sync_files":false},"future_setting":1}}}),
            json!({"id":3,"method":"set_sharing","params":{"enabled":false}}),
            json!({"id":4,"method":"pairing.start_host","params":{}}),
            json!({"id":5,"method":"pairing.cancel_host","params":{}}),
            json!({"id":6,"method":"return_home","params":{}}),
            json!({"id":7,"method":"permissions.open_settings","params":{"kind":"accessibility"}}),
            json!({"id":8,"method":"transfer.cancel","params":{"id":"missing"}}),
            json!({"id":9,"method":"transfer.confirm","params":{"id":"missing","accept":true}}),
            json!({"id":10,"method":"peer.unpair","params":{"device_id":"missing"}}),
            json!({"id":11,"method":"pairing.join","params":{"address":"127.0.0.1:1","code":"123456"}}),
            json!({"id":12,"method":"peer.add_manual","params":{"address":"127.0.0.1:1"}}),
            json!({"id":13,"method":"set_layout","params":{"devices":[]}}),
            json!({"id":14,"method":"unknown","params":{}}),
            json!({"id":15,"method":"app.shutdown","params":{}}),
        ],
    );
    assert_eq!(frames[0]["event"], "ready");
    assert_eq!(frames[1]["event"], "state");
    for id in 1..=15 {
        let frame = frames
            .iter()
            .find(|frame| frame["id"] == id)
            .expect("response for every method");
        assert_eq!(frame["ok"], id <= 7 || id == 15, "id={id}: {frame}");
    }
    let initial_id = frames[1]["data"]["self"]["device_id"].clone();
    let second = run(
        dir.path(),
        &[json!({"id":16,"method":"get_state","params":{}})],
    );
    assert_eq!(second[1]["data"]["self"]["device_id"], initial_id);
    assert_eq!(second[1]["data"]["settings"]["device_name"], "Desk");
    assert_eq!(
        second[1]["data"]["settings"]["clipboard"]["sync_files"],
        false
    );
    assert_eq!(
        second[1]["data"]["settings"]["clipboard"]["sync_text"],
        true
    );
    assert_eq!(second[1]["data"]["sharing_enabled"], false);
}

#[test]
fn native_mode_fails_closed_and_bad_config_is_preserved() {
    let dir = tempfile::tempdir().expect("directory");
    std::fs::write(dir.path().join("config.json"), b"corrupt").expect("config");
    let output = daemon()
        .arg("--data-dir")
        .arg(dir.path())
        .output()
        .expect("process");
    assert!(!output.status.success());
    let frames = String::from_utf8(output.stdout)
        .expect("UTF8")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("JSONL"))
        .collect::<Vec<_>>();
    assert_eq!(frames[0]["event"], "notification");
    assert_eq!(frames[1]["ok"], false);
    assert!(!frames.iter().any(|frame| frame["event"] == "ready"));
    let output = daemon()
        .args(["--mock-backends", "--data-dir"])
        .arg(dir.path())
        .output()
        .expect("process");
    assert!(!output.status.success());
    assert_eq!(
        std::fs::read(dir.path().join("config.json")).expect("data"),
        b"corrupt"
    );
}

#[test]
fn help_and_headless_eof() {
    let help = daemon().arg("--help").output().expect("help");
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).expect("text");
    for flag in [
        "--mock-platform",
        "--mock-backends",
        "--headless",
        "--no-discovery",
        "--port",
    ] {
        assert!(help.contains(flag));
    }
    let dir = tempfile::tempdir().expect("directory");
    let mut child = daemon()
        .args(["--headless", "--mock-backends", "--data-dir"])
        .arg(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("headless");
    std::thread::sleep(std::time::Duration::from_millis(250));
    let status = child.try_wait().expect("status");
    child.kill().expect("stop mock process");
    child.wait().expect("reap");
    assert!(status.is_none(), "stdin EOF must leave headless running");
}
