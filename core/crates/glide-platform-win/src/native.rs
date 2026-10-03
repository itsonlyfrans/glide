//! Windows display coordinates, DPI awareness, and capability reporting.
//!
//! The application manifest should declare `PerMonitorV2` in
//! `<dpiAwareness>PerMonitorV2</dpiAwareness>` (with the legacy
//! `<dpiAware>true/pm</dpiAware>` fallback). Call [`enable_per_monitor_v2`]
//! at process startup before creating any windows as a runtime fallback.
//!
//! Low-level hooks and `SendInput` operate on the active input desktop. UAC prompts
//! and the lock screen use a secure desktop; input there is reported as unavailable.
//! UIPI blocks injection from a medium-integrity daemon into elevated applications,
//! so forwarding should run elevated when elevated foreground applications are needed.
//! One primary-scale projection preserves the physical desktop, shared edges and gaps.

use glide_platform::{BackendError, Monitor, PermissionStatus, Permissions, Point};
use std::mem::size_of;
use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows_sys::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, IsValidSid,
    TokenIntegrityLevel, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::HiDpi::{
    AreDpiAwarenessContextsEqual, GetDpiForMonitor, GetThreadDpiAwarenessContext,
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, MDT_EFFECTIVE_DPI,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetForegroundWindow, GetWindowThreadProcessId, MONITORINFOF_PRIMARY,
};

/// One monitor's logical description and physical virtual-desktop rectangle.
#[derive(Clone)]
pub(crate) struct NativeMonitor {
    pub(crate) monitor: Monitor,
    pub(crate) physical: RECT,
    pub(crate) divisor: f64,
}

struct RawMonitor {
    id: String,
    physical: RECT,
    scale: f64,
    primary: bool,
}

/// Requests process-wide per-monitor-v2 DPI awareness.
///
/// The call can fail when a manifest or an earlier API call already selected an
/// awareness mode. That is accepted only when this thread is already PMv2-aware.
pub fn enable_per_monitor_v2() -> Result<(), BackendError> {
    // SAFETY: this API accepts a documented pseudo-handle constant and retains no pointer.
    let set = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    // SAFETY: the API returns an opaque, thread-local DPI context handle for comparison only.
    let current = unsafe { GetThreadDpiAwarenessContext() };
    // SAFETY: both values are opaque DPI context handles supplied by Windows.
    let already_set = unsafe {
        AreDpiAwarenessContextsEqual(current, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) != 0
    };
    if set != 0 || already_set {
        Ok(())
    } else {
        Err(BackendError::Failed(
            "process must start with Per-Monitor-V2 DPI awareness".into(),
        ))
    }
}

/// Enumerates displays with physical and logical dimensions.
pub(crate) fn enumerate_monitors() -> Result<Vec<NativeMonitor>, BackendError> {
    enable_per_monitor_v2()?;
    let mut handles = Vec::<HMONITOR>::with_capacity(64);
    // SAFETY: the callback only appends opaque handles to the live vector pointed to by `data`.
    let result = unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(collect_monitor),
            (&mut handles as *mut Vec<HMONITOR>) as isize,
        )
    };
    if result == 0 || handles.is_empty() {
        return Err(BackendError::Unavailable);
    }

    let mut found = Vec::with_capacity(handles.len());
    for handle in handles {
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        // SAFETY: `info` has the MONITORINFOEXW layout required by GetMonitorInfoW and is writable.
        if unsafe {
            GetMonitorInfoW(
                handle,
                (&mut info as *mut MONITORINFOEXW).cast::<MONITORINFO>(),
            )
        } == 0
        {
            return Err(BackendError::Unavailable);
        }
        let rect = info.monitorInfo.rcMonitor;
        if rect.right <= rect.left || rect.bottom <= rect.top {
            return Err(BackendError::Failed(
                "Windows reported an empty monitor".into(),
            ));
        }
        let mut dpi_x = 0;
        let mut dpi_y = 0;
        // SAFETY: output pointers reference initialized writable integers for the duration of the call.
        if unsafe { GetDpiForMonitor(handle, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) } < 0
            || dpi_x == 0
            || dpi_y == 0
        {
            return Err(BackendError::Unavailable);
        }
        let scale_x = f64::from(dpi_x) / 96.0;
        let scale_y = f64::from(dpi_y) / 96.0;
        if (scale_x - scale_y).abs() > f64::EPSILON {
            return Err(BackendError::Failed(
                "Windows reported non-uniform monitor DPI".into(),
            ));
        }
        let id_end = info
            .szDevice
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(info.szDevice.len());
        let id = String::from_utf16_lossy(&info.szDevice[..id_end]);
        if id.is_empty() {
            return Err(BackendError::Failed(
                "Windows reported a monitor without a device name".into(),
            ));
        }
        found.push(RawMonitor {
            id,
            physical: rect,
            scale: scale_x,
            primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
        });
    }

    project_monitors(found)
}

/// Pure geometry projection, with no OS calls.
fn project_monitors(mut found: Vec<RawMonitor>) -> Result<Vec<NativeMonitor>, BackendError> {
    found.sort_by(|left, right| left.id.cmp(&right.id));
    let divisor = found
        .iter()
        .find(|m| m.primary)
        .or(found.first())
        .ok_or(BackendError::Unavailable)?
        .scale;
    let left = found
        .iter()
        .map(|m| m.physical.left)
        .min()
        .ok_or(BackendError::Unavailable)?;
    let top = found
        .iter()
        .map(|m| m.physical.top)
        .min()
        .ok_or(BackendError::Unavailable)?;
    if !divisor.is_finite() || divisor <= 0.0 {
        return Err(BackendError::InvalidInput("invalid primary DPI".into()));
    }
    let projected: Vec<_> = found
        .into_iter()
        .map(|raw| NativeMonitor {
            monitor: Monitor {
                id: raw.id,
                x: (i64::from(raw.physical.left) - i64::from(left)) as f64 / divisor,
                y: (i64::from(raw.physical.top) - i64::from(top)) as f64 / divisor,
                w: (i64::from(raw.physical.right) - i64::from(raw.physical.left)) as f64 / divisor,
                h: (i64::from(raw.physical.bottom) - i64::from(raw.physical.top)) as f64 / divisor,
                scale: raw.scale,
                primary: raw.primary,
            },
            physical: raw.physical,
            divisor,
        })
        .collect();
    for (i, m) in projected.iter().enumerate() {
        if m.monitor.w <= 0.0
            || m.monitor.h <= 0.0
            || !m.monitor.scale.is_finite()
            || m.monitor.scale <= 0.0
            || !m.monitor.w.is_finite()
            || !m.monitor.h.is_finite()
            || projected[..i].iter().any(|other| {
                m.physical.left < other.physical.right
                    && other.physical.left < m.physical.right
                    && m.physical.top < other.physical.bottom
                    && other.physical.top < m.physical.bottom
            })
        {
            return Err(BackendError::InvalidInput(
                "invalid or overlapping physical displays".into(),
            ));
        }
    }
    Ok(projected)
}

/// Read-only diagnostic snapshot: engine geometry alongside raw PMv2 OS rectangles.
pub fn monitor_geometry_snapshot() -> Result<Vec<(Monitor, [i32; 4])>, BackendError> {
    enumerate_monitors().map(|ms| {
        ms.into_iter()
            .map(|m| {
                (
                    m.monitor,
                    [
                        m.physical.left,
                        m.physical.top,
                        m.physical.right,
                        m.physical.bottom,
                    ],
                )
            })
            .collect()
    })
}

/// Device-wide divisor for raw physical mouse deltas too.
pub(crate) fn device_scale(monitors: &[NativeMonitor]) -> f64 {
    monitors.first().map_or(1.0, |m| m.divisor)
}

/// Raw outward motion survives OS clipping at the last pixel. Internal physical shared
/// edges and ordinary motion use LL position deltas instead, avoiding double counting.
pub(crate) fn outer_edge_delta(monitors: &[NativeMonitor], position: Point, delta: Point) -> Point {
    let Some(m) = monitors.iter().find(|m| {
        position.x >= m.monitor.x
            && position.x < m.monitor.x + m.monitor.w
            && position.y >= m.monitor.y
            && position.y < m.monitor.y + m.monitor.h
    }) else {
        return Point { x: 0.0, y: 0.0 };
    };
    let Some((x, y)) = logical_to_physical(monitors, &m.monitor.id, position) else {
        return Point { x: 0.0, y: 0.0 };
    };
    let occupied = |x: i64, y: i64| {
        monitors.iter().any(|other| {
            x >= i64::from(other.physical.left)
                && x < i64::from(other.physical.right)
                && y >= i64::from(other.physical.top)
                && y < i64::from(other.physical.bottom)
        })
    };
    let r = m.physical;
    Point {
        x: if (delta.x > 0.0 && x == r.right - 1 && !occupied(i64::from(r.right), i64::from(y)))
            || (delta.x < 0.0 && x == r.left && !occupied(i64::from(r.left) - 1, i64::from(y)))
        {
            delta.x
        } else {
            0.0
        },
        y: if (delta.y > 0.0 && y == r.bottom - 1 && !occupied(i64::from(x), i64::from(r.bottom)))
            || (delta.y < 0.0 && y == r.top && !occupied(i64::from(x), i64::from(r.top) - 1))
        {
            delta.y
        } else {
            0.0
        },
    }
}

/// Converts a device-local logical point through an explicit monitor's transform.
pub(crate) fn logical_to_physical(
    monitors: &[NativeMonitor],
    monitor_id: &str,
    point: Point,
) -> Option<(i32, i32)> {
    if !point.x.is_finite() || !point.y.is_finite() {
        return None;
    }
    let monitor = monitors.iter().find(|item| item.monitor.id == monitor_id)?;
    let local_x = point.x - monitor.monitor.x;
    let local_y = point.y - monitor.monitor.y;
    let scale = monitor.divisor;
    if !scale.is_finite()
        || scale <= 0.0
        || local_x < 0.0
        || local_y < 0.0
        || local_x >= monitor.monitor.w
        || local_y >= monitor.monitor.h
    {
        return None;
    }
    let width = i64::from(monitor.physical.right) - i64::from(monitor.physical.left);
    let height = i64::from(monitor.physical.bottom) - i64::from(monitor.physical.top);
    if width <= 0 || height <= 0 {
        return None;
    }
    let pixel_x = ((local_x * scale).round() as i64).clamp(0, width - 1);
    let pixel_y = ((local_y * scale).round() as i64).clamp(0, height - 1);
    Some((
        (i64::from(monitor.physical.left) + pixel_x) as i32,
        (i64::from(monitor.physical.top) + pixel_y) as i32,
    ))
}

/// Converts a physical pixel through an explicit monitor's transform.
pub(crate) fn physical_to_logical(
    monitors: &[NativeMonitor],
    monitor_id: &str,
    x: i32,
    y: i32,
) -> Option<Point> {
    let monitor = monitors.iter().find(|item| item.monitor.id == monitor_id)?;
    if x < monitor.physical.left
        || x >= monitor.physical.right
        || y < monitor.physical.top
        || y >= monitor.physical.bottom
    {
        return None;
    }
    Some(Point {
        x: monitor.monitor.x
            + (i64::from(x) - i64::from(monitor.physical.left)) as f64 / monitor.divisor,
        y: monitor.monitor.y
            + (i64::from(y) - i64::from(monitor.physical.top)) as f64 / monitor.divisor,
    })
}

/// Normalizes a physical point for `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`.
pub(crate) fn normalize_virtual_point(x: i32, y: i32, virtual_rect: RECT) -> Option<(u16, u16)> {
    let width = i64::from(virtual_rect.right) - i64::from(virtual_rect.left);
    let height = i64::from(virtual_rect.bottom) - i64::from(virtual_rect.top);
    if width <= 0 || height <= 0 {
        return None;
    }
    let local_x = i64::from(x) - i64::from(virtual_rect.left);
    let local_y = i64::from(y) - i64::from(virtual_rect.top);
    if local_x < 0 || local_y < 0 || local_x >= width || local_y >= height {
        return None;
    }
    let normalize = |coordinate: i64, extent: i64| -> u16 {
        if extent == 1 || coordinate == 0 {
            0
        } else if coordinate == extent - 1 {
            u16::MAX
        } else {
            // Aim at the pixel center: SendInput maps units through floor(value*extent/65536).
            // Edge scaling alone can round the first interior pixel back onto its neighbor.
            (((coordinate * 2 + 1) * 65536) / (extent * 2)).clamp(0, i64::from(u16::MAX)) as u16
        }
    };
    Some((normalize(local_x, width), normalize(local_y, height)))
}

/// Reports current desktop and foreground integrity limits without claiming unknown access.
pub(crate) fn permissions() -> Permissions {
    let desktop = input_desktop_is_default();
    let (accessibility, input_monitoring) = match desktop {
        Some(true) => (PermissionStatus::Granted, PermissionStatus::Granted),
        Some(false) => (PermissionStatus::Denied, PermissionStatus::Denied),
        None => (PermissionStatus::Unknown, PermissionStatus::Unknown),
    };
    // Foreground UIPI is event-specific, not a revoked device permission.
    let injection = match desktop {
        Some(false) => PermissionStatus::Denied,
        None => PermissionStatus::Unknown,
        Some(true) => {
            let mut cursor = POINT::default();
            // SAFETY: read-only desktop access probe with writable stack storage.
            if unsafe { GetCursorPos(&mut cursor) } != 0 {
                PermissionStatus::Granted
            } else {
                PermissionStatus::Unknown
            }
        }
    };
    Permissions {
        accessibility,
        input_monitoring,
        injection,
    }
}

/// Known secure desktop is denied; unknown/switching is transient.
pub(crate) fn injection_access() -> Result<(), BackendError> {
    match input_desktop_is_default() {
        Some(false) => Err(BackendError::PermissionDenied),
        None => Err(BackendError::Transient),
        Some(true) => match foreground_uipi_access() {
            Some(false) => Err(BackendError::TargetElevated),
            _ => Ok(()), // SendInput remains authoritative if integrity cannot be inspected.
        },
    }
}

unsafe extern "system" fn collect_monitor(
    monitor: HMONITOR,
    _dc: windows_sys::Win32::Graphics::Gdi::HDC,
    _rect: *mut RECT,
    data: isize,
) -> BOOL {
    if data == 0 || monitor.is_null() {
        return 0;
    }
    // SAFETY: `data` is the mutable Vec pointer passed to EnumDisplayMonitors above; the API
    // invokes this callback synchronously and serially before that Vec goes out of scope.
    let handles = unsafe { &mut *(data as *mut Vec<HMONITOR>) };
    if handles.len() >= 64 {
        return 0;
    }
    handles.push(monitor);
    1
}

/// Checks whether the currently active input desktop is the ordinary interactive desktop.
fn input_desktop_is_default() -> Option<bool> {
    const DESKTOP_READOBJECTS: u32 = 0x0001;
    const UOI_NAME: u32 = 2;
    const NAME_CAPACITY: usize = 256;

    // SAFETY: this opens the current input desktop with read-only object-name access.
    let desktop = unsafe { OpenInputDesktop(0, 0, DESKTOP_READOBJECTS) };
    if desktop.is_null() {
        return None;
    }
    let _desktop = DesktopHandle(desktop);
    let mut name = [0u16; NAME_CAPACITY];
    let mut needed = 0;
    // SAFETY: the fixed UTF-16 buffer is writable; its byte length is exact and bounded.
    let read = unsafe {
        GetUserObjectInformationW(
            desktop,
            UOI_NAME,
            name.as_mut_ptr().cast(),
            (name.len() * size_of::<u16>()) as u32,
            &mut needed,
        )
    };
    if read == 0 || needed as usize > name.len() * size_of::<u16>() {
        return None;
    }
    let end = name
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(name.len());
    const DEFAULT_DESKTOP: [u16; 7] = [
        b'D' as u16,
        b'e' as u16,
        b'f' as u16,
        b'a' as u16,
        b'u' as u16,
        b'l' as u16,
        b't' as u16,
    ];
    Some(
        end == DEFAULT_DESKTOP.len()
            && name[..end]
                .iter()
                .zip(DEFAULT_DESKTOP)
                .all(|(actual, expected)| *actual == expected),
    )
}

/// Returns whether this process has enough integrity to inject into the foreground process.
fn foreground_uipi_access() -> Option<bool> {
    // SAFETY: this returns a borrowed foreground HWND; it is only passed to the documented
    // process-ID query and is not retained or dereferenced by Rust.
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.is_null() {
        return None;
    }
    let mut process_id = 0;
    // SAFETY: `process_id` is a writable output pointer and `foreground` came from Windows.
    if unsafe { GetWindowThreadProcessId(foreground, &mut process_id) } == 0 || process_id == 0 {
        return None;
    }
    // SAFETY: OpenProcess returns an owned kernel handle or null; it is closed by ProcessHandle.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if process.is_null() {
        return None;
    }
    let _process = ProcessHandle(process);
    // SAFETY: GetCurrentProcess returns a pseudo-handle owned by the OS and must not be closed.
    let current = unsafe { GetCurrentProcess() };
    let current_integrity = process_integrity(current)?;
    let foreground_integrity = process_integrity(process)?;
    Some(current_integrity >= foreground_integrity)
}

fn process_integrity(process: HANDLE) -> Option<u32> {
    let mut token = std::ptr::null_mut();
    // SAFETY: `token` is writable; successful OpenProcessToken returns an owned handle.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 || token.is_null() {
        return None;
    }
    let _token = ProcessHandle(token);
    token_integrity(token)
}

fn token_integrity(token: HANDLE) -> Option<u32> {
    let mut needed = 0;
    // SAFETY: a null buffer with length zero is the documented size-query form.
    unsafe {
        GetTokenInformation(
            token,
            TokenIntegrityLevel,
            std::ptr::null_mut(),
            0,
            &mut needed,
        );
    }
    let mut storage = [0usize; 64];
    let capacity = storage.len() * size_of::<usize>();
    if needed < size_of::<TOKEN_MANDATORY_LABEL>() as u32 || needed as usize > capacity {
        None
    } else {
        let mut returned = 0;
        // SAFETY: `storage` is suitably aligned and large enough for the bounded token label.
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenIntegrityLevel,
                storage.as_mut_ptr().cast(),
                capacity as u32,
                &mut returned,
            )
        };
        if ok == 0 || returned as usize > capacity {
            return None;
        }
        // SAFETY: the successful query initialized a TOKEN_MANDATORY_LABEL at the aligned start.
        let label = unsafe { &*storage.as_ptr().cast::<TOKEN_MANDATORY_LABEL>() };
        let sid = label.Label.Sid;
        let buffer_start = storage.as_ptr() as usize;
        let buffer_end = buffer_start.checked_add(returned as usize)?;
        let sid_address = sid as usize;
        if sid.is_null() || sid_address < buffer_start || sid_address.checked_add(8)? > buffer_end {
            return None;
        }
        // The SID header's second byte gives its subauthority count.
        // SAFETY: the pointer is in the buffer and the complete 8-byte header is in bounds.
        let count = unsafe { *sid.cast::<u8>().add(1) };
        if count == 0
            || sid_address.checked_add(8 + (count as usize) * size_of::<u32>())? > buffer_end
        {
            return None;
        }
        // SAFETY: the complete SID range was bounds-checked inside the token-information buffer.
        if unsafe { IsValidSid(sid) } == 0 {
            return None;
        }
        // SAFETY: IsValidSid verified the SID; this API returns its subauthority count.
        let count_ptr = unsafe { GetSidSubAuthorityCount(sid) };
        if count_ptr.is_null() {
            return None;
        }
        // SAFETY: the count pointer is part of the validated SID.
        if unsafe { *count_ptr } != count {
            return None;
        }
        // SAFETY: the validated SID contains the requested final subauthority.
        let rid_ptr = unsafe { GetSidSubAuthority(sid, u32::from(count - 1)) };
        if rid_ptr.is_null() {
            return None;
        }
        // SAFETY: the returned RID pointer lies within the validated token buffer.
        Some(unsafe { *rid_ptr })
    }
}

struct ProcessHandle(HANDLE);

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        // SAFETY: every ProcessHandle wraps one owned process or token HANDLE exactly once.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

struct DesktopHandle(HANDLE);

impl Drop for DesktopHandle {
    fn drop(&mut self) {
        // SAFETY: DesktopHandle uniquely owns the OpenInputDesktop result.
        unsafe {
            CloseDesktop(self.0);
        }
    }
}

#[link(name = "user32")]
unsafe extern "system" {
    fn OpenInputDesktop(flags: u32, inherit: BOOL, desired_access: u32) -> HANDLE;
    fn CloseDesktop(desktop: HANDLE) -> BOOL;
    fn GetUserObjectInformationW(
        object: HANDLE,
        index: u32,
        info: *mut core::ffi::c_void,
        length: u32,
        needed: *mut u32,
    ) -> BOOL;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_absolute_coords_cover_negative_origin_and_edges() {
        let bounds = RECT {
            left: -1920,
            top: -120,
            right: 1920,
            bottom: 960,
        };
        assert_eq!(normalize_virtual_point(-1920, -120, bounds), Some((0, 0)));
        assert_eq!(
            normalize_virtual_point(1919, 959, bounds),
            Some((u16::MAX, u16::MAX))
        );
        assert_eq!(normalize_virtual_point(-1921, 0, bounds), None);
        assert_eq!(normalize_virtual_point(0, 960, bounds), None);
    }

    #[test]
    fn absolute_interior_coordinates_land_on_the_requested_pixel() {
        let bounds = RECT {
            left: -1920,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        for local in [0, 1, 2, 99, 1920, 3838, 3839] {
            let Some((x, _)) = normalize_virtual_point(local - 1920, 0, bounds) else {
                panic!("valid point");
            };
            assert_eq!((u32::from(x) * 3840 / 65536) as i32, local);
        }
    }

    fn raw(id: &str, rect: [i32; 4], dpi: u32, primary: bool) -> RawMonitor {
        RawMonitor {
            id: id.into(),
            physical: RECT {
                left: rect[0],
                top: rect[1],
                right: rect[2],
                bottom: rect[3],
            },
            scale: f64::from(dpi) / 96.0,
            primary,
        }
    }

    fn pixel_roundtrips(monitors: &[NativeMonitor]) {
        for m in monitors {
            for x in [m.physical.left, m.physical.right - 1] {
                for y in [m.physical.top, m.physical.bottom - 1] {
                    let p = physical_to_logical(monitors, &m.monitor.id, x, y).expect("pixel maps");
                    let physical =
                        logical_to_physical(monitors, &m.monitor.id, p).expect("point unmaps");
                    assert_eq!(physical, (x, y));
                    assert_eq!(
                        physical_to_logical(monitors, &m.monitor.id, physical.0, physical.1),
                        Some(p)
                    );
                }
            }
        }
    }

    #[test]
    fn user_stacked_display_edge_and_far_right_pixels_match_windows() {
        // Prevents the user's mouse missing shared edges and mapping outside the primary.
        let ms = project_monitors(vec![
            raw("DISPLAY2", [0, 0, 7680, 2160], 144, true),
            raw("DISPLAY1", [1305, -1440, 6425, 0], 96, false),
        ])
        .expect("projection");
        let top = &ms[0].monitor;
        let primary = &ms[1].monitor;
        assert_eq!(
            (top.x, top.y, top.w, top.h, top.scale),
            (870.0, 0.0, 5120.0 / 1.5, 960.0, 1.0)
        );
        assert_eq!(
            (primary.x, primary.y, primary.w, primary.h, primary.scale),
            (0.0, 960.0, 5120.0, 1440.0, 1.5)
        );
        assert_eq!(top.y + top.h, primary.y);
        for (x, y) in [(6424, 0), (7679, 0), (7679, 2159)] {
            let p = physical_to_logical(&ms, "DISPLAY2", x, y).expect("primary pixel");
            assert_eq!(logical_to_physical(&ms, "DISPLAY2", p), Some((x, y)));
        }
        pixel_roundtrips(&ms);
    }

    #[test]
    fn clipped_outer_edge_retains_raw_motion_without_duplicating_internal_os_movement() {
        // Prevents Windows clipping Local motion before the cursor can enter the Mac's monitor.
        let ms = project_monitors(vec![
            raw("DISPLAY2", [0, 0, 7680, 2160], 144, true),
            raw("DISPLAY1", [1305, -1440, 6425, 0], 96, false),
        ])
        .expect("geometry");
        let p = physical_to_logical(&ms, "DISPLAY2", 7679, 100).expect("last pixel");
        assert_eq!(
            outer_edge_delta(
                &ms,
                p,
                Point {
                    x: 2.0 / 1.5,
                    y: 3.0
                }
            ),
            Point {
                x: 2.0 / 1.5,
                y: 0.0
            }
        );
        assert_eq!(
            outer_edge_delta(&ms, p, Point { x: -1.0, y: 0.0 }),
            Point { x: 0.0, y: 0.0 }
        );
        let shared = physical_to_logical(&ms, "DISPLAY2", 6424, 0).expect("shared edge");
        assert_eq!(
            outer_edge_delta(&ms, shared, Point { x: 0.0, y: -2.0 }),
            Point { x: 0.0, y: 0.0 }
        );
        let gap_edge = physical_to_logical(&ms, "DISPLAY2", 7000, 0).expect("outer top edge");
        assert_eq!(
            outer_edge_delta(&ms, gap_edge, Point { x: 0.0, y: -2.0 }),
            Point { x: 0.0, y: -2.0 }
        );
    }

    #[test]
    fn uniform_projection_preserves_mixed_dpi_offsets_stacks_and_disconnected_gaps() {
        // Prevents resized/shifted secondary monitors breaking crossing on mixed-DPI desks.
        for (a, b, dpi) in [
            ([-3840, -200, 0, 1960], [0, 0, 1920, 1080], 96),
            ([0, 0, 3840, 2160], [3840, -120, 5760, 960], 192),
            ([0, -2160, 3840, 0], [960, 0, 2880, 1080], 120),
            ([-2000, -1000, -1000, 0], [500, 400, 2420, 1480], 144),
        ] {
            let ms = project_monitors(vec![raw("a", a, 96, false), raw("b", b, dpi, true)])
                .expect("projection");
            let divisor = f64::from(dpi) / 96.0;
            assert!(
                ((ms[1].monitor.x - ms[0].monitor.x) - f64::from(b[0] - a[0]) / divisor).abs()
                    < 1e-10
            );
            assert!(
                ((ms[1].monitor.y - ms[0].monitor.y) - f64::from(b[1] - a[1]) / divisor).abs()
                    < 1e-10
            );
            assert_eq!(ms[0].monitor.w, f64::from(a[2] - a[0]) / divisor);
            pixel_roundtrips(&ms);
        }
    }

    #[test]
    fn overlapping_raw_monitors_and_invalid_dpi_are_rejected() {
        // Prevents ambiguous selection without inventing gaps or silently moving displays.
        assert!(project_monitors(vec![
            raw("a", [0, 0, 100, 100], 144, true),
            raw("b", [50, 0, 150, 100], 96, false)
        ])
        .is_err());
        assert!(project_monitors(vec![raw("a", [0, 0, 100, 100], 0, true)]).is_err());
    }

    #[test]
    fn monitor_enumeration_reads_live_display_snapshot() {
        let Ok(monitors) = enumerate_monitors() else {
            panic!("Windows monitor enumeration failed");
        };
        assert!(!monitors.is_empty(), "Windows reported no active monitors");
        assert!(monitors.iter().any(|monitor| monitor.monitor.primary));
        assert!(monitors.iter().all(|monitor| {
            monitor.monitor.scale.is_finite()
                && monitor.monitor.scale > 0.0
                && monitor.monitor.w > 0.0
                && monitor.monitor.h > 0.0
                && monitor.physical.right > monitor.physical.left
                && monitor.physical.bottom > monitor.physical.top
        }));
    }

    #[test]
    fn interactive_desktop_api_diagnostics() {
        use windows_sys::Win32::Foundation::{GetLastError, POINT};
        use windows_sys::Win32::System::DataExchange::{CloseClipboard, OpenClipboard};
        use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
        let mut point = POINT::default();
        // SAFETY: Read-only query into stack storage; no cursor movement is attempted.
        let cursor = unsafe { GetCursorPos(&mut point) } != 0;
        // SAFETY: Capture the error immediately after the failed Win32 query.
        let cursor_error = if cursor {
            None
        } else {
            Some(unsafe { GetLastError() })
        };
        // SAFETY: Read-only clipboard lock attempt; no contents are read or changed.
        let clipboard = unsafe { OpenClipboard(std::ptr::null_mut()) } != 0;
        // SAFETY: Capture its error before another API call overwrites the thread error.
        let clipboard_error = if clipboard {
            None
        } else {
            Some(unsafe { GetLastError() })
        };
        if clipboard {
            // SAFETY: Balance this test's successful OpenClipboard.
            unsafe {
                CloseClipboard();
            }
        }
        let capability = permissions();
        eprintln!("Desktop APIs: cursor={cursor} error={cursor_error:?}; clipboard={clipboard} error={clipboard_error:?}; permissions={capability:?}");
        if !cursor {
            assert_ne!(capability.injection, PermissionStatus::Granted);
        }
    }

    #[test]
    fn normalize_rejects_empty_or_single_pixel_rectangles_safely() {
        let empty = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 1,
        };
        assert_eq!(normalize_virtual_point(0, 0, empty), None);
        let one_pixel = RECT {
            left: -1,
            top: -1,
            right: 0,
            bottom: 0,
        };
        assert_eq!(normalize_virtual_point(-1, -1, one_pixel), Some((0, 0)));
    }
}
