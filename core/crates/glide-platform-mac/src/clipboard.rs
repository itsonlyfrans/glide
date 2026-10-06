//! NSPasteboard backend. All pasteboard access is serialized on one worker thread.

use crate::clipboard_logic::{
    choose_image_source, decode_marker, encode_marker, file_url_to_path, formats_for_types,
    next_poll_interval, owns_delayed_item, poll_decision, sensitivity_for_types, ImageSource,
    PollDecision, MARKER_TYPE,
};
use core_foundation::base::TCFType;
use core_foundation::date::CFDate;
use core_foundation::runloop::{
    kCFRunLoopDefaultMode, CFRunLoop, CFRunLoopMode, CFRunLoopTimer, CFRunLoopTimerContext,
    CFRunLoopTimerCreate, CFRunLoopTimerInvalidate, CFRunLoopTimerRef,
};
use crossbeam_channel::{bounded, Receiver, SendTimeoutError, Sender, TryRecvError};
use glide_platform::{
    validate_clipboard_bundle, BackendError, ClipboardAdmission, ClipboardBackend,
    ClipboardChangeToken, ClipboardContent, ClipboardData, ClipboardEvent, ClipboardFormat,
    ClipboardMarker, ClipboardPublish, ClipboardSensitivity, ClipboardSnapshot, FileEntry,
    FileList,
};
use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, AnyThread, DefinedClass};
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSPasteboard, NSPasteboardItem,
    NSPasteboardItemDataProvider, NSPasteboardType,
};
use objc2_foundation::{
    ns_string, NSArray, NSData, NSDictionary, NSObject, NSObjectProtocol, NSString, NSURL,
};
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{compiler_fence, AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const EVENT_CAPACITY: usize = 128;
const COMMAND_CAPACITY: usize = 64;
const MAX_NATIVE_TYPES: usize = 128;
const MAX_NATIVE_TYPE_UNITS: usize = 512;
const MAX_FILE_URL_UNITS: usize = 16 * 1024;
const MAX_NATIVE_ITEMS: usize = 4096;
const MAX_CLIPBOARD_BYTES: usize = 64 * 1024 * 1024;
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_MIN: Duration = Duration::from_millis(100);
const POLL_MAX: Duration = Duration::from_secs(1);
const COMMAND_TICK: Duration = Duration::from_millis(100);

#[derive(Debug)]
struct ProviderIvars {
    events: Sender<ClipboardEvent>,
    commands: Sender<Command>,
    resync_pending: Arc<AtomicBool>,
    request_overflow: Arc<AtomicBool>,
    marker: ClipboardMarker,
}

define_class!(
    #[unsafe(super = NSObject)]
    #[ivars = ProviderIvars]
    struct DelayedProvider;

    // SAFETY: NSObjectProtocol has no implementation safety requirements.
    unsafe impl NSObjectProtocol for DelayedProvider {}

    // SAFETY: This implementation only enqueues a bounded event. It never blocks or accesses
    // the pasteboard from the callback.
    unsafe impl NSPasteboardItemDataProvider for DelayedProvider {
        #[unsafe(method(pasteboard:item:provideDataForType:))]
        fn provide_data_for_type(
            &self,
            _pasteboard: Option<&NSPasteboard>,
            _item: &NSPasteboardItem,
            native_type: &NSPasteboardType,
        ) {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let Some(format) = format_for_native_type(native_type) else {
                    return;
                };
                if self
                    .ivars()
                    .commands
                    .try_send(Command::RenderRequested {
                        marker: self.ivars().marker,
                        format: format.clone(),
                    })
                    .is_err()
                {
                    self.ivars().resync_pending.store(true, Ordering::Release);
                    self.ivars().request_overflow.store(true, Ordering::Release);
                    return;
                }
                let event = ClipboardEvent::RenderRequested {
                    marker: self.ivars().marker,
                    format,
                };
                if self.ivars().events.try_send(event).is_err() {
                    self.ivars().resync_pending.store(true, Ordering::Release);
                    self.ivars().request_overflow.store(true, Ordering::Release);
                }
            }));
            if result.is_err() {
                self.ivars().resync_pending.store(true, Ordering::Release);
                self.ivars().request_overflow.store(true, Ordering::Release);
            }
        }
    }
);

impl DelayedProvider {
    fn new(
        events: Sender<ClipboardEvent>,
        commands: Sender<Command>,
        resync_pending: Arc<AtomicBool>,
        request_overflow: Arc<AtomicBool>,
        marker: ClipboardMarker,
    ) -> Retained<Self> {
        let this = Self::alloc().set_ivars(ProviderIvars {
            events,
            commands,
            resync_pending,
            request_overflow,
            marker,
        });
        // SAFETY: The allocated object is a subclass of NSObject and uses NSObject's init method.
        unsafe { msg_send![super(this), init] }
    }
}

/// Native macOS clipboard implementation.
///
/// This owning worker serializes all NSPasteboard calls. The AppKit delayed-data callback only
/// enqueues `RenderRequested`; the first paste can miss a remote payload and must be retried after
/// fulfillment.
pub struct MacClipboard {
    commands: Sender<Command>,
    events: Receiver<ClipboardEvent>,
    worker: Option<JoinHandle<()>>,
}

impl MacClipboard {
    /// Starts the NSPasteboard owner thread and its change-count poller.
    pub fn new() -> Result<Self, BackendError> {
        let (commands, command_rx) = bounded(COMMAND_CAPACITY);
        let (events, event_rx) = bounded(EVENT_CAPACITY);
        let (ready_tx, ready_rx) = bounded(1);
        let resync_pending = Arc::new(AtomicBool::new(false));
        let request_overflow = Arc::new(AtomicBool::new(false));
        let worker_events = events.clone();
        let worker_commands = commands.clone();
        let worker_resync = Arc::clone(&resync_pending);
        let worker_request_overflow = Arc::clone(&request_overflow);
        let worker = thread::Builder::new()
            .name("glide-mac-clipboard".into())
            .spawn(move || {
                worker_main(
                    command_rx,
                    worker_commands,
                    worker_events,
                    worker_resync,
                    worker_request_overflow,
                    ready_tx,
                )
            })
            .map_err(|_| BackendError::Unavailable)?;
        if let Err(error) = ready_rx
            .recv()
            .map_err(|_| BackendError::Unavailable)
            .and_then(std::convert::identity)
        {
            let _ = worker.join();
            return Err(error);
        }
        Ok(Self {
            commands,
            events: event_rx,
            worker: Some(worker),
        })
    }

    fn call<T>(
        &self,
        create: impl FnOnce(Sender<Result<T, BackendError>>) -> Command,
    ) -> Result<T, BackendError> {
        let (reply, response) = bounded(1);
        self.commands
            .send_timeout(create(reply), REQUEST_TIMEOUT)
            .map_err(|error| match error {
                SendTimeoutError::Timeout(_) | SendTimeoutError::Disconnected(_) => {
                    BackendError::Unavailable
                }
            })?;
        response.recv().map_err(|_| BackendError::Unavailable)?
    }
}

impl Drop for MacClipboard {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl ClipboardBackend for MacClipboard {
    fn read_snapshot(&self) -> Result<ClipboardSnapshot, BackendError> {
        self.call(Command::ReadSnapshot)
    }
    fn publish_snapshot(
        &self,
        contents: Vec<ClipboardContent>,
        marker: ClipboardMarker,
        expected: ClipboardChangeToken,
        admission: ClipboardAdmission,
    ) -> Result<ClipboardPublish, BackendError> {
        validate_clipboard_bundle(&contents)?;
        let contents = contents
            .into_iter()
            .map(OwnedContent::new)
            .collect::<Result<Vec<_>, _>>()?;
        self.call(|reply| Command::PublishSnapshot {
            contents,
            marker,
            expected,
            admission,
            reply,
        })
    }
    fn read(&self, format: &ClipboardFormat) -> Result<Option<ClipboardContent>, BackendError> {
        self.call(|reply| Command::Read {
            format: format.clone(),
            reply,
        })
    }

    fn write(
        &self,
        content: ClipboardContent,
        marker: ClipboardMarker,
    ) -> Result<(), BackendError> {
        let content = OwnedContent::new(content)?;
        self.call(|reply| Command::Write {
            content,
            marker,
            reply,
        })
    }

    fn formats(&self) -> Result<Vec<ClipboardFormat>, BackendError> {
        self.call(Command::Formats)
    }

    fn read_files(&self) -> Result<Option<FileList>, BackendError> {
        self.call(Command::ReadFiles)
    }

    fn write_files(&self, files: FileList, marker: ClipboardMarker) -> Result<(), BackendError> {
        self.call(|reply| Command::WriteFiles {
            files,
            marker,
            reply,
        })
    }

    fn subscribe(&self) -> Receiver<ClipboardEvent> {
        self.events.clone()
    }

    fn set_delayed_render(
        &self,
        format: ClipboardFormat,
        marker: ClipboardMarker,
    ) -> Result<(), BackendError> {
        self.call(|reply| Command::SetDelayed {
            format,
            marker,
            reply,
        })
    }

    fn fulfill_delayed_render(
        &self,
        content: ClipboardContent,
        marker: ClipboardMarker,
    ) -> Result<(), BackendError> {
        let content = OwnedContent::new(content)?;
        self.call(|reply| Command::Fulfill {
            content,
            marker,
            reply,
        })
    }

    fn cancel_delayed_render(&self, marker: ClipboardMarker) -> Result<(), BackendError> {
        self.call(|reply| Command::CancelDelayed { marker, reply })
    }
}

enum Command {
    ReadSnapshot(Sender<Result<ClipboardSnapshot, BackendError>>),
    PublishSnapshot {
        contents: Vec<OwnedContent>,
        marker: ClipboardMarker,
        expected: ClipboardChangeToken,
        admission: ClipboardAdmission,
        reply: Sender<Result<ClipboardPublish, BackendError>>,
    },
    Read {
        format: ClipboardFormat,
        reply: Sender<Result<Option<ClipboardContent>, BackendError>>,
    },
    Write {
        content: OwnedContent,
        marker: ClipboardMarker,
        reply: Sender<Result<(), BackendError>>,
    },
    Formats(Sender<Result<Vec<ClipboardFormat>, BackendError>>),
    ReadFiles(Sender<Result<Option<FileList>, BackendError>>),
    WriteFiles {
        files: FileList,
        marker: ClipboardMarker,
        reply: Sender<Result<(), BackendError>>,
    },
    SetDelayed {
        format: ClipboardFormat,
        marker: ClipboardMarker,
        reply: Sender<Result<(), BackendError>>,
    },
    Fulfill {
        content: OwnedContent,
        marker: ClipboardMarker,
        reply: Sender<Result<(), BackendError>>,
    },
    CancelDelayed {
        marker: ClipboardMarker,
        reply: Sender<Result<(), BackendError>>,
    },
    RenderRequested {
        marker: ClipboardMarker,
        format: ClipboardFormat,
    },
    Shutdown,
}

enum OwnedData {
    Bytes(WipeBytes),
    Files(FileList),
}

struct OwnedContent {
    format: ClipboardFormat,
    data: OwnedData,
    sensitivity: ClipboardSensitivity,
}

impl OwnedContent {
    fn new(content: ClipboardContent) -> Result<Self, BackendError> {
        let ClipboardContent {
            format,
            data,
            sensitivity,
        } = content;
        let data = match data {
            ClipboardData::Bytes(bytes) => OwnedData::Bytes(WipeBytes(bytes)),
            ClipboardData::Files(files) => OwnedData::Files(files),
        };
        let valid = matches!(
            (&format, &data),
            (ClipboardFormat::Files, OwnedData::Files(_))
                | (ClipboardFormat::Text, OwnedData::Bytes(_))
                | (ClipboardFormat::Html, OwnedData::Bytes(_))
                | (ClipboardFormat::Rtf, OwnedData::Bytes(_))
                | (ClipboardFormat::Png, OwnedData::Bytes(_))
                | (ClipboardFormat::Other(_), OwnedData::Bytes(_))
        );
        if !valid {
            return Err(BackendError::InvalidInput(
                "clipboard format and payload do not match".into(),
            ));
        }
        Ok(Self {
            format,
            data,
            sensitivity,
        })
    }
}

struct WipeBytes(Vec<u8>);

impl Drop for WipeBytes {
    fn drop(&mut self) {
        for byte in &mut self.0 {
            // SAFETY: Each pointer refers to an initialized, uniquely borrowed byte in this Vec.
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
        compiler_fence(Ordering::SeqCst);
    }
}

impl WipeBytes {
    fn as_slice(&self) -> &[u8] {
        &self.0
    }

    fn into_vec(mut self) -> Vec<u8> {
        std::mem::take(&mut self.0)
    }
}

struct ActiveDelayed {
    marker: ClipboardMarker,
    count: isize,
    item: Retained<NSPasteboardItem>,
    _provider: Retained<DelayedProvider>,
    registered: Vec<ClipboardFormat>,
    deadlines: HashMap<ClipboardFormat, Instant>,
}

struct OwnedClipboard {
    marker: ClipboardMarker,
    count: isize,
    primary: Option<Retained<NSPasteboardItem>>,
    files: Vec<Retained<NSPasteboardItem>>,
}

struct WorkerState {
    pasteboard: Retained<NSPasteboard>,
    last_count: isize,
    own_count: Option<isize>,
    owned: Option<OwnedClipboard>,
    delayed: Option<ActiveDelayed>,
    command_sender: Sender<Command>,
    request_overflow: Arc<AtomicBool>,
    resync_pending: Arc<AtomicBool>,
}

fn worker_main(
    commands: Receiver<Command>,
    command_sender: Sender<Command>,
    events: Sender<ClipboardEvent>,
    resync_pending: Arc<AtomicBool>,
    request_overflow: Arc<AtomicBool>,
    ready: Sender<Result<(), BackendError>>,
) {
    let pasteboard = autoreleasepool(|_| NSPasteboard::generalPasteboard());
    let mut state = WorkerState {
        last_count: pasteboard.changeCount(),
        pasteboard,
        own_count: None,
        owned: None,
        delayed: None,
        command_sender: command_sender.clone(),
        request_overflow,
        resync_pending,
    };
    let run_loop = CFRunLoop::get_current();
    let now = CFDate::now().abs_time();
    let mut timer_context = CFRunLoopTimerContext {
        version: 0,
        info: std::ptr::null_mut(),
        retain: None,
        release: None,
        copyDescription: None,
    };
    let tick_seconds = COMMAND_TICK.as_secs_f64();
    // SAFETY: Core Foundation permits a null allocator to select its default allocator.
    // The timer copies this valid context during creation; its info pointer is null.
    let timer_ref = unsafe {
        CFRunLoopTimerCreate(
            std::ptr::null(),
            now + tick_seconds,
            tick_seconds,
            0,
            0,
            run_loop_tick,
            &mut timer_context,
        )
    };
    if timer_ref.is_null() {
        let _ = ready.try_send(Err(BackendError::Unavailable));
        autoreleasepool(|_| drop(state));
        return;
    }
    // SAFETY: CFRunLoopTimerCreate returned a non-null +1 timer reference.
    let timer = unsafe { CFRunLoopTimer::wrap_under_create_rule(timer_ref) };
    // SAFETY: This exported constant is a valid static CFString run-loop mode.
    let mode = unsafe { kCFRunLoopDefaultMode };
    let timer_guard = RunLoopTimerGuard {
        run_loop,
        timer,
        mode,
    };
    timer_guard
        .run_loop
        .add_timer(&timer_guard.timer, timer_guard.mode);
    let _ = ready.try_send(Ok(()));

    let mut poll_interval = POLL_MIN;
    let mut next_poll = Instant::now() + poll_interval;
    let mut shutdown = false;
    loop {
        // Running a bounded mode interval services AppKit pasteboard IPC while the same thread
        // drains commands at 100 ms granularity. The repeating timer prevents a busy loop.
        autoreleasepool(|_| {
            let _ = CFRunLoop::run_in_mode(timer_guard.mode, COMMAND_TICK, false);
        });
        for _ in 0..COMMAND_CAPACITY {
            match commands.try_recv() {
                Ok(Command::Shutdown) | Err(TryRecvError::Disconnected) => {
                    shutdown = true;
                    break;
                }
                Ok(command) => {
                    autoreleasepool(|_| handle_command(&mut state, command, &events));
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        if shutdown {
            break;
        }
        if state.request_overflow.swap(false, Ordering::AcqRel) {
            autoreleasepool(|_| clear_owned_delayed(&mut state));
            state.resync_pending.store(true, Ordering::Release);
        }
        let expired = autoreleasepool(|_| expire_delayed(&mut state));
        if expired {
            poll_interval = POLL_MIN;
            next_poll = Instant::now();
        }
        flush_resync(&events, &state.resync_pending);
        if Instant::now() >= next_poll {
            let changed = autoreleasepool(|_| poll_pasteboard(&mut state, &events));
            poll_interval = next_poll_interval(poll_interval, changed, POLL_MIN, POLL_MAX);
            next_poll = Instant::now() + poll_interval;
        }
    }
    drop(timer_guard);
    autoreleasepool(|_| {
        if state.delayed.is_some() {
            clear_owned_delayed(&mut state);
        }
    });
}

struct RunLoopTimerGuard {
    run_loop: CFRunLoop,
    timer: CFRunLoopTimer,
    mode: CFRunLoopMode,
}

impl Drop for WorkerState {
    fn drop(&mut self) {
        if self.delayed.is_some() {
            autoreleasepool(|_| clear_owned_delayed(self));
        }
    }
}

impl Drop for RunLoopTimerGuard {
    fn drop(&mut self) {
        self.run_loop.remove_timer(&self.timer, self.mode);
        // SAFETY: `timer` is a live Core Foundation timer retained by this guard.
        unsafe { CFRunLoopTimerInvalidate(self.timer.as_concrete_TypeRef()) };
    }
}

extern "C" fn run_loop_tick(_timer: CFRunLoopTimerRef, _context: *mut c_void) {}

fn handle_command(state: &mut WorkerState, command: Command, events: &Sender<ClipboardEvent>) {
    if !matches!(command, Command::RenderRequested { .. } | Command::Shutdown) {
        let _ = poll_pasteboard(state, events);
    }
    match command {
        Command::ReadSnapshot(reply) => {
            let _ = reply.send(read_bundle(state));
        }
        Command::PublishSnapshot {
            contents,
            marker,
            expected,
            admission,
            reply,
        } => {
            let _ = reply.send(publish_bundle(state, contents, marker, expected, admission));
        }
        Command::Read { format, reply } => {
            let _ = reply.try_send(read_content(state, &format));
        }
        Command::Write {
            content,
            marker,
            reply,
        } => {
            let _ = reply.try_send(write_content(state, content, marker));
        }
        Command::Formats(reply) => {
            let _ = reply.try_send(read_formats(state));
        }
        Command::ReadFiles(reply) => {
            let _ = reply.try_send(read_files(state));
        }
        Command::WriteFiles {
            files,
            marker,
            reply,
        } => {
            let _ = reply.try_send(write_files(state, files, marker));
        }
        Command::SetDelayed {
            format,
            marker,
            reply,
        } => {
            let result = set_delayed(state, format, marker, events);
            let _ = reply.try_send(result);
        }
        Command::Fulfill {
            content,
            marker,
            reply,
        } => {
            let _ = reply.try_send(fulfill_delayed(state, content, marker));
        }
        Command::CancelDelayed { marker, reply } => {
            let _ = reply.try_send(cancel_delayed(state, marker));
        }
        Command::RenderRequested { marker, format } => {
            let current_count = state.pasteboard.changeCount();
            let current_marker = observed_marker(&state.pasteboard);
            if let Some(delayed) = state.delayed.as_mut() {
                if delayed.marker == marker
                    && delayed.registered.contains(&format)
                    && owns_delayed_item(delayed.count, current_count, current_marker, marker)
                    && state.pasteboard.changeCount() == current_count
                {
                    delayed
                        .deadlines
                        .entry(format)
                        .or_insert_with(|| Instant::now() + FETCH_TIMEOUT);
                }
            }
        }
        Command::Shutdown => {}
    }
}

fn poll_pasteboard(state: &mut WorkerState, events: &Sender<ClipboardEvent>) -> bool {
    let current_count = state.pasteboard.changeCount();
    let decision = match poll_decision(state.last_count, current_count, state.own_count) {
        PollDecision::OwnWrite if !native_write_is_still_owned(state, current_count) => {
            PollDecision::ExternalChange
        }
        decision => decision,
    };
    match decision {
        PollDecision::Unchanged => false,
        PollDecision::OwnWrite => {
            state.last_count = current_count;
            state.own_count = None;
            true
        }
        PollDecision::ExternalChange => {
            state.last_count = current_count;
            state.own_count = None;
            state.owned = None;
            state.delayed = None;
            let before_snapshot = state.pasteboard.changeCount();
            let snapshot = native_type_snapshot(&state.pasteboard);
            let after_snapshot = state.pasteboard.changeCount();
            let Ok((types, sensitivity)) = snapshot else {
                state.resync_pending.store(true, Ordering::Release);
                return true;
            };
            if before_snapshot != after_snapshot || after_snapshot != current_count {
                state.resync_pending.store(true, Ordering::Release);
                return true;
            }
            let formats = if sensitivity.should_exclude() {
                Vec::new()
            } else {
                formats_for_types(types.iter().map(String::as_str))
            };
            send_event(
                events,
                &state.resync_pending,
                ClipboardEvent::Changed {
                    marker: None,
                    formats,
                    sensitivity,
                },
            );
            true
        }
    }
}

fn expire_delayed(state: &mut WorkerState) -> bool {
    let Some(delayed) = state.delayed.as_ref() else {
        return false;
    };
    if !delayed
        .deadlines
        .values()
        .any(|deadline| *deadline <= Instant::now())
    {
        return false;
    }
    clear_owned_delayed(state);
    true
}

fn clear_owned_delayed(state: &mut WorkerState) {
    let still_owned = state.delayed.as_ref().is_some_and(|delayed| {
        let count = state.pasteboard.changeCount();
        let marker = observed_marker(&state.pasteboard);
        owns_delayed_item(delayed.count, count, marker, delayed.marker)
            && state.pasteboard.changeCount() == count
    });
    if still_owned {
        state.pasteboard.clearContents();
        state.last_count = state.pasteboard.changeCount();
        state.own_count = None;
    }
    state.delayed = None;
    state.owned = None;
}

fn send_event(events: &Sender<ClipboardEvent>, resync_pending: &AtomicBool, event: ClipboardEvent) {
    if events.try_send(event).is_err() {
        resync_pending.store(true, Ordering::Release);
    }
}

fn flush_resync(events: &Sender<ClipboardEvent>, pending: &AtomicBool) {
    if pending.load(Ordering::Acquire) && events.try_send(ClipboardEvent::ResyncRequired).is_ok() {
        pending.store(false, Ordering::Release);
    }
}

fn read_formats(state: &WorkerState) -> Result<Vec<ClipboardFormat>, BackendError> {
    let before = state.pasteboard.changeCount();
    let result = (|| {
        let (types, sensitivity) = native_type_snapshot(&state.pasteboard)?;
        if sensitivity.should_exclude() {
            return Ok(Vec::new());
        }
        Ok(formats_for_types(types.iter().map(String::as_str)))
    })();
    if state.pasteboard.changeCount() != before {
        return Ok(Vec::new());
    }
    result
}

fn native_type_snapshot(
    pasteboard: &NSPasteboard,
) -> Result<(Vec<String>, ClipboardSensitivity), BackendError> {
    let Some(native_types) = pasteboard.types() else {
        return Ok((Vec::new(), ClipboardSensitivity::default()));
    };
    if native_types.count() > MAX_NATIVE_TYPES {
        return Err(BackendError::InvalidInput(
            "pasteboard contains too many types".into(),
        ));
    }
    let mut types = Vec::with_capacity(native_types.count());
    for index in 0..native_types.count() {
        let native_type = native_types.objectAtIndex(index);
        if native_type.length() > MAX_NATIVE_TYPE_UNITS {
            return Err(BackendError::InvalidInput(
                "pasteboard type name exceeds the size limit".into(),
            ));
        }
        types.push(native_type.to_string());
    }
    let sensitivity = sensitivity_for_types(types.iter().map(String::as_str));
    Ok((types, sensitivity))
}

fn observed_marker(pasteboard: &NSPasteboard) -> Option<ClipboardMarker> {
    let marker_type = NSString::from_str(MARKER_TYPE);
    let bytes = pasteboard.dataForType(&marker_type)?;
    if bytes.len() != 8 {
        return None;
    }
    decode_marker(&bytes.to_vec())
}

fn native_write_is_still_owned(state: &WorkerState, count: isize) -> bool {
    let marker = observed_marker(&state.pasteboard);
    let still_current = state.pasteboard.changeCount() == count;
    still_current
        && (state.owned.as_ref().is_some_and(|owned| {
            owned.count == count
                && marker == Some(owned.marker)
                && owns_delayed_item(owned.count, count, marker, owned.marker)
        }) || state
            .delayed
            .as_ref()
            .is_some_and(|delayed| owns_delayed_item(delayed.count, count, marker, delayed.marker)))
}

fn read_content(
    state: &WorkerState,
    format: &ClipboardFormat,
) -> Result<Option<ClipboardContent>, BackendError> {
    if *format == ClipboardFormat::Files {
        return read_files(state).map(|files| files.map(ClipboardContent::files));
    }
    let before = state.pasteboard.changeCount();
    let result = (|| {
        let (types, sensitivity) = native_type_snapshot(&state.pasteboard)?;
        if sensitivity.should_exclude() {
            return Ok(None);
        }
        let bytes = match format {
            ClipboardFormat::Text => {
                data_for_type(&state.pasteboard, ns_string!("public.utf8-plain-text"))?
            }
            ClipboardFormat::Html => data_for_type(&state.pasteboard, ns_string!("public.html"))?,
            ClipboardFormat::Rtf => data_for_type(&state.pasteboard, ns_string!("public.rtf"))?,
            ClipboardFormat::Png => {
                let has_png = types.iter().any(|ty| {
                    crate::clipboard_logic::classify_type(ty)
                        == crate::clipboard_logic::NativeType::Png
                });
                let has_tiff = types.iter().any(|ty| {
                    crate::clipboard_logic::classify_type(ty)
                        == crate::clipboard_logic::NativeType::Tiff
                });
                match choose_image_source(has_png, has_tiff) {
                    Some(ImageSource::Png) => {
                        data_for_type(&state.pasteboard, ns_string!("public.png"))?
                    }
                    Some(ImageSource::Tiff) => {
                        data_for_type(&state.pasteboard, ns_string!("public.tiff"))?
                            .map(convert_tiff_to_png)
                            .transpose()?
                    }
                    None => None,
                }
            }
            ClipboardFormat::Other(_) => {
                return Err(BackendError::Unsupported);
            }
            ClipboardFormat::Files => {
                return Err(BackendError::InvalidInput(
                    "use read_files for file URLs".into(),
                ));
            }
        };
        let Some(bytes) = bytes else {
            return Ok(None);
        };
        let bytes = WipeBytes(bytes);
        validate_byte_content(format, bytes.as_slice())?;
        Ok(Some((bytes, sensitivity)))
    })();
    if state.pasteboard.changeCount() != before {
        drop(result);
        return Ok(None);
    }
    let Some((bytes, sensitivity)) = result? else {
        return Ok(None);
    };
    Ok(Some(ClipboardContent {
        format: format.clone(),
        data: ClipboardData::Bytes(bytes.into_vec()),
        sensitivity,
    }))
}

fn data_for_type(
    pasteboard: &NSPasteboard,
    native_type: &NSPasteboardType,
) -> Result<Option<Vec<u8>>, BackendError> {
    let Some(data) = pasteboard.dataForType(native_type) else {
        return Ok(None);
    };
    if data.len() > MAX_CLIPBOARD_BYTES {
        return Err(BackendError::InvalidInput(
            "clipboard representation exceeds the size limit".into(),
        ));
    }
    Ok(Some(data.to_vec()))
}

fn convert_tiff_to_png(data: Vec<u8>) -> Result<Vec<u8>, BackendError> {
    let data = WipeBytes(data);
    if crate::clipboard_logic::safe_tiff_dimensions(data.as_slice()).is_none() {
        return Err(BackendError::InvalidInput(
            "pasteboard TIFF dimensions are invalid or exceed the decode limit".into(),
        ));
    }
    // The compressed source may contain clipboard secrets. Keep the Rust-side copy zeroized
    // when this conversion returns; AppKit's internal decoder allocation is outside our control.
    let input = NSData::with_bytes(data.as_slice());
    let rep = NSBitmapImageRep::imageRepWithData(&input)
        .ok_or_else(|| BackendError::Failed("pasteboard TIFF could not be decoded".into()))?;
    let properties = NSDictionary::<objc2_app_kit::NSBitmapImageRepPropertyKey, AnyObject>::new();
    // SAFETY: An empty dictionary uses the exact generated property key/value types and is valid.
    let png =
        unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &properties) }
            .ok_or_else(|| {
                BackendError::Failed("pasteboard TIFF could not be encoded as PNG".into())
            })?;
    if png.len() > MAX_CLIPBOARD_BYTES {
        return Err(BackendError::InvalidInput(
            "converted clipboard image exceeds the size limit".into(),
        ));
    }
    Ok(png.to_vec())
}

fn write_content(
    state: &mut WorkerState,
    content: OwnedContent,
    marker: ClipboardMarker,
) -> Result<(), BackendError> {
    if content.sensitivity.should_exclude() {
        return Err(BackendError::InvalidInput(
            "sensitive clipboard content is excluded".into(),
        ));
    }
    let bytes = match content.data {
        OwnedData::Bytes(bytes) => bytes,
        OwnedData::Files(files) => return write_files(state, files, marker),
    };
    let native_type = validate_byte_content(&content.format, bytes.as_slice())?;
    let mut owned = reusable_owned_items(state, marker);
    let item = match owned.primary.take() {
        Some(item) => item,
        None => NSPasteboardItem::new(),
    };
    let data = NSData::with_bytes(bytes.as_slice());
    if !item.setData_forType(&data, native_type) || !set_marker(&item, marker) {
        return Err(BackendError::Failed("pasteboard write failed".into()));
    }
    let mut items = Vec::with_capacity(owned.files.len() + 1);
    items.push(item.clone());
    items.extend(owned.files.iter().cloned());
    if !write_items(&state.pasteboard, &items) {
        return Err(BackendError::Failed("pasteboard write failed".into()));
    }
    let count = state.pasteboard.changeCount();
    if observed_marker(&state.pasteboard) != Some(marker) || state.pasteboard.changeCount() != count
    {
        state.owned = None;
        state.delayed = None;
        return Err(BackendError::Unavailable);
    }
    state.last_count = count;
    state.own_count = Some(count);
    state.owned = Some(OwnedClipboard {
        marker,
        count,
        primary: Some(item),
        files: owned.files,
    });
    state.delayed = None;
    Ok(())
}

fn read_bundle(state: &WorkerState) -> Result<ClipboardSnapshot, BackendError> {
    let before = state.pasteboard.changeCount();
    let (types, sensitivity) = native_type_snapshot(&state.pasteboard)?;
    let marker = if state.own_count == Some(before) {
        observed_marker(&state.pasteboard)
    } else {
        None
    };
    let mut contents = Vec::new();
    if !sensitivity.should_exclude() {
        for format in formats_for_types(types.iter().map(String::as_str)) {
            if let Some(content) = read_content(state, &format)? {
                contents.push(content);
            }
            validate_clipboard_bundle(&contents)?;
        }
    }
    if state.pasteboard.changeCount() != before {
        return Err(BackendError::ClipboardChanged);
    }
    Ok(ClipboardSnapshot {
        contents,
        sensitivity,
        marker,
        change_token: ClipboardChangeToken(before as u64),
    })
}

fn prepare_file_url(
    entry: &FileEntry,
) -> Result<(Retained<NSString>, Retained<NSString>), BackendError> {
    let path = entry
        .path
        .to_str()
        .filter(|path| {
            entry.path.is_absolute()
                && !path.contains('\0')
                && path.encode_utf16().count() <= MAX_FILE_URL_UNITS
        })
        .ok_or_else(|| BackendError::InvalidInput("invalid clipboard file path".into()))?;
    let metadata = std::fs::symlink_metadata(&entry.path).map_err(|_| BackendError::Unavailable)?;
    if metadata.file_type().is_symlink()
        || metadata.is_dir() != entry.is_dir
        || !(metadata.is_file() || metadata.is_dir())
    {
        return Err(BackendError::InvalidInput(
            "clipboard file type mismatch".into(),
        ));
    }
    let path = NSString::from_str(path);
    let url = NSURL::fileURLWithPath(&path)
        .absoluteString()
        .ok_or(BackendError::Unavailable)?;
    Ok((path, url))
}

fn publish_bundle(
    state: &mut WorkerState,
    contents: Vec<OwnedContent>,
    marker: ClipboardMarker,
    expected: ClipboardChangeToken,
    admission: ClipboardAdmission,
) -> Result<ClipboardPublish, BackendError> {
    // One pasteboard item per file, each with its own public.file-url: the legacy NSFilenamesPboardType is not a
    // valid UTI and macOS refuses to store it on an item, which made every received file clipboard fail.
    let item = NSPasteboardItem::new();
    let mut has_bytes = false;
    let mut file_items: Vec<Retained<NSPasteboardItem>> = Vec::new();
    for content in contents {
        if content.sensitivity.should_exclude() {
            return Err(BackendError::InvalidInput(
                "sensitive clipboard content is excluded".into(),
            ));
        }
        match content.data {
            OwnedData::Bytes(bytes) => {
                let native_type = validate_byte_content(&content.format, bytes.as_slice())?;
                if !item.setData_forType(&NSData::with_bytes(bytes.as_slice()), native_type) {
                    return Err(BackendError::Unavailable);
                }
                has_bytes = true;
            }
            OwnedData::Files(files) => {
                if files.entries.is_empty() || files.entries.len() > MAX_NATIVE_ITEMS {
                    return Err(BackendError::InvalidInput(
                        "invalid clipboard file count".into(),
                    ));
                }
                for entry in files.entries.iter() {
                    let (_, url) = prepare_file_url(entry)?;
                    let file_item = NSPasteboardItem::new();
                    if !file_item.setString_forType(&url, ns_string!("public.file-url"))
                        || !set_marker(&file_item, marker)
                    {
                        return Err(BackendError::Unavailable);
                    }
                    file_items.push(file_item);
                }
            }
        }
    }
    if (has_bytes || file_items.is_empty()) && !set_marker(&item, marker) {
        return Err(BackendError::Unavailable);
    }
    let mut all_items: Vec<Retained<NSPasteboardItem>> = Vec::with_capacity(file_items.len() + 1);
    if has_bytes || file_items.is_empty() {
        all_items.push(item.clone());
    }
    all_items.extend(file_items.iter().cloned());
    let actual = ClipboardChangeToken(state.pasteboard.changeCount() as u64);
    // Only a copy made on this Mac wins over the incoming clipboard. A change Glide made itself (an earlier incoming
    // clipboard landing while this one was on its way) carries Glide's marker and must not drop this one.
    if actual != expected && observed_marker(&state.pasteboard).is_none() {
        return Ok(ClipboardPublish::ReplacedLocalChange {
            actual_change_token: actual,
        });
    }
    if !admission.is_admitted() {
        return Ok(ClipboardPublish::Revoked);
    }
    let owned_count = state.pasteboard.clearContents();
    state.owned = None;
    state.delayed = None;
    let writers: Vec<&ProtocolObject<dyn objc2_app_kit::NSPasteboardWriting>> = all_items
        .iter()
        .map(|item| ProtocolObject::from_ref(&**item))
        .collect();
    let objects = NSArray::from_slice(&writers);
    if !state.pasteboard.writeObjects(&objects) {
        // Never erase another application's newer copy during failure cleanup.
        // A marker alone is forgeable; a changed count may belong to a newer
        // local copy. Preserve it even when the failed write left partial data.
        let cleared = state.pasteboard.changeCount() == owned_count;
        if cleared {
            state.pasteboard.clearContents();
        }
        return Ok(ClipboardPublish::PartialFailure {
            formats_written: 0,
            cleared,
            error: BackendError::Unavailable,
        });
    }
    let count = state.pasteboard.changeCount();
    if observed_marker(&state.pasteboard) != Some(marker) || state.pasteboard.changeCount() != count
    {
        return Ok(ClipboardPublish::ReplacedLocalChange {
            actual_change_token: ClipboardChangeToken(state.pasteboard.changeCount() as u64),
        });
    }
    state.last_count = count;
    state.own_count = Some(count);
    state.owned = Some(OwnedClipboard {
        marker,
        count,
        primary: if has_bytes || file_items.is_empty() {
            Some(item)
        } else {
            None
        },
        files: file_items,
    });
    Ok(ClipboardPublish::Published {
        change_token: ClipboardChangeToken(count as u64),
    })
}

fn read_files(state: &WorkerState) -> Result<Option<FileList>, BackendError> {
    let before = state.pasteboard.changeCount();
    let result = (|| {
        let (types, sensitivity) = native_type_snapshot(&state.pasteboard)?;
        if sensitivity.should_exclude()
            || !types.iter().any(|ty| {
                crate::clipboard_logic::classify_type(ty)
                    == crate::clipboard_logic::NativeType::Files
            })
        {
            return Ok(None);
        }
        let Some(items) = state.pasteboard.pasteboardItems() else {
            return Ok(None);
        };
        if items.count() > MAX_NATIVE_ITEMS {
            return Err(BackendError::InvalidInput(
                "pasteboard contains too many file URLs".into(),
            ));
        }
        let mut paths = Vec::new();
        for index in 0..items.count() {
            let item = items.objectAtIndex(index);
            if let Some(url) = item.stringForType(ns_string!("public.file-url")) {
                if url.length() > MAX_FILE_URL_UNITS {
                    return Err(BackendError::InvalidInput(
                        "clipboard file URL too long".into(),
                    ));
                }
                paths.push(file_url_to_path(&url.to_string()).ok_or_else(|| {
                    BackendError::InvalidInput("invalid clipboard file URL".into())
                })?);
            }
            if paths.len() > MAX_NATIVE_ITEMS {
                return Err(BackendError::InvalidInput(
                    "too many clipboard files".into(),
                ));
            }
        }
        let mut entries = Vec::with_capacity(paths.len());
        for path in paths {
            let metadata = std::fs::symlink_metadata(&path)
                .map_err(|_| BackendError::Failed("pasteboard file URL is unavailable".into()))?;
            if metadata.file_type().is_symlink() {
                return Err(BackendError::InvalidInput(
                    "pasteboard file URL refers to a symbolic link".into(),
                ));
            }
            let name = match path.file_name().and_then(|name| name.to_str()) {
                Some(name) => name.to_owned(),
                None if metadata.is_dir() => path.to_str().map(str::to_owned).ok_or_else(|| {
                    BackendError::InvalidInput("file URL has no valid name".into())
                })?,
                None => {
                    return Err(BackendError::InvalidInput(
                        "file URL has no valid name".into(),
                    ));
                }
            };
            entries.push(FileEntry {
                path,
                name,
                size: if metadata.is_dir() { 0 } else { metadata.len() },
                is_dir: metadata.is_dir(),
            });
        }
        if entries.is_empty() {
            return Ok(None);
        }
        Ok(Some(FileList {
            entries,
            sensitivity,
        }))
    })();
    if state.pasteboard.changeCount() != before {
        return Ok(None);
    }
    result
}

fn write_files(
    state: &mut WorkerState,
    files: FileList,
    marker: ClipboardMarker,
) -> Result<(), BackendError> {
    if files.sensitivity.should_exclude() {
        return Err(BackendError::InvalidInput(
            "sensitive clipboard content is excluded".into(),
        ));
    }
    if files.entries.is_empty() || files.entries.len() > MAX_NATIVE_ITEMS {
        return Err(BackendError::InvalidInput(
            "file list is empty or exceeds the item limit".into(),
        ));
    }
    let mut items = Vec::with_capacity(files.entries.len());
    for entry in files.entries {
        if !entry.path.is_absolute() {
            return Err(BackendError::InvalidInput(
                "file URL path must be absolute".into(),
            ));
        }
        let path = entry.path.to_str().ok_or_else(|| {
            BackendError::InvalidInput("file URL path is not valid Unicode".into())
        })?;
        if path.len() > MAX_FILE_URL_UNITS * 4 || path.encode_utf16().count() > MAX_FILE_URL_UNITS {
            return Err(BackendError::InvalidInput(
                "file URL path exceeds the size limit".into(),
            ));
        }
        let metadata = std::fs::symlink_metadata(&entry.path)
            .map_err(|_| BackendError::Failed("file URL path is unavailable".into()))?;
        if metadata.file_type().is_symlink() || metadata.is_dir() != entry.is_dir {
            return Err(BackendError::InvalidInput(
                "file URL type does not match the local path".into(),
            ));
        }
        let native_url = NSURL::fileURLWithPath(&NSString::from_str(path));
        let url = native_url
            .absoluteString()
            .ok_or_else(|| BackendError::Failed("file URL could not be created".into()))?;
        let item = NSPasteboardItem::new();
        if !item.setString_forType(&url, ns_string!("public.file-url"))
            || !set_marker(&item, marker)
        {
            return Err(BackendError::Failed(
                "pasteboard file URL write failed".into(),
            ));
        }
        items.push(item);
    }
    let mut owned = reusable_owned_items(state, marker);
    let primary = owned.primary.take();
    let mut all_items = Vec::with_capacity(items.len() + if primary.is_some() { 1 } else { 0 });
    if let Some(primary_item) = primary.as_ref() {
        all_items.push(primary_item.clone());
    }
    all_items.extend(items.iter().cloned());
    if !write_items(&state.pasteboard, &all_items) {
        return Err(BackendError::Failed(
            "pasteboard file list write failed".into(),
        ));
    }
    let count = state.pasteboard.changeCount();
    if observed_marker(&state.pasteboard) != Some(marker) || state.pasteboard.changeCount() != count
    {
        state.owned = None;
        return Err(BackendError::Unavailable);
    }
    state.last_count = count;
    state.own_count = Some(count);
    state.owned = Some(OwnedClipboard {
        marker,
        count,
        primary,
        files: items,
    });
    state.delayed = None;
    Ok(())
}

fn reusable_owned_items(state: &WorkerState, marker: ClipboardMarker) -> OwnedClipboard {
    let current_count = state.pasteboard.changeCount();
    let observed = observed_marker(&state.pasteboard);
    if state.pasteboard.changeCount() == current_count {
        if let Some(owned) = state.owned.as_ref() {
            if owned.marker == marker
                && owns_delayed_item(owned.count, current_count, observed, marker)
            {
                return OwnedClipboard {
                    marker,
                    count: current_count,
                    primary: owned.primary.clone(),
                    files: owned.files.clone(),
                };
            }
        }
    }
    OwnedClipboard {
        marker,
        count: current_count,
        primary: None,
        files: Vec::new(),
    }
}

fn validate_byte_content(
    format: &ClipboardFormat,
    bytes: &[u8],
) -> Result<&'static NSPasteboardType, BackendError> {
    if bytes.len() > MAX_CLIPBOARD_BYTES {
        return Err(BackendError::InvalidInput(
            "clipboard representation exceeds the size limit".into(),
        ));
    }
    match format {
        ClipboardFormat::Text | ClipboardFormat::Html => {
            std::str::from_utf8(bytes)
                .map_err(|_| BackendError::InvalidInput("clipboard text must be UTF-8".into()))?;
        }
        ClipboardFormat::Png if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") => {
            return Err(BackendError::InvalidInput(
                "clipboard image is not PNG".into(),
            ));
        }
        ClipboardFormat::Rtf | ClipboardFormat::Png => {}
        ClipboardFormat::Files => {
            return Err(BackendError::InvalidInput(
                "file format requires a file list".into(),
            ));
        }
        ClipboardFormat::Other(_) => return Err(BackendError::Unsupported),
    }
    format_native_type(format).ok_or(BackendError::Unsupported)
}

fn set_delayed(
    state: &mut WorkerState,
    format: ClipboardFormat,
    marker: ClipboardMarker,
    events: &Sender<ClipboardEvent>,
) -> Result<(), BackendError> {
    let native_type = format_native_type(&format).ok_or(BackendError::Unsupported)?;
    if format == ClipboardFormat::Files {
        return Err(BackendError::Unsupported);
    }
    let current_count = state.pasteboard.changeCount();
    let can_reuse = state.delayed.as_ref().is_some_and(|delayed| {
        delayed.marker == marker
            && owns_delayed_item(
                delayed.count,
                current_count,
                observed_marker(&state.pasteboard),
                marker,
            )
    });
    if can_reuse {
        let Some(existing) = state.delayed.as_ref() else {
            return Err(BackendError::Unavailable);
        };
        if existing.registered.contains(&format) {
            return Ok(());
        }
        let item = existing.item.clone();
        let provider = existing._provider.clone();
        let mut owned = reusable_owned_items(state, marker);
        owned.primary = Some(item.clone());
        let native_types = NSArray::from_slice(&[native_type]);
        if !item.setDataProvider_forTypes(ProtocolObject::from_ref(&*provider), &native_types) {
            return Err(BackendError::Failed(
                "pasteboard delayed-render registration failed".into(),
            ));
        }
        let mut items = Vec::with_capacity(owned.files.len() + 1);
        items.push(item.clone());
        items.extend(owned.files.iter().cloned());
        if !write_items(&state.pasteboard, &items) {
            clear_owned_delayed(state);
            return Err(BackendError::Failed(
                "pasteboard delayed-render registration failed".into(),
            ));
        }
        let count = state.pasteboard.changeCount();
        if observed_marker(&state.pasteboard) != Some(marker)
            || state.pasteboard.changeCount() != count
        {
            state.delayed = None;
            state.owned = None;
            return Err(BackendError::Unavailable);
        }
        if let Some(delayed) = state.delayed.as_mut() {
            delayed.count = count;
            delayed.registered.push(format);
        }
        state.last_count = count;
        state.own_count = Some(count);
        state.owned = Some(OwnedClipboard {
            marker,
            count,
            primary: Some(item),
            files: owned.files,
        });
        return Ok(());
    }

    let mut owned = reusable_owned_items(state, marker);
    let item = match owned.primary.take() {
        Some(item) => item,
        None => NSPasteboardItem::new(),
    };
    if !set_marker(&item, marker) {
        return Err(BackendError::Failed(
            "pasteboard marker write failed".into(),
        ));
    }
    let provider = DelayedProvider::new(
        events.clone(),
        state.command_sender.clone(),
        Arc::clone(&state.resync_pending),
        Arc::clone(&state.request_overflow),
        marker,
    );
    let native_types = NSArray::from_slice(&[native_type]);
    if !item.setDataProvider_forTypes(ProtocolObject::from_ref(&*provider), &native_types) {
        return Err(BackendError::Failed(
            "pasteboard delayed-render registration failed".into(),
        ));
    }
    let mut items = Vec::with_capacity(owned.files.len() + 1);
    items.push(item.clone());
    items.extend(owned.files.iter().cloned());
    if !write_items(&state.pasteboard, &items) {
        return Err(BackendError::Failed(
            "pasteboard delayed-render registration failed".into(),
        ));
    }
    let count = state.pasteboard.changeCount();
    if observed_marker(&state.pasteboard) != Some(marker) || state.pasteboard.changeCount() != count
    {
        state.owned = None;
        return Err(BackendError::Unavailable);
    }
    state.last_count = count;
    state.own_count = Some(count);
    state.owned = Some(OwnedClipboard {
        marker,
        count,
        primary: Some(item.clone()),
        files: owned.files,
    });
    state.delayed = Some(ActiveDelayed {
        marker,
        count,
        item,
        _provider: provider,
        registered: vec![format],
        deadlines: HashMap::new(),
    });
    Ok(())
}

fn fulfill_delayed(
    state: &mut WorkerState,
    content: OwnedContent,
    marker: ClipboardMarker,
) -> Result<(), BackendError> {
    if content.sensitivity.should_exclude() {
        return fail_delayed(
            state,
            marker,
            BackendError::InvalidInput("sensitive clipboard content is excluded".into()),
        );
    }
    let OwnedData::Bytes(bytes) = content.data else {
        return fail_delayed(
            state,
            marker,
            BackendError::InvalidInput("delayed rendering requires bytes".into()),
        );
    };
    let native_type = match validate_byte_content(&content.format, bytes.as_slice()) {
        Ok(native_type) => native_type,
        Err(error) => return fail_delayed(state, marker, error),
    };
    let current_count = state.pasteboard.changeCount();
    let observed = observed_marker(&state.pasteboard);
    let valid = state.delayed.as_ref().is_some_and(|delayed| {
        delayed.marker == marker
            && delayed.registered.contains(&content.format)
            && owns_delayed_item(delayed.count, current_count, observed, marker)
            && delayed
                .deadlines
                .get(&content.format)
                .is_some_and(|deadline| *deadline > Instant::now())
    });
    if !valid || state.pasteboard.changeCount() != current_count {
        return fail_delayed(state, marker, BackendError::Unavailable);
    }
    let Some(delayed) = state.delayed.as_ref() else {
        return Err(BackendError::Unavailable);
    };
    let data = NSData::with_bytes(bytes.as_slice());
    if !delayed.item.setData_forType(&data, native_type) {
        return fail_delayed(
            state,
            marker,
            BackendError::Failed("delayed clipboard fulfillment failed".into()),
        );
    }
    // Mutate only the retained current-owned item. Rewriting the pasteboard here could clobber
    // another process that took ownership after the callback ran.
    if state.pasteboard.changeCount() != current_count
        || observed_marker(&state.pasteboard) != Some(marker)
        || state.pasteboard.changeCount() != current_count
    {
        return fail_delayed(state, marker, BackendError::Unavailable);
    }
    if let Some(delayed) = state.delayed.as_mut() {
        delayed.deadlines.remove(&content.format);
    }
    Ok(())
}

fn fail_delayed(
    state: &mut WorkerState,
    marker: ClipboardMarker,
    error: BackendError,
) -> Result<(), BackendError> {
    if state
        .delayed
        .as_ref()
        .is_some_and(|delayed| delayed.marker == marker)
    {
        clear_owned_delayed(state);
    }
    Err(error)
}

fn cancel_delayed(state: &mut WorkerState, marker: ClipboardMarker) -> Result<(), BackendError> {
    if state
        .delayed
        .as_ref()
        .is_some_and(|delayed| delayed.marker == marker)
    {
        clear_owned_delayed(state);
    }
    Ok(())
}

fn set_marker(item: &NSPasteboardItem, marker: ClipboardMarker) -> bool {
    let marker_type = NSString::from_str(MARKER_TYPE);
    let data = NSData::with_bytes(&encode_marker(marker));
    item.setData_forType(&data, &marker_type)
}

fn write_items(pasteboard: &NSPasteboard, items: &[Retained<NSPasteboardItem>]) -> bool {
    pasteboard.clearContents();
    let writers: Vec<&ProtocolObject<dyn objc2_app_kit::NSPasteboardWriting>> = items
        .iter()
        .map(|item| ProtocolObject::from_ref(&**item))
        .collect();
    let objects = NSArray::from_slice(&writers);
    pasteboard.writeObjects(&objects)
}

fn format_native_type(format: &ClipboardFormat) -> Option<&'static NSPasteboardType> {
    match format {
        ClipboardFormat::Text => Some(ns_string!("public.utf8-plain-text")),
        ClipboardFormat::Html => Some(ns_string!("public.html")),
        ClipboardFormat::Rtf => Some(ns_string!("public.rtf")),
        ClipboardFormat::Png => Some(ns_string!("public.png")),
        ClipboardFormat::Files | ClipboardFormat::Other(_) => None,
    }
}

fn format_for_native_type(native_type: &NSPasteboardType) -> Option<ClipboardFormat> {
    if native_type.isEqualToString(ns_string!("public.utf8-plain-text")) {
        Some(ClipboardFormat::Text)
    } else if native_type.isEqualToString(ns_string!("public.html")) {
        Some(ClipboardFormat::Html)
    } else if native_type.isEqualToString(ns_string!("public.rtf")) {
        Some(ClipboardFormat::Rtf)
    } else if native_type.isEqualToString(ns_string!("public.png")) {
        Some(ClipboardFormat::Png)
    } else {
        None
    }
}
