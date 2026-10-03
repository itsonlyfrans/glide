//! Windows-only security primitives for the daemon's local control pipe.

use std::ffi::c_void;
use std::fs::File;
use std::io::{self, Write};
use std::mem::size_of;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::net::windows::named_pipe::NamedPipeServer;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, SetLastError, ERROR_ALREADY_EXISTS,
    ERROR_SHARING_VIOLATION, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    STILL_ACTIVE,
};
use windows_sys::Win32::Security::{
    EqualSid, GetLengthSid, GetTokenInformation, SetKernelObjectSecurity, TokenUser,
    DACL_SECURITY_INFORMATION, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, DeleteFileW, MoveFileExW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_TEMPORARY, FILE_FLAG_WRITE_THROUGH, MOVEFILE_REPLACE_EXISTING,
    MOVEFILE_WRITE_THROUGH, OPEN_ALWAYS, READ_CONTROL, WRITE_DAC,
};
use windows_sys::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcess, GetExitCodeProcess, OpenProcess, OpenProcessToken,
    QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
};

const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
const PIPE_TYPE_BYTE: u32 = 0x0000_0000;
const PIPE_READMODE_BYTE: u32 = 0x0000_0000;
const PIPE_WAIT: u32 = 0x0000_0000;
const PIPE_REJECT_REMOTE_CLIENTS: u32 = 0x0000_0008;
// Four admitted clients, sixteen handshakes, listener and its replacement overlap.
const PIPE_MAX_INSTANCES: u32 =
    (crate::control::CLIENTS + crate::control::UNAUTHENTICATED + 2) as u32;
const PIPE_BUFFER_BYTES: u32 = 64 * 1024;
const SDDL_REVISION_1: u32 = 1;
const FILE_GENERIC_READ_WRITE: u32 = GENERIC_READ | GENERIC_WRITE | READ_CONTROL | WRITE_DAC;
const PROCESS_NAME_CAPACITY: usize = 32_768;

fn pipe_mode_flags() -> u32 {
    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS
}

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[link(name = "advapi32")]
unsafe extern "system" {
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        string_security_descriptor: *const u16,
        string_sd_revision: u32,
        security_descriptor: *mut *mut c_void,
        security_descriptor_size: *mut u32,
    ) -> i32;
    fn ConvertSidToStringSidW(sid: *mut c_void, string_sid: *mut *mut u16) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateNamedPipeW(
        name: *const u16,
        open_mode: u32,
        pipe_mode: u32,
        max_instances: u32,
        output_buffer_size: u32,
        input_buffer_size: u32,
        default_timeout: u32,
        security_attributes: *const SECURITY_ATTRIBUTES,
    ) -> HANDLE;
    fn GetNamedPipeClientProcessId(pipe: HANDLE, client_process_id: *mut u32) -> i32;
}

#[derive(Debug)]
struct Sid(Vec<u32>);

impl Sid {
    fn from_token(token: HANDLE) -> io::Result<Self> {
        let mut required = 0;
        unsafe {
            GetTokenInformation(token, TokenUser, null_mut(), 0, &mut required);
        }
        if required == 0 {
            return Err(io::Error::last_os_error());
        }

        let words = required.div_ceil(size_of::<usize>() as u32) as usize;
        let mut information = vec![0usize; words];
        let success = unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                information.as_mut_ptr().cast(),
                required,
                &mut required,
            )
        };
        if success == 0 {
            return Err(io::Error::last_os_error());
        }

        let token_user = unsafe { &*information.as_ptr().cast::<TOKEN_USER>() };
        if token_user.User.Sid.is_null() {
            return Err(io::Error::other("process token has no user SID"));
        }
        let length = unsafe { GetLengthSid(token_user.User.Sid) } as usize;
        if length == 0 {
            return Err(io::Error::other("process token has an invalid user SID"));
        }
        let words = length.div_ceil(size_of::<u32>());
        let mut sid = vec![0u32; words];
        unsafe {
            std::ptr::copy_nonoverlapping(
                token_user.User.Sid.cast::<u8>(),
                sid.as_mut_ptr().cast::<u8>(),
                length,
            );
        }
        Ok(Self(sid))
    }

    fn as_ptr(&self) -> *mut c_void {
        self.0.as_ptr().cast_mut().cast()
    }

    fn to_sddl_text(&self) -> io::Result<String> {
        let mut value = null_mut();
        let success = unsafe { ConvertSidToStringSidW(self.as_ptr(), &mut value) };
        if success == 0 || value.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut length = 0;
        unsafe {
            while *value.add(length) != 0 {
                length += 1;
            }
        }
        let result = String::from_utf16(unsafe { std::slice::from_raw_parts(value, length) })
            .map_err(|_| io::Error::other("Windows returned a malformed SID"));
        unsafe {
            LocalFree(value.cast());
        }
        result
    }
}

struct SecurityDescriptor(*mut c_void);

impl SecurityDescriptor {
    fn current_user_only() -> io::Result<Self> {
        let sid = current_user_sid()?;
        Self::for_sid(&sid)
    }

    fn for_sid(sid: &Sid) -> io::Result<Self> {
        let text = format!("D:P(A;;GA;;;{})", sid.to_sddl_text()?);
        let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
        let mut descriptor = null_mut();
        let success = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            )
        };
        if success == 0 || descriptor.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(descriptor))
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        }
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

fn current_user_sid() -> io::Result<Sid> {
    let mut token = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = OwnedHandle(token);
    Sid::from_token(token.0)
}

/// Creates an overlapped byte-stream pipe with a protected DACL.
///
/// `endpoint` must be the complete `\\.\pipe\...` name. Set `first_instance`
/// on the first server handle to reject a pre-created/squatted pipe name.
pub fn create_pipe(endpoint: &str, first_instance: bool) -> io::Result<NamedPipeServer> {
    if !endpoint.starts_with(r"\\.\pipe\")
        || endpoint.len() > 240
        || endpoint.chars().any(char::is_control)
        || endpoint.contains('\0')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid named pipe endpoint",
        ));
    }

    let descriptor = SecurityDescriptor::current_user_only()?;
    let attributes = descriptor.attributes();
    let endpoint: Vec<u16> = endpoint.encode_utf16().chain(Some(0)).collect();
    let open_mode = PIPE_ACCESS_DUPLEX
        | FILE_FLAG_OVERLAPPED
        | if first_instance {
            FILE_FLAG_FIRST_PIPE_INSTANCE
        } else {
            0
        };
    let pipe_mode = pipe_mode_flags();
    let handle = unsafe {
        CreateNamedPipeW(
            endpoint.as_ptr(),
            open_mode,
            pipe_mode,
            PIPE_MAX_INSTANCES,
            PIPE_BUFFER_BYTES,
            PIPE_BUFFER_BYTES,
            0,
            &attributes,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    unsafe { NamedPipeServer::from_raw_handle(handle as RawHandle) }
}

/// Rejects a connected pipe client unless its process token has this user's SID.
pub fn verify_peer(server: &NamedPipeServer, ui: Option<&Path>) -> io::Result<bool> {
    let mut pid = 0;
    if unsafe { GetNamedPipeClientProcessId(server.as_raw_handle() as HANDLE, &mut pid) } == 0
        || pid == 0
    {
        return Err(io::Error::last_os_error());
    }
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(io::Error::last_os_error());
    }
    let process = OwnedHandle(process);
    let mut token = null_mut();
    if unsafe { OpenProcessToken(process.0, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = OwnedHandle(token);
    let client_sid = Sid::from_token(token.0)?;
    let current_sid = current_user_sid()?;
    if unsafe { EqualSid(current_sid.as_ptr(), client_sid.as_ptr()) } == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "named pipe client is not the current user",
        ));
    }
    // The existing UI sends plain token auth. Identify its executable only after
    // verifying the SID; this affects notification suppression, never privileges.
    Ok(ui.is_some_and(|ui| {
        process_image(process.0)
            .ok()
            .and_then(|image| image.canonicalize().ok())
            .zip(ui.canonicalize().ok())
            .is_some_and(|(image, expected)| image == expected)
    }))
}

fn process_image(process: HANDLE) -> io::Result<PathBuf> {
    let mut image = vec![0u16; PROCESS_NAME_CAPACITY];
    let mut length = image.len() as u32;
    if unsafe { QueryFullProcessImageNameW(process, 0, image.as_mut_ptr(), &mut length) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(PathBuf::from(std::ffi::OsString::from_wide(
        &image[..length as usize],
    )))
}

/// Atomically replaces a file using a new file whose DACL grants only this user.
pub fn secure_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "file path has no parent directory",
        )
    })?;
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file path has no file name"))?;
    let descriptor = SecurityDescriptor::current_user_only()?;
    let attributes = descriptor.attributes();
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(
        ".{}.{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id(),
        sequence
    ));
    let wide_temp = wide_path(&temp);
    let handle = unsafe {
        CreateFileW(
            wide_temp.as_ptr(),
            GENERIC_WRITE,
            0,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_TEMPORARY | FILE_FLAG_WRITE_THROUGH,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }

    let mut file = unsafe { File::from_raw_handle(handle as RawHandle) };
    let write_result = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = write_result {
        unsafe {
            DeleteFileW(wide_temp.as_ptr());
        }
        return Err(error);
    }

    let wide_target = wide_path(path);
    if unsafe {
        MoveFileExW(
            wide_temp.as_ptr(),
            wide_target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        let error = io::Error::last_os_error();
        unsafe {
            DeleteFileW(wide_temp.as_ptr());
        }
        return Err(error);
    }
    Ok(())
}

/// Holds a per-session named mutex and an exclusive, user-only lock file.
pub struct InstanceLock {
    _mutex: OwnedHandle,
    _lock_file: File,
}

impl InstanceLock {
    /// Acquires the instance lock for this random 16-hex data-directory id.
    pub fn acquire(data_dir: &Path, id: &str) -> io::Result<Self> {
        if id.len() != 16 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "instance id must be 16 hexadecimal characters",
            ));
        }
        let sid = current_user_sid()?;
        let descriptor = SecurityDescriptor::for_sid(&sid)?;
        let attributes = descriptor.attributes();
        let mutex_name: Vec<u16> = format!(r"Local\GlideEngine-{id}-{}", sid.to_sddl_text()?)
            .encode_utf16()
            .chain(Some(0))
            .collect();
        unsafe {
            SetLastError(0);
        }
        let handle = unsafe { CreateMutexW(&attributes, 0, mutex_name.as_ptr()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe {
                CloseHandle(handle);
            }
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Glide is already running",
            ));
        }
        let mutex = OwnedHandle(handle);

        let lock_path = data_dir.join("engine.lock");
        let lock_handle = create_lock_file(&lock_path, &attributes)?;
        if unsafe { SetKernelObjectSecurity(lock_handle, DACL_SECURITY_INFORMATION, descriptor.0) }
            == 0
        {
            let error = io::Error::last_os_error();
            unsafe {
                CloseHandle(lock_handle);
            }
            return Err(error);
        }
        let mut lock_file = unsafe { File::from_raw_handle(lock_handle as RawHandle) };
        lock_file.set_len(0)?;
        writeln!(lock_file, "{}", std::process::id())?;
        lock_file.sync_all()?;
        Ok(Self {
            _mutex: mutex,
            _lock_file: lock_file,
        })
    }
}

fn create_lock_file(path: &Path, attributes: &SECURITY_ATTRIBUTES) -> io::Result<HANDLE> {
    let wide = wide_path(path);
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_GENERIC_READ_WRITE,
            0,
            attributes,
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Glide is already running",
            ));
        }
        return Err(error);
    }
    Ok(handle)
}

/// Returns true only for a live same-user process whose image is `glided.exe`.
pub fn process_is_engine(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return false;
    }
    let process = OwnedHandle(process);
    let mut exit_code = 0;
    if unsafe { GetExitCodeProcess(process.0, &mut exit_code) } == 0
        || exit_code != STILL_ACTIVE as u32
    {
        return false;
    }
    let Ok(image) = process_image(process.0) else {
        return false;
    };
    if !image
        .file_name()
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("glided.exe"))
    {
        return false;
    }
    let mut token = null_mut();
    if unsafe { OpenProcessToken(process.0, TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let token = OwnedHandle(token);
    let Ok(other_sid) = Sid::from_token(token.0) else {
        return false;
    };
    let Ok(current_sid) = current_user_sid() else {
        return false;
    };
    unsafe { EqualSid(current_sid.as_ptr(), other_sid.as_ptr()) != 0 }
}

fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::io::AsRawHandle;
    use tokio::net::windows::named_pipe::ClientOptions;
    use windows_sys::Win32::Security::{GetKernelObjectSecurity, ACCESS_ALLOWED_ACE};

    static TEST_PIPE_ID: AtomicU64 = AtomicU64::new(0);

    fn endpoint() -> String {
        format!(
            r"\\.\pipe\glide-control-test-{}-{}",
            std::process::id(),
            TEST_PIPE_ID.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn read_object_dacl(handle: HANDLE) -> io::Result<Vec<usize>> {
        let mut required = 0;
        unsafe {
            GetKernelObjectSecurity(
                handle,
                DACL_SECURITY_INFORMATION,
                null_mut(),
                0,
                &mut required,
            );
        }
        if required == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut descriptor = vec![0usize; required.div_ceil(size_of::<usize>() as u32) as usize];
        let success = unsafe {
            GetKernelObjectSecurity(
                handle,
                DACL_SECURITY_INFORMATION,
                descriptor.as_mut_ptr().cast(),
                required,
                &mut required,
            )
        };
        if success == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(descriptor)
    }

    fn assert_current_user_only_dacl(handle: HANDLE) {
        let current_sid = current_user_sid().unwrap();
        let descriptor = read_object_dacl(handle).unwrap();
        let mut dacl_present = 0;
        let mut dacl_defaulted = 0;
        let mut dacl = null_mut();
        assert_ne!(
            unsafe {
                windows_sys::Win32::Security::GetSecurityDescriptorDacl(
                    descriptor.as_ptr().cast_mut().cast(),
                    &mut dacl_present,
                    &mut dacl,
                    &mut dacl_defaulted,
                )
            },
            0
        );
        assert_ne!(dacl_present, 0);
        assert!(!dacl.is_null());
        let acl = unsafe { &*dacl };
        assert_eq!(acl.AceCount, 1, "DACL must have exactly one allow ACE");
        let mut ace = null_mut();
        assert_ne!(
            unsafe { windows_sys::Win32::Security::GetAce(dacl, 0, &mut ace) },
            0
        );
        let ace = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
        assert_eq!(ace.Header.AceType, 0);
        assert_eq!(
            ace.Mask,
            windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS
        );
        assert_ne!(
            unsafe {
                EqualSid(
                    current_sid.as_ptr(),
                    std::ptr::addr_of!(ace.SidStart).cast_mut().cast(),
                )
            },
            0
        );
    }

    #[tokio::test]
    async fn pipe_security_is_only_current_user_and_remote_rejection_is_enabled() {
        let pipe = create_pipe(&endpoint(), true).unwrap();
        assert_current_user_only_dacl(pipe.as_raw_handle() as HANDLE);
        assert_ne!(
            pipe_mode_flags() & PIPE_REJECT_REMOTE_CLIENTS,
            0,
            "CreateNamedPipeW must receive PIPE_REJECT_REMOTE_CLIENTS"
        );
    }

    #[tokio::test]
    async fn first_instance_rejects_a_preexisting_pipe_name() {
        let name = endpoint();
        let _squatter = create_pipe(&name, false).unwrap();
        assert!(create_pipe(&name, true).is_err());
    }

    #[tokio::test]
    async fn connected_current_user_pipe_client_is_verified() {
        let name = endpoint();
        let server = create_pipe(&name, true).unwrap();
        let client = ClientOptions::new().open(&name).unwrap();
        server.connect().await.unwrap();
        assert!(!verify_peer(&server, None).unwrap());
        let executable = std::env::current_exe().unwrap();
        assert!(verify_peer(&server, Some(&executable)).unwrap());
        assert!(!verify_peer(&server, executable.parent()).unwrap());
        drop(client);
    }

    #[test]
    fn instance_lock_excludes_second_engine_and_reopens_after_drop() {
        let directory = tempfile::tempdir().unwrap();
        let id = "0123456789abcdef";
        let first = InstanceLock::acquire(directory.path(), id).unwrap();
        assert_eq!(
            InstanceLock::acquire(directory.path(), id)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        drop(first);
        let _again = InstanceLock::acquire(directory.path(), id).unwrap();
    }

    #[test]
    fn secure_write_replaces_atomically_with_expected_contents() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ipc.json");
        secure_write(&path, br#"{"pid":1}"#).unwrap();
        secure_write(&path, br#"{"pid":2}"#).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), br#"{"pid":2}"#);
        let file = File::open(&path).unwrap();
        assert_current_user_only_dacl(file.as_raw_handle() as HANDLE);
    }
}
