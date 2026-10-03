//! System-wide visibility, independent of thread-local ShowCursor counters.
use glide_platform::BackendError;
use std::sync::Mutex;
use windows_sys::{core::BOOL, Win32::UI::WindowsAndMessaging::*};

static HIDDEN: Mutex<bool> = Mutex::new(false);
// Every standard cursor Windows uses (the obsolete OCR_SIZE/OCR_ICON are unused).
const CURSORS: &[u32] = &[
    OCR_NORMAL,
    OCR_IBEAM,
    OCR_WAIT,
    OCR_CROSS,
    OCR_UP,
    OCR_SIZENWSE,
    OCR_SIZENESW,
    OCR_SIZEWE,
    OCR_SIZENS,
    OCR_SIZEALL,
    OCR_NO,
    OCR_HAND,
    OCR_APPSTARTING,
    32651, // OCR_HELP (not exported by windows-sys)
    32671,
    32672, // OCR_PIN, OCR_PERSON
];

/// Reloads the user's cursor scheme, including after a previous engine crash.
pub fn restore_system_cursors() -> Result<(), BackendError> {
    let mut hidden = HIDDEN.lock().unwrap_or_else(|poison| poison.into_inner());
    // SAFETY: SPI_SETCURSORS reloads the saved scheme, takes no pointer and emits no input.
    if unsafe { SystemParametersInfoW(SPI_SETCURSORS, 0, std::ptr::null_mut(), 0) } == 0 {
        return Err(BackendError::Unavailable);
    }
    *hidden = false;
    Ok(())
}

pub(crate) fn set_visible(visible: bool) -> Result<(), BackendError> {
    if visible {
        return restore_system_cursors();
    }
    let mut hidden = HIDDEN.lock().unwrap_or_else(|poison| poison.into_inner());
    if *hidden {
        return Ok(());
    }
    let and_mask = [0xffu8; 128];
    let xor_mask = [0u8; 128];
    for &id in CURSORS {
        // SAFETY: a 32x32 monochrome cursor uses two complete 128-byte masks. Each call
        // creates a fresh cursor: SetSystemCursor consumes/destroys it on success.
        let cursor = unsafe {
            CreateCursor(
                std::ptr::null_mut(),
                0,
                0,
                32,
                32,
                and_mask.as_ptr().cast(),
                xor_mask.as_ptr().cast(),
            )
        };
        if cursor.is_null() || unsafe { SetSystemCursor(cursor, id) } == 0 {
            if !cursor.is_null() {
                // SAFETY: a failed replacement did not consume this owned cursor.
                unsafe {
                    DestroyCursor(cursor);
                }
            }
            // Roll back partial replacement before reporting a cosmetic failure.
            drop(hidden);
            let _ = restore_system_cursors();
            return Err(BackendError::Unavailable);
        }
    }
    *hidden = true;
    Ok(())
}

unsafe extern "system" fn console_restore(_event: u32) -> BOOL {
    let _ = restore_system_cursors();
    0 // Other registered handlers still deliver the daemon's normal shutdown signal.
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetConsoleCtrlHandler(
        handler: Option<unsafe extern "system" fn(u32) -> BOOL>,
        add: BOOL,
    ) -> BOOL;
}

pub(crate) fn console_handler(add: bool) {
    // SAFETY: a static callback; no stack state is retained. No new windows-sys feature.
    unsafe {
        SetConsoleCtrlHandler(Some(console_restore), i32::from(add));
    }
}

pub(crate) struct RestoreGuard;
impl Drop for RestoreGuard {
    fn drop(&mut self) {
        let _ = restore_system_cursors();
    }
}
