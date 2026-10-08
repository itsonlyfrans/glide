use crate::{
    keyboard::{key_input, translate_key, MAGIC},
    native::{self, NativeMonitor},
};
use crossbeam_channel::{bounded, Receiver, Sender};
use glide_platform::{
    BackendError, Button, CaptureMode, CaptureStatus, InputBackend, InputEvent, InputEventKind,
    InputSink, Key, Monitor, PermissionStatus, Permissions, Point,
};
use std::{
    cell::{Cell, RefCell},
    mem::size_of,
    ptr::{null, null_mut},
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Arc, Mutex, RwLock,
    },
    thread::{self, JoinHandle},
};
use windows_sys::Win32::{
    Foundation::*,
    System::{
        LibraryLoader::GetModuleHandleW,
        Threading::{
            CancelWaitableTimer, CreateWaitableTimerExW, GetCurrentThreadId, SetWaitableTimer,
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, INFINITE, TIMER_ALL_ACCESS,
        },
    },
    UI::{
        Input::{KeyboardAndMouse::*, *},
        WindowsAndMessaging::*,
    },
};

const DRAIN: u32 = WM_APP + 51;
const COMMAND: u32 = WM_APP + 52;
static CAPTURE_OWNED: AtomicBool = AtomicBool::new(false);
thread_local! { static HOOK_CONTEXT: Cell<*const HookContext> = const { Cell::new(null()) }; }

#[derive(Clone, Copy)]
enum Sample {
    Key(KBDLLHOOKSTRUCT),
    Mouse(u32, MSLLHOOKSTRUCT),
    Raw(i32, i32),
}
struct HookContext {
    sink: RefCell<Option<InputSink>>,
    status: Sender<CaptureStatus>,
    samples: Sender<Sample>,
    swallow: Arc<AtomicBool>,
    fault: Arc<AtomicBool>,
    wake: AtomicBool,
    thread: u32,
}
impl HookContext {
    fn fail(&self) {
        self.fail_as(CaptureStatus::UnsupportedInput);
    }
    fn fail_as(&self, reason: CaptureStatus) {
        if let Some(sink) = self.sink.borrow().as_ref() {
            sink.mark_overflow();
        }
        let _ = self.status.try_send(reason);
        self.fault.store(true, Ordering::Release);
        self.swallow.store(false, Ordering::Release);
    }
    fn push(&self, sample: Sample) -> bool {
        if self.samples.try_send(sample).is_err() {
            self.fail_as(CaptureStatus::QueueOverflow);
            // SAFETY: Thread ID belongs to our live message loop; messages contain no pointers.
            unsafe {
                PostThreadMessageW(self.thread, DRAIN, 0, 0);
            }
            return false;
        }
        if !self.wake.swap(true, Ordering::AcqRel) {
            // SAFETY: No pointers cross the Windows message queue.
            if unsafe { PostThreadMessageW(self.thread, DRAIN, 0, 0) } == 0 {
                self.fail();
                return false;
            }
        }
        true
    }
}

/// Physical keys Windows itself has seen go down. While control is on another computer every physical key is
/// swallowed, so a key held across the crossing would otherwise never see its release and stay stuck on this PC.
static OS_KEY_DOWN: [AtomicBool; 256] = [const { AtomicBool::new(false) }; 256];

/// Decide whether the hook swallows this key event, keeping `OS_KEY_DOWN` in step with what Windows has seen.
/// The Windows keys are always swallowed on release: a lone Win release would open the Start menu.
fn block_key(vk: u32, up: bool, injected: bool, swallowing: bool) -> bool {
    if injected {
        return false;
    }
    let Some(seen) = OS_KEY_DOWN.get(vk as usize) else {
        return swallowing;
    };
    if !swallowing {
        seen.store(!up, Ordering::Release);
        return false;
    }
    let is_win = vk == u32::from(VK_LWIN) || vk == u32::from(VK_RWIN);
    if up && !is_win && seen.swap(false, Ordering::AcqRel) {
        return false;
    }
    true
}

unsafe extern "system" fn keyboard_hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && lp != 0 {
        // SAFETY: Windows guarantees a KBDLLHOOKSTRUCT for HC_ACTION for this callback.
        let data = unsafe { *(lp as *const KBDLLHOOKSTRUCT) };
        let injected = data.flags & (LLKHF_INJECTED | LLKHF_LOWER_IL_INJECTED) != 0
            || data.dwExtraInfo == MAGIC;
        let block = HOOK_CONTEXT.with(|slot| {
            let context = slot.get();
            if context.is_null() {
                return false;
            }
            // SAFETY: TLS pointer is valid only while hooks run on the owning loop thread.
            let context = unsafe { &*context };
            let pushed = context.push(Sample::Key(data));
            let swallowing = pushed && !injected && context.swallow.load(Ordering::Acquire);
            block_key(
                data.vkCode,
                data.flags & LLKHF_UP != 0,
                injected,
                swallowing,
            )
        });
        if block {
            return 1;
        }
    }
    // SAFETY: Forward unchanged Windows callback parameters to the next hook.
    unsafe { CallNextHookEx(null_mut(), code, wp, lp) }
}
unsafe extern "system" fn mouse_hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && lp != 0 {
        // SAFETY: Windows guarantees an MSLLHOOKSTRUCT for HC_ACTION.
        let data = unsafe { *(lp as *const MSLLHOOKSTRUCT) };
        let injected = mouse_injected(wp as u32, &data);
        let block = HOOK_CONTEXT.with(|slot| {
            let context = slot.get();
            if context.is_null() {
                return false;
            }
            // SAFETY: Hooks run on the thread owning this TLS context.
            let context = unsafe { &*context };
            let swallow = context.swallow.load(Ordering::Acquire);
            // Physical motion in forwarding mode is obtained from Raw Input, never LL deltas.
            let pushed = if wp as u32 == WM_MOUSEMOVE && swallow && !injected {
                true
            } else {
                context.push(Sample::Mouse(wp as u32, data))
            };
            pushed && swallow && !injected && context.swallow.load(Ordering::Acquire)
        });
        if block {
            return 1;
        }
    }
    // SAFETY: Forward unchanged callback parameters.
    unsafe { CallNextHookEx(null_mut(), code, wp, lp) }
}
unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_INPUT {
        let mut input = RAWINPUT::default();
        let mut bytes = size_of::<RAWINPUT>() as u32;
        // SAFETY: Properly aligned, bounded stack RAWINPUT storage; handle is supplied by WM_INPUT.
        let read = unsafe {
            GetRawInputData(
                lp as HRAWINPUT,
                RID_INPUT,
                (&mut input as *mut RAWINPUT).cast(),
                &mut bytes,
                size_of::<RAWINPUTHEADER>() as u32,
            )
        };
        HOOK_CONTEXT.with(|slot| {
            let pointer = slot.get();
            if pointer.is_null() {
                return;
            }
            // SAFETY: Thread-local context is alive throughout this owning window dispatch.
            let context = unsafe { &*pointer };
            if read == u32::MAX
                || (read as usize) < size_of::<RAWINPUTHEADER>() + size_of::<RAWMOUSE>()
            {
                context.fail();
            } else if let Some((x, y)) = raw_delta(&input) {
                context.push(Sample::Raw(x, y));
            } else if unsupported_physical_raw(&input) && context.swallow.load(Ordering::Acquire) {
                context.fail();
            }
        });
    } else if msg == WM_DISPLAYCHANGE {
        HOOK_CONTEXT.with(|slot| {
            let context = slot.get();
            if !context.is_null() {
                // SAFETY: Live TLS context; scalar notification only.
                unsafe {
                    PostThreadMessageW((*context).thread, WM_DISPLAYCHANGE, 0, 0);
                }
            }
        });
    }
    // SAFETY: WM_INPUT requires default cleanup even for foreground raw messages.
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

fn raw_delta(input: &RAWINPUT) -> Option<(i32, i32)> {
    if input.header.dwType != RIM_TYPEMOUSE || input.header.hDevice.is_null() {
        return None;
    }
    // SAFETY: The discriminator identifies the mouse member. Callers supply full RAWINPUT storage.
    let mouse = unsafe { input.data.mouse };
    if mouse.usFlags & MOUSE_MOVE_ABSOLUTE != 0 || mouse.ulExtraInformation == MAGIC as u32 {
        return None;
    }
    Some((mouse.lLastX, mouse.lLastY))
}

fn unsupported_physical_raw(input: &RAWINPUT) -> bool {
    if input.header.dwType != RIM_TYPEMOUSE || input.header.hDevice.is_null() {
        return false;
    }
    // SAFETY: The mouse discriminator identifies the initialized RAWMOUSE union member.
    let mouse = unsafe { input.data.mouse };
    mouse.ulExtraInformation != MAGIC as u32 && mouse.usFlags & MOUSE_MOVE_ABSOLUTE != 0
}

/// Installs Glide's low-level keyboard and mouse hooks on the calling thread.
fn install_hooks(instance: HINSTANCE) -> Result<(Hook, Hook), BackendError> {
    // SAFETY: Static extern callbacks, module handle, and process-wide low-level hooks.
    let keyboard = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), instance, 0) };
    if keyboard.is_null() {
        return Err(BackendError::PermissionDenied);
    }
    let keyboard = Hook(keyboard);
    // SAFETY: As above for the mouse callback.
    let mouse = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), instance, 0) };
    if mouse.is_null() {
        return Err(BackendError::PermissionDenied);
    }
    Ok((keyboard, Hook(mouse)))
}

struct Hook(HHOOK);
impl Drop for Hook {
    fn drop(&mut self) {
        // SAFETY: Owned hook installed by this thread.
        unsafe {
            UnhookWindowsHookEx(self.0);
        }
    }
}
struct Window {
    handle: HWND,
    class: Vec<u16>,
    instance: HINSTANCE,
    raw: bool,
}
impl Drop for Window {
    fn drop(&mut self) {
        // SAFETY: Thread-affine resources are destroyed on their creating message thread.
        unsafe {
            if self.raw {
                let device = RAWINPUTDEVICE {
                    usUsagePage: 1,
                    usUsage: 2,
                    dwFlags: RIDEV_REMOVE,
                    hwndTarget: null_mut(),
                };
                RegisterRawInputDevices(&device, 1, size_of::<RAWINPUTDEVICE>() as u32);
            }
            DestroyWindow(self.handle);
            UnregisterClassW(self.class.as_ptr(), self.instance);
        }
    }
}
struct CursorGuard {
    saved: Option<RECT>,
}
impl CursorGuard {
    fn new() -> Self {
        Self { saved: None }
    }
    fn pin(&mut self) -> Result<(), BackendError> {
        if self.saved.is_some() {
            return Ok(());
        }
        let mut old = RECT::default();
        let mut pos = POINT::default();
        // SAFETY: OS writes initialized stack storage; clipping uses a valid one-pixel rect.
        unsafe {
            if GetClipCursor(&mut old) == 0 || GetCursorPos(&mut pos) == 0 {
                return Err(BackendError::Unavailable);
            }
            let rect = RECT {
                left: pos.x,
                top: pos.y,
                right: pos.x.saturating_add(1),
                bottom: pos.y.saturating_add(1),
            };
            if ClipCursor(&rect) == 0 {
                return Err(BackendError::PermissionDenied);
            }
            self.saved = Some(old);
        }
        Ok(())
    }
    fn unpin(&mut self) {
        if let Some(rect) = self.saved {
            // SAFETY: The saved rectangle came from GetClipCursor.
            if unsafe { ClipCursor(&rect) } != 0 {
                self.saved = None;
            }
        }
    }
    fn restore(&mut self) {
        self.unpin();
    }
}
impl Drop for CursorGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

struct RawTimer {
    handle: HANDLE,
    armed: bool,
}
impl RawTimer {
    fn new() -> Result<Self, BackendError> {
        // SAFETY: Create an unnamed, auto-reset high-resolution timer owned by this thread.
        let handle = unsafe {
            CreateWaitableTimerExW(
                null(),
                null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS,
            )
        };
        if handle.is_null() {
            Err(BackendError::Unavailable)
        } else {
            Ok(Self {
                handle,
                armed: false,
            })
        }
    }
    fn arm(&mut self) -> Result<(), BackendError> {
        if !self.armed {
            let due = -10_000_i64; // Relative 100ns units: coalesce for one millisecond.
                                   // SAFETY: Owned timer, valid due time, no APC callback or retained borrowed pointer.
            if unsafe { SetWaitableTimer(self.handle, &due, 0, None, null(), 0) } == 0 {
                return Err(BackendError::Unavailable);
            }
            self.armed = true;
        }
        Ok(())
    }
    fn disarm(&mut self) {
        if self.armed {
            // SAFETY: Owned kernel timer; cancellation releases no Rust data.
            unsafe {
                CancelWaitableTimer(self.handle);
            }
            self.armed = false;
        }
    }
}
impl Drop for RawTimer {
    fn drop(&mut self) {
        self.disarm(); // SAFETY: Unique owned timer handle.
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

enum Command {
    Capture(InputSink),
    Mode(CaptureMode),
    Stop,
}
struct Request {
    command: Command,
    reply: Sender<Result<(), BackendError>>,
    status: Arc<AtomicU8>,
}
struct Injected {
    keys: [bool; 256],
    buttons: [bool; 5],
    wheel: [f64; 2],
    relative: [f64; 2],
}
impl Default for Injected {
    fn default() -> Self {
        Self {
            keys: [false; 256],
            buttons: [false; 5],
            wheel: [0.0; 2],
            relative: [0.0; 2],
        }
    }
}

/// Owns a single dedicated Windows hook/message thread and tracked SendInput state.
pub struct WindowsInput {
    status: Receiver<CaptureStatus>,
    commands: Sender<Request>,
    thread_id: u32,
    thread: Mutex<Option<JoinHandle<()>>>,
    changes: Receiver<Vec<Monitor>>,
    injected: Mutex<Injected>,
    swallow: Arc<AtomicBool>,
    monitors: Arc<RwLock<Vec<NativeMonitor>>>,
    alive: Arc<AtomicBool>,
    fault: Arc<AtomicBool>,
}
impl WindowsInput {
    pub fn new() -> Result<Self, BackendError> {
        native::enable_per_monitor_v2()?;
        if !CAPTURE_OWNED.load(Ordering::Acquire) {
            let _ = crate::cursor::restore_system_cursors();
        }
        let monitors = Arc::new(RwLock::new(native::enumerate_monitors()?));
        if CAPTURE_OWNED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(BackendError::Unavailable);
        }
        let (commands, requests) = bounded(64);
        let (change_tx, changes) = bounded(1);
        let (status_tx, status) = bounded(16);
        let (ready_tx, ready) = bounded(1);
        let swallow = Arc::new(AtomicBool::new(false));
        let mode = swallow.clone();
        let snapshot = monitors.clone();
        let alive = Arc::new(AtomicBool::new(true));
        let running = alive.clone();
        let fault = Arc::new(AtomicBool::new(false));
        let lost = fault.clone();
        let thread = match thread::Builder::new()
            .name("glide-input".into())
            .spawn(move || {
                match capture_thread(
                    requests,
                    change_tx,
                    status_tx,
                    mode,
                    snapshot,
                    lost,
                    ready_tx.clone(),
                ) {
                    Ok(()) => {}
                    Err(error) => {
                        let _ = ready_tx.try_send(Err(error));
                    }
                }
                running.store(false, Ordering::Release);
                CAPTURE_OWNED.store(false, Ordering::Release);
            }) {
            Ok(thread) => thread,
            Err(_) => {
                CAPTURE_OWNED.store(false, Ordering::Release);
                return Err(BackendError::Unavailable);
            }
        };
        match ready.recv() {
            Ok(Ok(thread_id)) => {
                crate::cursor::console_handler(true);
                Ok(Self {
                    commands,
                    thread_id,
                    thread: Mutex::new(Some(thread)),
                    changes,
                    injected: Mutex::new(Injected::default()),
                    status,
                    swallow,
                    monitors,
                    alive,
                    fault,
                })
            }
            result => {
                let _ = thread.join();
                Err(result
                    .ok()
                    .and_then(Result::err)
                    .unwrap_or(BackendError::Unavailable))
            }
        }
    }
    fn request(&self, command: Command) -> Result<(), BackendError> {
        let (reply, answer) = bounded(1);
        let status = Arc::new(AtomicU8::new(0));
        self.commands
            .try_send(Request {
                command,
                reply,
                status: status.clone(),
            })
            .map_err(|_| BackendError::Unavailable)?;
        // SAFETY: Scalar wake message, no borrowed data. Once queued, wait for application;
        // never return a timeout while an operation could still apply later.
        if unsafe { PostThreadMessageW(self.thread_id, COMMAND, 0, 0) } == 0 {
            self.swallow.store(false, Ordering::Release);
            if status
                .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Err(BackendError::Unavailable);
            }
        }
        answer.recv().map_err(|_| BackendError::Unavailable)?
    }
    /// Synchronizes only Ctrl/Alt/Shift/Win holds, preserving other injected keys.
    pub fn sync_modifiers(&self, down: &[Key]) -> Result<(), BackendError> {
        self.require_live()?;
        require_injection()?;
        if down.iter().any(|key| !(224..=231).contains(&key.0)) {
            return Err(BackendError::InvalidInput("modifier usage required".into()));
        }
        let mut held = self
            .injected
            .lock()
            .map_err(|_| BackendError::StateUnavailable)?;
        let mut error = None;
        for hid in 224..=231 {
            let key = Key(hid);
            let target = down.contains(&key);
            if held.keys[hid as usize] != target {
                match send(&key_input(key, target)?) {
                    Ok(()) => held.keys[hid as usize] = target,
                    Err(e) => error = Some(e),
                }
            }
        }
        error.map_or(Ok(()), Err)
    }
    /// Relative logical deltas use the same primary-scale divisor as absolute positions.
    pub fn inject_relative(&self, dx: f64, dy: f64) -> Result<(), BackendError> {
        self.require_live()?;
        require_injection()?;
        if !dx.is_finite()
            || !dy.is_finite()
            || dx.abs() > i32::MAX as f64
            || dy.abs() > i32::MAX as f64
        {
            return Err(BackendError::InvalidInput("invalid mouse delta".into()));
        }
        let mut held = self
            .injected
            .lock()
            .map_err(|_| BackendError::StateUnavailable)?;
        let divisor = native::device_scale(
            &self
                .monitors
                .read()
                .map_err(|_| BackendError::StateUnavailable)?,
        );
        let x = dx * divisor + held.relative[0];
        let y = dy * divisor + held.relative[1];
        if !x.is_finite()
            || !y.is_finite()
            || x.abs() > f64::from(i32::MAX)
            || y.abs() > f64::from(i32::MAX)
        {
            return Err(BackendError::InvalidInput(
                "invalid scaled mouse delta".into(),
            ));
        }
        send(&mouse_input(
            x.trunc() as i32,
            y.trunc() as i32,
            0,
            MOUSEEVENTF_MOVE,
        ))?;
        held.relative = [x.fract(), y.fract()];
        Ok(())
    }
    fn require_live(&self) -> Result<(), BackendError> {
        if self.alive.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(BackendError::Unavailable)
        }
    }
    /// Diagnostic native loss flag, cleared by an explicit capture-mode request.
    /// The shared sink overflow latch and `capture_status_changes()` drive daemon
    /// cleanup; re-entry does not require replacing the sink.
    pub fn capture_faulted(&self) -> bool {
        self.fault.load(Ordering::Acquire)
    }
}
impl InputBackend for WindowsInput {
    fn capture_status_changes(&self) -> Receiver<CaptureStatus> {
        self.status.clone()
    }
    fn start_capture(&self, sink: InputSink) -> Result<(), BackendError> {
        self.request(Command::Capture(sink))
    }
    fn set_mode(&self, mode: CaptureMode) -> Result<(), BackendError> {
        self.request(Command::Mode(mode))
    }
    fn wake_display(&self) -> Result<(), BackendError> {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn SetThreadExecutionState(flags: u32) -> u32;
        }
        // ES_DISPLAY_REQUIRED resets the display idle timer; a zero-distance move marked as Glide's own counts as
        // input and turns a dark screen back on, just like nudging this PC's own mouse.
        // SAFETY: one-shot flag (no ES_CONTINUOUS), no pointers.
        unsafe {
            SetThreadExecutionState(0x0000_0002);
        }
        send(&mouse_input(0, 0, 0, MOUSEEVENTF_MOVE))
    }
    fn set_cursor_visible(&self, visible: bool) -> Result<(), BackendError> {
        crate::cursor::set_visible(visible)
    }
    fn inject(&self, event: InputEvent) -> Result<(), BackendError> {
        self.require_live()?;
        require_injection()?;
        let mut held = self
            .injected
            .lock()
            .map_err(|_| BackendError::StateUnavailable)?;
        match event.kind {
            InputEventKind::Key { key, down } => {
                let input = key_input(key, down)?;
                send(&input)?;
                held.keys[key.0 as usize] = down;
            }
            InputEventKind::Button { button, down } => {
                let (index, flags, data) = button_flags(button, down)?;
                send(&mouse_input(0, 0, data, flags))?;
                held.buttons[index] = down;
            }
            InputEventKind::PointerMoved { position, .. } => {
                if !position.x.is_finite() || !position.y.is_finite() {
                    return Err(BackendError::InvalidInput(
                        "non-finite cursor position".into(),
                    ));
                }
                let monitors = self
                    .monitors
                    .read()
                    .map_err(|_| BackendError::StateUnavailable)?;
                let position = monitors
                    .iter()
                    .filter_map(|m| {
                        glide_platform::clamp_cursor_to_monitors(
                            std::slice::from_ref(&m.monitor),
                            position,
                        )
                    })
                    .min_by(|a, b| {
                        (a.x - position.x)
                            .hypot(a.y - position.y)
                            .total_cmp(&(b.x - position.x).hypot(b.y - position.y))
                    })
                    .ok_or(BackendError::Transient)?;
                let monitor = monitors
                    .iter()
                    .find(|m| contains_logical(&m.monitor, position))
                    .ok_or(BackendError::InvalidPosition)?;
                let (x, y) = native::logical_to_physical(&monitors, &monitor.monitor.id, position)
                    .ok_or(BackendError::Unavailable)?;
                // SAFETY: Read-only system metrics under the caller's DPI-aware context.
                let rect = unsafe {
                    RECT {
                        left: GetSystemMetrics(SM_XVIRTUALSCREEN),
                        top: GetSystemMetrics(SM_YVIRTUALSCREEN),
                        right: GetSystemMetrics(SM_XVIRTUALSCREEN)
                            .saturating_add(GetSystemMetrics(SM_CXVIRTUALSCREEN)),
                        bottom: GetSystemMetrics(SM_YVIRTUALSCREEN)
                            .saturating_add(GetSystemMetrics(SM_CYVIRTUALSCREEN)),
                    }
                };
                let (x, y) =
                    native::normalize_virtual_point(x, y, rect).ok_or(BackendError::Unavailable)?;
                send(&mouse_input(
                    x as i32,
                    y as i32,
                    0,
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                ))?;
            }
            InputEventKind::Wheel { dx, dy, .. } => {
                if !dx.is_finite()
                    || !dy.is_finite()
                    || dx.abs() > i32::MAX as f64
                    || dy.abs() > i32::MAX as f64
                {
                    return Err(BackendError::InvalidInput("invalid wheel delta".into()));
                }
                for (index, delta, flag) in
                    [(0, dx, MOUSEEVENTF_HWHEEL), (1, dy, MOUSEEVENTF_WHEEL)]
                {
                    let total = delta + held.wheel[index];
                    let units = total.trunc() as i32;
                    if units != 0 {
                        send(&mouse_input(0, 0, units as u32, flag))?;
                    }
                    held.wheel[index] = total.fract();
                }
            }
        }
        Ok(())
    }
    fn release_all(&self) -> Result<(), BackendError> {
        let mut held = self
            .injected
            .lock()
            .map_err(|_| BackendError::StateUnavailable)?;
        let mut error = None;
        for hid in 0..256 {
            if held.keys[hid] {
                match key_input(Key(hid as u16), false).and_then(|input| send(&input)) {
                    Ok(()) => held.keys[hid] = false,
                    Err(e) => error = Some(e),
                }
            }
        }
        for (index, button) in [
            Button::Left,
            Button::Right,
            Button::Middle,
            Button::Back,
            Button::Forward,
        ]
        .into_iter()
        .enumerate()
        {
            if held.buttons[index] {
                match button_flags(button, false)
                    .and_then(|(_, flags, data)| send(&mouse_input(0, 0, data, flags)))
                {
                    Ok(()) => held.buttons[index] = false,
                    Err(e) => error = Some(e),
                }
            }
        }
        held.wheel = [0.0; 2];
        held.relative = [0.0; 2];
        error.map_or(Ok(()), Err)
    }
    fn monitors(&self) -> Result<Vec<Monitor>, BackendError> {
        native::enumerate_monitors().map(|ms| ms.into_iter().map(|m| m.monitor).collect())
    }
    fn monitor_changes(&self) -> Receiver<Vec<Monitor>> {
        self.changes.clone()
    }
    fn local_cursor_pos(&self) -> Result<Point, BackendError> {
        logical_cursor(&native::enumerate_monitors()?)
    }
    fn permissions(&self) -> Permissions {
        if !self.alive.load(Ordering::Acquire) {
            Permissions {
                accessibility: PermissionStatus::Denied,
                input_monitoring: PermissionStatus::Denied,
                injection: PermissionStatus::Denied,
            }
        } else {
            native::permissions()
        }
    }
}
impl Drop for WindowsInput {
    fn drop(&mut self) {
        let _ = crate::cursor::restore_system_cursors();
        crate::cursor::console_handler(false);
        self.swallow.store(false, Ordering::Release);
        let _ = self.release_all();
        if self.alive.load(Ordering::Acquire) && self.request(Command::Stop).is_err() {
            // SAFETY: A failed command wake is canceled; a quit message still gives the
            // owning thread a chance to run every RAII cleanup before it exits. Do not
            // target a known terminated thread whose numeric ID could have been reused.
            unsafe {
                PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0);
            }
        }
        if let Ok(thread) = self.thread.get_mut() {
            if let Some(thread) = thread.take() {
                let _ = thread.join();
            }
        }
    }
}
fn send(input: &INPUT) -> Result<(), BackendError> {
    // SAFETY: Clear stale last-error before reading the result of this exact SendInput call.
    unsafe {
        SetLastError(0);
    }
    // SAFETY: SendInput copies a fully initialized INPUT synchronously; no OS pointer escapes.
    if unsafe { SendInput(1, input, size_of::<INPUT>() as i32) } == 1 {
        Ok(())
    } else {
        // SendInput does not identify UIPI in GetLastError; inspect the actual desktop and
        // foreground integrity again after failure. Unknown/last-error alone isn't revocation.
        let last_error = unsafe { GetLastError() };
        native::injection_access().and(Err(BackendError::Failed(format!(
            "SendInput temporarily rejected input (Windows error {last_error})"
        ))))
    }
}
fn require_injection() -> Result<(), BackendError> {
    native::injection_access()
}
fn mouse_input(x: i32, y: i32, data: u32, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: x,
                dy: y,
                mouseData: data,
                dwFlags: flags,
                dwExtraInfo: MAGIC,
                ..Default::default()
            },
        },
    }
}
fn button_flags(button: Button, down: bool) -> Result<(usize, u32, u32), BackendError> {
    Ok(match button {
        Button::Left => (
            0,
            if down {
                MOUSEEVENTF_LEFTDOWN
            } else {
                MOUSEEVENTF_LEFTUP
            },
            0,
        ),
        Button::Right => (
            1,
            if down {
                MOUSEEVENTF_RIGHTDOWN
            } else {
                MOUSEEVENTF_RIGHTUP
            },
            0,
        ),
        Button::Middle => (
            2,
            if down {
                MOUSEEVENTF_MIDDLEDOWN
            } else {
                MOUSEEVENTF_MIDDLEUP
            },
            0,
        ),
        Button::Back => (
            3,
            if down {
                MOUSEEVENTF_XDOWN
            } else {
                MOUSEEVENTF_XUP
            },
            XBUTTON1 as u32,
        ),
        Button::Forward => (
            4,
            if down {
                MOUSEEVENTF_XDOWN
            } else {
                MOUSEEVENTF_XUP
            },
            XBUTTON2 as u32,
        ),
        Button::Other(_) => return Err(BackendError::Unsupported),
    })
}
fn contains_logical(m: &Monitor, p: Point) -> bool {
    p.x >= m.x && p.y >= m.y && p.x < m.x + m.w && p.y < m.y + m.h
}
fn physical_point(monitors: &[NativeMonitor], point: POINT) -> Option<Point> {
    let m = monitors.iter().find(|m| {
        point.x >= m.physical.left
            && point.x < m.physical.right
            && point.y >= m.physical.top
            && point.y < m.physical.bottom
    })?;
    native::physical_to_logical(monitors, &m.monitor.id, point.x, point.y)
}
fn logical_cursor(monitors: &[NativeMonitor]) -> Result<Point, BackendError> {
    let mut point = POINT::default();
    // SAFETY: GetCursorPos writes stack POINT storage.
    if unsafe { GetCursorPos(&mut point) } == 0 {
        return Err(BackendError::Unavailable);
    }
    physical_point(monitors, point).ok_or(BackendError::Unavailable)
}

fn capture_thread(
    requests: Receiver<Request>,
    changes: Sender<Vec<Monitor>>,
    status: Sender<CaptureStatus>,
    swallow: Arc<AtomicBool>,
    snapshot: Arc<RwLock<Vec<NativeMonitor>>>,
    fault: Arc<AtomicBool>,
    ready: Sender<Result<u32, BackendError>>,
) -> Result<(), BackendError> {
    let _cursor_restore = crate::cursor::RestoreGuard;
    // SAFETY: Establish this thread's message queue before publishing its ID.
    let id = unsafe {
        let mut message = MSG::default();
        PeekMessageW(&mut message, null_mut(), 0, 0, PM_NOREMOVE);
        GetCurrentThreadId()
    };
    let monitors = snapshot
        .read()
        .map_err(|_| BackendError::StateUnavailable)?
        .clone();
    let (sample_tx, samples) = bounded(4096);
    let context = HookContext {
        samples: sample_tx,
        sink: RefCell::new(None),
        status,
        swallow: swallow.clone(),
        fault: fault.clone(),
        wake: AtomicBool::new(false),
        thread: id,
    };
    HOOK_CONTEXT.with(|slot| slot.set(&context));
    struct ClearContext;
    impl Drop for ClearContext {
        fn drop(&mut self) {
            HOOK_CONTEXT.with(|slot| slot.set(null()));
        }
    }
    let _clear = ClearContext;
    let class: Vec<u16> = format!("GlideInput{id}")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    // SAFETY: Instance handle is borrowed from this process, valid throughout window lifetime.
    let instance = unsafe { GetModuleHandleW(null()) };
    let wc = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        lpszClassName: class.as_ptr(),
        ..Default::default()
    };
    // SAFETY: Class and window strings live until window registration is released.
    let handle = unsafe {
        if RegisterClassW(&wc) == 0 {
            return Err(BackendError::Unavailable);
        }
        CreateWindowExW(
            0,
            class.as_ptr(),
            class.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            null_mut(),
            instance,
            null(),
        )
    };
    if handle.is_null() {
        // SAFETY: Failed creation; release the class we registered.
        unsafe {
            UnregisterClassW(class.as_ptr(), instance);
        }
        return Err(BackendError::Unavailable);
    }
    let mut raw_window = Window {
        handle,
        class,
        instance,
        raw: false,
    };
    let raw = RAWINPUTDEVICE {
        usUsagePage: 1,
        usUsage: 2,
        dwFlags: RIDEV_INPUTSINK,
        hwndTarget: handle,
    };
    // SAFETY: Register the single owned mouse sink on our message-only window.
    if unsafe { RegisterRawInputDevices(&raw, 1, size_of::<RAWINPUTDEVICE>() as u32) } == 0 {
        return Err(BackendError::Unavailable);
    }
    raw_window.raw = true;
    let display_class: Vec<u16> = format!("GlideDisplay{id}")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let wc = WNDCLASSW {
        lpszClassName: display_class.as_ptr(),
        ..wc
    };
    // SAFETY: Invisible top-level window receives broadcasts that message-only windows cannot.
    let display_handle = unsafe {
        if RegisterClassW(&wc) == 0 {
            return Err(BackendError::Unavailable);
        }
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            display_class.as_ptr(),
            display_class.as_ptr(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            instance,
            null(),
        )
    };
    if display_handle.is_null() {
        // SAFETY: Failed window creation releases its owned class.
        unsafe {
            UnregisterClassW(display_class.as_ptr(), instance);
        }
        return Err(BackendError::Unavailable);
    }
    let _display_window = Window {
        handle: display_handle,
        class: display_class,
        instance,
        raw: false,
    };
    let mut _hooks = install_hooks(instance)?;
    let mut cursor = CursorGuard::new();
    let mut sink: Option<InputSink> = None;
    let mut monitors = monitors;
    let mut previous = logical_cursor(&monitors).unwrap_or(Point { x: 0.0, y: 0.0 });
    let mut delta = [0_i64; 2];
    let mut timer = RawTimer::new()?;
    let _ = ready.send(Ok(id));
    loop {
        let mut message = MSG::default();
        // SAFETY: Wait indefinitely for messages or the owned one-shot movement timer.
        // No timer runs while idle; unlike WM_TIMER this avoids its 10ms minimum.
        let waited = unsafe {
            MsgWaitForMultipleObjectsEx(
                1,
                &timer.handle,
                INFINITE,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            )
        };
        if waited == WAIT_FAILED {
            context.fail();
            break;
        }
        // Callback overflow can fail its own wake post. Every subsequent loop turn
        // still restores the physical cursor as soon as the atomic mode becomes Local.
        if !swallow.load(Ordering::Acquire) {
            cursor.restore();
        }
        if waited == WAIT_OBJECT_0 {
            timer.armed = false;
            emit_raw(
                &mut delta,
                &sink,
                &context,
                &mut cursor,
                &monitors,
                native::device_scale(&monitors),
            );
            continue;
        }
        // SAFETY: Remove a pending message; PeekMessage also dispatches pending sent hooks.
        if unsafe { PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) } == 0 {
            continue;
        }
        if message.message == WM_QUIT {
            break;
        }
        if message.message == COMMAND {
            while let Ok(request) = requests.try_recv() {
                if request
                    .status
                    .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    let _ = request.reply.send(Err(BackendError::Unavailable));
                    continue;
                }
                let result = match request.command {
                    Command::Capture(new) => {
                        // A new sink begins a local capture generation. Never replay samples
                        // queued for its predecessor after a fail-closed restart.
                        swallow.store(false, Ordering::Release);
                        cursor.restore();
                        timer.disarm();
                        delta = [0; 2];
                        while samples.try_recv().is_ok() {}
                        context.wake.store(false, Ordering::Release);
                        if let Ok(position) = logical_cursor(&monitors) {
                            previous = position;
                        }
                        *context.sink.borrow_mut() = Some(new.clone());
                        sink = Some(new);
                        fault.store(false, Ordering::Release);
                        Ok(())
                    }
                    Command::Mode(mode) => {
                        if matches!(mode, CaptureMode::Swallow { .. }) && sink.is_none() {
                            Err(BackendError::Unavailable)
                        } else {
                            if fault.swap(false, Ordering::AcqRel) {
                                while samples.try_recv().is_ok() {}
                                delta = [0; 2];
                                timer.disarm();
                                context.wake.store(false, Ordering::Release);
                            }
                            let pin = matches!(mode, CaptureMode::Swallow { lock_pos: true });
                            let result = if pin {
                                cursor.pin()
                            } else {
                                cursor.unpin();
                                Ok(())
                            };
                            if result.is_ok() {
                                // Windows calls the newest low-level hook first. An app that hooked the keyboard
                                // after Glide (RustDesk while its window has focus) would swallow keys meant for the
                                // other computer, so put Glide back in front whenever control leaves. The old hooks
                                // are removed only after the new ones exist, and no hook runs during this handler.
                                if mode != CaptureMode::Local && !swallow.load(Ordering::Acquire) {
                                    if let Ok(fresh) = install_hooks(instance) {
                                        _hooks = fresh;
                                    }
                                }
                                timer.disarm();
                                delta = [0; 2];
                                swallow.store(mode != CaptureMode::Local, Ordering::Release);
                            }
                            result
                        }
                    }
                    Command::Stop => {
                        swallow.store(false, Ordering::Release);
                        cursor.restore();
                        let _ = request.reply.send(Ok(()));
                        return Ok(());
                    }
                };
                let _ = request.reply.send(result);
            }
        } else if message.message == DRAIN {
            context.wake.store(false, Ordering::Release);
            if !swallow.load(Ordering::Acquire) {
                cursor.restore();
            }
            while let Ok(sample) = samples.try_recv() {
                if let Sample::Raw(x, y) = sample {
                    delta[0] += x as i64;
                    delta[1] += y as i64;
                    continue;
                }
                timer.disarm();
                emit_raw(
                    &mut delta,
                    &sink,
                    &context,
                    &mut cursor,
                    &monitors,
                    native::device_scale(&monitors),
                );
                // A key Glide cannot name is dropped, not a reason to give up the whole keyboard and send the
                // cursor home; an unreadable mouse event still is.
                let is_key = matches!(sample, Sample::Key(_));
                let event = match sample {
                    Sample::Key(data) => translate_key(data),
                    Sample::Mouse(message, data) => {
                        translate_mouse(message, data, &monitors, &mut previous)
                    }
                    Sample::Raw(..) => None,
                };
                if let Some(event) = event {
                    deliver(event, &sink, &context, &mut cursor);
                } else if swallow.load(Ordering::Acquire) && !is_key {
                    context.fail();
                    cursor.restore();
                }
            }
            if delta != [0; 2] && timer.arm().is_err() {
                context.fail();
                cursor.restore();
                delta = [0; 2];
            }
        } else if message.message == WM_DISPLAYCHANGE && message.hwnd.is_null() {
            if let Ok(new) = native::enumerate_monitors() {
                let _ = changes.try_send(new.iter().map(|m| m.monitor.clone()).collect());
                if let Ok(mut cache) = snapshot.write() {
                    *cache = new.clone();
                }
                monitors = new;
            } else {
                context.fail();
            }
            swallow.store(false, Ordering::Release);
            cursor.restore();
            timer.disarm();
            delta = [0; 2];
            previous = logical_cursor(&monitors).unwrap_or(Point { x: 0.0, y: 0.0 });
        } else {
            // SAFETY: Dispatch valid Windows message to our window callbacks.
            unsafe {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
    swallow.store(false, Ordering::Release);
    Ok(())
}

fn deliver(
    event: InputEvent,
    sink: &Option<InputSink>,
    context: &HookContext,
    cursor: &mut CursorGuard,
) {
    if context.fault.load(Ordering::Acquire) {
        return;
    }
    if let Some(sink) = sink {
        if sink.try_push(event).is_err() {
            context.fail_as(CaptureStatus::QueueOverflow);
            cursor.restore();
        }
    }
}
fn emit_raw(
    delta: &mut [i64; 2],
    sink: &Option<InputSink>,
    context: &HookContext,
    cursor: &mut CursorGuard,
    monitors: &[NativeMonitor],
    scale: f64,
) {
    let movement = std::mem::take(delta);
    if movement == [0; 2] {
        return;
    }
    let position = match logical_cursor(monitors) {
        Ok(position) => position,
        Err(_) => {
            context.fail();
            cursor.restore();
            return;
        }
    };
    // Local LL positions already account for OS movement. Only add raw motion where the
    // OS has clipped it at an outer edge; otherwise we'd count ordinary movement twice.
    let mut delta = Point {
        x: movement[0] as f64 / scale,
        y: movement[1] as f64 / scale,
    };
    if !context.swallow.load(Ordering::Acquire) {
        delta = native::outer_edge_delta(monitors, position, delta);
        if delta.x == 0.0 && delta.y == 0.0 {
            return;
        }
    }
    // Raw motion uses the device-wide primary scale, independent of the current monitor.
    deliver(
        InputEvent {
            kind: InputEventKind::PointerMoved {
                position,
                delta_x: delta.x,
                delta_y: delta.y,
            },
            injected: false,
        },
        sink,
        context,
        cursor,
    );
}

/// True for input Glide must not capture: its own injections, and cursor movement injected by other software.
/// Buttons and wheels injected by other software are real user input: mouse utilities such as Logitech Options+ turn the
/// side buttons and gestures into injected events, and ignoring those left Back/Forward dead on the other computer.
fn mouse_injected(message: u32, data: &MSLLHOOKSTRUCT) -> bool {
    let ours = data.dwExtraInfo == MAGIC;
    let foreign = data.flags & (LLMHF_INJECTED | LLMHF_LOWER_IL_INJECTED) != 0;
    ours || (foreign && message == WM_MOUSEMOVE)
}

fn translate_mouse(
    message: u32,
    data: MSLLHOOKSTRUCT,
    monitors: &[NativeMonitor],
    previous: &mut Point,
) -> Option<InputEvent> {
    let injected = mouse_injected(message, &data);
    let kind = match message {
        WM_MOUSEMOVE => {
            let position = physical_point(monitors, data.pt)?;
            let kind = InputEventKind::PointerMoved {
                position,
                delta_x: position.x - previous.x,
                delta_y: position.y - previous.y,
            };
            *previous = position;
            kind
        }
        WM_LBUTTONDOWN | WM_LBUTTONUP => InputEventKind::Button {
            button: Button::Left,
            down: message == WM_LBUTTONDOWN,
        },
        WM_RBUTTONDOWN | WM_RBUTTONUP => InputEventKind::Button {
            button: Button::Right,
            down: message == WM_RBUTTONDOWN,
        },
        WM_MBUTTONDOWN | WM_MBUTTONUP => InputEventKind::Button {
            button: Button::Middle,
            down: message == WM_MBUTTONDOWN,
        },
        WM_XBUTTONDOWN | WM_XBUTTONUP => InputEventKind::Button {
            button: match data.mouseData >> 16 {
                1 => Button::Back,
                2 => Button::Forward,
                _ => return None,
            },
            down: message == WM_XBUTTONDOWN,
        },
        WM_MOUSEWHEEL => InputEventKind::Wheel {
            dx: 0.0,
            dy: (data.mouseData >> 16) as i16 as f64,
            precise: ((data.mouseData >> 16) as i16) % 120 != 0,
        },
        WM_MOUSEHWHEEL => InputEventKind::Wheel {
            dx: (data.mouseData >> 16) as i16 as f64,
            dy: 0.0,
            precise: ((data.mouseData >> 16) as i16) % 120 != 0,
        },
        _ => return None,
    };
    Some(InputEvent { kind, injected })
}

#[cfg(test)]
mod tests {
    use super::*;
    // Bug: Ctrl held on this PC while the cursor crossed to the Mac stayed stuck here, because its release was swallowed.
    #[test]
    fn a_modifier_held_across_the_crossing_gets_its_release_but_other_swallowed_keys_do_not() {
        let ctrl = u32::from(VK_LCONTROL);
        let a = u32::from(b'A');
        assert!(!block_key(ctrl, false, false, false));
        assert!(block_key(a, false, false, true));
        assert!(block_key(a, true, false, true));
        assert!(!block_key(ctrl, true, false, true));
        assert!(block_key(ctrl, true, false, true));
        let win = u32::from(VK_LWIN);
        assert!(!block_key(win, false, false, false));
        assert!(block_key(win, true, false, true));
        assert!(!block_key(ctrl, true, true, true));
    }
    // Bug: with a Logitech MX Master, Back and Forward did nothing on the other computer, because Logitech Options+
    // delivers them as injected events and Glide ignored every injected mouse event.
    #[test]
    fn side_buttons_injected_by_mouse_software_are_captured_but_glides_own_and_foreign_motion_are_not(
    ) {
        let mut point = Point { x: 0.0, y: 0.0 };
        let by_logitech = MSLLHOOKSTRUCT {
            mouseData: 1 << 16,
            flags: LLMHF_INJECTED,
            ..Default::default()
        };
        for message in [WM_XBUTTONDOWN, WM_XBUTTONUP, WM_MBUTTONDOWN, WM_MOUSEWHEEL] {
            let event = translate_mouse(message, by_logitech, &[], &mut point).expect("event");
            assert!(
                !event.injected,
                "message {message:#x} from mouse software must be forwarded"
            );
        }
        let ours = MSLLHOOKSTRUCT {
            mouseData: 1 << 16,
            dwExtraInfo: MAGIC,
            ..Default::default()
        };
        for message in [WM_XBUTTONDOWN, WM_LBUTTONDOWN, WM_MOUSEWHEEL] {
            let event = translate_mouse(message, ours, &[], &mut point).expect("event");
            assert!(
                event.injected,
                "Glide's own {message:#x} must never be captured again"
            );
        }
        assert!(
            mouse_injected(WM_MOUSEMOVE, &by_logitech),
            "cursor movement from other software stays ignored"
        );
        assert!(!mouse_injected(WM_MOUSEMOVE, &MSLLHOOKSTRUCT::default()));
    }

    #[test]
    fn synthetic_mouse_structs_preserve_buttons_wheel_and_injection() {
        let mut point = Point { x: 0.0, y: 0.0 };
        for (message, button, down) in [
            (WM_LBUTTONDOWN, Button::Left, true),
            (WM_RBUTTONUP, Button::Right, false),
            (WM_MBUTTONDOWN, Button::Middle, true),
            (WM_XBUTTONUP, Button::Forward, false),
        ] {
            assert_eq!(
                translate_mouse(
                    message,
                    MSLLHOOKSTRUCT {
                        mouseData: 2 << 16,
                        flags: LLMHF_INJECTED,
                        ..Default::default()
                    },
                    &[],
                    &mut point
                ),
                Some(InputEvent {
                    kind: InputEventKind::Button { button, down },
                    injected: false
                })
            );
        }
        assert_eq!(
            translate_mouse(
                WM_MOUSEHWHEEL,
                MSLLHOOKSTRUCT {
                    mouseData: ((-15_i16 as u16) as u32) << 16,
                    ..Default::default()
                },
                &[],
                &mut point
            ),
            Some(InputEvent {
                kind: InputEventKind::Wheel {
                    dx: -15.0,
                    dy: 0.0,
                    precise: true
                },
                injected: false
            })
        );
    }
    #[test]
    fn raw_mouse_preserves_high_resolution_counts_and_rejects_synthetic_sources() {
        let mut sample = RAWINPUT {
            header: RAWINPUTHEADER {
                dwType: RIM_TYPEMOUSE,
                hDevice: std::ptr::dangling_mut::<u8>().cast(),
                ..Default::default()
            },
            data: RAWINPUT_0 {
                mouse: RAWMOUSE {
                    lLastX: 4097,
                    lLastY: -8193,
                    ..Default::default()
                },
            },
        };
        assert_eq!(raw_delta(&sample), Some((4097, -8193)));
        sample.data = RAWINPUT_0 {
            mouse: RAWMOUSE {
                ulExtraInformation: MAGIC as u32,
                ..Default::default()
            },
        };
        assert_eq!(raw_delta(&sample), None);
        sample.data = RAWINPUT_0 {
            mouse: RAWMOUSE {
                usFlags: MOUSE_MOVE_ABSOLUTE,
                ..Default::default()
            },
        };
        assert_eq!(raw_delta(&sample), None);
        assert!(unsupported_physical_raw(&sample));
        sample.header.hDevice = null_mut();
        assert_eq!(raw_delta(&sample), None);
        assert!(!unsupported_physical_raw(&sample));
        sample.header.dwType = RIM_TYPEKEYBOARD;
        assert_eq!(raw_delta(&sample), None);
    }
    #[test]
    fn bounded_hook_overflow_returns_to_local() {
        let (tx, _rx) = bounded(1);
        let (sink, events) = InputSink::bounded(1).expect("sink");
        let (status, statuses) = bounded(16);
        let swallow = Arc::new(AtomicBool::new(true));
        let context = HookContext {
            samples: tx,
            sink: RefCell::new(Some(sink.clone())),
            status,
            swallow: swallow.clone(),
            fault: Arc::new(AtomicBool::new(false)),
            wake: AtomicBool::new(true),
            thread: 0,
        };
        assert!(context.push(Sample::Raw(1, 2)));
        assert!(!context.push(Sample::Raw(1, 2)));
        assert!(!swallow.load(Ordering::Acquire));
        assert!(context.fault.load(Ordering::Acquire));
        assert!(sink.take_overflow());
        assert!(events.is_empty());
        assert_eq!(
            statuses.try_recv().expect("status"),
            CaptureStatus::QueueOverflow
        );
    }
    #[test]
    fn daemon_sink_overflow_returns_to_local_and_latches_escape() {
        let (sink, _events) = match InputSink::bounded(1) {
            Ok(queue) => queue,
            Err(_) => panic!("positive capacity"),
        };
        let (tx, _samples) = bounded(1);
        let context = HookContext {
            samples: tx,
            sink: RefCell::new(None),
            status: bounded(16).0,
            swallow: Arc::new(AtomicBool::new(true)),
            fault: Arc::new(AtomicBool::new(false)),
            wake: AtomicBool::new(false),
            thread: 0,
        };
        let event = InputEvent {
            kind: InputEventKind::Key {
                key: Key(4),
                down: true,
            },
            injected: false,
        };
        let mut cursor = CursorGuard::new();
        deliver(event, &Some(sink.clone()), &context, &mut cursor);
        deliver(event, &Some(sink.clone()), &context, &mut cursor);
        assert!(!context.swallow.load(Ordering::Acquire));
        assert!(sink.take_overflow());
        assert!(context.fault.load(Ordering::Acquire));
        // Faulted capture must not resume delivering merely because a queue becomes free.
        let (fresh, events) = match InputSink::bounded(1) {
            Ok(queue) => queue,
            Err(_) => panic!("positive capacity"),
        };
        deliver(event, &Some(fresh), &context, &mut cursor);
        assert!(events.try_recv().is_err());
    }
    #[test]
    #[ignore = "moves the cursor one pixel and restores it; requires an interactive input desktop"]
    fn injected_mouse_loopback_is_synthetic_and_reversed() -> Result<(), BackendError> {
        let input = WindowsInput::new()?;
        let (sink, events) = InputSink::bounded(64).map_err(|_| BackendError::Unavailable)?;
        input.start_capture(sink)?;
        let mut original = POINT::default();
        // SAFETY: Stack point is valid; restoration guard owns the cursor nudge.
        if unsafe { GetCursorPos(&mut original) } == 0 {
            return Err(BackendError::Unavailable);
        }
        struct Restore(POINT);
        impl Drop for Restore {
            fn drop(&mut self) {
                // SAFETY: Original cursor location from GetCursorPos.
                unsafe {
                    SetCursorPos(self.0.x, self.0.y);
                }
            }
        }
        let _restore = Restore(original);
        input.inject_relative(1.0, 0.0)?;
        let event = events
            .recv_timeout(std::time::Duration::from_secs(2))
            .map_err(|_| BackendError::Unavailable)?;
        assert!(event.injected);
        assert!(matches!(event.kind, InputEventKind::PointerMoved { .. }));
        input.inject_relative(-1.0, 0.0)?;
        input.release_all()?;
        Ok(())
    }
}
