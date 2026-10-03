use crate::input_logic::*;
use core_foundation::{
    base::{CFRelease, TCFType},
    boolean::CFBoolean,
    runloop::*,
    string::CFString,
};
use core_graphics::geometry::{CGPoint, CGRect};
use crossbeam_channel::{bounded, Receiver, Sender};
use glide_platform::{
    BackendError, CaptureMode, CaptureStatus, InputBackend, InputEvent, InputEventKind, InputSink,
    Key, Monitor, PermissionStatus, Permissions, Point,
};
use std::{
    ffi::c_void,
    ptr::{self, NonNull},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::Instant,
};

const MAGIC: i64 = 0x474c4944454d4143;
const MAX_DISPLAYS: usize = 64;
const EVENT_MASK: u64 = (1 << 1)
    | (1 << 2)
    | (1 << 3)
    | (1 << 4)
    | (1 << 5)
    | (1 << 6)
    | (1 << 7)
    | (1 << 10)
    | (1 << 11)
    | (1 << 12)
    | (1 << 22)
    | (1 << 25)
    | (1 << 26)
    | (1 << 27);

type Handle = *mut c_void;
type TapCallback = extern "C" fn(Handle, u32, Handle, Handle) -> Handle;

// Signatures follow CGEvent.h/CGEventSource.h/CGDisplayConfiguration.h. Raw event APIs
// avoid the dependency's owning event-tap callback adapter (it allocates per event).
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapCreate(
        location: u32,
        placement: u32,
        options: u32,
        mask: u64,
        callback: TapCallback,
        info: Handle,
    ) -> Handle;
    fn CGEventTapEnable(tap: Handle, enabled: bool);
    fn CGEventSourceCreate(state: i32) -> Handle;
    fn CGEventSourceSetUserData(source: Handle, data: i64);
    fn CGEventSourceSetLocalEventsSuppressionInterval(source: Handle, seconds: f64);
    fn CGEventSourceKeyState(state: i32, code: u16) -> bool;
    fn CGEventSourceFlagsState(state: i32) -> u64;
    fn CGEventCreate(source: Handle) -> Handle;
    fn CGEventCreateKeyboardEvent(source: Handle, code: u16, down: bool) -> Handle;
    fn CGEventCreateMouseEvent(source: Handle, kind: u32, pos: CGPoint, button: u32) -> Handle;
    fn CGEventCreateScrollWheelEvent(source: Handle, units: u32, count: u32, ...) -> Handle;
    fn CGEventGetLocation(event: Handle) -> CGPoint;
    fn CGEventGetFlags(event: Handle) -> u64;
    fn CGEventGetIntegerValueField(event: Handle, field: u32) -> i64;
    fn CGEventGetDoubleValueField(event: Handle, field: u32) -> f64;
    fn CGEventSetIntegerValueField(event: Handle, field: u32, value: i64);
    fn CGEventSetDoubleValueField(event: Handle, field: u32, value: f64);
    fn CGEventSetFlags(event: Handle, flags: u64);
    fn CGEventSetType(event: Handle, kind: u32);
    fn CGEventSetLocation(event: Handle, position: CGPoint);
    fn CGEventPost(location: u32, event: Handle);
    fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGMainDisplayID() -> u32;
    fn CGDisplayCopyDisplayMode(display: u32) -> Handle;
    fn CGDisplayModeGetWidth(mode: Handle) -> usize;
    fn CGDisplayModeGetPixelWidth(mode: Handle) -> usize;
    fn CGDisplayRegisterReconfigurationCallback(
        callback: extern "C" fn(u32, u32, Handle),
        info: Handle,
    ) -> i32;
    fn CGDisplayRemoveReconfigurationCallback(
        callback: extern "C" fn(u32, u32, Handle),
        info: Handle,
    ) -> i32;
    fn CGAssociateMouseAndMouseCursorPosition(connected: bool) -> i32;
    fn CGDisplayHideCursor(display: u32) -> i32;
    fn CGDisplayShowCursor(display: u32) -> i32;
    fn CGPreflightListenEventAccess() -> bool;
    fn CGRequestListenEventAccess() -> bool;
    fn CGPreflightPostEventAccess() -> bool;
    fn CGRequestPostEventAccess() -> bool;
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> u8; // AX Boolean.
    static kAXTrustedCheckOptionPrompt: core_foundation::string::CFStringRef;
}
#[link(name = "Carbon", kind = "framework")]
extern "C" {
    fn IsSecureEventInputEnabled() -> u8; // Carbon Boolean, not C99 bool.
}
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFMachPortCreateRunLoopSource(
        allocator: Handle,
        port: Handle,
        order: isize,
    ) -> CFRunLoopSourceRef;
    fn CFMachPortInvalidate(port: Handle);
}

/// Owns one Create/Copy-rule CF object. Used only on the input thread.
struct OwnedCf(NonNull<c_void>);
impl OwnedCf {
    fn new(raw: Handle) -> Result<Self, BackendError> {
        NonNull::new(raw).map(Self).ok_or(BackendError::Unavailable)
    }
    fn raw(&self) -> Handle {
        self.0.as_ptr()
    }
}
impl Drop for OwnedCf {
    fn drop(&mut self) {
        // SAFETY: this guard exclusively owns a non-null Create/Copy-rule CF reference.
        unsafe {
            CFRelease(self.raw());
        }
    }
}

struct Wake {
    source: NonNull<c_void>,
    runloop: core_foundation::runloop::CFRunLoop,
}
// SAFETY: only CFRunLoopSourceSignal/CFRunLoopWakeUp (documented thread-safe) and
// CFRelease are invoked across threads; the retained source never exposes its context.
unsafe impl Send for Wake {}
// SAFETY: same restriction as Send; the source/runloop remain retained until last Arc drop.
unsafe impl Sync for Wake {}
impl Wake {
    fn signal(&self) {
        use core_foundation::base::TCFType;
        // SAFETY: retained, live run-loop objects; these two operations are thread-safe.
        unsafe {
            CFRunLoopSourceSignal(self.source.as_ptr().cast());
            CFRunLoopWakeUp(self.runloop.as_concrete_TypeRef());
        }
    }
}
impl Drop for Wake {
    fn drop(&mut self) {
        // SAFETY: Wake owns the source's original Create-rule reference.
        unsafe {
            CFRelease(self.source.as_ptr());
        }
    }
}

/// The newest pending cursor move. Moves never queue behind each other: when the Mac is busy the input thread jumps
/// straight to the latest position and skips the ones in between, so load can make the cursor coarser but never late.
struct MoveSlot {
    latest: Mutex<Option<InputEvent>>,
    queued: AtomicBool,
}

extern "C" {
    fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
}
#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOPMAssertionDeclareUserActivity(name: *const c_void, user_type: u32, id: *mut u32) -> i32;
}
static USER_ACTIVITY_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Tells macOS the user is active here (kIOPMUserActiveLocal), which turns a dark display back on, exactly like a
/// touch on this Mac's own trackpad. It does not unlock a locked screen.
fn declare_user_activity() {
    let name = CFString::from_static_string("Glide: the cursor arrived from another computer");
    let mut id = USER_ACTIVITY_ID.load(Ordering::Relaxed);
    // SAFETY: a live CFString for the duration of the call and a writable assertion id; 0 = kIOPMUserActiveLocal.
    let result =
        unsafe { IOPMAssertionDeclareUserActivity(name.as_concrete_TypeRef().cast(), 0, &mut id) };
    if result == 0 {
        USER_ACTIVITY_ID.store(id, Ordering::Relaxed);
    }
}

/// QOS_CLASS_USER_INTERACTIVE: scheduled ahead of background work and on the fast cores.
const QOS_USER_INTERACTIVE: u32 = 0x21;

/// Ask macOS to schedule the calling thread as part of the live user interface. Best effort.
pub fn prioritize_current_thread() {
    // SAFETY: only adjusts the scheduling class of the calling thread; the result is advisory and ignored.
    unsafe {
        pthread_set_qos_class_self_np(QOS_USER_INTERACTIVE, 0);
    }
}

enum Command {
    Move,
    Capture(InputSink),
    Mode(CaptureMode),
    Visibility(bool),
    Inject(InputEvent),
    Release,
    Monitors,
    Cursor,
    Stop,
}
enum Reply {
    Unit,
    Monitors(Vec<Monitor>),
    Cursor(Point),
}

/// Why capture stopped swallowing. Consumers must return home and release remote input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum CaptureFallback {
    QueueOverflow,
    UnsupportedEvent,
    TapDisabled,
    PermissionLost,
    TopologyChanged,
    SecureInput,
}

/// CGEventTap capture and private, tagged CGEvent injection, marshalled to one CFRunLoop.
pub struct MacInput {
    permissions_requested: AtomicBool,
    commands: Sender<Command>,
    replies: Receiver<Result<Reply, BackendError>>,
    serial: Mutex<()>,
    wake: Arc<Wake>,
    moves: Arc<MoveSlot>,
    changes: Receiver<Vec<Monitor>>,
    status: Receiver<CaptureStatus>,
    thread: Option<JoinHandle<()>>,
}

impl MacInput {
    /// Creates the owning run loop without prompting or installing a capture tap.
    pub fn new() -> Result<Self, BackendError> {
        let (commands, commands_rx) = bounded(16);
        let (replies_tx, replies) = bounded(1);
        let (changes_tx, changes) = bounded(8);
        let (status_tx, status) = bounded(16);
        let (ready_tx, ready_rx) = bounded(1);
        let secure = Arc::new(AtomicBool::new(secure_input()));
        let secure_thread = secure.clone();
        let moves = Arc::new(MoveSlot {
            latest: Mutex::new(None),
            queued: AtomicBool::new(false),
        });
        let moves_thread = moves.clone();
        let thread = thread::Builder::new()
            .name("glide-mac-input".into())
            .spawn(move || {
                let result = run(
                    commands_rx,
                    replies_tx,
                    changes_tx,
                    status_tx,
                    secure_thread,
                    moves_thread,
                    &ready_tx,
                );
                if let Err(error) = result {
                    let _ = ready_tx.try_send(Err(error));
                }
            })
            .map_err(|_| BackendError::Unavailable)?;
        match ready_rx.recv() {
            Ok(Ok(wake)) => Ok(Self {
                permissions_requested: AtomicBool::new(false),
                commands,
                replies,
                serial: Mutex::new(()),
                wake,
                moves,
                changes,
                status,
                thread: Some(thread),
            }),
            result => {
                let _ = thread.join();
                match result {
                    Ok(Err(error)) => Err(error),
                    _ => Err(BackendError::Unavailable),
                }
            }
        }
    }

    fn call(&self, command: Command) -> Result<Reply, BackendError> {
        let _serial = self
            .serial
            .lock()
            .map_err(|_| BackendError::StateUnavailable)?;
        self.commands
            .send(command)
            .map_err(|_| BackendError::Unavailable)?;
        self.wake.signal();
        // No timeout: a success means applied, and an error cannot hide a pending command.
        self.replies.recv().map_err(|_| BackendError::Unavailable)?
    }
    /// Store the newest cursor position and make sure exactly one `Move` command is waiting. Never waits for the input
    /// thread: under load, older positions are overwritten instead of building a backlog.
    fn inject_move(&self, event: InputEvent) -> Result<(), BackendError> {
        *self
            .moves
            .latest
            .lock()
            .map_err(|_| BackendError::StateUnavailable)? = Some(event);
        if !self.moves.queued.swap(true, Ordering::AcqRel) {
            if self.commands.try_send(Command::Move).is_err() {
                self.moves.queued.store(false, Ordering::Release);
                return Err(BackendError::Transient);
            }
            self.wake.signal();
        }
        Ok(())
    }

    fn unit(&self, command: Command) -> Result<(), BackendError> {
        match self.call(command)? {
            Reply::Unit => Ok(()),
            _ => Err(BackendError::StateUnavailable),
        }
    }

    /// Explicit setup action; constructors and capture never trigger a permission prompt.
    pub fn request_permissions(&self) -> Permissions {
        // TCC owns dialog lifetime. Do not recreate options or request prompts
        // again in this process, even after denial or revocation.
        if self.permissions_requested.swap(true, Ordering::AcqRel) {
            return self.permissions();
        }
        use core_foundation::{
            base::TCFType, boolean::CFBoolean, dictionary::CFDictionary, string::CFString,
        };
        // SAFETY: Apple's exported constant is a process-lifetime CFString; retain by Get rule.
        let key = unsafe { CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt) };
        let options = CFDictionary::from_CFType_pairs(&[(key, CFBoolean::true_value())]);
        // SAFETY: the retained dictionary lives through AX's synchronous query; TCC owns prompting.
        unsafe {
            AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef().cast());
            CGRequestListenEventAccess();
            CGRequestPostEventAccess();
        }
        self.permissions()
    }
}

impl InputBackend for MacInput {
    fn request_permissions(&self) -> Permissions {
        MacInput::request_permissions(self)
    }
    fn secure_input_enabled(&self) -> Option<bool> {
        Some(secure_input())
    }
    fn capture_status_changes(&self) -> Receiver<CaptureStatus> {
        self.status.clone()
    }
    fn start_capture(&self, sink: InputSink) -> Result<(), BackendError> {
        self.unit(Command::Capture(sink))
    }
    fn set_mode(&self, mode: CaptureMode) -> Result<(), BackendError> {
        self.unit(Command::Mode(mode))
    }
    fn set_cursor_visible(&self, visible: bool) -> Result<(), BackendError> {
        self.unit(Command::Visibility(visible))
    }
    fn wake_display(&self) -> Result<(), BackendError> {
        declare_user_activity();
        Ok(())
    }
    fn inject(&self, event: InputEvent) -> Result<(), BackendError> {
        if let InputEventKind::PointerMoved {
            position,
            delta_x,
            delta_y,
        } = event.kind
        {
            if !position.x.is_finite()
                || !position.y.is_finite()
                || !finite_deltas(delta_x, delta_y)
            {
                return Err(BackendError::InvalidInput(
                    "invalid pointer position".into(),
                ));
            }
            return self.inject_move(event);
        }
        self.unit(Command::Inject(event))
    }
    fn release_all(&self) -> Result<(), BackendError> {
        self.unit(Command::Release)
    }
    fn monitors(&self) -> Result<Vec<Monitor>, BackendError> {
        match self.call(Command::Monitors)? {
            Reply::Monitors(m) => Ok(m),
            _ => Err(BackendError::StateUnavailable),
        }
    }
    fn monitor_changes(&self) -> Receiver<Vec<Monitor>> {
        self.changes.clone()
    }
    fn local_cursor_pos(&self) -> Result<Point, BackendError> {
        match self.call(Command::Cursor)? {
            Reply::Cursor(p) => Ok(p),
            _ => Err(BackendError::StateUnavailable),
        }
    }
    fn permissions(&self) -> Permissions {
        permissions()
    }
}
impl Drop for MacInput {
    fn drop(&mut self) {
        // Stop restores capture and attempts every held release before the run loop exits.
        // Unique &mut ownership permits bypassing a poisoned serialization mutex at shutdown.
        if self.commands.send(Command::Stop).is_ok() {
            self.wake.signal();
            let _ = self.replies.recv();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn secure_input() -> bool {
    // SAFETY: Carbon's process-independent query takes no pointers or owned resources.
    unsafe { IsSecureEventInputEnabled() != 0 }
}
fn permissions() -> Permissions {
    // SAFETY: preflight APIs do not prompt; null AX options means no prompt.
    unsafe {
        permission_statuses(
            AXIsProcessTrustedWithOptions(ptr::null()) != 0,
            CGPreflightListenEventAccess(),
            CGPreflightPostEventAccess(),
        )
    }
}

// Resolve optional private Quartz symbols at runtime: absent symbols never break a link.
#[link(name = "System")]
unsafe extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const std::ffi::c_char) -> *mut c_void;
}
fn enable_background_cursor() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        // SAFETY: Darwin RTLD_DEFAULT (-2); static NUL-terminated symbol names.
        let (connection, property) = unsafe {
            (
                dlsym((-2isize) as *mut c_void, c"_CGSDefaultConnection".as_ptr()),
                dlsym(
                    (-2isize) as *mut c_void,
                    c"CGSSetConnectionProperty".as_ptr(),
                ),
            )
        };
        if connection.is_null() || property.is_null() {
            return false;
        }
        // SAFETY: these are the Quartz connection/property ABIs; checked symbols are code pointers.
        let connection: unsafe extern "C" fn() -> i32 = unsafe { std::mem::transmute(connection) };
        let property: unsafe extern "C" fn(i32, i32, *const c_void, *const c_void) -> i32 =
            unsafe { std::mem::transmute(property) };
        let key = CFString::new("SetsCursorInBackground");
        let enabled = CFBoolean::true_value();
        // SAFETY: borrowed CF values stay alive through this synchronous call.
        unsafe {
            let cid = connection();
            property(
                cid,
                cid,
                key.as_concrete_TypeRef().cast(),
                enabled.as_concrete_TypeRef().cast(),
            ) == 0
        }
    })
}

/// Hide counts and cursor association are paired even during a Rust panic on the owning thread.
#[derive(Default)]
struct CursorGuard {
    hidden: Vec<u32>,
    detached: bool,
}
impl CursorGuard {
    fn show(display: u32) -> bool {
        // SAFETY: inverse of our successful hide. If its display was unplugged, Quartz's
        // process-wide hide count can be balanced against the new active main display.
        unsafe {
            if CGDisplayShowCursor(display) == 0 {
                return true;
            }
            let current = CGMainDisplayID();
            current != display && CGDisplayShowCursor(current) == 0
        }
    }
    fn apply(&mut self, mode: CaptureMode) -> Result<(), BackendError> {
        let locked = matches!(mode, CaptureMode::Swallow { lock_pos: true });
        if locked != self.detached {
            // SAFETY: process-wide cursor API is serialized on the input run loop.
            if unsafe { CGAssociateMouseAndMouseCursorPosition(!locked) } != 0 {
                return Err(BackendError::Unavailable);
            }
            self.detached = locked;
        }
        Ok(())
    }
    fn set_visible(&mut self, visible: bool, displays: &[DisplayRect]) -> Result<(), BackendError> {
        if visible {
            self.restore();
            return if self.hidden.is_empty() {
                Ok(())
            } else {
                Err(BackendError::Unavailable)
            };
        }
        if !self.hidden.is_empty() || !enable_background_cursor() {
            return Ok(()); // Missing private API is cosmetic only: leave the cursor visible.
        }
        for display in displays {
            // SAFETY: live enumerated display ID; record only successful reference-counted hides.
            if unsafe { CGDisplayHideCursor(display.id) } != 0 {
                self.restore();
                return Err(BackendError::Unavailable);
            }
            self.hidden.push(display.id);
        }
        Ok(())
    }
    fn restore(&mut self) {
        // Failure retains the guard flags so a later timer/shutdown can retry.
        if self.detached {
            // SAFETY: serialized inverse of this guard's successful detach.
            if unsafe { CGAssociateMouseAndMouseCursorPosition(true) } == 0 {
                self.detached = false;
            }
        }
        self.hidden.retain(|display| !Self::show(*display));
    }
}
impl Drop for CursorGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

struct Tap {
    port: OwnedCf,
    source: OwnedCf,
}
impl Drop for Tap {
    fn drop(&mut self) {
        // SAFETY: owned source and tap remain alive; invalidation removes callbacks before context drops.
        unsafe {
            CFRunLoopSourceInvalidate(self.source.raw().cast());
            CFMachPortInvalidate(self.port.raw());
        }
    }
}

struct Context {
    moves: Arc<MoveSlot>,
    commands: Receiver<Command>,
    replies: Sender<Result<Reply, BackendError>>,
    changes: Sender<Vec<Monitor>>,
    status: Sender<CaptureStatus>,
    secure: Arc<AtomicBool>,
    dirty: Arc<AtomicBool>,
    sink: Option<InputSink>,
    mode: CaptureMode,
    cursor: CursorGuard,
    tap: Option<Tap>,
    source: OwnedCf,
    mouse_event: OwnedCf,
    displays: [DisplayRect; MAX_DISPLAYS],
    count: usize,
    base: Point,
    keys: [bool; 256],
    physical: [bool; 256],
    buttons: [bool; 32],
    caps: bool,
    mouse: Point,
    mouse_valid: bool,
    clicks: [Click; 32],
    clock: Instant,
}
impl Context {
    fn refresh(&mut self) -> Result<Vec<Monitor>, BackendError> {
        // Clear BEFORE querying: a reconfiguration during enumeration must remain pending.
        let pending_topology = self.dirty.swap(false, Ordering::AcqRel);
        if pending_topology {
            self.fallback(CaptureFallback::TopologyChanged);
        }
        let (displays, count) = match displays() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.dirty.store(true, Ordering::Release);
                return Err(error);
            }
        };
        let Some(base) = origin(&displays[..count]) else {
            self.dirty.store(true, Ordering::Release);
            return Err(BackendError::Unavailable);
        };
        let mouse = match self.cursor_pos().ok().and_then(|p| to_global(p, self.base)) {
            Some(p) => p,
            None => {
                self.dirty.store(true, Ordering::Release);
                return Err(BackendError::Unavailable);
            }
        };
        self.displays = displays;
        self.count = count;
        self.base = base;
        self.mouse = mouse;
        self.mouse_valid = false;
        // SAFETY: Quartz query has no input pointers.
        let primary = unsafe { CGMainDisplayID() };
        let monitors: Vec<Monitor> = displays[..count]
            .iter()
            .map(|d| Monitor {
                id: d.id.to_string(),
                x: d.x - base.x,
                y: d.y - base.y,
                w: d.w,
                h: d.h,
                scale: d.scale,
                primary: d.id == primary,
            })
            .collect();
        if pending_topology {
            let _ = self.changes.try_send(monitors.clone());
        }
        Ok(monitors)
    }
    fn local(&mut self) {
        self.mode = CaptureMode::Local;
        self.mouse_valid = false;
        self.cursor.restore();
    }
    fn fallback(&mut self, reason: CaptureFallback) {
        let swallowed = !matches!(self.mode, CaptureMode::Local);
        self.local();
        if swallowed {
            if let Some(sink) = &self.sink {
                sink.mark_overflow();
            }
            let _ = self.status.try_send(match reason {
                CaptureFallback::QueueOverflow => CaptureStatus::QueueOverflow,
                CaptureFallback::UnsupportedEvent | CaptureFallback::TopologyChanged => {
                    CaptureStatus::UnsupportedInput
                }
                CaptureFallback::TapDisabled => CaptureStatus::TapDisabled,
                CaptureFallback::PermissionLost => CaptureStatus::PermissionLost,
                CaptureFallback::SecureInput => CaptureStatus::SecureInput(true),
            });
        }
    }
    fn cursor_pos(&self) -> Result<Point, BackendError> {
        // SAFETY: null source is explicitly permitted for a current cursor query.
        let event = OwnedCf::new(unsafe { CGEventCreate(ptr::null_mut()) })?;
        // SAFETY: event is a live non-null CF event owned by the guard.
        let pos = unsafe { CGEventGetLocation(event.raw()) };
        let local = to_local(Point { x: pos.x, y: pos.y }, self.base);
        if !finite_deltas(local.x, local.y) {
            return Err(BackendError::Unavailable);
        }
        Ok(local)
    }
    fn capture(&mut self, sink: InputSink) -> Result<(), BackendError> {
        let grant = permissions();
        if grant.accessibility != PermissionStatus::Granted
            || grant.input_monitoring != PermissionStatus::Granted
            || secure_input()
        {
            return Err(BackendError::PermissionDenied);
        }
        if self.tap.is_none() {
            // SAFETY: Context is boxed at a stable address through tap invalidation; callback is
            // exclusively dispatched by this thread's CFRunLoop. Session/head/active filter = 1/0/0.
            let port = OwnedCf::new(unsafe {
                CGEventTapCreate(
                    1,
                    0,
                    0,
                    EVENT_MASK,
                    tap_callback,
                    (self as *mut Self).cast(),
                )
            })?;
            // SAFETY: a live owned mach port, with default allocator and zero priority.
            let raw = unsafe { CFMachPortCreateRunLoopSource(ptr::null_mut(), port.raw(), 0) };
            let source = match OwnedCf::new(raw.cast()) {
                Ok(s) => s,
                Err(e) => {
                    // SAFETY: invalidate the uninstalled owned tap before it is released.
                    unsafe {
                        CFMachPortInvalidate(port.raw());
                    }
                    return Err(e);
                }
            };
            // SAFETY: add the retained source to this thread's run loop; Context owns its lifetime.
            unsafe {
                CFRunLoopAddSource(CFRunLoopGetCurrent(), raw, kCFRunLoopCommonModes);
            }
            self.tap = Some(Tap { port, source });
        }
        // SAFETY: read-only HID state queries let a newly installed tap handle the first
        // modifier release and Caps Lock OFF correctly, including keys held before startup.
        unsafe {
            self.physical[0x39] = CGEventSourceFlagsState(1) & CAPS != 0;
            for key in 0xe0..=0xe7 {
                if let Some(code) = keycode(key) {
                    self.physical[usize::from(key)] = CGEventSourceKeyState(1, code);
                }
            }
        }
        self.sink = Some(sink);
        Ok(())
    }
    fn mode(&mut self, mode: CaptureMode) -> Result<(), BackendError> {
        if !matches!(mode, CaptureMode::Local) {
            if self.sink.is_none() || self.tap.is_none() {
                return Err(BackendError::Unavailable);
            }
            let grant = permissions();
            if grant.accessibility != PermissionStatus::Granted
                || grant.input_monitoring != PermissionStatus::Granted
                || secure_input()
            {
                return Err(BackendError::PermissionDenied);
            }
            if self.dirty.load(Ordering::Acquire) {
                let _ = self.refresh()?;
            }
        }
        let mouse = if matches!(mode, CaptureMode::Local) {
            None
        } else {
            Some(
                self.cursor_pos()
                    .ok()
                    .and_then(|p| to_global(p, self.base))
                    .ok_or(BackendError::Unavailable)?,
            )
        };
        self.cursor.apply(mode)?;
        if let Some(mouse) = mouse {
            self.mouse = mouse;
        }
        self.mouse_valid = false;
        self.mode = mode;
        Ok(())
    }
    /// Apply the newest stored cursor position, if any. The flag is cleared before reading so a move that arrives while
    /// this one is applied queues its own command and the final position is never left behind.
    fn apply_latest_move(&mut self) {
        self.moves.queued.store(false, Ordering::Release);
        let latest = self
            .moves
            .latest
            .lock()
            .ok()
            .and_then(|mut slot| slot.take());
        if let Some(event) = latest {
            let _ = self.inject(event.kind);
        }
    }

    fn inject(&mut self, kind: InputEventKind) -> Result<(), BackendError> {
        if permissions().injection != PermissionStatus::Granted {
            return Err(BackendError::PermissionDenied);
        }
        // Secure Input blocks safe local takeover. Stop new remote input, but still permit
        // the release guard to clear already-held keys/buttons while posting remains granted.
        if secure_input() && !is_release(kind) {
            return Err(BackendError::Unavailable);
        }
        if matches!(kind, InputEventKind::PointerMoved { .. }) && self.dirty.load(Ordering::Acquire)
        {
            self.fallback(CaptureFallback::TopologyChanged);
            let _ = self.refresh()?;
        }
        if matches!(
            kind,
            InputEventKind::PointerMoved { .. } | InputEventKind::Button { down: true, .. }
        ) && !self.mouse_valid
        {
            self.mouse = self
                .cursor_pos()
                .ok()
                .and_then(|p| to_global(p, self.base))
                .ok_or(BackendError::Unavailable)?;
        }
        let source = self.source.raw();
        let mut next_keys = self.keys;
        let mut next_buttons = self.buttons;
        let mut caps = self.caps;
        let mut pos = self.mouse;
        let mut hid = None;
        let created;
        let event = match kind {
            InputEventKind::Key {
                key: Key(key),
                down,
            } => {
                let code = keycode(key).ok_or(BackendError::Unsupported)?;
                hid = Some(key);
                let index = usize::from(key);
                if !down && shared_key_held(&self.keys, key) {
                    // ANSI backslash and ISO non-US # share one Mac keycode. Hold it
                    // until BOTH source usages release, including release_all's loop.
                    self.keys[index] = false;
                    return Ok(());
                }
                if key == 0x39 && down && !next_keys[index] {
                    caps = !caps;
                }
                let repeated = next_keys[index] && down;
                next_keys[index] = down;
                // SAFETY: live private source and keycode from the bounded physical table.
                let event =
                    OwnedCf::new(unsafe { CGEventCreateKeyboardEvent(source, code, down) })?;
                if (0xe0..=0xe7).contains(&key) || key == 0x39 {
                    // SAFETY: owned keyboard event; FlagsChanged is the modifier event type.
                    unsafe {
                        CGEventSetType(event.raw(), 12);
                    }
                } else {
                    // SAFETY: owned keyboard event, boolean autorepeat metadata.
                    unsafe {
                        CGEventSetIntegerValueField(event.raw(), 8, i64::from(repeated));
                    }
                }
                created = event;
                &created
            }
            InputEventKind::PointerMoved {
                position,
                delta_x,
                delta_y,
            } => {
                if !finite_deltas(delta_x, delta_y) {
                    return Err(BackendError::InvalidInput("invalid pointer delta".into()));
                }
                pos = to_global(position, self.base).ok_or_else(|| {
                    BackendError::InvalidInput("non-finite cursor position".into())
                })?;
                pos = self.displays[..self.count]
                    .iter()
                    .map(|d| Point {
                        x: pos.x.clamp(d.x, (d.x + d.w).next_down().max(d.x)),
                        y: pos.y.clamp(d.y, (d.y + d.h).next_down().max(d.y)),
                    })
                    .min_by(|a, b| {
                        (a.x - pos.x)
                            .hypot(a.y - pos.y)
                            .total_cmp(&(b.x - pos.x).hypot(b.y - pos.y))
                    })
                    .ok_or(BackendError::Transient)?;
                let (kind, button) = movement_type(&self.buttons);
                // SAFETY: this thread exclusively owns the reusable event. CGEventPost
                // serializes its contents; no callback receives this mutable CF reference.
                // Position/event/button types are finite and validated. No per-move create.
                unsafe {
                    CGEventSetType(self.mouse_event.raw(), kind);
                    CGEventSetLocation(self.mouse_event.raw(), CGPoint::new(pos.x, pos.y));
                    CGEventSetIntegerValueField(self.mouse_event.raw(), 3, button as i64);
                    CGEventSetDoubleValueField(self.mouse_event.raw(), 4, pos.x - self.mouse.x);
                    CGEventSetDoubleValueField(self.mouse_event.raw(), 5, pos.y - self.mouse.y);
                    CGEventSetDoubleValueField(
                        self.mouse_event.raw(),
                        2,
                        if kind == 5 { 0.0 } else { 1.0 },
                    );
                }
                &self.mouse_event
            }
            InputEventKind::Button { button, down } => {
                let n = button_number(button).ok_or(BackendError::Unsupported)?;
                let kind = match n {
                    0 => {
                        if down {
                            1
                        } else {
                            2
                        }
                    }
                    1 => {
                        if down {
                            3
                        } else {
                            4
                        }
                    }
                    _ => {
                        if down {
                            25
                        } else {
                            26
                        }
                    }
                };
                next_buttons[n] = down;
                // SAFETY: live source, finite cached point, validated button/event type.
                let event = OwnedCf::new(unsafe {
                    CGEventCreateMouseEvent(source, kind, CGPoint::new(pos.x, pos.y), n as u32)
                })?;
                let count = if down {
                    self.clicks[n].press(
                        self.clock.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                        pos,
                    )
                } else {
                    self.clicks[n].count.max(1)
                };
                // SAFETY: live owned mouse event, bounded button, click count and pressure.
                unsafe {
                    CGEventSetIntegerValueField(event.raw(), 3, n as i64);
                    CGEventSetIntegerValueField(event.raw(), 1, count);
                    CGEventSetDoubleValueField(event.raw(), 2, if down { 1.0 } else { 0.0 });
                }
                created = event;
                &created
            }
            InputEventKind::Wheel { dx, dy, precise } => {
                if !dx.is_finite()
                    || !dy.is_finite()
                    || dx.abs() > f64::from(i32::MAX)
                    || dy.abs() > f64::from(i32::MAX)
                {
                    return Err(BackendError::InvalidInput("invalid scroll delta".into()));
                }
                // SAFETY: live source; exactly two promoted int32 varargs, finite/range checked.
                let event = OwnedCf::new(unsafe {
                    CGEventCreateScrollWheelEvent(
                        source,
                        u32::from(!precise),
                        2,
                        dy.round() as i32,
                        dx.round() as i32,
                    )
                })?;
                // SAFETY: live owned wheel event; checked deltas set matching wheel metadata.
                unsafe {
                    CGEventSetIntegerValueField(event.raw(), 88, i64::from(precise));
                    CGEventSetDoubleValueField(event.raw(), 93, dy);
                    CGEventSetDoubleValueField(event.raw(), 94, dx);
                }
                if precise {
                    // SAFETY: live precise wheel event, finite point-unit deltas.
                    unsafe {
                        CGEventSetDoubleValueField(event.raw(), 96, dy);
                        CGEventSetDoubleValueField(event.raw(), 97, dx);
                    }
                }
                created = event;
                &created
            }
        };
        // SAFETY: event is live and private, all flags derive from bounded tracked key state;
        // the magic is attached to every event, then Quartz posts at kCGHIDEventTap (0).
        unsafe {
            CGEventSetFlags(event.raw(), key_flags(&next_keys, caps, hid));
            CGEventSetIntegerValueField(event.raw(), 42, MAGIC);
            CGEventPost(0, event.raw());
        }
        let posting_granted = permissions().injection == PermissionStatus::Granted;
        // Posting has no delivery result. If TCC changed during a release, conservatively
        // retain the held state for retry; a newly posted press must still be tracked.
        if posting_granted || !is_release(kind) {
            self.keys = next_keys;
            self.buttons = next_buttons;
        }
        self.caps = caps;
        self.mouse = pos;
        if matches!(
            kind,
            InputEventKind::PointerMoved { .. } | InputEventKind::Button { .. }
        ) {
            self.mouse_valid = true;
        }
        // CGEventPost has no success result. Pre/post permission checks cannot prove delivery.
        if !posting_granted {
            return Err(BackendError::PermissionDenied);
        }
        Ok(())
    }
    fn release(&mut self) -> Result<(), BackendError> {
        let mut error = None;
        for key in 0..self.keys.len() {
            if self.keys[key] {
                if let Err(e) = self.inject(InputEventKind::Key {
                    key: Key(key as u16),
                    down: false,
                }) {
                    error = Some(e);
                }
            }
        }
        for n in 0..self.buttons.len() {
            if self.buttons[n] {
                if let Some(button) = button_from_number(n) {
                    if let Err(e) = self.inject(InputEventKind::Button {
                        button,
                        down: false,
                    }) {
                        error = Some(e);
                    }
                }
            }
        }
        self.mouse_valid = false;
        error.map_or(Ok(()), Err)
    }
}
impl Drop for Context {
    fn drop(&mut self) {
        self.local();
        let _ = self.release();
        self.tap.take();
    }
}

fn displays() -> Result<([DisplayRect; MAX_DISPLAYS], usize), BackendError> {
    let mut ids = [0; MAX_DISPLAYS + 1];
    let mut count = 0;
    // SAFETY: ids has capacity for exactly the supplied bound, count is a writable u32.
    if unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) } != 0
        || count == 0
        || count as usize > MAX_DISPLAYS
    {
        return Err(BackendError::Unavailable);
    }
    let mut result = [DisplayRect::default(); MAX_DISPLAYS];
    for (index, &id) in ids[..count as usize].iter().enumerate() {
        // SAFETY: id came from Quartz; mode uses Copy rule and is owned until its values are read.
        let (bounds, mode) = unsafe {
            (
                CGDisplayBounds(id),
                OwnedCf::new(CGDisplayCopyDisplayMode(id))?,
            )
        };
        // SAFETY: live non-null display mode, read-only queries.
        let (logical, physical) = unsafe {
            (
                CGDisplayModeGetWidth(mode.raw()),
                CGDisplayModeGetPixelWidth(mode.raw()),
            )
        };
        if logical == 0 {
            return Err(BackendError::Unavailable);
        }
        result[index] = DisplayRect {
            id,
            x: bounds.origin.x,
            y: bounds.origin.y,
            w: bounds.size.width,
            h: bounds.size.height,
            scale: physical as f64 / logical as f64,
        };
    }
    Ok((result, count as usize))
}

extern "C" fn tap_callback(_: Handle, kind: u32, event: Handle, info: Handle) -> Handle {
    if info.is_null() {
        return event;
    }
    // SAFETY: tap lifetime is strictly inside the boxed Context lifetime, same owning thread.
    let context = unsafe { &mut *info.cast::<Context>() };
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        capture_event(context, kind, event)
    })) {
        Ok(result) => result,
        Err(_) => {
            context.fallback(CaptureFallback::UnsupportedEvent);
            stop_current_loop();
            event
        }
    }
}

fn capture_event(context: &mut Context, kind: u32, event: Handle) -> Handle {
    if kind == u32::MAX || kind == u32::MAX - 1 {
        context.fallback(CaptureFallback::TapDisabled);
        if let Some(tap) = &context.tap {
            // SAFETY: tap owns its port; both disabled notifications require re-enable.
            unsafe {
                CGEventTapEnable(tap.port.raw(), true);
            }
        }
        return event;
    }
    if event.is_null() {
        context.fallback(CaptureFallback::UnsupportedEvent);
        return event;
    }
    // SAFETY: event is borrowed from Quartz for this callback only; no ownership is acquired.
    let (magic, source_pid) = unsafe {
        (
            CGEventGetIntegerValueField(event, 42),
            CGEventGetIntegerValueField(event, 41),
        )
    };
    if magic == MAGIC {
        return event;
    }
    let injected = source_pid != 0;
    if matches!(kind, 10..=12) {
        let secure = secure_input();
        if context.secure.swap(secure, Ordering::AcqRel) != secure {
            let _ = context.status.try_send(CaptureStatus::SecureInput(secure));
        }
    }
    if context.secure.load(Ordering::Acquire) {
        context.fallback(CaptureFallback::SecureInput);
        return event;
    }
    if context.dirty.load(Ordering::Acquire) {
        context.fallback(CaptureFallback::TopologyChanged);
        return event;
    }
    let mut caps_toggle = false;
    let sample = match kind {
        5..=7 | 27 => {
            // SAFETY: non-null event borrowed from Quartz for this callback only.
            let p = unsafe { CGEventGetLocation(event) };
            let p = Point { x: p.x, y: p.y };
            if !context.displays[..context.count]
                .iter()
                .any(|d| d.contains(p))
            {
                context.fallback(CaptureFallback::TopologyChanged);
                return event;
            };
            // SAFETY: live mouse callback event, read-only native delta fields.
            let (delta_x, delta_y) = unsafe {
                (
                    CGEventGetDoubleValueField(event, 4),
                    CGEventGetDoubleValueField(event, 5),
                )
            };
            if !finite_deltas(delta_x, delta_y) {
                context.fallback(CaptureFallback::UnsupportedEvent);
                return event;
            }
            if !injected {
                context.mouse = p;
                context.mouse_valid = true;
            }
            InputEventKind::PointerMoved {
                position: to_local(p, context.base),
                delta_x,
                delta_y,
            }
        }
        10..=12 => {
            // SAFETY: live keyboard callback event, read-only keycode/type/modifier metadata.
            let (code, keyboard_type, flags) = unsafe {
                (
                    CGEventGetIntegerValueField(event, 9),
                    CGEventGetIntegerValueField(event, 10),
                    CGEventGetFlags(event),
                )
            };
            let Some(key) = u16::try_from(code)
                .ok()
                .and_then(|c| hid_usage(c, keyboard_type))
            else {
                context.fallback(CaptureFallback::UnsupportedEvent);
                return event;
            };
            let down = if kind == 12 && key == 0x39 {
                // Caps Lock flags reflect the latch, not the physical down/up transition.
                // Emit one complete press per latch change so turning Caps OFF also toggles.
                let enabled = flags & CAPS != 0;
                if context.physical[0x39] == enabled {
                    return event;
                }
                if !injected {
                    context.physical[0x39] = enabled;
                }
                caps_toggle = true;
                true
            } else if kind == 12 {
                let Some(down) = modifier_down(key, flags, context.physical[usize::from(key)])
                else {
                    context.fallback(CaptureFallback::UnsupportedEvent);
                    return event;
                };
                down
            } else {
                kind == 10
            };
            if !injected && !caps_toggle {
                context.physical[usize::from(key)] = down;
            }
            InputEventKind::Key {
                key: Key(key),
                down,
            }
        }
        1..=4 | 25..=26 => {
            // SAFETY: live mouse callback event, read-only button field.
            let number = unsafe { CGEventGetIntegerValueField(event, 3) };
            let Some(button) = usize::try_from(number).ok().and_then(button_from_number) else {
                context.fallback(CaptureFallback::UnsupportedEvent);
                return event;
            };
            InputEventKind::Button {
                button,
                down: matches!(kind, 1 | 3 | 25),
            }
        }
        22 => {
            // SAFETY: live wheel callback event, read-only unit and matching delta fields.
            let (precise, dx, dy) = unsafe {
                let precise = CGEventGetIntegerValueField(event, 88) != 0;
                (
                    precise,
                    CGEventGetDoubleValueField(event, if precise { 97 } else { 94 }),
                    CGEventGetDoubleValueField(event, if precise { 96 } else { 93 }),
                )
            };
            if !finite_deltas(dx, dy) {
                context.fallback(CaptureFallback::UnsupportedEvent);
                return event;
            }
            InputEventKind::Wheel { dx, dy, precise }
        }
        _ => return event,
    };
    let pushed = context.sink.as_ref().is_some_and(|sink| {
        sink.try_push(InputEvent {
            kind: sample,
            injected,
        })
        .is_ok()
    });
    if !pushed {
        context.fallback(CaptureFallback::QueueOverflow);
        return event;
    }
    if caps_toggle {
        let released = context.sink.as_ref().is_some_and(|sink| {
            sink.try_push(InputEvent {
                kind: InputEventKind::Key {
                    key: Key(0x39),
                    down: false,
                },
                injected,
            })
            .is_ok()
        });
        if !released {
            context.fallback(CaptureFallback::QueueOverflow);
            return event;
        }
    }
    if !injected && !matches!(context.mode, CaptureMode::Local) {
        ptr::null_mut()
    } else {
        event
    }
}

extern "C" fn commands_callback(info: *const c_void) {
    if info.is_null() {
        return;
    }
    // SAFETY: custom source belongs to the Context's CFRunLoop; stable box lives until invalidation.
    let context = unsafe { &mut *info.cast_mut().cast::<Context>() };
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| process_commands(context))).is_err()
    {
        context.fallback(CaptureFallback::UnsupportedEvent);
        let _ = context
            .replies
            .try_send(Err(BackendError::StateUnavailable));
        stop_current_loop();
    }
}

fn process_commands(context: &mut Context) {
    // Fixed bounded batch: no source callback can indefinitely starve the event tap.
    for _ in 0..16 {
        let Ok(command) = context.commands.try_recv() else {
            break;
        };
        let result = match command {
            Command::Move => {
                context.apply_latest_move();
                continue; // moves are fire-and-forget: no reply is waiting for them
            }
            Command::Capture(sink) => context.capture(sink).map(|_| Reply::Unit),
            Command::Mode(mode) => context.mode(mode).map(|_| Reply::Unit),
            Command::Visibility(visible) => context
                .cursor
                .set_visible(visible, &context.displays[..context.count])
                .map(|_| Reply::Unit),
            Command::Inject(event) => context.inject(event.kind).map(|_| Reply::Unit),
            Command::Release => context.release().map(|_| Reply::Unit),
            Command::Monitors => context.refresh().map(Reply::Monitors),
            Command::Cursor => context.cursor_pos().map(Reply::Cursor),
            Command::Stop => {
                context.local();
                let result = context.release().map(|_| Reply::Unit);
                stop_current_loop();
                result
            }
        };
        let _ = context.replies.try_send(result);
    }
}

extern "C" fn timer_callback(_: CFRunLoopTimerRef, info: Handle) {
    if info.is_null() {
        return;
    }
    // SAFETY: timer is invalidated before the stable boxed context goes away, owning thread only.
    let context = unsafe { &mut *info.cast::<Context>() };
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| safety_tick(context))).is_err() {
        context.fallback(CaptureFallback::UnsupportedEvent);
        stop_current_loop();
    }
}

fn stop_current_loop() {
    // SAFETY: called on the owning thread and only stops that thread's current loop.
    unsafe {
        CFRunLoopStop(CFRunLoopGetCurrent());
    }
}

fn safety_tick(context: &mut Context) {
    let secure = secure_input();
    if context.secure.swap(secure, Ordering::AcqRel) != secure {
        let _ = context.status.try_send(CaptureStatus::SecureInput(secure));
    }
    if secure {
        context.fallback(CaptureFallback::SecureInput);
    } else {
        let grant = permissions();
        if grant.input_monitoring != PermissionStatus::Granted
            || grant.accessibility != PermissionStatus::Granted
        {
            context.fallback(CaptureFallback::PermissionLost);
        }
    }
    if matches!(context.mode, CaptureMode::Local) {
        let _ = context.cursor.apply(CaptureMode::Local);
    }
    if context.dirty.load(Ordering::Acquire) {
        context.fallback(CaptureFallback::TopologyChanged);
        let _ = context.refresh();
    }
}
extern "C" fn display_callback(_: u32, _: u32, info: Handle) {
    if !info.is_null() {
        // SAFETY: registration owns an Arc reference until callbacks are unregistered;
        // callbacks access only an AtomicBool, never thread-affine Context state.
        unsafe { &*info.cast::<AtomicBool>() }.store(true, Ordering::Release);
    }
}
struct DisplayRegistration {
    dirty: Arc<AtomicBool>,
}
impl Drop for DisplayRegistration {
    fn drop(&mut self) {
        // SAFETY: unregister same callback/context before the retained Arc can be freed.
        unsafe {
            CGDisplayRemoveReconfigurationCallback(
                display_callback,
                Arc::as_ptr(&self.dirty).cast_mut().cast(),
            );
        }
    }
}
struct Sources {
    command: Arc<Wake>,
    timer: OwnedCf,
}
impl Drop for Sources {
    fn drop(&mut self) {
        // SAFETY: all owned CF sources are live; invalidation prevents further callbacks.
        unsafe {
            CFRunLoopSourceInvalidate(self.command.source.as_ptr().cast());
            CFRunLoopTimerInvalidate(self.timer.raw().cast());
        }
    }
}

fn run(
    commands: Receiver<Command>,
    replies: Sender<Result<Reply, BackendError>>,
    changes: Sender<Vec<Monitor>>,
    status: Sender<CaptureStatus>,
    secure: Arc<AtomicBool>,
    moves: Arc<MoveSlot>,
    ready: &Sender<Result<Arc<Wake>, BackendError>>,
) -> Result<(), BackendError> {
    prioritize_current_thread();
    // SAFETY: private source owns independent injected key/button state, never session state.
    let source = OwnedCf::new(unsafe { CGEventSourceCreate(-1) })?;
    // SAFETY: live owned source; no suppression of real user input after injection.
    unsafe {
        CGEventSourceSetUserData(source.raw(), MAGIC);
        CGEventSourceSetLocalEventsSuppressionInterval(source.raw(), 0.0);
    }
    // SAFETY: a live private source and a finite initial point; pointer fields are refreshed
    // before every post. The reusable event stays exclusively on this owning thread.
    let mouse_event = OwnedCf::new(unsafe {
        CGEventCreateMouseEvent(source.raw(), 5, CGPoint::new(0.0, 0.0), 0)
    })?;
    let dirty = Arc::new(AtomicBool::new(false));
    let mut context = Box::new(Context {
        moves,
        commands,
        replies,
        changes,
        status,
        secure,
        dirty: dirty.clone(),
        sink: None,
        mode: CaptureMode::Local,
        cursor: CursorGuard::default(),
        tap: None,
        source,
        mouse_event,
        displays: [DisplayRect::default(); MAX_DISPLAYS],
        count: 0,
        base: Point { x: 0.0, y: 0.0 },
        keys: [false; 256],
        physical: [false; 256],
        buttons: [false; 32],
        // SAFETY: read-only Quartz query, initializing the private source's lock state.
        caps: unsafe { CGEventSourceFlagsState(1) & CAPS != 0 },
        mouse: Point { x: 0.0, y: 0.0 },
        mouse_valid: false,
        clicks: [Click::default(); 32],
        clock: Instant::now(),
    });
    let _ = context.refresh()?;
    let local = context.cursor_pos()?;
    context.mouse = to_global(local, context.base).ok_or(BackendError::Unavailable)?;
    let info = (&mut *context as *mut Context).cast();
    let mut source_context = CFRunLoopSourceContext {
        version: 0,
        info,
        retain: None,
        release: None,
        copyDescription: None,
        equal: None,
        hash: None,
        schedule: None,
        cancel: None,
        perform: commands_callback,
    };
    // SAFETY: context box is stable and lives past source invalidation; default allocator.
    let source =
        NonNull::new(unsafe { CFRunLoopSourceCreate(ptr::null(), 0, &mut source_context) }.cast())
            .ok_or(BackendError::Unavailable)?;
    let command = Arc::new(Wake {
        source,
        runloop: core_foundation::runloop::CFRunLoop::get_current(),
    });
    let mut timer_context = CFRunLoopTimerContext {
        version: 0,
        info,
        retain: None,
        release: None,
        copyDescription: None,
    };
    // SAFETY: timer copies the context, stable box survives until timer invalidation; checked return.
    let timer = OwnedCf::new(
        unsafe {
            CFRunLoopTimerCreate(
                ptr::null(),
                core_foundation::date::CFAbsoluteTimeGetCurrent() + 0.25,
                0.25,
                0,
                0,
                timer_callback,
                &mut timer_context,
            )
        }
        .cast(),
    )?;
    let sources = Sources { command, timer };
    // SAFETY: all source/timer references live, registration gets a retained stable Arc address.
    unsafe {
        CFRunLoopAddSource(
            CFRunLoopGetCurrent(),
            sources.command.source.as_ptr().cast(),
            kCFRunLoopCommonModes,
        );
        CFRunLoopAddTimer(
            CFRunLoopGetCurrent(),
            sources.timer.raw().cast(),
            kCFRunLoopCommonModes,
        );
        if CGDisplayRegisterReconfigurationCallback(
            display_callback,
            Arc::as_ptr(&dirty).cast_mut().cast(),
        ) != 0
        {
            return Err(BackendError::Unavailable);
        }
    }
    let _registration = DisplayRegistration { dirty };
    ready
        .send(Ok(sources.command.clone()))
        .map_err(|_| BackendError::Unavailable)?;
    // SAFETY: dedicated owning thread with installed source and timer; Stop command exits it.
    unsafe {
        CFRunLoopRun();
    }
    drop(sources); // Invalidate every callback before dropping Context.
    Ok(())
}
