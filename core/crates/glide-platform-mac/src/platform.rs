use crate::platform_logic::{
    data_directory, gui_domain, launch_agent_path, launch_agent_plist, permission_settings_url,
    LAUNCH_AGENT_LABEL,
};
use glide_platform::{BackendError, ClipboardBackend, InputBackend, Os, PermissionKind, Platform};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::raw::c_uint;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

// SAFETY: This matches Darwin libc geteuid ABI and has no pointer or ownership contract.
unsafe extern "C" {
    fn geteuid() -> c_uint;
}

const LAUNCH_AGENT_LIMIT: u64 = 64 * 1024;
// macOS O_NOFOLLOW, used so opening an existing LaunchAgent never follows a symlink.
const O_NOFOLLOW: i32 = 0x0100;
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Native macOS platform integration.
pub struct MacPlatform {
    input: crate::input::MacInput,
    clipboard: crate::clipboard::MacClipboard,
}

impl MacPlatform {
    /// Initializes the native input and clipboard backends.
    pub fn new() -> Result<Self, BackendError> {
        keep_responsive();
        Ok(Self {
            input: crate::input::MacInput::new()?,
            clipboard: crate::clipboard::MacClipboard::new()?,
        })
    }

    /// Exposes macOS-only input status and permission helpers beyond the shared trait.
    pub fn native_input(&self) -> &crate::input::MacInput {
        &self.input
    }
}

impl Platform for MacPlatform {
    fn os(&self) -> Os {
        Os::Macos
    }

    fn input_backend(&self) -> &dyn InputBackend {
        &self.input
    }

    fn clipboard_backend(&self) -> &dyn ClipboardBackend {
        &self.clipboard
    }

    fn data_dir(&self) -> Result<PathBuf, BackendError> {
        let home = home_directory()?;
        let path = data_directory(&home);
        ensure_directory_tree(&path, Some(0o700))?;
        let canonical = fs::canonicalize(&path).map_err(map_io_error)?;
        if !canonical.starts_with(&home) {
            return Err(BackendError::Unavailable);
        }
        Ok(canonical)
    }

    fn autostart_enabled(&self) -> Result<bool, BackendError> {
        let home = home_directory()?;
        let path = launch_agent_path(&home);
        match read_launch_agent(&path)? {
            Some(contents) if is_glide_agent(&contents) => Ok(true),
            Some(_) => Err(BackendError::Failed(
                "LaunchAgent path is occupied by another configuration".into(),
            )),
            None => Ok(false),
        }
    }

    fn set_autostart_enabled(&self, enabled: bool) -> Result<(), BackendError> {
        let home = home_directory()?;
        let path = launch_agent_path(&home);
        let domain = current_gui_domain()?;
        let target = format!("{domain}/{LAUNCH_AGENT_LABEL}");
        if enabled {
            let data_dir = data_directory(&home);
            enable_autostart(&path, &domain, &data_dir, None)
        } else {
            disable_autostart(&path, &domain, &target)
        }
    }

    fn open_permission_settings(&self, kind: PermissionKind) -> Result<(), BackendError> {
        let status = Command::new("/usr/bin/open")
            .arg(permission_settings_url(kind))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|_| BackendError::Unavailable)?;
        if status.success() {
            Ok(())
        } else {
            Err(BackendError::Unavailable)
        }
    }
}

/// Reconcile the user LaunchAgent with the daemon's current executable and paths.
pub fn repair_autostart(
    enabled: bool,
    data_dir: &Path,
    ui: Option<&Path>,
) -> Result<(), BackendError> {
    let home = home_directory()?;
    let path = launch_agent_path(&home);
    let domain = current_gui_domain()?;
    let target = format!("{domain}/{LAUNCH_AGENT_LABEL}");
    if enabled {
        enable_autostart(&path, &domain, data_dir, ui)
    } else {
        disable_autostart(&path, &domain, &target)
    }
}

fn home_directory() -> Result<PathBuf, BackendError> {
    let home = env::var_os("HOME").ok_or(BackendError::Unavailable)?;
    let home = PathBuf::from(home);
    if !home.is_absolute() {
        return Err(BackendError::Unavailable);
    }
    let metadata = fs::symlink_metadata(&home).map_err(map_io_error)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(BackendError::Unavailable);
    }
    let canonical = fs::canonicalize(&home).map_err(map_io_error)?;
    if canonical != home || !canonical.is_absolute() || !canonical.is_dir() {
        return Err(BackendError::Unavailable);
    }
    if metadata.uid() != current_uid() {
        return Err(BackendError::Unavailable);
    }
    Ok(canonical)
}

fn ensure_directory_tree(path: &Path, final_mode: Option<u32>) -> Result<(), BackendError> {
    if !path.is_absolute() {
        return Err(BackendError::Unavailable);
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir => current.push(component.as_os_str()),
            Component::Normal(part) => {
                current.push(part);
                match fs::symlink_metadata(&current) {
                    Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                        return Err(BackendError::Unavailable)
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        match fs::create_dir(&current) {
                            Ok(()) => {}
                            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                            Err(error) => return Err(map_io_error(error)),
                        }
                        let metadata = fs::symlink_metadata(&current).map_err(map_io_error)?;
                        if metadata.file_type().is_symlink() || !metadata.is_dir() {
                            return Err(BackendError::Unavailable);
                        }
                    }
                    Err(error) => return Err(map_io_error(error)),
                }
            }
            Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) => return Err(BackendError::Unavailable),
        }
    }
    if let Some(mode) = final_mode {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(map_io_error)?;
    }
    Ok(())
}

fn current_gui_domain() -> Result<String, BackendError> {
    gui_domain(&current_uid().to_string()).ok_or(BackendError::Unavailable)
}

fn current_uid() -> u32 {
    // SAFETY: geteuid has no arguments and returns the calling process's effective UID.
    unsafe { geteuid() }
}

fn service_is_loaded(domain: &str) -> bool {
    Command::new("/bin/launchctl")
        .arg("print")
        .arg(format!("{domain}/{LAUNCH_AGENT_LABEL}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn launchctl(action: &str, domain: &str, target: Option<&Path>) -> Result<(), BackendError> {
    let mut command = Command::new("/bin/launchctl");
    command.arg(action);
    match (action, target) {
        ("bootstrap", Some(target)) => {
            command.arg(domain).arg(target);
        }
        ("enable" | "disable", Some(target)) => {
            command.arg(target);
        }
        _ => return Err(BackendError::Unavailable),
    }
    let status = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|_| BackendError::Unavailable)?;
    if status.success() {
        Ok(())
    } else {
        Err(BackendError::Unavailable)
    }
}

fn enable_autostart(
    path: &Path,
    domain: &str,
    data_dir: &Path,
    ui: Option<&Path>,
) -> Result<(), BackendError> {
    let directory = path.parent().ok_or(BackendError::Unavailable)?;
    ensure_directory_tree(directory, None)?;
    let prior = read_launch_agent(path)?;
    if prior
        .as_deref()
        .is_some_and(|contents| !is_glide_agent(contents))
    {
        return Err(BackendError::Failed(
            "LaunchAgent path is occupied by another configuration".into(),
        ));
    }
    let executable = env::current_exe().map_err(map_io_error)?;
    let executable = fs::canonicalize(executable).map_err(map_io_error)?;
    if !data_dir.is_absolute()
        || data_dir
            .to_str()
            .is_none_or(|value| value.chars().any(char::is_control))
    {
        return Err(BackendError::InvalidInput(
            "data directory must be an absolute path without control characters".into(),
        ));
    }
    if let Some(ui) = ui {
        if !ui.is_absolute()
            || ui
                .to_str()
                .is_none_or(|value| value.chars().any(char::is_control))
            || !ui.is_file()
        {
            return Err(BackendError::InvalidInput(
                "UI path must be an absolute existing file without control characters".into(),
            ));
        }
    }
    ensure_directory_tree(data_dir, Some(0o700))?;
    let metadata = fs::symlink_metadata(data_dir).map_err(map_io_error)?;
    if metadata.uid() != current_uid() {
        return Err(BackendError::PermissionDenied);
    }
    let content = launch_agent_plist(&executable, data_dir, ui)?;
    let content = content.as_bytes();
    if prior.as_deref() != Some(content) {
        write_launch_agent_atomic(path, content)?;
    }
    let target = format!("{domain}/{LAUNCH_AGENT_LABEL}");
    if let Err(error) = launchctl("enable", domain, Some(Path::new(&target))) {
        let rollback = match prior {
            Some(previous) => write_launch_agent_atomic(path, &previous),
            None => remove_launch_agent(path),
        };
        return if rollback.is_err() {
            Err(BackendError::Failed(
                "LaunchAgent enable failed and rollback was incomplete".into(),
            ))
        } else {
            Err(error)
        };
    }
    // launchd has no safe in-place argument reload: leave the active engine untouched.
    if service_is_loaded(domain) {
        return Ok(());
    }
    match launchctl("bootstrap", domain, Some(path)) {
        Ok(()) => Ok(()),
        Err(error) => {
            let rollback = match prior {
                Some(previous) => write_launch_agent_atomic(path, &previous),
                None => remove_launch_agent(path),
            };
            if rollback.is_err() {
                Err(BackendError::Failed(
                    "LaunchAgent registration failed and rollback was incomplete".into(),
                ))
            } else {
                Err(error)
            }
        }
    }
}

fn disable_autostart(path: &Path, domain: &str, target: &str) -> Result<(), BackendError> {
    let contents = read_launch_agent(path)?;
    if contents
        .as_deref()
        .is_some_and(|contents| !is_glide_agent(contents))
    {
        return Err(BackendError::Failed(
            "LaunchAgent path is occupied by another configuration".into(),
        ));
    }
    if contents.is_some() || service_is_loaded(domain) {
        // Persistently prevent the next bootstrap without stopping this live engine.
        launchctl("disable", domain, Some(Path::new(target)))?;
    }
    if contents.is_some() {
        remove_launch_agent(path)?;
    }
    Ok(())
}

fn read_launch_agent(path: &Path) -> Result<Option<Vec<u8>>, BackendError> {
    let directory = path.parent().ok_or(BackendError::Unavailable)?;
    if !validate_existing_directory_tree(directory)? {
        return Ok(None);
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(map_io_error(error)),
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > LAUNCH_AGENT_LIMIT
    {
        return Err(BackendError::Failed(
            "LaunchAgent file has an unsafe type or size".into(),
        ));
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW)
        .open(path)
        .map_err(map_io_error)?;
    let opened_metadata = file.metadata().map_err(map_io_error)?;
    if !opened_metadata.is_file() || opened_metadata.len() > LAUNCH_AGENT_LIMIT {
        return Err(BackendError::Failed(
            "LaunchAgent file has an unsafe type or size".into(),
        ));
    }
    let mut contents = Vec::with_capacity(opened_metadata.len() as usize);
    file.take(LAUNCH_AGENT_LIMIT + 1)
        .read_to_end(&mut contents)
        .map_err(map_io_error)?;
    if contents.len() as u64 > LAUNCH_AGENT_LIMIT {
        return Err(BackendError::Failed(
            "LaunchAgent file has an unsafe type or size".into(),
        ));
    }
    Ok(Some(contents))
}

fn is_glide_agent(contents: &[u8]) -> bool {
    std::str::from_utf8(contents).is_ok_and(|plist| {
        plist.contains(&format!(
            "<key>Label</key><string>{LAUNCH_AGENT_LABEL}</string>"
        ))
    })
}

fn write_launch_agent_atomic(path: &Path, contents: &[u8]) -> Result<(), BackendError> {
    let directory = path.parent().ok_or(BackendError::Unavailable)?;
    let (temporary_path, mut temporary) = create_temporary_file(directory)?;
    let _cleanup = TemporaryPath(temporary_path.clone());
    let write_result = temporary
        .write_all(contents)
        .and_then(|()| temporary.sync_all());
    drop(temporary);
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary_path);
        return Err(map_io_error(error));
    }

    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(BackendError::Failed(
                "LaunchAgent destination is not a regular file".into(),
            ))
        }
        Ok(_) => fs::rename(&temporary_path, path).map_err(map_io_error)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::hard_link(&temporary_path, path).map_err(map_io_error)?;
            fs::remove_file(&temporary_path).map_err(map_io_error)?;
        }
        Err(error) => return Err(map_io_error(error)),
    }
    sync_directory(directory)
}

fn create_temporary_file(directory: &Path) -> Result<(PathBuf, File), BackendError> {
    for _ in 0..32 {
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(
            ".{LAUNCH_AGENT_LABEL}.{}.{}.tmp",
            std::process::id(),
            sequence
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(O_NOFOLLOW)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(map_io_error(error)),
        }
    }
    Err(BackendError::Unavailable)
}

struct TemporaryPath(PathBuf);

impl Drop for TemporaryPath {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn validate_existing_directory_tree(path: &Path) -> Result<bool, BackendError> {
    if !path.is_absolute() {
        return Err(BackendError::Unavailable);
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir => current.push(component.as_os_str()),
            Component::Normal(part) => {
                current.push(part);
                let metadata = match fs::symlink_metadata(&current) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                    Err(error) => return Err(map_io_error(error)),
                };
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(BackendError::Unavailable);
                }
            }
            Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) => return Err(BackendError::Unavailable),
        }
    }
    Ok(true)
}

fn remove_launch_agent(path: &Path) -> Result<(), BackendError> {
    match fs::remove_file(path) {
        Ok(()) => {
            if let Some(directory) = path.parent() {
                sync_directory(directory)?;
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(map_io_error(error)),
    }
}

fn sync_directory(directory: &Path) -> Result<(), BackendError> {
    File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(map_io_error)
}

fn map_io_error(error: std::io::Error) -> BackendError {
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        BackendError::PermissionDenied
    } else {
        BackendError::Unavailable
    }
}

/// Glide runs in the background, so macOS treats it as idle: App Nap and timer coalescing then delay its wake-ups by
/// 10 ms and more, which made remote cursor moves wait before they were applied. Declare latency-critical, user-initiated
/// work for the life of the process (the computer may still go to sleep).
fn keep_responsive() {
    use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let activity = NSProcessInfo::processInfo().beginActivityWithOptions_reason(
            NSActivityOptions::UserInitiatedAllowingIdleSystemSleep
                | NSActivityOptions::LatencyCritical,
            &NSString::from_str("Sharing the keyboard and mouse"),
        );
        // Ending the activity would let macOS throttle Glide again; keep it until the process exits.
        std::mem::forget(activity);
    });
}
