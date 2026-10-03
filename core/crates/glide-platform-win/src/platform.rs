//! Windows host integration. The engine owns launch-at-login; it stays off until asked.

use crate::{clipboard::WindowsClipboard, input::WindowsInput, native::enable_per_monitor_v2};
use glide_platform::{BackendError, ClipboardBackend, InputBackend, Os, PermissionKind, Platform};
use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
use std::ptr;
use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
};

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE: &str = "Glide";
const MAX_REGISTRY_STRING_BYTES: u32 = 16 * 1024;

/// Native Windows services used by the daemon.
pub struct WindowsPlatform {
    input: WindowsInput,
    clipboard: WindowsClipboard,
}

impl WindowsPlatform {
    /// Initializes the native backends without enabling launch-at-login.
    pub fn new() -> Result<Self, BackendError> {
        enable_per_monitor_v2()?;
        Ok(Self {
            input: WindowsInput::new()?,
            clipboard: WindowsClipboard::new()?,
        })
    }
}

impl Platform for WindowsPlatform {
    fn os(&self) -> Os {
        Os::Windows
    }

    fn input_backend(&self) -> &dyn InputBackend {
        &self.input
    }

    fn clipboard_backend(&self) -> &dyn ClipboardBackend {
        &self.clipboard
    }

    fn data_dir(&self) -> Result<PathBuf, BackendError> {
        let mut path = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or(BackendError::Unavailable)?;
        path.push("Glide");
        Ok(path)
    }

    fn autostart_enabled(&self) -> Result<bool, BackendError> {
        let Some(actual) = read_run_value(RUN_VALUE)? else {
            return Ok(false);
        };
        let expected = executable_run_value()?;
        let executable = &expected[..expected.len() - 1];
        Ok(actual == executable
            || actual
                .strip_prefix(executable)
                .is_some_and(|arguments| arguments.first() == Some(&(b' ' as u16))))
    }

    fn set_autostart_enabled(&self, enabled: bool) -> Result<(), BackendError> {
        if enabled {
            let command = current_executable_command()?;
            set_autostart_command(RUN_VALUE, Some(command.as_os_str()))
        } else {
            set_autostart_command(RUN_VALUE, None)
        }
    }

    fn open_permission_settings(&self, _kind: PermissionKind) -> Result<(), BackendError> {
        Ok(())
    }
}

fn open_run_key(access: u32) -> Result<RegistryKey, u32> {
    let path = wide_nul(RUN_KEY);
    let mut key = ptr::null_mut();
    // SAFETY: the path is nul-terminated and `key` is a writable output pointer.
    let result = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, access, &mut key) };
    if result == ERROR_SUCCESS && !key.is_null() {
        Ok(RegistryKey(key))
    } else {
        Err(result)
    }
}

fn create_run_key(access: u32) -> Result<RegistryKey, BackendError> {
    let path = wide_nul(RUN_KEY);
    let mut key = ptr::null_mut();
    // SAFETY: the path is nul-terminated and `key` is a writable output pointer. No custom
    // security descriptor or inherited handle is supplied.
    let result = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            path.as_ptr(),
            0,
            ptr::null(),
            REG_OPTION_NON_VOLATILE,
            access,
            ptr::null(),
            &mut key,
            ptr::null_mut(),
        )
    };
    if result == ERROR_SUCCESS && !key.is_null() {
        Ok(RegistryKey(key))
    } else {
        Err(registry_error("create startup key", result))
    }
}

fn wide_nul(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(Some(0)).collect()
}

/// Write or remove a per-user Run value. The caller supplies a complete Windows command line.
pub fn set_autostart_command(
    value_name: &str,
    command_line: Option<&OsStr>,
) -> Result<(), BackendError> {
    if value_name.is_empty()
        || value_name.chars().any(|character| character.is_control())
        || value_name.encode_utf16().count() > 256
    {
        return Err(BackendError::InvalidInput(
            "invalid startup registry value name".into(),
        ));
    }
    let value_name = wide_nul(value_name);
    let Some(command_line) = command_line else {
        let key = match open_run_key(KEY_SET_VALUE) {
            Ok(key) => key,
            Err(ERROR_FILE_NOT_FOUND) => return Ok(()),
            Err(error) => return Err(registry_error("open startup key", error)),
        };
        // SAFETY: key and nul-terminated value name are valid for this registry call.
        let result = unsafe { RegDeleteValueW(key.0, value_name.as_ptr()) };
        return match result {
            ERROR_SUCCESS | ERROR_FILE_NOT_FOUND => Ok(()),
            error => Err(registry_error("remove startup value", error)),
        };
    };
    let mut command = command_line.encode_wide().collect::<Vec<_>>();
    if command.is_empty()
        || command
            .iter()
            .any(|unit| *unit == 0 || *unit < 0x20 || (0x7f..=0x9f).contains(unit))
    {
        return Err(BackendError::InvalidInput(
            "startup command contains a control character".into(),
        ));
    }
    command.push(0);
    if command.len() * 2 > MAX_REGISTRY_STRING_BYTES as usize {
        return Err(BackendError::InvalidInput(
            "startup command is too long for the registry".into(),
        ));
    }
    let key = create_run_key(KEY_SET_VALUE)?;
    let bytes = command
        .iter()
        .flat_map(|unit| unit.to_le_bytes())
        .collect::<Vec<_>>();
    // SAFETY: key, value name, UTF-16 data, and byte length are valid for RegSetValueExW.
    let result = unsafe {
        RegSetValueExW(
            key.0,
            value_name.as_ptr(),
            0,
            REG_SZ,
            bytes.as_ptr(),
            bytes.len() as u32,
        )
    };
    if result == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(registry_error("write startup value", result))
    }
}

fn read_run_value(value_name: &str) -> Result<Option<Vec<u16>>, BackendError> {
    let key = match open_run_key(KEY_QUERY_VALUE) {
        Ok(key) => key,
        Err(ERROR_FILE_NOT_FOUND) => return Ok(None),
        Err(error) => return Err(registry_error("open startup key", error)),
    };
    let value_name = wide_nul(value_name);
    let mut data_type = 0;
    let mut size = 0;
    // SAFETY: the null data pointer requests the documented size-only first pass.
    let result = unsafe {
        RegQueryValueExW(
            key.0,
            value_name.as_ptr(),
            ptr::null(),
            &mut data_type,
            ptr::null_mut(),
            &mut size,
        )
    };
    if result == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if result != ERROR_SUCCESS {
        return Err(registry_error("read startup value", result));
    }
    if data_type != REG_SZ || size == 0 || size > MAX_REGISTRY_STRING_BYTES || size % 2 != 0 {
        return Err(BackendError::Failed(
            "invalid startup registry value".into(),
        ));
    }
    let mut bytes = vec![0u8; size as usize];
    // SAFETY: `bytes` is writable for the bounded size returned by the first query.
    let result = unsafe {
        RegQueryValueExW(
            key.0,
            value_name.as_ptr(),
            ptr::null(),
            &mut data_type,
            bytes.as_mut_ptr(),
            &mut size,
        )
    };
    if result != ERROR_SUCCESS || size as usize > bytes.len() {
        return Err(registry_error("read startup value", result));
    }
    let mut wide = bytes[..size as usize]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect::<Vec<_>>();
    if let Some(end) = wide.iter().position(|unit| *unit == 0) {
        wide.truncate(end);
    }
    Ok(Some(wide))
}

fn executable_run_value() -> Result<Vec<u16>, BackendError> {
    let executable = std::env::current_exe().map_err(|_| BackendError::Unavailable)?;
    let mut command = Vec::new();
    command.push(b'"' as u16);
    command.extend(executable.as_os_str().encode_wide());
    command.push(b'"' as u16);
    command.push(0);
    if command.len() * 2 > MAX_REGISTRY_STRING_BYTES as usize {
        return Err(BackendError::InvalidInput(
            "executable path is too long for the startup registry".into(),
        ));
    }
    Ok(command)
}

fn current_executable_command() -> Result<OsString, BackendError> {
    let executable = std::env::current_exe().map_err(|_| BackendError::Unavailable)?;
    let mut command = Vec::with_capacity(executable.as_os_str().encode_wide().count() + 2);
    command.push(b'"' as u16);
    command.extend(executable.as_os_str().encode_wide());
    command.push(b'"' as u16);
    Ok(OsString::from_wide(&command))
}

fn registry_error(operation: &str, code: u32) -> BackendError {
    BackendError::Failed(format!("could not {operation} (Windows error {code})"))
}

struct RegistryKey(HKEY);

impl Drop for RegistryKey {
    fn drop(&mut self) {
        // SAFETY: this wrapper uniquely owns a successful RegOpenKeyExW/RegCreateKeyExW handle.
        unsafe {
            RegCloseKey(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestValue(String);

    impl Drop for TestValue {
        fn drop(&mut self) {
            let _ = set_autostart_command(&self.0, None);
        }
    }

    #[test]
    #[ignore = "requires permission to write HKCU Run; uses only a unique Glide-Test value"]
    fn custom_run_value_is_written_repaired_and_removed() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!("Glide-Test-{}-{suffix}", std::process::id());
        let _cleanup = TestValue(name.clone());
        let original =
            OsString::from(r#""C:\Glide Path\glided.exe" --headless --data-dir "C:\Data Dir""#);
        set_autostart_command(&name, Some(original.as_os_str())).unwrap();
        assert_eq!(
            read_run_value(&name).unwrap(),
            Some(original.encode_wide().collect())
        );

        let repaired =
            OsString::from(r#""C:\Glide Path\glided.exe" --headless --ui "C:\UI\Glide.exe""#);
        set_autostart_command(&name, Some(repaired.as_os_str())).unwrap();
        assert_eq!(
            read_run_value(&name).unwrap(),
            Some(repaired.encode_wide().collect())
        );
        set_autostart_command(&name, None).unwrap();
        assert_eq!(read_run_value(&name).unwrap(), None);
    }
}
