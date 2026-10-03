//! Unix filesystem and peer-identity primitives for the local control socket.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::raw::c_uint;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const IPC_DIR_MODE: u32 = 0o700;
const SOCKET_MODE: u32 = 0o600;
const FILE_MODE: u32 = 0o600;
const LOCK_EX: i32 = 2;
const LOCK_NB: i32 = 4;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[cfg(target_os = "macos")]
const O_NOFOLLOW: i32 = 0x0100;
#[cfg(target_os = "linux")]
const O_NOFOLLOW: i32 = 0x20_000;
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
const O_NOFOLLOW: i32 = 0;

unsafe extern "C" {
    fn geteuid() -> c_uint;
    fn flock(fd: i32, operation: i32) -> i32;
    fn kill(pid: i32, signal: i32) -> i32;
}

/// The current process's effective user ID.
pub fn current_uid() -> u32 {
    // SAFETY: geteuid takes no arguments and returns the calling process's effective UID.
    unsafe { geteuid() }
}

/// Return the stable socket pathname after creating its owner-only directory.
pub fn prepare_socket(data_dir: &Path) -> io::Result<PathBuf> {
    ensure_owned_dir(data_dir, Some(IPC_DIR_MODE))?;
    let ipc_dir = data_dir.join("ipc");
    ensure_owned_dir(&ipc_dir, Some(IPC_DIR_MODE))?;
    Ok(ipc_dir.join("ctl.sock"))
}

/// Remove only a stale socket owned by this user. Call after taking the engine lock.
pub fn remove_stale_socket(path: &Path) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_socket() || metadata.uid() != current_uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "control socket path is occupied by an unsafe file",
        ));
    }
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "control socket is already accepting connections",
            ));
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            ) => {}
        Err(error) => return Err(error),
    }
    fs::remove_file(path)
}

/// Restrict a newly bound Unix socket to the current user and verify it stayed a socket.
pub fn secure_socket(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_socket() || metadata.uid() != current_uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "bound control socket has an unexpected owner or type",
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(SOCKET_MODE))
}

/// Atomically write owner-only metadata at `path`, replacing only a regular same-user file.
pub fn secure_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "metadata path has no parent")
    })?;
    ensure_owned_dir(parent, Some(IPC_DIR_MODE))?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() || metadata.uid() != current_uid() => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "metadata path is occupied by an unsafe file",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    for _ in 0..32 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary_path = parent.join(format!(
            ".{}.{}.{}.tmp",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("ipc"),
            std::process::id(),
            sequence
        ));
        let mut temporary = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(FILE_MODE)
            .custom_flags(O_NOFOLLOW)
            .open(&temporary_path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        let mut cleanup = TemporaryPath(Some(temporary_path.clone()));
        temporary.write_all(bytes)?;
        temporary.set_permissions(fs::Permissions::from_mode(FILE_MODE))?;
        temporary.sync_all()?;
        drop(temporary);

        match fs::symlink_metadata(path) {
            Ok(metadata) if !metadata.is_file() || metadata.uid() != current_uid() => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "metadata destination changed to an unsafe file",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        fs::rename(&temporary_path, path)?;
        cleanup.0 = None;
        File::open(parent)?.sync_all()?;
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique metadata temporary file",
    ))
}

struct TemporaryPath(Option<PathBuf>);

impl Drop for TemporaryPath {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_file(path);
        }
    }
}

/// Verify the connected process has the same UID as this engine.
pub fn verify_peer(stream: &tokio::net::UnixStream) -> io::Result<()> {
    if peer_uid(stream)? == current_uid() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "control client belongs to another user",
        ))
    }
}

pub fn peer_uid(stream: &tokio::net::UnixStream) -> io::Result<u32> {
    peer_uid_fd(stream.as_raw_fd())
}

#[cfg(target_os = "macos")]
fn peer_uid_fd(fd: i32) -> io::Result<u32> {
    let mut uid = 0;
    let mut gid = 0;
    // SAFETY: both output pointers are valid and getpeereid does not retain them.
    if unsafe { getpeereid(fd, &mut uid, &mut gid) } == 0 {
        Ok(uid)
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn getpeereid(fd: i32, uid: *mut u32, gid: *mut u32) -> i32;
}

#[cfg(target_os = "linux")]
fn peer_uid_fd(fd: i32) -> io::Result<u32> {
    #[repr(C)]
    struct Ucred {
        pid: i32,
        uid: u32,
        gid: u32,
    }
    unsafe extern "C" {
        fn getsockopt(
            socket: i32,
            level: i32,
            option: i32,
            value: *mut std::ffi::c_void,
            length: *mut u32,
        ) -> i32;
    }
    const SOL_SOCKET: i32 = 1;
    const SO_PEERCRED: i32 = 17;
    let mut credentials = Ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<Ucred>() as u32;
    // SAFETY: `credentials` and `length` are valid output storage for SO_PEERCRED.
    if unsafe {
        getsockopt(
            fd,
            SOL_SOCKET,
            SO_PEERCRED,
            (&mut credentials as *mut Ucred).cast(),
            &mut length,
        )
    } == 0
        && length as usize == std::mem::size_of::<Ucred>()
    {
        Ok(credentials.uid)
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn peer_uid_fd(_fd: i32) -> io::Result<u32> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "peer credential checks are unavailable on this Unix target",
    ))
}

/// Exclusive nonblocking per-data-directory process lock.
#[derive(Debug)]
pub struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    pub fn acquire(data_dir: &Path, id: &str) -> io::Result<Self> {
        if id.len() != 16 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "data directory id must be 16 hexadecimal characters",
            ));
        }
        ensure_owned_dir(data_dir, Some(IPC_DIR_MODE))?;
        let path = data_dir.join("engine.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(FILE_MODE)
            .custom_flags(O_NOFOLLOW)
            .open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.uid() != current_uid() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "engine lock is not a regular file owned by this user",
            ));
        }
        file.set_permissions(fs::Permissions::from_mode(FILE_MODE))?;
        // SAFETY: flock acts on this live descriptor; File owns it until InstanceLock drops.
        if unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        use std::io::Seek;
        let mut file = file;
        file.set_len(0)?;
        file.rewind()?;
        writeln!(file, "{}\n{id}", std::process::id())?;
        file.sync_all()?;
        Ok(Self { _file: file })
    }
}

/// Confirm the PID still names the Glide daemon image.
pub fn process_is_engine(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // SAFETY: kill(pid, 0) checks liveness/permissions without sending a signal.
    if unsafe { kill(pid as i32, 0) } != 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        fs::read_link(format!("/proc/{pid}/exe"))
            .ok()
            .and_then(|path| {
                path.file_name()
                    .map(|name| name == "glided" || name == "glided.exe")
            })
            .unwrap_or(false)
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let mut buffer = [0i8; 4096];
        // SAFETY: the buffer is writable for the supplied size and proc_pidpath writes at most it.
        if unsafe { proc_pidpath(pid as i32, buffer.as_mut_ptr().cast(), buffer.len() as u32) } <= 0
        {
            return false;
        }
        let bytes = buffer
            .iter()
            .take_while(|byte| **byte != 0)
            .map(|byte| *byte as u8)
            .collect::<Vec<_>>();
        Path::new(std::ffi::OsStr::from_bytes(&bytes))
            .file_name()
            .is_some_and(|name| name == "glided" || name == "glided.exe")
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        false
    }
}

#[cfg(target_os = "macos")]
#[link(name = "proc")]
unsafe extern "C" {
    fn proc_pidpath(pid: i32, buffer: *mut std::ffi::c_void, buffersize: u32) -> i32;
}

fn ensure_owned_dir(path: &Path, mode: Option<u32>) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "control path must be absolute",
        ));
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "control directory is a symlink or not a directory",
            ));
        }
        Ok(metadata) if metadata.uid() != current_uid() => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "control directory belongs to another user",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound && mode.is_some() => {
            match fs::create_dir(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(error) => return Err(error),
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() || metadata.uid() != current_uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "control directory has an unsafe owner or type",
        ));
    }
    if let Some(mode) = mode {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn metadata_and_socket_directory_are_owner_only() {
        let data_dir = tempfile::tempdir().unwrap();
        let socket = prepare_socket(data_dir.path()).unwrap();
        assert_eq!(
            fs::metadata(socket.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            IPC_DIR_MODE
        );
        secure_write(&data_dir.path().join("ipc.json"), b"metadata").unwrap();
        assert_eq!(
            fs::metadata(data_dir.path().join("ipc.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            FILE_MODE
        );
    }

    #[test]
    fn engine_lock_is_exclusive_and_released_on_drop() {
        let data_dir = tempfile::tempdir().unwrap();
        let id = "0123456789abcdef";
        let lock = InstanceLock::acquire(data_dir.path(), id).unwrap();
        assert_eq!(
            InstanceLock::acquire(data_dir.path(), id)
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        drop(lock);
        assert!(InstanceLock::acquire(data_dir.path(), id).is_ok());
    }
}
