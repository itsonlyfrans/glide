//! Engine-owned launch-at-login registration.

use anyhow::{bail, Context, Result};
use std::path::Path;

pub fn validate_ui(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("UI path must be absolute");
    }
    let text = path.to_str().context("UI path must be valid Unicode")?;
    if text.chars().any(char::is_control) {
        bail!("UI path contains a control character");
    }
    if !path.is_file() {
        bail!("UI path must name an existing file");
    }
    Ok(())
}

/// Reconcile the platform login item with the setting persisted by the daemon.
pub fn repair(enabled: bool, data_dir: &Path, ui: Option<&Path>) -> Result<()> {
    if enabled {
        if let Some(ui) = ui {
            validate_ui(ui)?;
        }
        if !data_dir.is_absolute() || path_has_control(data_dir)? {
            bail!("data directory must be an absolute path without control characters");
        }
    }
    #[cfg(windows)]
    {
        let command = if enabled {
            use std::os::windows::ffi::OsStringExt;
            let executable =
                std::env::current_exe().context("could not resolve glided executable")?;
            if path_has_control(&executable)? {
                bail!("engine path contains a control character");
            }
            Some(std::ffi::OsString::from_wide(&windows_autostart_command(
                &executable,
                data_dir,
                ui,
            )))
        } else {
            None
        };
        glide_platform_win::set_autostart_command("Glide", command.as_deref())?;
    }

    #[cfg(target_os = "macos")]
    {
        glide_platform_mac::repair_autostart(enabled, data_dir, ui)?;
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (enabled, ui);
    }

    Ok(())
}

fn path_has_control(path: &Path) -> Result<bool> {
    let path = path.to_str().context("path must be valid Unicode")?;
    Ok(path.chars().any(char::is_control))
}

#[cfg(windows)]
fn windows_autostart_command(executable: &Path, data_dir: &Path, ui: Option<&Path>) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    let mut command = Vec::new();
    quote_windows_arg(
        &executable.as_os_str().encode_wide().collect::<Vec<_>>(),
        &mut command,
    );
    command.extend(" --headless --data-dir ".encode_utf16());
    quote_windows_arg(
        &data_dir.as_os_str().encode_wide().collect::<Vec<_>>(),
        &mut command,
    );
    if let Some(ui) = ui {
        command.extend(" --ui ".encode_utf16());
        quote_windows_arg(
            &ui.as_os_str().encode_wide().collect::<Vec<_>>(),
            &mut command,
        );
    }
    command
}

#[cfg(windows)]
fn quote_windows_arg(arg: &[u16], output: &mut Vec<u16>) {
    output.push(b'"' as u16);
    let mut slashes = 0usize;
    for unit in arg {
        if *unit == b'\\' as u16 {
            slashes += 1;
        } else if *unit == b'"' as u16 {
            output.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2 + 1));
            output.push(*unit);
            slashes = 0;
        } else {
            output.extend(std::iter::repeat_n(b'\\' as u16, slashes));
            output.push(*unit);
            slashes = 0;
        }
    }
    output.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
    output.push(b'"' as u16);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn ui_path_must_be_absolute_existing_and_control_free() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("Glide UI.exe");
        fs::write(&file, b"ui").unwrap();
        assert!(validate_ui(&file).is_ok());
        assert!(validate_ui(Path::new("relative.exe")).is_err());
        assert!(validate_ui(&directory.path().join("missing.exe")).is_err());
        assert!(validate_ui(&directory.path().join("bad\nui.exe")).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_command_quotes_spaces_and_trailing_backslashes() {
        let command = String::from_utf16(&windows_autostart_command(
            Path::new(r"C:\Program Files\Glide\glided.exe"),
            Path::new(r"C:\Users\Test User\Glide Data\"),
            None,
        ))
        .unwrap();
        assert_eq!(
            command,
            r#""C:\Program Files\Glide\glided.exe" --headless --data-dir "C:\Users\Test User\Glide Data\\""#
        );
    }
}
