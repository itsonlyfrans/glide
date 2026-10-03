use crate::{Error, Result};
use std::fs::{self, File, Metadata, OpenOptions};
use std::path::{Component, Path};

pub(crate) fn regular(metadata: &Metadata) -> bool {
    !link(metadata) && metadata.is_file()
}

pub(crate) fn link(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

/// Rejects names rather than rewriting them (rewriting can alias existing files).
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name.contains("..")
        || name.len() > 255
        || name.encode_utf16().count() > 255
        || name.ends_with(['.', ' '])
        || name
            .chars()
            .any(|c| c.is_control() || "/\\:<>\"|?*".contains(c))
    {
        return Err(Error::Invalid("unsafe name"));
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ')
        .to_uppercase();
    if matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(prefix).is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    }) {
        return Err(Error::Invalid("reserved name"));
    }
    Ok(())
}

pub(crate) fn validate_relative(path: &str, depth: usize, max_path: usize) -> Result<()> {
    if path.len() > max_path || path.split('/').count() > depth {
        return Err(Error::Limit("path length/depth"));
    }
    for name in path.split('/') {
        validate_name(name)?;
    }
    Ok(())
}

/// Checks every existing component, including junctions/reparse points on Windows.
/// The daemon must own the data dir and deny untrusted local writers (see README).
pub(crate) fn check_chain(path: &Path) -> Result<()> {
    let mut current = std::path::PathBuf::new();
    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            return Err(Error::Invalid("parent path component"));
        }
        current.push(component.as_os_str());
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        let metadata = fs::symlink_metadata(&current)?;
        if link(&metadata) {
            return Err(Error::Invalid("symlink/reparse point"));
        }
    }
    Ok(())
}

pub(crate) fn open_file(path: &Path, write: bool, create: bool) -> Result<File> {
    let parent = path.parent().ok_or(Error::Invalid("file parent"))?;
    check_chain(parent)?;
    let mut options = OpenOptions::new();
    options.read(true).write(write).create_new(create);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Open the reparse point itself; never follow it. Deny writers and deletion.
        options.custom_flags(0x0020_0000).share_mode(1);
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0x100).mode(0o600); // O_NOFOLLOW on Darwin
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0x20000).mode(0o600); // O_NOFOLLOW on Linux
    }
    let file = options.open(path)?;
    if !regular(&file.metadata()?) {
        return Err(Error::Invalid("non-regular file"));
    }
    Ok(file)
}

pub(crate) fn private_dir(path: &Path) -> Result<()> {
    check_chain(path.parent().ok_or(Error::Invalid("directory parent"))?)?;
    fs::create_dir(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// The File owns the OS lease; closing it releases the lock on every exit path.
pub(crate) fn lease(path: &Path) -> Result<File> {
    check_chain(path.parent().ok_or(Error::Invalid("lease parent"))?)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(0).custom_flags(0x0020_0000);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        #[cfg(target_os = "macos")]
        options.custom_flags(0x100).mode(0o600);
        #[cfg(not(target_os = "macos"))]
        options.custom_flags(0x20000).mode(0o600);
    }
    let file = options.open(path).map_err(|error| {
        #[cfg(windows)]
        if matches!(error.raw_os_error(), Some(32 | 33)) {
            return Error::Busy;
        }
        Error::Io(error)
    })?;
    if !regular(&file.metadata()?) {
        return Err(Error::Invalid("lease type"));
    }
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        unsafe extern "C" {
            fn flock(fd: i32, operation: i32) -> i32;
        }
        // SAFETY: file is a live owned descriptor; LOCK_EX | LOCK_NB acquires a lease.
        if unsafe { flock(file.as_raw_fd(), 2 | 4) } != 0 {
            let error = std::io::Error::last_os_error();
            return Err(if error.kind() == std::io::ErrorKind::WouldBlock {
                Error::Busy
            } else {
                error.into()
            });
        }
    }
    Ok(file)
}

pub(crate) fn disk_available(path: &Path) -> Result<u64> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetDiskFreeSpaceExW(
                path: *const u16,
                available: *mut u64,
                total: *mut u64,
                free: *mut u64,
            ) -> i32;
        }
        let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut available = 0;
        // SAFETY: path is NUL-terminated and available points to a live u64; other outputs are optional.
        if unsafe {
            GetDiskFreeSpaceExW(
                path.as_ptr(),
                &mut available,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(available)
    }
    #[cfg(target_os = "macos")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        #[repr(C)]
        struct StatFs {
            bsize: u32,
            iosize: i32,
            blocks: u64,
            bfree: u64,
            bavail: u64,
            files: u64,
            ffree: u64,
            fsid: [i32; 2],
            owner: u32,
            kind: u32,
            flags: u32,
            subtype: u32,
            typename: [u8; 16],
            mounted: [u8; 1024],
            source: [u8; 1024],
            flags_ext: u32,
            reserved: [u32; 7],
        }
        unsafe extern "C" {
            #[cfg_attr(not(target_arch = "aarch64"), link_name = "statfs$INODE64")]
            fn statfs(path: *const std::ffi::c_char, result: *mut StatFs) -> i32;
        }
        let path =
            CString::new(path.as_os_str().as_bytes()).map_err(|_| Error::Invalid("data path"))?;
        let mut result = std::mem::MaybeUninit::<StatFs>::zeroed();
        // SAFETY: Darwin statfs fills the repr(C) statfs64 layout on success; path is NUL-terminated.
        if unsafe { statfs(path.as_ptr(), result.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: statfs returned success and initialized the complete structure.
        let result = unsafe { result.assume_init() };
        Ok(result.bavail.saturating_mul(u64::from(result.bsize)))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = path;
        Err(Error::Invalid("disk-space query unsupported on this OS"))
    }
}
