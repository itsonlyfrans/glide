// Glide's window. The screens are plain HTML in ../ui; this program shows them in the operating system's own
// web view, connects them to the Glide engine, and adds the tray (macOS), notifications and self-updating.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod engine;
mod logs;

use engine::{Engine, EngineOptions, Mode};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::webview::PageLoadEvent;
use tauri::{AppHandle, Emitter, Manager, RunEvent, State, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_updater::UpdaterExt;

const MAC_BUNDLE_ID: &str = "app.glide.desktop";
const UPDATE_CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);

struct Shared {
    engine: Arc<Engine>,
    mock: bool,
    quitting: AtomicBool,
    data_root: PathBuf,
    app_log: Arc<logs::RotatingLog>,
    last_state: Mutex<Option<Value>>,
    name_seeded: AtomicBool,
    permissions_asked: AtomicBool,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    login_item: Mutex<Option<bool>>,
    update: Mutex<Option<tauri_plugin_updater::Update>>,
}

fn platform() -> &'static str {
    if cfg!(windows) {
        "win32"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

// ------------------------------------------------------------------------------------------------ window

fn create_window(app: &AppHandle) -> tauri::Result<()> {
    let builder = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
        .title("Glide")
        .inner_size(1180.0, 760.0)
        .min_inner_size(900.0, 600.0)
        .visible(false)
        .background_color(tauri::window::Color(0x16, 0x1a, 0x24, 0xff))
        // Nothing but Glide's own screens may ever load in this window.
        .on_navigation(|url| url.scheme() == "tauri" || url.host_str() == Some("tauri.localhost"))
        .on_page_load(|window, payload| {
            if payload.event() == PageLoadEvent::Finished {
                let _ = window.show();
                if std::env::var_os("GLIDE_TEST_OFFSCREEN").is_none() {
                    let _ = window.set_focus();
                }
            }
        });
    #[cfg(target_os = "macos")]
    let builder = builder
        .title_bar_style(tauri::TitleBarStyle::Overlay)
        .hidden_title(true)
        .traffic_light_position(tauri::LogicalPosition::new(16.0, 16.0));
    #[cfg(windows)]
    let builder = builder.decorations(false); // the page draws its own minimize / maximize / close buttons
    // Automated checks: render normally but far off-screen, out of the taskbar and without taking focus, so a
    // person using the computer never sees test windows appear.
    let builder = if std::env::var_os("GLIDE_TEST_OFFSCREEN").is_some() {
        builder.position(-32000.0, -32000.0).skip_taskbar(true).focused(false)
    } else {
        builder
    };
    builder.build()?;
    Ok(())
}

fn show_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    } else if let Err(e) = create_window(app) {
        app.state::<Shared>().app_log.write(&format!("could not open the window: {e}"));
    }
}

// ------------------------------------------------------------------------------------------------ engine events

fn on_engine_event(app: &AppHandle, name: &str, data: Value) {
    let shared = app.state::<Shared>();
    match name {
        "state" => {
            *lock(&shared.last_state) = Some(data.clone());
            // First run: name this computer after itself so devices are easy to tell apart.
            if data["settings"]["device_name"] == "Glide" && !shared.name_seeded.swap(true, Ordering::AcqRel) {
                let host = gethostname::gethostname().to_string_lossy().to_string();
                let host: String = host
                    .split('.')
                    .next()
                    .unwrap_or_default()
                    .chars()
                    .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '_' | '-'))
                    .take(40)
                    .collect();
                let host = host.trim().to_string();
                if !host.is_empty() && host != "Glide" {
                    let engine = shared.engine.clone();
                    tauri::async_runtime::spawn(async move {
                        engine.call("set_settings", json!({ "patch": { "device_name": host } })).await;
                    });
                }
            }
            // The first time a permission is missing, ask macOS to show its dialog (it shows each one only once).
            let perms = &data["permissions"];
            let missing = ["accessibility", "input_monitoring"]
                .iter()
                .any(|k| perms[*k] == "denied" || perms[*k] == "unknown");
            if missing && !shared.permissions_asked.swap(true, Ordering::AcqRel) {
                let engine = shared.engine.clone();
                tauri::async_runtime::spawn(async move {
                    engine.call("permissions.request", json!({})).await;
                });
            }
            #[cfg(target_os = "macos")]
            {
                tray::refresh(app, &data);
                apply_login_item(app, &shared, &data);
            }
        }
        "notification" => {
            let title = data["title"].as_str().unwrap_or("Glide").to_string();
            let body = data["body"].as_str().unwrap_or_default().to_string();
            let _ = app.notification().builder().title(title).body(body).show();
        }
        _ => {}
    }
    let _ = app.emit_to("main", "glide:event", json!({ "name": name, "data": data }));
}

#[cfg(target_os = "macos")]
fn apply_login_item(app: &AppHandle, shared: &Shared, data: &Value) {
    use tauri_plugin_autostart::ManagerExt;
    if cfg!(debug_assertions) || shared.mock {
        return;
    }
    let want = data["settings"]["startup"]["launch_at_login"].as_bool().unwrap_or(false);
    let mut applied = lock(&shared.login_item);
    if *applied == Some(want) {
        return;
    }
    *applied = Some(want);
    let autolaunch = app.autolaunch();
    let _ = if want { autolaunch.enable() } else { autolaunch.disable() };
}

// ------------------------------------------------------------------------------------------------ macOS tray

#[cfg(target_os = "macos")]
mod tray {
    use super::{lock, show_window, Shared};
    use serde_json::{json, Value};
    use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
    use tauri::{AppHandle, Manager, Wry};

    pub struct TrayItems {
        pub share: CheckMenuItem<Wry>,
    }

    pub fn create(app: &AppHandle) -> tauri::Result<()> {
        let open = MenuItem::with_id(app, "open", "Open Glide", true, None::<&str>)?;
        let share = CheckMenuItem::with_id(app, "share", "Share keyboard and mouse", true, false, None::<&str>)?;
        let home = MenuItem::with_id(app, "home", "Return to this computer", true, None::<&str>)?;
        let logs = MenuItem::with_id(app, "logs", "Show logs", true, None::<&str>)?;
        let updates = MenuItem::with_id(app, "updates", "Check for Updates…", true, None::<&str>)?;
        let quit = MenuItem::with_id(app, "quit", "Quit Glide", true, None::<&str>)?;
        let menu = Menu::with_items(
            app,
            &[
                &open,
                &PredefinedMenuItem::separator(app)?,
                &share,
                &home,
                &updates,
                &logs,
                &PredefinedMenuItem::separator(app)?,
                &quit,
            ],
        )?;
        let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/trayTemplate@2x.png"))?;
        TrayIconBuilder::with_id("glide")
            .icon(icon)
            .icon_as_template(true)
            .tooltip("Glide")
            .menu(&menu)
            .show_menu_on_left_click(false)
            .on_menu_event(|app, event| {
                let shared = app.state::<Shared>();
                let engine = shared.engine.clone();
                match event.id().as_ref() {
                    "open" => show_window(app),
                    "share" => {
                        let enabled = lock(&shared.last_state)
                            .as_ref()
                            .map(|s| !s["sharing_enabled"].as_bool().unwrap_or(false))
                            .unwrap_or(true);
                        tauri::async_runtime::spawn(async move {
                            engine.call("set_sharing", json!({ "enabled": enabled })).await;
                        });
                    }
                    "home" => {
                        tauri::async_runtime::spawn(async move {
                            engine.call("return_home", json!({})).await;
                        });
                    }
                    "logs" => super::open_logs(app),
                    "updates" => {
                        show_window(app);
                        use tauri::Emitter;
                        let _ = app.emit_to("main", "glide:event", json!({ "name": "update.check", "data": {} }));
                    }
                    "quit" => {
                        shared.quitting.store(true, std::sync::atomic::Ordering::Release);
                        app.exit(0);
                    }
                    _ => {}
                }
            })
            .on_tray_icon_event(|tray, event| {
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } = event
                {
                    show_window(tray.app_handle());
                }
            })
            .build(app)?;
        app.manage(TrayItems { share });
        Ok(())
    }

    pub fn refresh(app: &AppHandle, state: &Value) {
        if let Some(items) = app.try_state::<TrayItems>() {
            let _ = items.share.set_checked(state["sharing_enabled"].as_bool().unwrap_or(false));
        }
        let connected = state["peers"]
            .as_array()
            .map(|p| p.iter().filter(|p| p["connection"] == "connected").count())
            .unwrap_or(0);
        if let Some(tray) = app.tray_by_id("glide") {
            let tip = if connected > 0 {
                format!("Glide — {connected} connected")
            } else {
                "Glide — no devices connected".to_string()
            };
            let _ = tray.set_tooltip(Some(tip));
        }
    }
}

fn open_logs(app: &AppHandle) {
    let shared = app.state::<Shared>();
    // On a Mac the window starts the engine and keeps its full output (with any error) in logs/engine.log. On Windows
    // the engine runs on its own and writes engine/logs itself.
    let window_logs = shared.data_root.join("logs");
    let engine_logs = shared.data_root.join("engine").join("logs");
    let dir = if window_logs.join("engine.log").is_file() || !engine_logs.is_dir() { window_logs } else { engine_logs };
    let _ = std::fs::create_dir_all(&dir);
    let _ = app.opener().open_path(dir.to_string_lossy(), None::<&str>);
}

// ------------------------------------------------------------------------------------------------ commands (window.glide.*)

#[tauri::command]
async fn glide_call(state: State<'_, Shared>, method: String, params: Value) -> Result<Value, ()> {
    let params = if params.is_object() { params } else { json!({}) };
    Ok(state.engine.call(&method, params).await)
}

#[tauri::command]
fn glide_meta(app: AppHandle, state: State<'_, Shared>) -> Value {
    json!({
        "platform": platform(),
        "mock": state.mock,
        "version": app.package_info().version.to_string(),
        "attached": state.engine.attached(),
    })
}

#[tauri::command]
fn glide_relaunch(app: AppHandle, state: State<'_, Shared>) {
    state.quitting.store(true, Ordering::Release);
    app.restart();
}

#[tauri::command]
async fn glide_quit_engine(app: AppHandle, state: State<'_, Shared>) -> Result<(), ()> {
    state.quitting.store(true, Ordering::Release);
    state.engine.shutdown_engine().await;
    app.exit(0);
    Ok(())
}

#[tauri::command]
async fn glide_start_engine(state: State<'_, Shared>) -> Result<(), ()> {
    state.engine.reset_restarts();
    state.engine.start().await;
    Ok(())
}

#[tauri::command]
fn glide_open_logs(app: AppHandle) {
    open_logs(&app);
}

#[tauri::command]
fn glide_open_display_settings(app: AppHandle) {
    let url = if cfg!(windows) {
        "ms-settings:display"
    } else {
        "x-apple.systempreferences:com.apple.Displays-Settings.extension"
    };
    let _ = app.opener().open_url(url, None::<&str>);
}

#[tauri::command]
fn glide_open_external(app: AppHandle, url: String) {
    if url.starts_with("https://") {
        let _ = app.opener().open_url(url, None::<&str>);
    }
}

/// macOS only asks for a permission when the app is not already listed. An entry left by an older build (different
/// signature) is listed but does not count, so macOS stays silent. This clears ONLY Glide's own entries for the
/// permissions that are still missing, using Apple's tccutil, so the next launch asks again.
#[tauri::command]
fn glide_reset_permissions(kinds: Vec<String>) -> bool {
    if !cfg!(target_os = "macos") || cfg!(debug_assertions) {
        return false;
    }
    let mut services: Vec<&str> = Vec::new();
    for kind in &kinds {
        match kind.as_str() {
            "accessibility" => services.extend(["Accessibility", "PostEvent"]),
            "input_monitoring" => services.push("ListenEvent"),
            _ => {}
        }
    }
    services.dedup();
    !services.is_empty()
        && services.iter().all(|service| {
            std::process::Command::new("/usr/bin/tccutil")
                .args(["reset", service, MAC_BUNDLE_ID])
                .status()
                .is_ok_and(|s| s.success())
        })
}

#[tauri::command]
async fn glide_check_update(app: AppHandle, state: State<'_, Shared>) -> Result<Value, String> {
    let update = app
        .updater()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|_| "Could not check for updates. Check your internet connection.".to_string())?;
    let answer = match &update {
        Some(u) => json!({ "available": true, "version": u.version, "notes": u.body }),
        None => json!({ "available": false, "version": app.package_info().version.to_string() }),
    };
    *lock(&state.update) = update;
    Ok(answer)
}

#[tauri::command]
async fn glide_install_update(app: AppHandle, state: State<'_, Shared>) -> Result<(), String> {
    let update = lock(&state.update).take().ok_or("No update is waiting.")?;
    let progress = app.clone();
    let mut received: u64 = 0;
    update
        .download_and_install(
            move |chunk, total| {
                received += chunk as u64;
                let _ = progress.emit_to(
                    "main",
                    "glide:event",
                    json!({ "name": "update.progress", "data": { "received": received, "total": total } }),
                );
            },
            || {},
        )
        .await
        .map_err(|_| "The update could not be installed. Try again later.".to_string())?;
    // Windows: the installer has replaced Glide and starts it again. macOS: the new app is in place; restart into it.
    state.quitting.store(true, Ordering::Release);
    app.restart();
}

/// Check now and then; tell the window when an update is ready to install.
fn schedule_update_checks(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(20)).await;
        loop {
            if let Ok(updater) = app.updater() {
                if let Ok(Some(update)) = updater.check().await {
                    let version = update.version.clone();
                    *lock(&app.state::<Shared>().update) = Some(update);
                    let _ = app.emit_to(
                        "main",
                        "glide:event",
                        json!({ "name": "update.available", "data": { "version": version } }),
                    );
                }
            }
            tokio::time::sleep(UPDATE_CHECK_EVERY).await;
        }
    });
}

// ------------------------------------------------------------------------------------------------ main

fn main() {
    let hidden = std::env::args().any(|a| a == "--hidden");
    let mock = std::env::var_os("GLIDE_MOCK").is_some();
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_window(app)))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            app.handle().plugin(tauri_plugin_autostart::init(
                tauri_plugin_autostart::MacosLauncher::LaunchAgent,
                Some(vec!["--hidden"]),
            ))?;

            let handle = app.handle().clone();
            let data_root = app.path().data_dir()?.join(if mock { "Glide-dev" } else { "Glide" });
            let log_dir = data_root.join("logs");
            let app_log = Arc::new(logs::RotatingLog::new(log_dir.clone(), "app"));
            let engine_log = Arc::new(logs::RotatingLog::new(log_dir, "engine"));

            let dev_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
            let attach = cfg!(windows) && !mock && std::env::var("GLIDE_ENGINE_MODE").as_deref() != Ok("stdio");
            let extra: Vec<String> = std::env::var("GLIDE_ENGINE_ARGS")
                .unwrap_or_default()
                .split_whitespace()
                .map(String::from)
                .collect();
            let command = if mock {
                Some((
                    PathBuf::from("node"),
                    vec![dev_root.join("desktop").join("dev").join("mock-engine.js").to_string_lossy().to_string()],
                ))
            } else {
                engine::resolve_engine(&dev_root).map(|p| (p, extra))
            };
            let packaged = !cfg!(debug_assertions);
            let options = EngineOptions {
                mode: if attach { Mode::Attach } else { Mode::Stdio },
                data_dir: data_root.join("engine"),
                command,
                ui_path: if packaged { std::env::current_exe().ok() } else { None },
                expected_version: packaged.then(|| app.package_info().version.to_string()),
            };
            let events_handle = handle.clone();
            let engine = Engine::new(
                options,
                Arc::new(move |name, data| on_engine_event(&events_handle, name, data)),
                Arc::new(move |line| engine_log.write(line)),
            );
            app.manage(Shared {
                engine: engine.clone(),
                mock,
                quitting: AtomicBool::new(false),
                data_root,
                app_log,
                last_state: Mutex::new(None),
                name_seeded: AtomicBool::new(false),
                permissions_asked: AtomicBool::new(false),
                login_item: Mutex::new(None),
                update: Mutex::new(None),
            });

            #[cfg(target_os = "macos")]
            tray::create(&handle)?;

            let started = handle.clone();
            tauri::async_runtime::spawn(async move {
                engine.start().await;
                // Launched at login on Windows by an older version: make sure the engine runs, then leave.
                if attach && hidden {
                    started.exit(0);
                }
            });
            if !hidden {
                create_window(&handle)?;
            }
            if packaged && !mock {
                schedule_update_checks(handle);
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            glide_call,
            glide_meta,
            glide_relaunch,
            glide_quit_engine,
            glide_start_engine,
            glide_open_logs,
            glide_open_display_settings,
            glide_open_external,
            glide_reset_permissions,
            glide_check_update,
            glide_install_update,
        ])
        .build(tauri::generate_context!())
        .expect("Glide could not start")
        .run(|app, event| match event {
            RunEvent::ExitRequested { api, code, .. } => {
                // macOS: closing the window keeps Glide running in the menu bar, like before.
                let shared = app.state::<Shared>();
                if cfg!(target_os = "macos") && code.is_none() && !shared.quitting.load(Ordering::Acquire) {
                    api.prevent_exit();
                }
            }
            RunEvent::Exit => {
                let shared = app.state::<Shared>();
                shared.app_log.write("exit");
                tauri::async_runtime::block_on(shared.engine.stop());
            }
            _ => {}
        });
}
