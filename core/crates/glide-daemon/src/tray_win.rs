//! Windows tray icon and hidden message windows. The Shell_NotifyIcon owner is
//! message-only; a separate hidden top-level window receives Explorer broadcasts.

use std::ffi::c_void;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    mpsc as std_mpsc, Arc, Mutex, TryLockError,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use glide_proto::ipc::Event;
use tokio::sync::mpsc;
use tracing::debug;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow};
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_ERROR, NIIF_INFO,
    NIIF_WARNING, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NIN_SELECT, NOTIFYICONDATAW,
    NOTIFYICON_VERSION_4,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconFromResourceEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW,
    KillTimer, PostMessageW, PostQuitMessage, PostThreadMessageW, RegisterClassW,
    RegisterWindowMessageW, SetForegroundWindow, SetTimer, SetWindowLongPtrW, TrackPopupMenu,
    TranslateMessage, CREATESTRUCTW, CW_USEDEFAULT, GWLP_USERDATA, HMENU, HWND_MESSAGE, MF_CHECKED,
    MF_GRAYED, MF_SEPARATOR, MF_STRING, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_CLOSE,
    WM_CONTEXTMENU, WM_DESTROY, WM_DPICHANGED, WM_ENDSESSION, WM_LBUTTONUP, WM_NCCREATE,
    WM_NCDESTROY, WM_QUERYENDSESSION, WM_QUIT, WM_RBUTTONUP, WM_TIMER, WM_USER, WNDCLASSW,
};

use super::{icon_frame, Balloon, BalloonLevel, MenuItem, TrayAction, TrayError, TrayModel};

const TRAY_CALLBACK: u32 = WM_APP + 1;
const REFRESH_MESSAGE: u32 = WM_APP + 2;
const TASKBAR_READD_MESSAGE: u32 = WM_APP + 3;
const DPI_MESSAGE: u32 = WM_APP + 4;
const SESSION_END_MESSAGE: u32 = WM_APP + 5;
const REFRESH_TIMER: usize = 1;
const ICON_ID: u32 = 1;
const ICON_RESOURCE_VERSION: u32 = 0x0003_0000;
const TOOLTIP_LIMIT: Duration = Duration::from_secs(1);
const DROP_WAIT: Duration = Duration::from_millis(200);
const KEY_SELECT: u32 = WM_USER + 1;
static TASKBAR_MESSAGE: AtomicU32 = AtomicU32::new(0);
const OWNER_CLASS: &[u16] = &[
    71, 108, 105, 100, 101, 84, 114, 97, 121, 79, 119, 110, 101, 114, 0,
];
const BROADCAST_CLASS: &[u16] = &[
    71, 108, 105, 100, 101, 84, 114, 97, 121, 66, 114, 111, 97, 100, 99, 97, 115, 116, 0,
];
const WINDOW_TITLE: &[u16] = &[
    71, 108, 105, 100, 101, 84, 114, 97, 121, 87, 105, 110, 100, 111, 119, 0,
];
const TASKBAR_CREATED_NAME: &[u16] = &[
    84, 97, 115, 107, 98, 97, 114, 67, 114, 101, 97, 116, 101, 100, 0,
];

#[derive(Clone)]
struct Shared(Arc<Mutex<TrayModel>>);

pub struct Tray {
    owner: isize,
    thread_id: u32,
    thread: Option<JoinHandle<()>>,
    done: std_mpsc::Receiver<()>,
    shared: Shared,
}

impl Tray {
    pub fn start(
        ui: Option<PathBuf>,
        actions: mpsc::Sender<TrayAction>,
    ) -> Result<Self, TrayError> {
        let shared = Shared(Arc::new(Mutex::new(TrayModel::default())));
        let (ready_tx, ready_rx) = std_mpsc::sync_channel(1);
        let (done_tx, done) = std_mpsc::sync_channel(1);
        let thread_shared = shared.clone();
        let thread = thread::Builder::new()
            .name("glide-tray".to_owned())
            .spawn(move || {
                run(thread_shared, ui, actions, ready_tx);
                let _ = done_tx.send(());
            })
            .map_err(|error| TrayError(format!("could not start Windows tray thread: {error}")))?;
        match ready_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok((owner, thread_id))) => Ok(Self {
                owner,
                thread_id,
                thread: Some(thread),
                done,
                shared,
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(TrayError(error))
            }
            Err(_) => Err(TrayError(
                "Windows tray startup timed out or stopped".to_owned(),
            )),
        }
    }

    pub fn update(&self, event: &Event, clients: usize) {
        let changed = match self.shared.0.try_lock() {
            Ok(mut model) => model.update(event, clients, Instant::now()),
            Err(TryLockError::WouldBlock) => return,
            Err(TryLockError::Poisoned(_)) => {
                debug!("tray state lock was poisoned");
                return;
            }
        };
        if changed {
            // SAFETY: owner is a message-only window on the live tray thread.
            unsafe {
                PostMessageW(self.owner as HWND, REFRESH_MESSAGE, 0, 0);
            }
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        // SAFETY: the native calls only enqueue shutdown messages for the tray thread.
        unsafe {
            PostMessageW(self.owner as HWND, WM_CLOSE, 0, 0);
        }
        if self.done.recv_timeout(DROP_WAIT).is_ok() {
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        } else {
            // A menu or shell call can delay the window procedure; keep daemon exit bounded.
            unsafe {
                PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0);
            }
            drop(self.thread.take());
        }
    }
}

struct WindowState {
    shared: Shared,
    ui: Option<PathBuf>,
    actions: mpsc::Sender<TrayAction>,
    owner: HWND,
    broadcast: HWND,
    icons: [isize; 2],
    active_icon: usize,
    taskbar_created: u32,
    last_tip_update: Instant,
    initialized: Arc<AtomicBool>,
}

fn run(
    shared: Shared,
    ui: Option<PathBuf>,
    actions: mpsc::Sender<TrayAction>,
    ready: std_mpsc::SyncSender<Result<(isize, u32), String>>,
) {
    // SAFETY: all Win32 handles and both window procedures are confined to this thread.
    unsafe {
        let instance = GetModuleHandleW(null());
        if instance.is_null() {
            let _ = ready.send(Err("GetModuleHandleW failed for the tray".to_owned()));
            return;
        }
        if !register_window_class(instance, OWNER_CLASS.as_ptr(), Some(owner_proc))
            || !register_window_class(instance, BROADCAST_CLASS.as_ptr(), Some(broadcast_proc))
        {
            let _ = ready.send(Err(
                "could not register hidden tray window classes".to_owned()
            ));
            return;
        }
        let icons = match load_icons() {
            Ok(icons) => icons,
            Err(error) => {
                let _ = ready.send(Err(error));
                return;
            }
        };
        let taskbar_created = RegisterWindowMessageW(TASKBAR_CREATED_NAME.as_ptr());
        if taskbar_created == 0 {
            DestroyIcon(icons[0] as _);
            DestroyIcon(icons[1] as _);
            let _ = ready.send(Err(
                "could not register the Explorer restart message".to_owned()
            ));
            return;
        }
        TASKBAR_MESSAGE.store(taskbar_created, Ordering::Relaxed);
        let initialized = Arc::new(AtomicBool::new(false));
        let mut state = Box::new(WindowState {
            shared,
            ui,
            actions,
            owner: null_mut(),
            broadcast: null_mut(),
            icons,
            active_icon: icon_index(GetDpiForSystem()),
            taskbar_created,
            last_tip_update: Instant::now() - TOOLTIP_LIMIT,
            initialized: initialized.clone(),
        });
        // Keep storage alive through nested menu message loops, even if WM_CLOSE destroys the HWND.
        let state_ptr = &mut *state as *mut WindowState;
        let owner = CreateWindowExW(
            0,
            OWNER_CLASS.as_ptr(),
            WINDOW_TITLE.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            null_mut(),
            instance,
            state_ptr.cast::<c_void>(),
        );
        if owner.is_null() {
            if !initialized.load(Ordering::Acquire) {
                DestroyIcon(icons[0] as _);
                DestroyIcon(icons[1] as _);
            }
            let _ = ready.send(Err(
                "could not create the message-only tray owner".to_owned()
            ));
            return;
        }
        (*state_ptr).owner = owner;
        let broadcast = CreateWindowExW(
            0,
            BROADCAST_CLASS.as_ptr(),
            WINDOW_TITLE.as_ptr(),
            0,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            0,
            0,
            null_mut(),
            null_mut(),
            instance,
            owner.cast::<c_void>(),
        );
        if broadcast.is_null() {
            DestroyWindow(owner);
            let _ = ready.send(Err(
                "could not create the hidden tray broadcast window".to_owned()
            ));
            return;
        }
        (*state_ptr).broadcast = broadcast;
        let tooltip = (*state_ptr)
            .shared
            .0
            .lock()
            .map(|model| model.view.tooltip())
            .unwrap_or_else(|_| "Glide".to_owned());
        if !shell_add(owner, icons[(*state_ptr).active_icon] as _, &tooltip) {
            DestroyWindow(owner);
            let _ = ready.send(Err(
                "Shell_NotifyIconW could not add the Glide icon".to_owned()
            ));
            return;
        }
        let thread_id = GetCurrentThreadId();
        if ready.send(Ok((owner as isize, thread_id))).is_err() {
            DestroyWindow(owner);
            return;
        }

        let mut message = std::mem::zeroed();
        loop {
            let result = GetMessageW(&mut message, null_mut(), 0, 0);
            if result <= 0 {
                break;
            }
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        if !is_window_destroyed(owner) {
            DestroyWindow(owner);
        }
    }
}

unsafe fn register_window_class(
    instance: windows_sys::Win32::Foundation::HINSTANCE,
    name: *const u16,
    proc: windows_sys::Win32::UI::WindowsAndMessaging::WNDPROC,
) -> bool {
    let class = WNDCLASSW {
        lpfnWndProc: proc,
        hInstance: instance,
        lpszClassName: name,
        ..Default::default()
    };
    RegisterClassW(&class) != 0
}

unsafe extern "system" fn owner_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = &*(lparam as *const CREATESTRUCTW);
        let state = create.lpCreateParams as *mut WindowState;
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
        (*state).initialized.store(true, Ordering::Release);
        return DefWindowProcW(hwnd, message, wparam, lparam);
    }
    let state_ptr =
        windows_sys::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(hwnd, GWLP_USERDATA)
            as *mut WindowState;
    if state_ptr.is_null() {
        return DefWindowProcW(hwnd, message, wparam, lparam);
    }
    let state = &mut *state_ptr;
    if message == state.taskbar_created || message == TASKBAR_READD_MESSAGE {
        let view = state
            .shared
            .0
            .lock()
            .map(|model| model.view.clone())
            .unwrap_or_else(|_| super::TrayModel::default().view);
        let _ = shell_add(hwnd, state.icons[state.active_icon] as _, &view.tooltip());
        state.last_tip_update = Instant::now();
        return 0;
    }
    match message {
        SESSION_END_MESSAGE => {
            let _ = glide_platform_win::restore_system_cursors();
            let _ = state.actions.try_send(TrayAction::Quit);
        }
        REFRESH_MESSAGE => refresh(state),
        DPI_MESSAGE | WM_DPICHANGED => change_icon_for_dpi(state, wparam),
        WM_TIMER if wparam == REFRESH_TIMER => {
            KillTimer(hwnd, REFRESH_TIMER);
            refresh(state);
        }
        TRAY_CALLBACK if is_left_click(lparam) => open_ui(state),
        TRAY_CALLBACK if is_right_click(lparam) => {
            let view = state
                .shared
                .0
                .try_lock()
                .ok()
                .map(|model| model.view.clone());
            let owner = state.owner;
            let ui_available = state.ui.is_some();
            let actions = state.actions.clone();
            if let Some(view) = view {
                show_menu(view, owner, ui_available, actions);
            }
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            return 0;
        }
        WM_DESTROY => {
            let data = icon_data(hwnd, state.icons[state.active_icon] as _);
            Shell_NotifyIconW(NIM_DELETE, &data);
            for icon in state.icons {
                DestroyIcon(icon as _);
            }
            if !state.broadcast.is_null() {
                DestroyWindow(state.broadcast);
            }
            PostQuitMessage(0);
            return 0;
        }
        WM_NCDESTROY => {
            let result = DefWindowProcW(hwnd, message, wparam, lparam);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            return result;
        }
        _ => {}
    }
    DefWindowProcW(hwnd, message, wparam, lparam)
}

unsafe extern "system" fn broadcast_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = &*(lparam as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        return DefWindowProcW(hwnd, message, wparam, lparam);
    }
    let owner =
        windows_sys::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(hwnd, GWLP_USERDATA) as HWND;
    if message == TASKBAR_MESSAGE.load(Ordering::Relaxed) && !owner.is_null() {
        PostMessageW(owner, TASKBAR_READD_MESSAGE, 0, 0);
        return 0;
    }
    if message == WM_DPICHANGED && !owner.is_null() {
        PostMessageW(owner, DPI_MESSAGE, wparam, 0);
        return 0;
    }
    if message == WM_QUERYENDSESSION || (message == WM_ENDSESSION && wparam != 0) {
        let _ = glide_platform_win::restore_system_cursors();
        if !owner.is_null() {
            PostMessageW(owner, SESSION_END_MESSAGE, 0, 0);
        }
        return if message == WM_QUERYENDSESSION { 1 } else { 0 };
    }
    if message == WM_NCDESTROY {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
    }
    DefWindowProcW(hwnd, message, wparam, lparam)
}

fn is_left_click(lparam: LPARAM) -> bool {
    let event = lparam as u16 as u32;
    event == WM_LBUTTONUP || event == NIN_SELECT || event == KEY_SELECT
}

fn is_right_click(lparam: LPARAM) -> bool {
    let event = lparam as u16 as u32;
    event == WM_RBUTTONUP || event == WM_CONTEXTMENU
}

unsafe fn refresh(state: &mut WindowState) {
    let (view, balloon) = match state.shared.0.try_lock() {
        Ok(mut model) => (model.view.clone(), model.pending_balloon.take()),
        Err(_) => return,
    };
    let now = Instant::now();
    if now.duration_since(state.last_tip_update) >= TOOLTIP_LIMIT {
        let mut data = icon_data(state.owner, state.icons[state.active_icon] as _);
        data.uFlags = NIF_TIP;
        write_wide(&mut data.szTip, &view.tooltip());
        if Shell_NotifyIconW(NIM_MODIFY, &data) != 0 {
            state.last_tip_update = now;
        }
    } else {
        let delay = TOOLTIP_LIMIT
            .saturating_sub(now.duration_since(state.last_tip_update))
            .as_millis()
            .max(1) as u32;
        SetTimer(state.owner, REFRESH_TIMER, delay, None);
    }
    if let Some(balloon) = balloon {
        show_balloon(state, balloon);
    }
}

unsafe fn change_icon_for_dpi(state: &mut WindowState, wparam: WPARAM) {
    let dpi = if wparam == 0 {
        GetDpiForWindow(state.broadcast)
    } else {
        (wparam & 0xffff) as u32
    };
    let dpi = if dpi == 0 { GetDpiForSystem() } else { dpi };
    let desired = icon_index(dpi);
    if desired != state.active_icon {
        state.active_icon = desired;
        let mut data = icon_data(state.owner, state.icons[state.active_icon] as _);
        data.uFlags = NIF_ICON;
        Shell_NotifyIconW(NIM_MODIFY, &data);
    }
}

fn icon_index(dpi: u32) -> usize {
    usize::from(dpi >= 144)
}

unsafe fn show_balloon(state: &WindowState, balloon: Balloon) {
    let mut data = icon_data(state.owner, state.icons[state.active_icon] as _);
    data.uFlags = NIF_INFO;
    write_wide(&mut data.szInfoTitle, &balloon.title);
    write_wide(&mut data.szInfo, &balloon.body);
    data.dwInfoFlags = match balloon.level {
        BalloonLevel::Info => NIIF_INFO,
        BalloonLevel::Warning => NIIF_WARNING,
        BalloonLevel::Error => NIIF_ERROR,
    };
    Shell_NotifyIconW(NIM_MODIFY, &data);
}

unsafe fn show_menu(
    view: super::TrayView,
    owner: HWND,
    ui_available: bool,
    actions: mpsc::Sender<TrayAction>,
) {
    let menu = CreatePopupMenu();
    if menu.is_null() {
        return;
    }
    let mut id = 1usize;
    for item in view.menu(ui_available) {
        match item {
            MenuItem::OpenGlide { enabled } => append(menu, id, "Open Glide", !enabled, false),
            MenuItem::ShareKeyboardMouse { checked } => {
                append(menu, id, "Share keyboard and mouse", false, checked)
            }
            MenuItem::ReturnHome => append(menu, id, "Return to this computer", false, false),
            MenuItem::OpenLogs => append(menu, id, "Open logs folder", false, false),
            MenuItem::Status(text) => append(menu, id, &text, true, false),
            MenuItem::Separator => {
                AppendMenuW(menu, MF_SEPARATOR, 0, null());
                continue;
            }
            MenuItem::Quit => append(menu, id, "Quit Glide", false, false),
        }
        id += 1;
    }
    let mut point = POINT { x: 0, y: 0 };
    GetCursorPos(&mut point);
    SetForegroundWindow(owner);
    let selected = TrackPopupMenu(
        menu,
        TPM_RIGHTBUTTON | TPM_RETURNCMD,
        point.x,
        point.y,
        0,
        owner,
        null(),
    );
    PostMessageW(
        owner,
        windows_sys::Win32::UI::WindowsAndMessaging::WM_NULL,
        0,
        0,
    );
    DestroyMenu(menu);
    let action = match selected {
        1 if ui_available => Some(TrayAction::Open),
        2 => Some(TrayAction::ToggleSharing),
        3 => Some(TrayAction::ReturnHome),
        5 => Some(TrayAction::OpenLogs),
        6 => Some(TrayAction::Quit),
        _ => None,
    };
    if let Some(action) = action {
        if matches!(action, TrayAction::Quit) {
            let _ = glide_platform_win::restore_system_cursors();
        }
        let _ = actions.try_send(action);
    }
}

unsafe fn append(menu: HMENU, id: usize, label: &str, disabled: bool, checked: bool) {
    let mut flags = MF_STRING;
    if disabled {
        flags |= MF_GRAYED;
    }
    if checked {
        flags |= MF_CHECKED;
    }
    let text: Vec<u16> = label.encode_utf16().chain(std::iter::once(0)).collect();
    AppendMenuW(menu, flags, id, text.as_ptr());
}

fn action(state: &WindowState, action: TrayAction) {
    if state.actions.try_send(action).is_err() {
        debug!(?action, "tray action queue is full or closed");
    }
}

fn open_ui(state: &WindowState) {
    if state.ui.is_some() {
        action(state, TrayAction::Open);
    }
}

unsafe fn shell_add(
    hwnd: HWND,
    icon: windows_sys::Win32::UI::WindowsAndMessaging::HICON,
    tip: &str,
) -> bool {
    let mut data = icon_data(hwnd, icon);
    data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    data.uCallbackMessage = TRAY_CALLBACK;
    data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    write_wide(&mut data.szTip, tip);
    Shell_NotifyIconW(NIM_ADD, &data) != 0 && Shell_NotifyIconW(NIM_SETVERSION, &data) != 0
}

fn icon_data(
    hwnd: HWND,
    icon: windows_sys::Win32::UI::WindowsAndMessaging::HICON,
) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: ICON_ID,
        hIcon: icon,
        ..Default::default()
    }
}

fn write_wide<const N: usize>(buffer: &mut [u16; N], text: &str) {
    buffer.fill(0);
    for (slot, value) in buffer
        .iter_mut()
        .take(N.saturating_sub(1))
        .zip(text.encode_utf16())
    {
        *slot = value;
    }
}

fn load_icons() -> Result<[isize; 2], String> {
    let bytes = include_bytes!("../assets/tray.ico");
    let small =
        icon_frame(bytes, 16).ok_or_else(|| "embedded tray icon lacks a 16 px frame".to_owned())?;
    let large =
        icon_frame(bytes, 32).ok_or_else(|| "embedded tray icon lacks a 32 px frame".to_owned())?;
    // SAFETY: both slices are bounds-checked ICO image payloads valid for the duration of each call.
    let small_icon = unsafe {
        CreateIconFromResourceEx(
            small.as_ptr(),
            small.len() as u32,
            1,
            ICON_RESOURCE_VERSION,
            16,
            16,
            0,
        )
    };
    let large_icon = unsafe {
        CreateIconFromResourceEx(
            large.as_ptr(),
            large.len() as u32,
            1,
            ICON_RESOURCE_VERSION,
            32,
            32,
            0,
        )
    };
    if small_icon.is_null() || large_icon.is_null() {
        unsafe {
            if !small_icon.is_null() {
                DestroyIcon(small_icon);
            }
            if !large_icon.is_null() {
                DestroyIcon(large_icon);
            }
        }
        Err("Windows could not decode the embedded tray icon".to_owned())
    } else {
        Ok([small_icon as isize, large_icon as isize])
    }
}

fn is_window_destroyed(hwnd: HWND) -> bool {
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::IsWindow(hwnd) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_decodes_both_embedded_dpi_icon_frames() {
        let icons = load_icons().expect("Windows decodes 16/32 px embedded icons");
        for icon in icons {
            assert_ne!(icon, 0);
            unsafe {
                DestroyIcon(icon as _);
            }
        }
        assert_eq!(icon_index(96), 0);
        assert_eq!(icon_index(144), 1);
        assert!(is_left_click(NIN_SELECT as LPARAM));
        assert!(is_right_click(WM_CONTEXTMENU as LPARAM));
    }

    #[test]
    #[ignore = "requires the Windows Explorer notification area"]
    fn native_tray_readds_on_explorer_message_and_handles_session_shutdown() {
        let (sender, mut receiver) = mpsc::channel(16);
        let tray = Tray::start(None, sender).expect("native tray creation");
        unsafe {
            PostMessageW(tray.owner as HWND, TASKBAR_READD_MESSAGE, 0, 0);
            PostMessageW(tray.owner as HWND, SESSION_END_MESSAGE, 0, 0);
        }
        assert_eq!(receiver.blocking_recv(), Some(TrayAction::Quit));
        let started = Instant::now();
        drop(tray);
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
