#![cfg(windows)]

use crate::formats::{cf_html_decode, cf_html_encode, dib_to_png, png_to_dib};
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use glide_platform::{
    validate_clipboard_bundle, BackendError, ClipboardAdmission, ClipboardBackend,
    ClipboardChangeToken, ClipboardContent, ClipboardData, ClipboardEvent, ClipboardFormat,
    ClipboardMarker, ClipboardPublish, ClipboardSensitivity, ClipboardSnapshot, FileEntry,
    FileList,
};
use std::cell::Cell;
use std::collections::{BTreeSet, HashMap};
use std::ffi::c_void;
use std::mem::size_of;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::slice;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use windows_sys::core::PCWSTR;
use windows_sys::Win32::Foundation::{
    GetLastError, GlobalFree, HANDLE, HWND, LPARAM, LRESULT, WPARAM,
};
use windows_sys::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, EnumClipboardFormats,
    GetClipboardData, GetClipboardFormatNameW, GetClipboardOwner, GetClipboardSequenceNumber,
    OpenClipboard, RegisterClipboardFormatW, RemoveClipboardFormatListener, SetClipboardData,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GHND};
use windows_sys::Win32::System::Ole::{CF_DIB, CF_DIBV5, CF_HDROP, CF_UNICODETEXT};
use windows_sys::Win32::System::Threading::{GetCurrentThreadId, Sleep};
use windows_sys::Win32::UI::Shell::DragQueryFileW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetWindowLongPtrW, KillTimer, PeekMessageW, PostThreadMessageW, RegisterClassW, SetTimer,
    SetWindowLongPtrW, TranslateMessage, CREATESTRUCTW, GWLP_USERDATA, HWND_MESSAGE, MSG,
    PM_NOREMOVE, WM_APP, WM_CLIPBOARDUPDATE, WM_DESTROYCLIPBOARD, WM_NCCREATE, WM_NCDESTROY,
    WM_QUIT, WM_RENDERALLFORMATS, WM_RENDERFORMAT, WM_TIMER, WNDCLASSW,
};

const COMMAND_MESSAGE: u32 = WM_APP + 0x431;
const REQUEST_PENDING: u8 = 0;
const REQUEST_STARTED: u8 = 1;
const REQUEST_CANCELED: u8 = 2;
const COMMAND_QUEUE_CAPACITY: usize = 2;
const MAX_CLIPBOARD_BYTES: usize = 64 * 1024 * 1024;
const MAX_CLIPBOARD_FORMATS: usize = 512;
const MAX_FILE_ENTRIES: u32 = 4096;
const MAX_PATH_UNITS: usize = 32768;
const DELAYED_CONTENT_TIMEOUT: Duration = Duration::from_secs(30);
const DELAYED_TIMEOUT_TIMER: usize = 0x474c;
const EVENT_QUEUE_CAPACITY: usize = 128;
const NOTICE_QUEUE_CAPACITY: usize = 64;
const CLIPBOARD_MARKER_NAME: &str = "Glide.ClipboardMarker.v1";
const SENSITIVE_MONITOR_NAME: &str = "ExcludeClipboardContentFromMonitorProcessing";
const SENSITIVE_HISTORY_NAME: &str = "CanIncludeInClipboardHistory";
const HTML_FORMAT_NAME: &str = "HTML Format";
const RTF_FORMAT_NAME: &str = "Rich Text Format";
const PNG_FORMAT_NAME: &str = "PNG";
const DROP_EFFECT_NAME: &str = "Preferred DropEffect";
const CF_DIB_ID: u32 = CF_DIB as u32;
const CF_DIBV5_ID: u32 = CF_DIBV5 as u32;
const CF_HDROP_ID: u32 = CF_HDROP as u32;
const CF_UNICODETEXT_ID: u32 = CF_UNICODETEXT as u32;
const FORMAT_PRIVATE_FIRST: u32 = 0x0200;
const FORMAT_PRIVATE_LAST: u32 = 0x02ff;

/// The Windows clipboard implementation. All owner-window messages run on one private thread.
pub struct WindowsClipboard {
    command_tx: Sender<CommandRequest>,
    command_lock: Mutex<()>,
    thread_id: u32,
    failed: AtomicBool,
    _events_tx: Sender<ClipboardEvent>,
    events_rx: Receiver<ClipboardEvent>,
    notices_tx: Sender<Notice>,
    stopping: Arc<AtomicBool>,
    _listener_thread: Mutex<Option<JoinHandle<()>>>,
    _event_thread: Mutex<Option<JoinHandle<()>>>,
}

impl WindowsClipboard {
    pub fn new() -> Result<Self, BackendError> {
        crate::native::enable_per_monitor_v2()?;
        let (command_tx, command_rx) = bounded(COMMAND_QUEUE_CAPACITY);
        let (notice_tx, notice_rx) = bounded(NOTICE_QUEUE_CAPACITY);
        let (events_tx, events_rx) = bounded(EVENT_QUEUE_CAPACITY);
        let delayed = Arc::new(DelayedStore::default());
        let (started_tx, started_rx) = bounded(1);
        let listener_events_tx = events_tx.clone();
        let listener_events_rx = events_rx.clone();
        let listener_notice_rx = notice_rx.clone();
        let listener_notice_tx = notice_tx.clone();
        let listener_delayed = Arc::clone(&delayed);
        let listener_thread = thread::Builder::new()
            .name("glide-clipboard-window".into())
            .spawn(move || {
                listener_thread(
                    command_rx,
                    listener_notice_tx,
                    listener_notice_rx,
                    listener_events_tx,
                    listener_events_rx,
                    listener_delayed,
                    started_tx,
                )
            })
            .map_err(|_| BackendError::Unavailable)?;
        let thread_id = match started_rx.recv() {
            Ok(Ok(thread_id)) => thread_id,
            Ok(Err(error)) => {
                let _ = listener_thread.join();
                return Err(error);
            }
            Err(_) => {
                let _ = listener_thread.join();
                return Err(BackendError::Unavailable);
            }
        };

        let event_command_tx = command_tx.clone();
        let event_sender = events_tx.clone();
        let event_receiver = events_rx.clone();
        let stopping = Arc::new(AtomicBool::new(false));
        let event_stopping = Arc::clone(&stopping);
        let event_thread = thread::Builder::new()
            .name("glide-clipboard-events".into())
            .spawn(move || {
                event_thread(
                    notice_rx,
                    event_command_tx,
                    thread_id,
                    event_sender,
                    event_receiver,
                    event_stopping,
                )
            })
            .map_err(|_| {
                // SAFETY: the listener message queue is live after started_rx succeeded.
                unsafe { PostThreadMessageW(thread_id, WM_QUIT, 0, 0) };
                BackendError::Unavailable
            })?;

        Ok(Self {
            command_tx,
            command_lock: Mutex::new(()),
            thread_id,
            failed: AtomicBool::new(false),
            _events_tx: events_tx,
            events_rx,
            notices_tx: notice_tx,
            stopping,
            _listener_thread: Mutex::new(Some(listener_thread)),
            _event_thread: Mutex::new(Some(event_thread)),
        })
    }

    fn command<T>(
        &self,
        make: impl FnOnce(Sender<Result<T, BackendError>>) -> Command,
    ) -> Result<T, BackendError> {
        if self.failed.load(Ordering::Acquire) {
            return Err(BackendError::Unavailable);
        }
        let _lock = self
            .command_lock
            .lock()
            .map_err(|_| BackendError::StateUnavailable)?;
        if self.failed.load(Ordering::Acquire) {
            return Err(BackendError::Unavailable);
        }
        let (reply_tx, reply_rx) = bounded(1);
        let request_status = Arc::new(AtomicU8::new(REQUEST_PENDING));
        self.command_tx
            .try_send(CommandRequest {
                command: make(reply_tx),
                status: Arc::clone(&request_status),
            })
            .map_err(|_| BackendError::Unavailable)?;
        // SAFETY: this thread id was published after its message queue was created; the window
        // thread remains owned by this backend until Drop posts WM_QUIT and joins it.
        if unsafe { PostThreadMessageW(self.thread_id, COMMAND_MESSAGE, 0, 0) } == 0 {
            let error = last_error("clipboard thread stopped");
            match request_status.compare_exchange(
                REQUEST_PENDING,
                REQUEST_CANCELED,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.failed.store(true, Ordering::Release);
                    return Err(error);
                }
                Err(REQUEST_STARTED) => {}
                Err(_) => return Err(BackendError::Unavailable),
            }
        }
        reply_rx.recv().map_err(|_| BackendError::Unavailable)?
    }

    fn snapshot(&self) -> Result<Snapshot, BackendError> {
        self.command(Command::Snapshot)
    }
}

impl Drop for WindowsClipboard {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        let _ = self.notices_tx.try_send(Notice::Stop);
        if let Ok(thread) = self._listener_thread.get_mut() {
            if let Some(thread) = thread.take() {
                if !thread.is_finished() {
                    // SAFETY: WM_QUIT targets the owned listener's shutdown path before join.
                    // Skip known terminated threads because numeric IDs can be reused.
                    unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0) };
                }
                let _ = thread.join();
            }
        }
        if let Ok(thread) = self._event_thread.get_mut() {
            if let Some(thread) = thread.take() {
                let _ = thread.join();
            }
        }
    }
}

impl ClipboardBackend for WindowsClipboard {
    fn read_snapshot(&self) -> Result<ClipboardSnapshot, BackendError> {
        self.command(Command::ReadSnapshot)
    }
    fn publish_snapshot(
        &self,
        contents: Vec<ClipboardContent>,
        marker: ClipboardMarker,
        expected: ClipboardChangeToken,
        admission: ClipboardAdmission,
    ) -> Result<ClipboardPublish, BackendError> {
        validate_clipboard_bundle(&contents)?;
        self.command(|reply| Command::PublishSnapshot(contents, marker, expected, admission, reply))
    }
    fn read(&self, format: &ClipboardFormat) -> Result<Option<ClipboardContent>, BackendError> {
        match format {
            ClipboardFormat::Files => Ok(self.read_files()?.map(ClipboardContent::files)),
            _ => self.command(|reply| Command::Read(format.clone(), reply)),
        }
    }

    fn write(
        &self,
        content: ClipboardContent,
        marker: ClipboardMarker,
    ) -> Result<(), BackendError> {
        content.validate()?;
        self.command(|reply| Command::Write(content, marker, reply))
    }

    fn formats(&self) -> Result<Vec<ClipboardFormat>, BackendError> {
        Ok(self.snapshot()?.formats)
    }

    fn read_files(&self) -> Result<Option<FileList>, BackendError> {
        self.command(Command::ReadFiles)
    }

    fn write_files(&self, files: FileList, marker: ClipboardMarker) -> Result<(), BackendError> {
        self.command(|reply| Command::WriteFiles(files, marker, reply))
    }

    fn subscribe(&self) -> Receiver<ClipboardEvent> {
        self.events_rx.clone()
    }

    fn set_delayed_render(
        &self,
        format: ClipboardFormat,
        marker: ClipboardMarker,
    ) -> Result<(), BackendError> {
        self.command(|reply| Command::SetDelayed(format, marker, reply))
    }

    fn fulfill_delayed_render(
        &self,
        content: ClipboardContent,
        marker: ClipboardMarker,
    ) -> Result<(), BackendError> {
        content.validate()?;
        self.command(|reply| Command::Fulfill(content, marker, reply))
    }

    fn cancel_delayed_render(&self, marker: ClipboardMarker) -> Result<(), BackendError> {
        self.command(|reply| Command::CancelDelayed(marker, reply))
    }
}

enum Command {
    ReadSnapshot(Sender<Result<ClipboardSnapshot, BackendError>>),
    PublishSnapshot(
        Vec<ClipboardContent>,
        ClipboardMarker,
        ClipboardChangeToken,
        ClipboardAdmission,
        Sender<Result<ClipboardPublish, BackendError>>,
    ),
    Snapshot(Sender<Result<Snapshot, BackendError>>),
    Read(
        ClipboardFormat,
        Sender<Result<Option<ClipboardContent>, BackendError>>,
    ),
    ReadFiles(Sender<Result<Option<FileList>, BackendError>>),
    Write(
        ClipboardContent,
        ClipboardMarker,
        Sender<Result<(), BackendError>>,
    ),
    WriteFiles(FileList, ClipboardMarker, Sender<Result<(), BackendError>>),
    SetDelayed(
        ClipboardFormat,
        ClipboardMarker,
        Sender<Result<(), BackendError>>,
    ),
    Fulfill(
        ClipboardContent,
        ClipboardMarker,
        Sender<Result<(), BackendError>>,
    ),
    CancelDelayed(ClipboardMarker, Sender<Result<(), BackendError>>),
    #[cfg(test)]
    Clear(Sender<Result<(), BackendError>>),
}

struct CommandRequest {
    command: Command,
    status: Arc<AtomicU8>,
}

fn claim_request(status: &AtomicU8) -> bool {
    status
        .compare_exchange(
            REQUEST_PENDING,
            REQUEST_STARTED,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok()
}

#[derive(Clone)]
enum Notice {
    Changed(u32),
    Stop,
}

#[derive(Default)]
struct DelayedStore {
    state: Mutex<DelayedState>,
}

#[derive(Default)]
struct DelayedState {
    current_marker: Option<ClipboardMarker>,
    deadline: Option<std::time::Instant>,
    formats: HashMap<u32, DelayedFormat>,
}

struct DelayedFormat {
    marker: ClipboardMarker,
    format: ClipboardFormat,
    bytes: Option<Arc<[u8]>>,
}

struct WindowState {
    notices_tx: Sender<Notice>,
    notices_rx: Receiver<Notice>,
    events_tx: Sender<ClipboardEvent>,
    events_rx: Receiver<ClipboardEvent>,
    delayed: Arc<DelayedStore>,
    current_marker: Cell<Option<ClipboardMarker>>,
    last_sequence: AtomicU32,
    timeout_timer_set: Cell<bool>,
}

#[derive(Clone, Debug)]
struct Snapshot {
    formats: Vec<ClipboardFormat>,
    sensitivity: ClipboardSensitivity,
    marker: Option<ClipboardMarker>,
}

fn listener_thread(
    command_rx: Receiver<CommandRequest>,
    notices_tx: Sender<Notice>,
    notices_rx: Receiver<Notice>,
    events_tx: Sender<ClipboardEvent>,
    events_rx: Receiver<ClipboardEvent>,
    delayed: Arc<DelayedStore>,
    started_tx: Sender<Result<u32, BackendError>>,
) {
    // Ensure PostThreadMessageW has a queue to target before publishing this thread.
    let mut ignored = MSG::default();
    // SAFETY: `ignored` is a valid writable MSG and PM_NOREMOVE only initializes the queue.
    unsafe { PeekMessageW(&mut ignored, null_mut(), 0, 0, PM_NOREMOVE) };
    // SAFETY: GetCurrentThreadId returns a scalar identifier for this live listener thread.
    let thread_id = unsafe { GetCurrentThreadId() };
    let state = Box::new(WindowState {
        notices_tx,
        notices_rx,
        events_tx,
        events_rx,
        delayed,
        current_marker: Cell::new(None),
        last_sequence: AtomicU32::new(0),
        timeout_timer_set: Cell::new(false),
    });

    let class_name: Vec<u16> = "GlideClipboardListenerWindow"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let class_result = register_window_class(class_name.as_ptr());
    if let Err(error) = class_result {
        let _ = started_tx.send(Err(error));
        return;
    }
    // SAFETY: a null module name requests this process's borrowed module handle; no ownership transfers.
    let instance = unsafe { GetModuleHandleW(null()) };
    if instance.is_null() {
        let _ = started_tx.send(Err(last_error("cannot load Windows module")));
        return;
    }
    // SAFETY: class_name is NUL-terminated and alive for the call; instance and HWND_MESSAGE are
    // valid borrowed handles. The creation parameter points to the stable Box retained until the
    // listener window is destroyed, and creation is performed on its owning thread.
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class_name.as_ptr(),
            class_name.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            null_mut(),
            instance,
            // All mutable state shared with callbacks uses atomics, Cells, or mutexes. Pass a
            // stable pointer derived from a shared borrow so no &mut WindowState spans this call.
            (&*state as *const WindowState).cast_mut().cast::<c_void>(),
        )
    };
    if hwnd.is_null() {
        let _ = started_tx.send(Err(last_error("cannot create clipboard listener window")));
        return;
    }
    let listener_window = ListenerWindow::new(hwnd);
    // SAFETY: the HWND was created successfully on this thread and this guard owns its lifetime.
    if unsafe { AddClipboardFormatListener(hwnd) } == 0 {
        let _ = started_tx.send(Err(last_error("cannot subscribe to clipboard changes")));
        return;
    }
    listener_window.registered.set(true);
    if started_tx.send(Ok(thread_id)).is_err() {
        return;
    }

    loop {
        let mut message = MSG::default();
        // SAFETY: `message` is writable and a null HWND receives this thread's whole queue.
        let result = unsafe { GetMessageW(&mut message, null_mut(), 0, 0) };
        if result <= 0 || message.message == WM_QUIT {
            break;
        }
        if message.message == COMMAND_MESSAGE {
            while let Ok(request) = command_rx.try_recv() {
                if claim_request(&request.status) {
                    process_command(request.command, hwnd, &state);
                }
            }
        } else {
            // SAFETY: `message` was filled by GetMessageW and belongs to this thread.
            unsafe {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
    drop(listener_window);
    if let Ok(mut delayed_state) = state.delayed.state.lock() {
        delayed_state.formats.clear();
        delayed_state.current_marker = None;
        delayed_state.deadline = None;
    };
}

/// Owns the listener window and registration on the thread that created it.
struct ListenerWindow {
    hwnd: HWND,
    registered: Cell<bool>,
}

impl ListenerWindow {
    fn new(hwnd: HWND) -> Self {
        Self {
            hwnd,
            registered: Cell::new(false),
        }
    }
}

impl Drop for ListenerWindow {
    fn drop(&mut self) {
        if self.registered.get() {
            // SAFETY: this guard is dropped on the creating thread and owns the registration.
            unsafe { RemoveClipboardFormatListener(self.hwnd) };
        }
        // SAFETY: the guard uniquely owns this live HWND and destroys it exactly once.
        unsafe { DestroyWindow(self.hwnd) };
    }
}

fn register_window_class(class_name: PCWSTR) -> Result<(), BackendError> {
    static REGISTERED: OnceLock<Result<(), String>> = OnceLock::new();
    let result = REGISTERED.get_or_init(|| {
        // SAFETY: a null module name requests this process's borrowed module handle; no ownership transfers.
        let instance = unsafe { GetModuleHandleW(null()) };
        if instance.is_null() {
            return Err("cannot load Windows module".to_owned());
        }
        let class = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(window_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: null_mut(),
            hCursor: null_mut(),
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: class_name,
        };
        // SAFETY: class_name is a valid null-terminated string alive for this call and the
        // WNDCLASSW contains a valid callback and module handle.
        if unsafe { RegisterClassW(&class) } == 0 {
            // SAFETY: GetLastError returns this thread's scalar error code from RegisterClassW.
            let error = unsafe { GetLastError() };
            if error != 1410 {
                return Err("cannot register clipboard listener window".to_owned());
            }
        }
        Ok(())
    });
    if let Err(message) = result {
        Err(BackendError::Failed(message.clone()))
    } else {
        Ok(())
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: hwnd is the window supplied by USER32; querying its user-data slot does not
    // dereference the stored pointer and is valid during this window procedure call.
    let existing_state = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) };
    if message == WM_NCCREATE && existing_state == 0 {
        // The system sends WM_NCCREATE synchronously from CreateWindowExW before the HWND is
        // published. Ignore repeated/forged WM_NCCREATE messages once the state is installed.
        // SAFETY: only the early creation message for our newly-created HWND supplies this
        // CREATESTRUCTW; its lpCreateParams points to the Box held through the message loop.
        let create = unsafe { &*(lparam as *const CREATESTRUCTW) };
        let state = create.lpCreateParams as *mut WindowState;
        if !state.is_null() {
            // SAFETY: the pointer is the stable shared-state allocation supplied to CreateWindowExW.
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize) };
        }
    }
    // SAFETY: GWLP_USERDATA was initialized from a live Box before this window began dispatching.
    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowState };
    if state_ptr.is_null() {
        // SAFETY: forwarding unknown early messages to the system default window procedure.
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    // SAFETY: the Box remains live for the complete message loop and is only accessed on this thread.
    let state = unsafe { &*state_ptr };
    match message {
        WM_CLIPBOARDUPDATE => {
            // SAFETY: GetClipboardSequenceNumber reads a system scalar and retains no handles.
            let sequence = unsafe { GetClipboardSequenceNumber() };
            let previous = state.last_sequence.swap(sequence, Ordering::AcqRel);
            if sequence != previous {
                push_bounded_latest(
                    &state.notices_tx,
                    &state.notices_rx,
                    Notice::Changed(sequence),
                );
            }
            0
        }
        WM_RENDERFORMAT => {
            render_delayed(hwnd, state, wparam as u32);
            0
        }
        WM_RENDERALLFORMATS => {
            render_all_delayed(hwnd, state);
            0
        }
        WM_DESTROYCLIPBOARD => {
            clear_delayed(hwnd, state);
            0
        }
        WM_TIMER if wparam == DELAYED_TIMEOUT_TIMER => {
            expire_delayed(hwnd, state);
            0
        }
        WM_NCDESTROY => {
            // SAFETY: clear the borrowed pointer before Windows finishes destroying this HWND.
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
            // SAFETY: let Windows perform the default teardown behavior.
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
        _ => {
            // SAFETY: this window has no custom handling for the remaining system messages.
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
    }
}

fn push_bounded_latest<T>(tx: &Sender<T>, rx: &Receiver<T>, value: T)
where
    T: Clone,
{
    match tx.try_send(value.clone()) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            let _ = rx.try_recv();
            let _ = tx.try_send(value);
        }
        Err(TrySendError::Disconnected(_)) => {}
    }
}

fn push_event(tx: &Sender<ClipboardEvent>, rx: &Receiver<ClipboardEvent>, event: ClipboardEvent) {
    match tx.try_send(event) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            let _ = rx.try_recv();
            let _ = tx.try_send(ClipboardEvent::ResyncRequired);
        }
        Err(TrySendError::Disconnected(_)) => {}
    }
}

fn event_thread(
    notices_rx: Receiver<Notice>,
    command_tx: Sender<CommandRequest>,
    thread_id: u32,
    events_tx: Sender<ClipboardEvent>,
    events_rx: Receiver<ClipboardEvent>,
    stopping: Arc<AtomicBool>,
) {
    while !stopping.load(Ordering::Acquire) {
        let notice = match notices_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(notice) => notice,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        };
        let Notice::Changed(sequence) = notice else {
            break;
        };
        let (reply_tx, reply_rx) = bounded(1);
        let request_status = Arc::new(AtomicU8::new(REQUEST_PENDING));
        if command_tx
            .try_send(CommandRequest {
                command: Command::Snapshot(reply_tx),
                status: Arc::clone(&request_status),
            })
            .is_err()
        {
            push_event(&events_tx, &events_rx, ClipboardEvent::ResyncRequired);
            continue;
        }
        // SAFETY: the listener thread owns this message queue while the command sender is live.
        if unsafe { PostThreadMessageW(thread_id, COMMAND_MESSAGE, 0, 0) } == 0 {
            match request_status.compare_exchange(
                REQUEST_PENDING,
                REQUEST_CANCELED,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    push_event(&events_tx, &events_rx, ClipboardEvent::ResyncRequired);
                    break;
                }
                Err(REQUEST_STARTED) => {}
                Err(_) => {
                    push_event(&events_tx, &events_rx, ClipboardEvent::ResyncRequired);
                    break;
                }
            }
        }
        let snapshot = match reply_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok(snapshot)) => snapshot,
            _ => {
                push_event(&events_tx, &events_rx, ClipboardEvent::ResyncRequired);
                continue;
            }
        };
        // SAFETY: GetClipboardSequenceNumber reads a system scalar and retains no handles.
        if unsafe { GetClipboardSequenceNumber() } < sequence {
            continue;
        }
        push_event(
            &events_tx,
            &events_rx,
            ClipboardEvent::Changed {
                marker: snapshot.marker,
                formats: snapshot.formats,
                sensitivity: snapshot.sensitivity,
            },
        );
    }
}

fn process_command(command: Command, hwnd: HWND, window: &WindowState) {
    match command {
        Command::ReadSnapshot(reply) => {
            let _ = reply.send(read_bundle(hwnd, &window.current_marker.get()));
        }
        Command::PublishSnapshot(contents, marker, expected, admission, reply) => {
            let _ = reply.send(publish_bundle(
                hwnd, window, contents, marker, expected, admission,
            ));
        }
        Command::Snapshot(reply) => {
            let marker = window.current_marker.get();
            let _ = reply.send(snapshot(hwnd, &marker));
        }
        Command::Read(format, reply) => {
            let _ = reply.send(read_content(hwnd, format));
        }
        Command::ReadFiles(reply) => {
            let _ = reply.send(read_files(hwnd));
        }
        Command::Write(content, marker, reply) => {
            let result = write_content(
                hwnd,
                &window.delayed,
                &window.current_marker,
                content,
                marker,
            );
            let _ = reply.send(result);
        }
        Command::WriteFiles(files, marker, reply) => {
            let result = write_files(hwnd, &window.delayed, &window.current_marker, files, marker);
            let _ = reply.send(result);
        }
        Command::SetDelayed(format, marker, reply) => {
            let result = set_delayed(
                hwnd,
                &window.delayed,
                &window.current_marker,
                format,
                marker,
            );
            let _ = reply.send(result);
        }
        Command::Fulfill(content, marker, reply) => {
            let result = fulfill_delayed(
                hwnd,
                &window.delayed,
                &window.timeout_timer_set,
                content,
                marker,
            );
            let _ = reply.send(result);
        }
        Command::CancelDelayed(marker, reply) => {
            let result = cancel_delayed(
                hwnd,
                &window.delayed,
                &window.current_marker,
                &window.timeout_timer_set,
                marker,
            );
            let _ = reply.send(result);
        }
        #[cfg(test)]
        Command::Clear(reply) => {
            let result = clear_clipboard(hwnd, window);
            let _ = reply.send(result);
        }
    }
}

fn snapshot(
    hwnd: HWND,
    current_marker: &Option<ClipboardMarker>,
) -> Result<Snapshot, BackendError> {
    let _open = open_clipboard(hwnd)?;
    snapshot_open(hwnd, current_marker)
}

fn snapshot_open(
    hwnd: HWND,
    current_marker: &Option<ClipboardMarker>,
) -> Result<Snapshot, BackendError> {
    let mut ids = Vec::new();
    let mut format_id = 0u32;
    loop {
        // SAFETY: the clipboard is open on this thread and EnumClipboardFormats takes a format id.
        format_id = unsafe { EnumClipboardFormats(format_id) };
        if format_id == 0 {
            break;
        }
        if ids.len() == MAX_CLIPBOARD_FORMATS {
            return Err(BackendError::Failed(
                "clipboard exposes too many formats".into(),
            ));
        }
        ids.push(format_id);
    }
    let marker_format = register_format(CLIPBOARD_MARKER_NAME)?;
    // SAFETY: the clipboard is open through `_open`; the returned HWND is borrowed and only
    // compared with this backend's own live message-only window.
    let owner = unsafe { GetClipboardOwner() };
    let marker = if owner == hwnd && current_marker.is_some() {
        read_u64(marker_format)?.and_then(|value| {
            (Some(ClipboardMarker(value)) == *current_marker).then_some(ClipboardMarker(value))
        })
    } else {
        None
    };
    let sensitivity = read_sensitivity()?;
    let excluded: BTreeSet<u32> = [
        marker_format,
        register_format(SENSITIVE_MONITOR_NAME)?,
        register_format(SENSITIVE_HISTORY_NAME)?,
        register_format(DROP_EFFECT_NAME)?,
    ]
    .into_iter()
    .collect();
    let mut formats = BTreeSet::new();
    for id in ids {
        if excluded.contains(&id) {
            continue;
        }
        if let Some(format) = normalize_format(id)? {
            formats.insert(format);
        }
    }
    Ok(Snapshot {
        formats: formats.into_iter().collect(),
        sensitivity,
        marker,
    })
}

fn normalize_format(id: u32) -> Result<Option<ClipboardFormat>, BackendError> {
    if id == CF_UNICODETEXT_ID {
        return Ok(Some(ClipboardFormat::Text));
    }
    if id == CF_HDROP_ID {
        return Ok(Some(ClipboardFormat::Files));
    }
    if id == CF_DIBV5_ID || id == CF_DIB_ID {
        return Ok(Some(ClipboardFormat::Png));
    }
    let name = clipboard_format_name(id)?;
    if name == HTML_FORMAT_NAME {
        return Ok(Some(ClipboardFormat::Html));
    }
    if name == RTF_FORMAT_NAME {
        return Ok(Some(ClipboardFormat::Rtf));
    }
    if name == PNG_FORMAT_NAME {
        return Ok(Some(ClipboardFormat::Png));
    }
    if id < FORMAT_PRIVATE_FIRST || (FORMAT_PRIVATE_FIRST..=FORMAT_PRIVATE_LAST).contains(&id) {
        return Ok(None);
    }
    if name.is_empty() {
        return Ok(None);
    }
    Ok(Some(ClipboardFormat::Other(name)))
}

fn read_content(
    hwnd: HWND,
    format: ClipboardFormat,
) -> Result<Option<ClipboardContent>, BackendError> {
    let _open = open_clipboard(hwnd)?;
    read_content_open(hwnd, format)
}

fn read_content_open(
    hwnd: HWND,
    format: ClipboardFormat,
) -> Result<Option<ClipboardContent>, BackendError> {
    let sensitivity = read_sensitivity()?;
    if sensitivity.should_exclude() {
        return Ok(None);
    }
    let content = match format {
        ClipboardFormat::Text => {
            let bytes = if clipboard_format_available(CF_UNICODETEXT_ID) {
                let raw = read_global_bytes(CF_UNICODETEXT_ID)?;
                let (pairs, remainder) = raw.as_chunks::<2>();
                if !remainder.is_empty() {
                    return Err(BackendError::Failed(
                        "clipboard UTF-16 text is truncated".into(),
                    ));
                }
                let units = pairs.iter().map(|pair| u16::from_ne_bytes(*pair));
                let text: Vec<u16> = units.take_while(|unit| *unit != 0).collect();
                String::from_utf16_lossy(&text).into_bytes()
            } else {
                return Ok(None);
            };
            ClipboardContent::bytes(ClipboardFormat::Text, bytes, sensitivity)?
        }
        ClipboardFormat::Html => {
            let id = register_format(HTML_FORMAT_NAME)?;
            if !clipboard_format_available(id) {
                return Ok(None);
            }
            let raw = read_global_bytes(id)?;
            ClipboardContent::bytes(ClipboardFormat::Html, cf_html_decode(&raw)?, sensitivity)?
        }
        ClipboardFormat::Rtf => {
            let id = register_format(RTF_FORMAT_NAME)?;
            if !clipboard_format_available(id) {
                return Ok(None);
            }
            ClipboardContent::bytes(ClipboardFormat::Rtf, read_global_bytes(id)?, sensitivity)?
        }
        ClipboardFormat::Png => {
            let png = register_format(PNG_FORMAT_NAME)?;
            let bytes = if clipboard_format_available(png) {
                read_global_bytes(png)?
            } else if clipboard_format_available(CF_DIBV5_ID) {
                dib_to_png(&read_global_bytes(CF_DIBV5_ID)?)?
            } else if clipboard_format_available(CF_DIB_ID) {
                dib_to_png(&read_global_bytes(CF_DIB_ID)?)?
            } else {
                return Ok(None);
            };
            ClipboardContent::bytes(ClipboardFormat::Png, bytes, sensitivity)?
        }
        ClipboardFormat::Other(name) => {
            let id = register_format(&name)?;
            if !clipboard_format_available(id) {
                return Ok(None);
            }
            ClipboardContent::bytes(
                ClipboardFormat::Other(name),
                read_global_bytes(id)?,
                sensitivity,
            )?
        }
        ClipboardFormat::Files => {
            return Ok(read_files_open(hwnd, sensitivity)?.map(ClipboardContent::files))
        }
    };
    Ok(Some(content))
}

fn read_files(hwnd: HWND) -> Result<Option<FileList>, BackendError> {
    let _open = open_clipboard(hwnd)?;
    let sensitivity = read_sensitivity()?;
    if sensitivity.should_exclude() {
        return Ok(None);
    }
    read_files_open(hwnd, sensitivity)
}

fn read_files_open(
    _hwnd: HWND,
    sensitivity: ClipboardSensitivity,
) -> Result<Option<FileList>, BackendError> {
    if !clipboard_format_available(CF_HDROP_ID) {
        return Ok(None);
    }
    let data = read_global_bytes(CF_HDROP_ID)?;
    let validated_count = validate_drop_files(&data)?;
    // SAFETY: the caller keeps the clipboard open for this function, and CF_HDROP data was
    // already copied and structurally bounded before the borrowed clipboard handle is queried.
    let handle = unsafe { GetClipboardData(CF_HDROP_ID) };
    if handle.is_null() {
        return Err(BackendError::Unavailable);
    }
    // SAFETY: CF_HDROP is a system-defined clipboard format and the clipboard is open. Its
    // DROPFILES header and bounded filename list were validated against GlobalSize above.
    let drop = handle;
    // SAFETY: `drop` is the current clipboard's CF_HDROP handle; the backing DROPFILES payload
    // was validated and the null output requests only its bounded item count.
    let count = unsafe { DragQueryFileW(drop, u32::MAX, null_mut(), 0) };
    if count > MAX_FILE_ENTRIES || count != validated_count {
        return Err(BackendError::Failed(
            "clipboard file list count is invalid".into(),
        ));
    }
    let mut entries = Vec::with_capacity(count as usize);
    for index in 0..count {
        // SAFETY: querying the filename with a null output buffer returns its UTF-16 length.
        let length = unsafe { DragQueryFileW(drop, index, null_mut(), 0) } as usize;
        if length == 0 || length >= MAX_PATH_UNITS {
            return Err(BackendError::Failed(
                "clipboard file path is invalid or too long".into(),
            ));
        }
        let mut wide = vec![0u16; length + 1];
        // SAFETY: `wide` has length+1 writable UTF-16 units as requested by DragQueryFileW.
        let written =
            unsafe { DragQueryFileW(drop, index, wide.as_mut_ptr(), wide.len() as u32) } as usize;
        if written != length {
            return Err(BackendError::Failed(
                "clipboard file path changed during read".into(),
            ));
        }
        let path = PathBuf::from(std::ffi::OsString::from_wide(&wide[..written]));
        let metadata = std::fs::symlink_metadata(&path).ok();
        let is_dir = metadata.as_ref().is_some_and(|m| m.is_dir());
        let size = metadata
            .as_ref()
            .filter(|m| m.is_file())
            .map_or(0, |m| m.len());
        let name = path
            .file_name()
            .map(|value| value.to_string_lossy().into_owned())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| BackendError::Failed("clipboard file path has no basename".into()))?;
        entries.push(FileEntry {
            path,
            name,
            size,
            is_dir,
        });
    }
    Ok(Some(FileList {
        entries,
        sensitivity,
    }))
}

fn write_content(
    hwnd: HWND,
    delayed: &Arc<DelayedStore>,
    current_marker: &Cell<Option<ClipboardMarker>>,
    content: ClipboardContent,
    marker: ClipboardMarker,
) -> Result<(), BackendError> {
    content.validate()?;
    match &content.data {
        ClipboardData::Files(files) => {
            write_files(hwnd, delayed, current_marker, files.clone(), marker)
        }
        ClipboardData::Bytes(bytes) => {
            validate_payload_size(bytes.len())?;
            let reps = native_representations(&content.format, bytes)?;
            let _open = open_clipboard(hwnd)?;
            ensure_clipboard_owner(hwnd, delayed, current_marker, marker)?;
            if content.sensitivity.should_exclude() {
                write_sensitivity_markers()?;
            }
            for (format_id, bytes) in reps {
                write_global_bytes(format_id, &bytes)?;
            }
            Ok(())
        }
    }
}

fn read_bundle(
    hwnd: HWND,
    marker: &Option<ClipboardMarker>,
) -> Result<ClipboardSnapshot, BackendError> {
    let _open = open_clipboard(hwnd)?;
    // SAFETY: query has no pointer arguments; delayed owners may materialize data during reads.
    let before = unsafe { GetClipboardSequenceNumber() };
    let snapshot = snapshot_open(hwnd, marker)?;
    if snapshot.formats.len() > 32 {
        return Err(BackendError::InvalidInput(
            "too many clipboard formats".into(),
        ));
    }
    let mut contents = Vec::new();
    if !snapshot.sensitivity.should_exclude() {
        for format in snapshot.formats {
            if let Some(content) = read_content_open(hwnd, format)? {
                contents.push(content);
            }
            validate_clipboard_bundle(&contents)?;
        }
    }
    // SAFETY: clipboard remains exclusively open on this thread for the whole snapshot.
    let change_token = ClipboardChangeToken(u64::from(unsafe { GetClipboardSequenceNumber() }));
    if change_token.0 != u64::from(before) {
        return Err(BackendError::ClipboardChanged);
    }
    Ok(ClipboardSnapshot {
        contents,
        marker: snapshot.marker,
        sensitivity: snapshot.sensitivity,
        change_token,
    })
}

fn publish_bundle(
    hwnd: HWND,
    window: &WindowState,
    contents: Vec<ClipboardContent>,
    marker: ClipboardMarker,
    expected: ClipboardChangeToken,
    admission: ClipboardAdmission,
) -> Result<ClipboardPublish, BackendError> {
    validate_clipboard_bundle(&contents)?;
    let mut representations = Vec::new();
    let sensitive = contents.iter().any(|c| c.sensitivity.should_exclude());
    for content in contents {
        match content.data {
            ClipboardData::Bytes(bytes) => {
                representations.extend(native_representations(&content.format, &bytes)?)
            }
            ClipboardData::Files(files) => {
                representations.push((CF_HDROP_ID, build_drop_files(&files.entries)?));
                representations.push((
                    register_format(DROP_EFFECT_NAME)?,
                    1u32.to_le_bytes().to_vec(),
                ));
            }
        }
    }
    representations.push((
        register_format(CLIPBOARD_MARKER_NAME)?,
        marker.0.to_le_bytes().to_vec(),
    ));
    if sensitive {
        representations.push((
            register_format(SENSITIVE_MONITOR_NAME)?,
            1u32.to_le_bytes().to_vec(),
        ));
        representations.push((
            register_format(SENSITIVE_HISTORY_NAME)?,
            0u32.to_le_bytes().to_vec(),
        ));
    }
    let mut ids = BTreeSet::new();
    let mut total = 0usize;
    let mut prepared = Vec::with_capacity(representations.len());
    for (id, bytes) in representations {
        total = total
            .checked_add(bytes.len())
            .filter(|size| *size <= MAX_CLIPBOARD_BYTES)
            .ok_or_else(|| {
                BackendError::InvalidInput("native clipboard bundle exceeds size limit".into())
            })?;
        if !ids.insert(id) {
            return Err(BackendError::InvalidInput(
                "colliding native clipboard types".into(),
            ));
        }
        prepared.push((id, allocate_global(&bytes)?));
    }
    let _open = open_clipboard(hwnd)?;
    // SAFETY: query has no pointer arguments and runs while clipboard ownership is locked.
    let actual = ClipboardChangeToken(u64::from(unsafe { GetClipboardSequenceNumber() }));
    // Only a copy made on this computer wins over the incoming clipboard. A change Glide made itself (an earlier
    // incoming clipboard landing while this one was on its way) leaves Glide as the owner and must not drop this one.
    // SAFETY: query has no pointer arguments and runs while the clipboard is open.
    let glide_owned = unsafe { GetClipboardOwner() } == hwnd;
    if expected != actual && !glide_owned {
        return Ok(ClipboardPublish::ReplacedLocalChange {
            actual_change_token: actual,
        });
    }
    if !admission.is_admitted() {
        return Ok(ClipboardPublish::Revoked);
    }
    // SAFETY: all movable handles were prepared before this ownership change; clipboard is open.
    if unsafe { EmptyClipboard() } == 0 {
        return Err(last_error("cannot acquire clipboard ownership"));
    }
    window.current_marker.set(None);
    clear_delayed(hwnd, window);
    for (written, (id, handle)) in prepared.into_iter().enumerate() {
        // SAFETY: clipboard is open; successful SetClipboardData takes this owned HGLOBAL.
        if unsafe { SetClipboardData(id, handle.as_handle()) }.is_null() {
            let error = last_error("clipboard bundle publication failed");
            // SAFETY: still within the same OpenClipboard transaction; discard partial representations.
            let cleared = unsafe { EmptyClipboard() } != 0;
            return Ok(ClipboardPublish::PartialFailure {
                formats_written: written,
                cleared,
                error,
            });
        }
        handle.disarm();
    }
    window.current_marker.set(Some(marker));
    // SAFETY: sequence number is queried before closing our single transaction.
    Ok(ClipboardPublish::Published {
        change_token: ClipboardChangeToken(u64::from(unsafe { GetClipboardSequenceNumber() })),
    })
}

fn native_representations(
    format: &ClipboardFormat,
    bytes: &[u8],
) -> Result<Vec<(u32, Vec<u8>)>, BackendError> {
    validate_payload_size(bytes.len())?;
    match format {
        ClipboardFormat::Text => {
            let text = std::str::from_utf8(bytes).map_err(|_| {
                BackendError::InvalidInput("text clipboard payload is not UTF-8".into())
            })?;
            let mut wide: Vec<u16> = text.encode_utf16().collect();
            wide.push(0);
            let raw = wide.into_iter().flat_map(u16::to_ne_bytes).collect();
            Ok(vec![(CF_UNICODETEXT_ID, raw)])
        }
        ClipboardFormat::Html => Ok(vec![(
            register_format(HTML_FORMAT_NAME)?,
            cf_html_encode(bytes)?,
        )]),
        ClipboardFormat::Rtf => Ok(vec![(register_format(RTF_FORMAT_NAME)?, bytes.to_vec())]),
        ClipboardFormat::Png => {
            let dib = png_to_dib(bytes)?;
            validate_payload_size(dib.len())?;
            Ok(vec![
                (register_format(PNG_FORMAT_NAME)?, bytes.to_vec()),
                (CF_DIBV5_ID, dib),
            ])
        }
        ClipboardFormat::Other(name) => Ok(vec![(register_format(name)?, bytes.to_vec())]),
        ClipboardFormat::Files => Err(BackendError::InvalidInput(
            "file format requires a file-list payload".into(),
        )),
    }
}

fn write_files(
    hwnd: HWND,
    delayed: &Arc<DelayedStore>,
    current_marker: &Cell<Option<ClipboardMarker>>,
    files: FileList,
    marker: ClipboardMarker,
) -> Result<(), BackendError> {
    if files.entries.len() > MAX_FILE_ENTRIES as usize {
        return Err(BackendError::InvalidInput(
            "too many clipboard file entries".into(),
        ));
    }
    let drop_bytes = build_drop_files(&files.entries)?;
    let _open = open_clipboard(hwnd)?;
    ensure_clipboard_owner(hwnd, delayed, current_marker, marker)?;
    if files.sensitivity.should_exclude() {
        write_sensitivity_markers()?;
    }
    write_global_bytes(CF_HDROP_ID, &drop_bytes)?;
    let effect_id = register_format(DROP_EFFECT_NAME)?;
    write_global_bytes(effect_id, &1u32.to_le_bytes())?;
    Ok(())
}

#[repr(C)]
struct DropFilesHeader {
    files_offset: u32,
    x: i32,
    y: i32,
    non_client: i32,
    wide: i32,
}

fn build_drop_files(entries: &[FileEntry]) -> Result<Vec<u8>, BackendError> {
    let mut names = Vec::with_capacity(entries.len());
    let mut total_units = 1usize;
    for entry in entries {
        let path = &entry.path;
        if !path.is_absolute() {
            return Err(BackendError::InvalidInput(
                "clipboard file paths must be absolute".into(),
            ));
        }
        let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        if wide.is_empty() || wide.contains(&0) || wide.len() >= MAX_PATH_UNITS {
            return Err(BackendError::InvalidInput(
                "clipboard file path is invalid or too long".into(),
            ));
        }
        total_units = total_units
            .checked_add(wide.len() + 1)
            .ok_or_else(|| BackendError::InvalidInput("clipboard file list is too large".into()))?;
        names.push(wide);
    }
    if names.is_empty() {
        return Err(BackendError::InvalidInput(
            "clipboard file list cannot be empty".into(),
        ));
    }
    let total_bytes = size_of::<DropFilesHeader>()
        .checked_add(total_units * size_of::<u16>())
        .filter(|size| *size <= MAX_CLIPBOARD_BYTES)
        .ok_or_else(|| {
            BackendError::InvalidInput("clipboard file list exceeds size limit".into())
        })?;
    let header = DropFilesHeader {
        files_offset: size_of::<DropFilesHeader>() as u32,
        x: 0,
        y: 0,
        non_client: 0,
        wide: 1,
    };
    let mut output = Vec::with_capacity(total_bytes);
    // SAFETY: the header is a plain C-layout struct of integer fields and is serialized locally.
    let header_bytes = unsafe {
        slice::from_raw_parts(
            (&header as *const DropFilesHeader).cast::<u8>(),
            size_of::<DropFilesHeader>(),
        )
    };
    output.extend_from_slice(header_bytes);
    for name in &names {
        output.extend(name.iter().flat_map(|unit| unit.to_ne_bytes()));
        output.extend_from_slice(&[0, 0]);
    }
    output.extend_from_slice(&[0, 0]);
    Ok(output)
}

fn validate_drop_files(bytes: &[u8]) -> Result<u32, BackendError> {
    const HEADER_SIZE: usize = size_of::<DropFilesHeader>();
    if bytes.len() < HEADER_SIZE {
        return Err(BackendError::Failed(
            "clipboard DROPFILES header is truncated".into(),
        ));
    }
    let files_offset = u32::from_ne_bytes(
        bytes[0..4]
            .try_into()
            .map_err(|_| BackendError::Failed("clipboard DROPFILES header is invalid".into()))?,
    ) as usize;
    let wide = i32::from_ne_bytes(
        bytes[16..20]
            .try_into()
            .map_err(|_| BackendError::Failed("clipboard DROPFILES header is invalid".into()))?,
    );
    if files_offset < HEADER_SIZE || files_offset >= bytes.len() || !matches!(wide, 0 | 1) {
        return Err(BackendError::Failed(
            "clipboard DROPFILES header is invalid".into(),
        ));
    }
    let mut count = 0u32;
    let mut path_units = 0usize;
    let mut previous_was_nul = false;
    if wide == 1 {
        if files_offset & 1 != 0 || (bytes.len() - files_offset) & 1 != 0 {
            return Err(BackendError::Failed(
                "clipboard DROPFILES UTF-16 data is misaligned".into(),
            ));
        }
        let (pairs, _) = bytes[files_offset..].as_chunks::<2>();
        for pair in pairs {
            let unit = u16::from_ne_bytes([pair[0], pair[1]]);
            if unit == 0 {
                if previous_was_nul {
                    if count == 0 {
                        return Err(BackendError::Failed("clipboard file list is empty".into()));
                    }
                    return Ok(count);
                }
                if path_units == 0 || path_units >= MAX_PATH_UNITS {
                    return Err(BackendError::Failed(
                        "clipboard file path is invalid".into(),
                    ));
                }
                count = count.checked_add(1).ok_or_else(|| {
                    BackendError::Failed("clipboard file list count overflowed".into())
                })?;
                if count > MAX_FILE_ENTRIES {
                    return Err(BackendError::Failed(
                        "clipboard file list exceeds item limit".into(),
                    ));
                }
                path_units = 0;
                previous_was_nul = true;
            } else {
                path_units += 1;
                previous_was_nul = false;
            }
        }
    } else {
        for byte in &bytes[files_offset..] {
            if *byte == 0 {
                if previous_was_nul {
                    if count == 0 {
                        return Err(BackendError::Failed("clipboard file list is empty".into()));
                    }
                    return Ok(count);
                }
                if path_units == 0 || path_units >= MAX_PATH_UNITS {
                    return Err(BackendError::Failed(
                        "clipboard file path is invalid".into(),
                    ));
                }
                count = count.checked_add(1).ok_or_else(|| {
                    BackendError::Failed("clipboard file list count overflowed".into())
                })?;
                if count > MAX_FILE_ENTRIES {
                    return Err(BackendError::Failed(
                        "clipboard file list exceeds item limit".into(),
                    ));
                }
                path_units = 0;
                previous_was_nul = true;
            } else {
                path_units += 1;
                previous_was_nul = false;
            }
        }
    }
    Err(BackendError::Failed(
        "clipboard DROPFILES list is unterminated".into(),
    ))
}

fn set_delayed(
    hwnd: HWND,
    delayed: &Arc<DelayedStore>,
    current_marker: &Cell<Option<ClipboardMarker>>,
    format: ClipboardFormat,
    marker: ClipboardMarker,
) -> Result<(), BackendError> {
    if format == ClipboardFormat::Files {
        return Err(BackendError::Unsupported);
    }
    let native_formats = delayed_native_format_ids(&format)?;
    let _open = open_clipboard(hwnd)?;
    ensure_clipboard_owner(hwnd, delayed, current_marker, marker)?;
    for native_format in native_formats {
        // SAFETY: NULL requests standard Windows delayed rendering for this registered format.
        let _ = unsafe { SetClipboardData(native_format, null_mut()) };
        if !clipboard_format_available(native_format) {
            return Err(last_error("cannot register delayed clipboard format"));
        }
    }
    {
        let mut state = delayed
            .state
            .lock()
            .map_err(|_| BackendError::StateUnavailable)?;
        state.current_marker = Some(marker);
        for native_format in delayed_native_format_ids(&format)? {
            state.formats.insert(
                native_format,
                DelayedFormat {
                    marker,
                    format: format.clone(),
                    bytes: None,
                },
            );
        }
    }
    Ok(())
}

fn delayed_native_format_ids(format: &ClipboardFormat) -> Result<Vec<u32>, BackendError> {
    match format {
        ClipboardFormat::Text => Ok(vec![CF_UNICODETEXT_ID]),
        ClipboardFormat::Html => Ok(vec![register_format(HTML_FORMAT_NAME)?]),
        ClipboardFormat::Rtf => Ok(vec![register_format(RTF_FORMAT_NAME)?]),
        ClipboardFormat::Png => Ok(vec![register_format(PNG_FORMAT_NAME)?, CF_DIBV5_ID]),
        ClipboardFormat::Other(name) => Ok(vec![register_format(name)?]),
        ClipboardFormat::Files => Err(BackendError::Unsupported),
    }
}

fn fulfill_delayed(
    hwnd: HWND,
    delayed: &Arc<DelayedStore>,
    timeout_timer_set: &Cell<bool>,
    content: ClipboardContent,
    marker: ClipboardMarker,
) -> Result<(), BackendError> {
    content.validate()?;
    let ClipboardData::Bytes(bytes) = &content.data else {
        return Err(BackendError::InvalidInput(
            "delayed file rendering is unsupported".into(),
        ));
    };
    let native = native_representations(&content.format, bytes)?;
    let mut state = delayed
        .state
        .lock()
        .map_err(|_| BackendError::StateUnavailable)?;
    if state.current_marker != Some(marker) {
        return Err(BackendError::InvalidInput(
            "delayed clipboard marker is no longer active".into(),
        ));
    }
    let mut native_to_render = Vec::new();
    for (format_id, bytes) in native {
        if let Some(entry) = state.formats.get_mut(&format_id) {
            if entry.marker == marker && entry.format == content.format {
                let bytes: Arc<[u8]> = Arc::from(bytes);
                entry.bytes = Some(Arc::clone(&bytes));
                native_to_render.push((format_id, bytes));
            }
        }
    }
    if native_to_render.is_empty() {
        return Err(BackendError::InvalidInput(
            "format was not registered for delayed rendering".into(),
        ));
    }
    if state.formats.values().all(|entry| entry.bytes.is_some()) {
        state.deadline = None;
        if timeout_timer_set.get() {
            // SAFETY: this window owns the expiration timer on the same thread.
            unsafe { KillTimer(hwnd, DELAYED_TIMEOUT_TIMER) };
            timeout_timer_set.set(false);
        }
    }
    drop(state);
    if let Ok(_open) = open_clipboard(hwnd) {
        // SAFETY: `_open` owns the open clipboard; this returns a borrowed owner HWND compared only.
        if unsafe { GetClipboardOwner() } == hwnd {
            for (format_id, bytes) in &native_to_render {
                // Failed eager installs remain in the checked cache and can be retried by a
                // later WM_RENDERFORMAT/WM_RENDERALLFORMATS request.
                let _ = write_global_bytes(*format_id, bytes);
            }
        }
    }
    let delayed = delayed
        .state
        .try_lock()
        .map_err(|_| BackendError::StateUnavailable)?;
    let complete_cache = delayed.current_marker == Some(marker)
        && native_to_render.iter().all(|(format_id, _)| {
            delayed.formats.get(format_id).is_some_and(|entry| {
                entry.marker == marker && entry.format == content.format && entry.bytes.is_some()
            })
        });
    if complete_cache {
        Ok(())
    } else {
        Err(BackendError::Unavailable)
    }
}

fn cancel_delayed(
    hwnd: HWND,
    delayed: &Arc<DelayedStore>,
    current_marker: &Cell<Option<ClipboardMarker>>,
    timeout_timer_set: &Cell<bool>,
    marker: ClipboardMarker,
) -> Result<(), BackendError> {
    let cancelled = {
        let mut state = delayed
            .state
            .lock()
            .map_err(|_| BackendError::StateUnavailable)?;
        if state.current_marker == Some(marker) {
            state.formats.clear();
            state.current_marker = None;
            state.deadline = None;
            true
        } else {
            false
        }
    };
    if current_marker.get() == Some(marker) {
        let _open = open_clipboard(hwnd)?;
        // SAFETY: the clipboard is open and owned by this backend's listener window.
        if unsafe { GetClipboardOwner() } == hwnd && unsafe { EmptyClipboard() } == 0 {
            return Err(last_error("cannot clear delayed clipboard data"));
        }
        current_marker.set(None);
    }
    if cancelled && timeout_timer_set.get() {
        // SAFETY: this window owns the expiration timer on the same thread.
        unsafe { KillTimer(hwnd, DELAYED_TIMEOUT_TIMER) };
        timeout_timer_set.set(false);
    }
    Ok(())
}

#[cfg(test)]
fn clear_clipboard(hwnd: HWND, window: &WindowState) -> Result<(), BackendError> {
    let _open = open_clipboard(hwnd)?;
    // SAFETY: clipboard is open; EmptyClipboard clears the current owner and all representations.
    if unsafe { EmptyClipboard() } == 0 {
        return Err(last_error("cannot clear clipboard contents"));
    }
    window.current_marker.set(None);
    clear_delayed(hwnd, window);
    Ok(())
}

fn ensure_clipboard_owner(
    hwnd: HWND,
    delayed: &Arc<DelayedStore>,
    current_marker: &Cell<Option<ClipboardMarker>>,
    marker: ClipboardMarker,
) -> Result<(), BackendError> {
    // SAFETY: every caller holds the clipboard open; the returned HWND is a borrowed owner token
    // compared only against this listener window.
    let is_same_owner = unsafe { GetClipboardOwner() } == hwnd;
    if !is_same_owner || current_marker.get() != Some(marker) {
        // SAFETY: the clipboard is open; EmptyClipboard assigns ownership to this HWND.
        if unsafe { EmptyClipboard() } == 0 {
            return Err(last_error("cannot replace clipboard contents"));
        }
        write_marker(marker)?;
        current_marker.set(Some(marker));
        let mut state = delayed
            .state
            .lock()
            .map_err(|_| BackendError::StateUnavailable)?;
        state.current_marker = Some(marker);
        state.formats.clear();
        state.deadline = None;
    } else if !clipboard_format_available(register_format(CLIPBOARD_MARKER_NAME)?) {
        write_marker(marker)?;
    }
    Ok(())
}

fn clear_delayed(hwnd: HWND, state: &WindowState) {
    if let Ok(mut delayed) = state.delayed.state.try_lock() {
        delayed.formats.clear();
        delayed.current_marker = None;
        delayed.deadline = None;
    }
    state.current_marker.set(None);
    if state.timeout_timer_set.replace(false) {
        // SAFETY: called on this window's own thread; KillTimer is nonblocking.
        unsafe { KillTimer(hwnd, DELAYED_TIMEOUT_TIMER) };
    }
}

fn render_delayed(hwnd: HWND, state: &WindowState, native_format: u32) {
    let DelayedPayload {
        marker,
        format,
        bytes,
    } = match delayed_payload(state, native_format) {
        Some(value) => value,
        None => return,
    };
    if let Some(bytes) = bytes {
        let _ = set_rendered_clipboard_data(native_format, &bytes);
        return;
    }
    push_event(
        &state.events_tx,
        &state.events_rx,
        ClipboardEvent::RenderRequested { marker, format },
    );
    let mut arm_timer = false;
    if let Ok(mut delayed) = state.delayed.state.try_lock() {
        if delayed.deadline.is_none() {
            delayed.deadline = Some(std::time::Instant::now() + DELAYED_CONTENT_TIMEOUT);
            arm_timer = true;
        }
    }
    if arm_timer && !state.timeout_timer_set.get() {
        // Start the expiration window only after the first actual paste request.
        // SAFETY: this owner window's timer is managed on its own message thread.
        if unsafe { SetTimer(hwnd, DELAYED_TIMEOUT_TIMER, 1000, None) } != 0 {
            state.timeout_timer_set.set(true);
        }
    }
    // WM_RENDERFORMAT is synchronous. Return now; a later fulfill command installs bytes
    // proactively, so this first paste may have no data while the subsequent paste succeeds.
}

struct DelayedPayload {
    marker: ClipboardMarker,
    format: ClipboardFormat,
    bytes: Option<Arc<[u8]>>,
}

fn delayed_payload(state: &WindowState, native_format: u32) -> Option<DelayedPayload> {
    let delayed = state.delayed.state.try_lock().ok()?;
    let entry = delayed.formats.get(&native_format)?;
    Some(DelayedPayload {
        marker: entry.marker,
        format: entry.format.clone(),
        bytes: entry.bytes.clone(),
    })
}

fn render_all_delayed(hwnd: HWND, state: &WindowState) {
    let formats: Vec<(u32, Arc<[u8]>)> = match state.delayed.state.try_lock() {
        Ok(delayed) => delayed
            .formats
            .iter()
            .filter_map(|(format, entry)| {
                entry
                    .bytes
                    .as_ref()
                    .map(|bytes| (*format, Arc::clone(bytes)))
            })
            .collect(),
        Err(_) => return,
    };
    if formats.is_empty() {
        return;
    }
    let Ok(_open) = open_clipboard_once(hwnd) else {
        return;
    };
    // SAFETY: `_open` owns the open clipboard and the result is only compared with this window.
    if unsafe { GetClipboardOwner() } != hwnd {
        return;
    }
    for (format, bytes) in formats {
        let _ = set_rendered_clipboard_data(format, &bytes);
    }
}

fn expire_delayed(hwnd: HWND, state: &WindowState) {
    let expired = match state.delayed.state.try_lock() {
        Ok(delayed) => delayed
            .deadline
            .is_some_and(|deadline| std::time::Instant::now() >= deadline),
        Err(std::sync::TryLockError::WouldBlock) => false,
        Err(std::sync::TryLockError::Poisoned(_)) => true,
    };
    if !expired {
        return;
    }
    // SAFETY: GetClipboardOwner returns a borrowed HWND token; equality is the only use here.
    if unsafe { GetClipboardOwner() } == hwnd {
        let Ok(_open) = open_clipboard_once(hwnd) else {
            return;
        };
        // SAFETY: `_open` owns the open clipboard; the returned owner HWND is compared only.
        if unsafe { GetClipboardOwner() } == hwnd {
            // SAFETY: the clipboard is open and remains owned by this listener window.
            unsafe { EmptyClipboard() };
        }
    }
    state.current_marker.set(None);
    clear_delayed(hwnd, state);
    // SAFETY: this window owns the expiration timer.
    unsafe { KillTimer(hwnd, DELAYED_TIMEOUT_TIMER) };
    state.timeout_timer_set.set(false);
}

fn set_rendered_clipboard_data(format: u32, bytes: &[u8]) -> Result<(), BackendError> {
    let handle = allocate_global(bytes)?;
    // SAFETY: the owner handles WM_RENDERFORMAT/WM_RENDERALLFORMATS and Windows transfers
    // ownership of this HGLOBAL to the clipboard when SetClipboardData succeeds.
    if unsafe { SetClipboardData(format, handle.as_handle()) }.is_null() {
        return Err(last_error("cannot render clipboard data"));
    }
    handle.disarm();
    Ok(())
}

fn open_clipboard(hwnd: HWND) -> Result<ClipboardGuard, BackendError> {
    const BACKOFF_MS: [u32; 6] = [2, 4, 8, 16, 32, 50];
    for delay in BACKOFF_MS {
        // SAFETY: hwnd is a live owner window or null for a read-only open; both are
        // documented OpenClipboard inputs. This call retains no Rust pointer.
        if unsafe { OpenClipboard(hwnd) } != 0 {
            return Ok(ClipboardGuard);
        }
        // SAFETY: Sleep receives a bounded millisecond backoff and does not busy-spin.
        unsafe { Sleep(delay) };
    }
    Err(BackendError::Unavailable)
}

fn open_clipboard_once(hwnd: HWND) -> Result<ClipboardGuard, BackendError> {
    // SAFETY: hwnd is this backend's live message-only listener window.
    if unsafe { OpenClipboard(hwnd) } != 0 {
        Ok(ClipboardGuard)
    } else {
        Err(BackendError::Unavailable)
    }
}

struct ClipboardGuard;

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        // SAFETY: constructed only after OpenClipboard succeeded; this releases that open.
        unsafe { CloseClipboard() };
    }
}

fn register_format(name: &str) -> Result<u32, BackendError> {
    if name.is_empty() || name.len() > 255 || name.contains('\0') {
        return Err(BackendError::InvalidInput(
            "clipboard format name is invalid".into(),
        ));
    }
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is a valid null-terminated UTF-16 clipboard format name.
    let id = unsafe { RegisterClipboardFormatW(wide.as_ptr()) };
    if id == 0 {
        return Err(last_error("cannot register clipboard format"));
    }
    Ok(id)
}

fn clipboard_format_name(id: u32) -> Result<String, BackendError> {
    let mut wide = vec![0u16; 256];
    // SAFETY: output buffer is writable for 256 UTF-16 code units.
    let length = unsafe { GetClipboardFormatNameW(id, wide.as_mut_ptr(), wide.len() as i32) };
    if length <= 0 {
        return Ok(String::new());
    }
    wide.truncate(length as usize);
    Ok(String::from_utf16_lossy(&wide))
}

fn clipboard_format_available(format: u32) -> bool {
    // Clipboard must be open for GetClipboardData; EnumClipboardFormats avoids triggering a render.
    let mut current = 0u32;
    for _ in 0..MAX_CLIPBOARD_FORMATS {
        // SAFETY: the caller holds the clipboard open and the current ID is a valid enumeration cursor.
        current = unsafe { EnumClipboardFormats(current) };
        if current == 0 {
            return false;
        }
        if current == format {
            return true;
        }
    }
    false
}

fn read_global_bytes(format: u32) -> Result<Vec<u8>, BackendError> {
    // SAFETY: callers hold the clipboard open; the returned handle is clipboard-owned and remains
    // valid for the duration of that open operation. It is checked for null before use.
    let handle = unsafe { GetClipboardData(format) };
    if handle.is_null() {
        return Err(BackendError::Unavailable);
    }
    // SAFETY: GetClipboardData returns a clipboard-owned HGLOBAL while the clipboard remains open.
    let size = unsafe { GlobalSize(handle.cast()) };
    if size == 0 || size > MAX_CLIPBOARD_BYTES {
        return Err(BackendError::Failed(
            "clipboard payload size is invalid".into(),
        ));
    }
    // SAFETY: handle is a valid clipboard-owned global block and the clipboard remains open.
    let pointer = unsafe { GlobalLock(handle.cast()) };
    if pointer.is_null() {
        return Err(last_error("cannot lock clipboard data"));
    }
    let guard = GlobalLockGuard(handle.cast());
    // SAFETY: GlobalSize bounds the readable allocation and GlobalLock keeps it stable.
    let bytes = unsafe { slice::from_raw_parts(pointer.cast::<u8>(), size) }.to_vec();
    drop(guard);
    Ok(bytes)
}

fn read_u64(format: u32) -> Result<Option<u64>, BackendError> {
    if !clipboard_format_available(format) {
        return Ok(None);
    }
    let bytes = read_global_bytes(format)?;
    if bytes.len() < size_of::<u64>() {
        return Ok(None);
    }
    let mut value = [0u8; 8];
    value.copy_from_slice(&bytes[..8]);
    Ok(Some(u64::from_le_bytes(value)))
}

fn read_sensitivity() -> Result<ClipboardSensitivity, BackendError> {
    let exclude = read_clipboard_dword(register_format(SENSITIVE_MONITOR_NAME)?)?;
    let history = read_clipboard_dword(register_format(SENSITIVE_HISTORY_NAME)?)?;
    Ok(sensitivity_from_flags(exclude, history))
}

fn sensitivity_from_flags(exclude: Option<u32>, history: Option<u32>) -> ClipboardSensitivity {
    ClipboardSensitivity {
        sensitive: exclude.is_some_and(|value| value != 0) || history == Some(0),
        ..ClipboardSensitivity::default()
    }
}

fn read_clipboard_dword(format: u32) -> Result<Option<u32>, BackendError> {
    let Some(bytes) = read_if_available(format)? else {
        return Ok(None);
    };
    decode_clipboard_dword(&bytes).map(Some)
}

fn decode_clipboard_dword(bytes: &[u8]) -> Result<u32, BackendError> {
    let bytes = bytes
        .get(..size_of::<u32>())
        .ok_or_else(|| BackendError::Failed("clipboard sensitivity flag is truncated".into()))?;
    let mut value = [0u8; 4];
    value.copy_from_slice(bytes);
    Ok(u32::from_le_bytes(value))
}

fn read_if_available(format: u32) -> Result<Option<Vec<u8>>, BackendError> {
    if !clipboard_format_available(format) {
        return Ok(None);
    }
    read_global_bytes(format).map(Some)
}

fn write_marker(marker: ClipboardMarker) -> Result<(), BackendError> {
    let id = register_format(CLIPBOARD_MARKER_NAME)?;
    write_global_bytes(id, &marker.0.to_le_bytes())
}

fn write_sensitivity_markers() -> Result<(), BackendError> {
    let monitor_id = register_format(SENSITIVE_MONITOR_NAME)?;
    write_global_bytes(monitor_id, &1u32.to_le_bytes())?;
    let history_id = register_format(SENSITIVE_HISTORY_NAME)?;
    write_global_bytes(history_id, &0u32.to_le_bytes())
}

fn write_global_bytes(format: u32, bytes: &[u8]) -> Result<(), BackendError> {
    validate_payload_size(bytes.len())?;
    let handle = allocate_global(bytes)?;
    // SAFETY: the clipboard is open; SetClipboardData takes ownership of the movable HGLOBAL
    // on success, and `handle` frees it on failure.
    if unsafe { SetClipboardData(format, handle.as_handle()) }.is_null() {
        return Err(last_error("cannot set clipboard data"));
    }
    handle.disarm();
    Ok(())
}

fn allocate_global(bytes: &[u8]) -> Result<OwnedGlobal, BackendError> {
    let allocation = bytes.len().max(1);
    // SAFETY: GHND allocates a zeroed movable global block of the bounded requested size.
    let handle = unsafe { GlobalAlloc(GHND, allocation) };
    if handle.is_null() {
        return Err(last_error("cannot allocate clipboard memory"));
    }
    // SAFETY: handle is a valid movable HGLOBAL returned by GlobalAlloc.
    let pointer = unsafe { GlobalLock(handle) };
    if pointer.is_null() {
        // SAFETY: this handle has not been transferred and GlobalLock failed.
        unsafe { GlobalFree(handle) };
        return Err(last_error("cannot lock clipboard memory"));
    }
    if !bytes.is_empty() {
        // SAFETY: `pointer` is locked for at least `allocation` bytes and source is `bytes.len()`.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.cast::<u8>(), bytes.len()) };
    }
    // SAFETY: unlock balances the successful GlobalLock; global memory remains allocated.
    unsafe { GlobalUnlock(handle) };
    Ok(OwnedGlobal(Some(handle)))
}

struct OwnedGlobal(Option<windows_sys::Win32::Foundation::HGLOBAL>);

impl OwnedGlobal {
    fn as_handle(&self) -> HANDLE {
        self.0.map_or(null_mut(), |handle| handle.cast::<c_void>())
    }

    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for OwnedGlobal {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            // SAFETY: this HGLOBAL has not been transferred to the clipboard.
            unsafe { GlobalFree(handle) };
        }
    }
}

struct GlobalLockGuard(windows_sys::Win32::Foundation::HGLOBAL);

impl Drop for GlobalLockGuard {
    fn drop(&mut self) {
        // SAFETY: this guard exists only after a successful GlobalLock.
        unsafe { GlobalUnlock(self.0) };
    }
}

fn validate_payload_size(size: usize) -> Result<(), BackendError> {
    if size > MAX_CLIPBOARD_BYTES {
        return Err(BackendError::InvalidInput(
            "clipboard payload exceeds 64 MiB".into(),
        ));
    }
    Ok(())
}

fn last_error(message: &str) -> BackendError {
    // SAFETY: GetLastError returns only the calling thread's scalar Win32 error code.
    let error = unsafe { GetLastError() };
    BackendError::Failed(format!("{message} (Windows error {error})"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    static NEXT_MARKER: AtomicU64 = AtomicU64::new(0x474c_4944_4500_0000);

    fn next_marker() -> ClipboardMarker {
        ClipboardMarker(NEXT_MARKER.fetch_add(1, Ordering::Relaxed))
    }

    fn probe_stage<T>(
        stage: &'static str,
        result: Result<T, BackendError>,
    ) -> Result<T, BackendError> {
        result.inspect_err(|_| {
            eprintln!("clipboard integration failed at static stage: {stage}");
        })
    }

    struct ClipboardRestore<'a> {
        clipboard: &'a WindowsClipboard,
        contents: Vec<ClipboardContent>,
        original_text: Option<Vec<u8>>,
        restored: bool,
    }

    impl<'a> ClipboardRestore<'a> {
        fn capture(clipboard: &'a WindowsClipboard) -> Result<Self, BackendError> {
            let snapshot = clipboard.snapshot()?;
            if snapshot.sensitivity.should_exclude() {
                return Err(BackendError::PermissionDenied);
            }
            let mut contents = Vec::new();
            for format in snapshot.formats {
                if let Some(content) = clipboard.read(&format)? {
                    contents.push(content);
                }
            }
            let original_text = contents.iter().find_map(|content| {
                if content.format == ClipboardFormat::Text {
                    match &content.data {
                        ClipboardData::Bytes(bytes) => Some(bytes.clone()),
                        ClipboardData::Files(_) => None,
                    }
                } else {
                    None
                }
            });
            Ok(Self {
                clipboard,
                contents,
                original_text,
                restored: false,
            })
        }

        fn restore(&mut self) -> Result<(), BackendError> {
            if self.restored {
                return Ok(());
            }
            self.clipboard.command(Command::Clear)?;
            let marker = next_marker();
            for content in self.contents.iter().cloned() {
                if let ClipboardData::Files(files) = content.data {
                    self.clipboard.write_files(files, marker)?;
                } else {
                    self.clipboard.write(content, marker)?;
                }
            }
            if let Some(expected) = self.original_text.as_ref() {
                if read_bytes(self.clipboard, ClipboardFormat::Text)?.as_ref() != Some(expected) {
                    return Err(BackendError::Failed(
                        "clipboard text was not restored exactly".into(),
                    ));
                }
            }
            self.restored = true;
            Ok(())
        }
    }

    impl Drop for ClipboardRestore<'_> {
        fn drop(&mut self) {
            let _ = self.restore();
        }
    }

    #[test]
    #[ignore = "changes the user's clipboard; run only when the desktop is idle"]
    fn clipboard_formats_round_trip_and_restore_prior_formats() -> Result<(), BackendError> {
        let clipboard = probe_stage("new backend", WindowsClipboard::new())?;
        let mut restore = probe_stage(
            "capture prior formats",
            ClipboardRestore::capture(&clipboard),
        )?;
        let marker = next_marker();
        let text = b"Glide clipboard probe \xf0\x9f\x8c\x90".to_vec();
        probe_stage(
            "write text",
            clipboard.write(
                ClipboardContent::bytes(
                    ClipboardFormat::Text,
                    text.clone(),
                    ClipboardSensitivity::default(),
                )?,
                marker,
            ),
        )?;
        assert_eq!(
            probe_stage("read text", read_bytes(&clipboard, ClipboardFormat::Text))?,
            Some(text)
        );

        probe_stage(
            "write HTML",
            clipboard.write(
                ClipboardContent::bytes(
                    ClipboardFormat::Html,
                    b"<b>Glide</b>".to_vec(),
                    ClipboardSensitivity::default(),
                )?,
                marker,
            ),
        )?;
        assert_eq!(
            probe_stage("read HTML", read_bytes(&clipboard, ClipboardFormat::Html))?,
            Some(b"<b>Glide</b>".to_vec())
        );

        probe_stage(
            "write RTF",
            clipboard.write(
                ClipboardContent::bytes(
                    ClipboardFormat::Rtf,
                    b"{\\rtf1 Glide}".to_vec(),
                    ClipboardSensitivity::default(),
                )?,
                marker,
            ),
        )?;
        assert_eq!(
            probe_stage("read RTF", read_bytes(&clipboard, ClipboardFormat::Rtf))?,
            Some(b"{\\rtf1 Glide}".to_vec())
        );

        let png = tiny_png();
        probe_stage(
            "write PNG",
            clipboard.write(
                ClipboardContent::bytes(
                    ClipboardFormat::Png,
                    png.clone(),
                    ClipboardSensitivity::default(),
                )?,
                marker,
            ),
        )?;
        assert_eq!(
            probe_stage("read PNG", read_bytes(&clipboard, ClipboardFormat::Png))?,
            Some(png)
        );

        let path = std::env::current_exe().map_err(|_| BackendError::Unavailable)?;
        let metadata = std::fs::metadata(&path).map_err(|_| BackendError::Unavailable)?;
        let name = path
            .file_name()
            .map(|value| value.to_string_lossy().into_owned())
            .ok_or_else(|| BackendError::InvalidInput("test executable has no basename".into()))?;
        probe_stage(
            "write files",
            clipboard.write_files(
                FileList {
                    entries: vec![FileEntry {
                        path: path.clone(),
                        name,
                        size: metadata.len(),
                        is_dir: false,
                    }],
                    sensitivity: ClipboardSensitivity::default(),
                },
                marker,
            ),
        )?;
        let observed =
            probe_stage("read files", clipboard.read_files())?.ok_or(BackendError::Unavailable)?;
        assert_eq!(observed.entries.len(), 1);
        assert_eq!(observed.entries[0].path, path);

        let before = clipboard.read_snapshot()?;
        assert_eq!(
            before.contents.len(),
            5,
            "one snapshot contains text, HTML, RTF, PNG and real files"
        );
        let bundle = before.contents.clone();
        let atomic_marker = next_marker();
        assert!(matches!(
            clipboard.publish_snapshot(
                bundle.clone(),
                atomic_marker,
                before.change_token,
                ClipboardAdmission::default()
            )?,
            ClipboardPublish::Published { .. }
        ));
        let after = clipboard.read_snapshot()?;
        assert_eq!(after.marker, Some(atomic_marker));
        assert_eq!(after.contents, bundle);
        let revoked = ClipboardAdmission::default();
        revoked.revoke();
        assert_eq!(
            clipboard.publish_snapshot(
                bundle.clone(),
                next_marker(),
                after.change_token,
                revoked
            )?,
            ClipboardPublish::Revoked
        );
        assert_eq!(clipboard.read_snapshot()?, after);
        clipboard.write(
            ClipboardContent::bytes(
                ClipboardFormat::Text,
                b"new local copy".to_vec(),
                ClipboardSensitivity::default(),
            )?,
            next_marker(),
        )?;
        let local = clipboard.read_snapshot()?;
        assert!(matches!(
            clipboard.publish_snapshot(
                bundle,
                next_marker(),
                after.change_token,
                ClipboardAdmission::default()
            )?,
            ClipboardPublish::ReplacedLocalChange { .. }
        ));
        assert_eq!(
            clipboard.read_snapshot()?,
            local,
            "new local copy survives stale publication"
        );

        let delayed_marker = next_marker();
        let render_events = clipboard.subscribe();
        probe_stage(
            "register delayed text",
            clipboard.set_delayed_render(ClipboardFormat::Text, delayed_marker),
        )?;
        let requester = thread::spawn(|| {
            // Use the same bounded contention backoff as real reads; the notification
            // worker may be taking its format snapshot concurrently with this paste.
            let _open = open_clipboard(null_mut())?;
            // SAFETY: the clipboard owner receives WM_RENDERFORMAT for this delayed format.
            let _ = unsafe { GetClipboardData(CF_UNICODETEXT_ID) };
            Ok::<(), BackendError>(())
        });
        let request_result = requester
            .join()
            .map_err(|_| BackendError::Failed("clipboard requester thread failed".into()))
            .and_then(|result| result);
        probe_stage("request delayed text", request_result)?;
        let requested = loop {
            match render_events.recv_timeout(Duration::from_secs(2)) {
                Ok(ClipboardEvent::RenderRequested { marker, format })
                    if marker == delayed_marker && format == ClipboardFormat::Text =>
                {
                    break true;
                }
                Ok(_) => continue,
                Err(_) => break false,
            }
        };
        assert!(requested, "clipboard did not request delayed text");
        probe_stage(
            "fulfill delayed text",
            clipboard.fulfill_delayed_render(
                ClipboardContent::bytes(
                    ClipboardFormat::Text,
                    b"Glide delayed render".to_vec(),
                    ClipboardSensitivity::default(),
                )?,
                delayed_marker,
            ),
        )?;
        assert_eq!(
            probe_stage(
                "read delayed text",
                read_bytes(&clipboard, ClipboardFormat::Text)
            )?,
            Some(b"Glide delayed render".to_vec())
        );
        probe_stage("restore prior clipboard", restore.restore())?;
        Ok(())
    }

    #[test]
    fn utf16_encoding_and_sensitive_flags_fail_closed() -> Result<(), BackendError> {
        let text = native_representations(&ClipboardFormat::Text, "A🌐".as_bytes())?;
        let (pairs, remainder) = text[0].1.as_chunks::<2>();
        assert!(remainder.is_empty());
        let units = pairs
            .iter()
            .map(|pair| u16::from_ne_bytes(*pair))
            .collect::<Vec<_>>();
        assert_eq!(
            &units[..units.len() - 1],
            &"A🌐".encode_utf16().collect::<Vec<_>>()
        );
        assert_eq!(units.last(), Some(&0));
        assert!(native_representations(&ClipboardFormat::Text, &[0xff]).is_err());
        assert!(sensitivity_from_flags(Some(1), Some(1)).should_exclude());
        assert!(sensitivity_from_flags(None, Some(0)).should_exclude());
        assert!(!sensitivity_from_flags(None, Some(1)).should_exclude());
        assert!(decode_clipboard_dword(&[1, 0, 0]).is_err());
        assert_eq!(decode_clipboard_dword(&[1, 0, 0, 0]), Ok(1));
        Ok(())
    }

    #[test]
    fn independent_requests_fit_bounded_queue_and_canceled_work_is_skipped() {
        let (requests_tx, requests_rx) = bounded(COMMAND_QUEUE_CAPACITY);
        let user_status = Arc::new(AtomicU8::new(REQUEST_PENDING));
        let event_status = Arc::new(AtomicU8::new(REQUEST_PENDING));
        let excess_status = Arc::new(AtomicU8::new(REQUEST_PENDING));
        let (user_reply, _user_reply_rx) = bounded(1);
        let (event_reply, _event_reply_rx) = bounded(1);
        let (excess_reply, _excess_reply_rx) = bounded(1);

        assert!(requests_tx
            .try_send(CommandRequest {
                command: Command::Snapshot(user_reply),
                status: Arc::clone(&user_status),
            })
            .is_ok());
        assert!(requests_tx
            .try_send(CommandRequest {
                command: Command::Snapshot(event_reply),
                status: Arc::clone(&event_status),
            })
            .is_ok());
        assert!(matches!(
            requests_tx.try_send(CommandRequest {
                command: Command::Snapshot(excess_reply),
                status: excess_status,
            }),
            Err(TrySendError::Full(_))
        ));
        assert!(user_status
            .compare_exchange(
                REQUEST_PENDING,
                REQUEST_CANCELED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok());

        let first = requests_rx.try_recv();
        assert!(matches!(first, Ok(request) if !claim_request(&request.status)));
        let second = requests_rx.try_recv();
        assert!(matches!(second, Ok(request) if claim_request(&request.status)));
        assert_eq!(event_status.load(Ordering::Acquire), REQUEST_STARTED);
        assert!(requests_rx.is_empty());
    }

    #[test]
    fn hdrop_data_is_double_terminated_and_rejects_relative_paths() -> Result<(), BackendError> {
        let entry = FileEntry {
            path: PathBuf::from(r"C:\test\clip.txt"),
            name: "clip.txt".into(),
            size: 1,
            is_dir: false,
        };
        let bytes = build_drop_files(&[entry])?;
        assert_eq!(
            u32::from_ne_bytes(
                bytes[..4]
                    .try_into()
                    .map_err(|_| BackendError::Unavailable)?
            ),
            20
        );
        assert_eq!(&bytes[16..20], &[1, 0, 0, 0]);
        assert_eq!(&bytes[bytes.len() - 4..], &[0, 0, 0, 0]);
        assert_eq!(validate_drop_files(&bytes), Ok(1));
        let mut truncated = bytes.clone();
        truncated.pop();
        assert!(validate_drop_files(&truncated).is_err());
        let mut invalid_offset = bytes.clone();
        invalid_offset[..4].copy_from_slice(&u32::MAX.to_ne_bytes());
        assert!(validate_drop_files(&invalid_offset).is_err());
        let relative = FileEntry {
            path: PathBuf::from("relative.txt"),
            name: "relative.txt".into(),
            size: 0,
            is_dir: false,
        };
        assert!(build_drop_files(&[relative]).is_err());
        Ok(())
    }

    fn read_bytes(
        clipboard: &WindowsClipboard,
        format: ClipboardFormat,
    ) -> Result<Option<Vec<u8>>, BackendError> {
        Ok(clipboard
            .read(&format)?
            .and_then(|content| match content.data {
                ClipboardData::Bytes(bytes) => Some(bytes),
                ClipboardData::Files(_) => None,
            }))
    }

    fn tiny_png() -> Vec<u8> {
        vec![
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78,
            0xda, 0x63, 0x60, 0x00, 0x02, 0x00, 0x00, 0x05, 0x00, 0x01, 0xa5, 0x68, 0x1b, 0xf7,
            0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ]
    }
}
