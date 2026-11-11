//! The system-wide quick search hotkey and the notification area (tray) icon.
//!
//! Both belong to a hidden window on their own thread, because they have to work while the main
//! window is hidden and `eframe` only runs the app's logic now and then. The hotkey shows the bar
//! straight from this thread, so it appears without waiting for a repaint of the hidden main window;
//! the bar then takes the foreground, which Windows allows the process that received a hotkey.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};

const MOD_ALT: u32 = 0x1;
const MOD_CONTROL: u32 = 0x2;
const MOD_SHIFT: u32 = 0x4;
const MOD_WIN: u32 = 0x8;

/// A key combination, in the terms `RegisterHotKey` takes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Hotkey {
    modifiers: u32,
    key: u32,
}

pub const DEFAULT_HOTKEY: &str = "Alt+Space";

/// Named keys and their virtual-key codes. Letters, digits and F1–F24 are handled separately.
const NAMED_KEYS: &[(&str, u32)] = &[
    ("Space", 0x20),
    ("Tab", 0x09),
    ("Enter", 0x0D),
    ("PageUp", 0x21),
    ("PageDown", 0x22),
    ("End", 0x23),
    ("Home", 0x24),
    ("Insert", 0x2D),
    ("Delete", 0x2E),
    ("`", 0xC0),
];

fn key_from_name(name: &str) -> Option<u32> {
    if let &[c] = name.as_bytes()
        && c.is_ascii_alphanumeric()
    {
        return Some(u32::from(c.to_ascii_uppercase()));
    }
    if let Some(n) = name.strip_prefix(['F', 'f']).and_then(|n| n.parse::<u32>().ok())
        && (1..=24).contains(&n)
    {
        return Some(0x70 + n - 1);
    }
    NAMED_KEYS.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|&(_, vk)| vk)
}

fn is_function_key(key: u32) -> bool {
    (0x70..=0x87).contains(&key)
}

impl Hotkey {
    /// Parses text like `Alt+Space` or `Ctrl+Shift+K`. Only a function key may go without a
    /// modifier: any other key alone would stop working in every other program.
    pub fn parse(text: &str) -> Result<Hotkey, String> {
        let mut modifiers = 0;
        let mut key = None;
        for part in text.split('+').map(str::trim) {
            let modifier = match part.to_ascii_lowercase().as_str() {
                "alt" => Some(MOD_ALT),
                "ctrl" | "control" => Some(MOD_CONTROL),
                "shift" => Some(MOD_SHIFT),
                "win" | "windows" => Some(MOD_WIN),
                _ => None,
            };
            if let Some(m) = modifier {
                modifiers |= m;
            } else if key.is_some() {
                return Err(format!("“{text}” names more than one key"));
            } else {
                key = Some(key_from_name(part).ok_or_else(|| format!("Unknown key “{part}”"))?);
            }
        }
        let key = key.ok_or_else(|| format!("“{text}” does not name a key"))?;
        if modifiers == 0 && !is_function_key(key) {
            return Err("Add Ctrl, Alt, Shift or Win (only F1–F24 work alone)".into());
        }
        Ok(Hotkey { modifiers, key })
    }
}

impl fmt::Display for Hotkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (bit, name) in [(MOD_CONTROL, "Ctrl"), (MOD_ALT, "Alt"), (MOD_SHIFT, "Shift"), (MOD_WIN, "Win")] {
            if self.modifiers & bit != 0 {
                write!(f, "{name}+")?;
            }
        }
        if let Some((name, _)) = NAMED_KEYS.iter().find(|&&(_, vk)| vk == self.key) {
            f.write_str(name)
        } else if is_function_key(self.key) {
            write!(f, "F{}", self.key - 0x70 + 1)
        } else {
            write!(f, "{}", char::from_u32(self.key).unwrap_or('?'))
        }
    }
}

/// What the hotkey thread tells the bar, which picks it up on its next frame.
#[derive(Default)]
pub struct BarSignals {
    /// Whether the bar is meant to be open. The hotkey toggles this rather than reading the
    /// window's visibility, which lags behind the asynchronous show and hide: a quick second press
    /// would see the window still hidden and open it again instead of closing it.
    shown: AtomicBool,
    /// The window is being shown: start afresh.
    open: AtomicBool,
    /// The bar should close itself. It has to be the bar's own thread that hides the window: only
    /// then does Windows pass the foreground on to the window the user was in before.
    close: AtomicBool,
}

/// What the bar has to do on its next frame: first close, then open.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BarRequest {
    pub close: bool,
    pub open: bool,
}

impl BarSignals {
    /// Flips whether the bar is meant to be open, for the hotkey, and asks the bar to follow.
    /// Returns `true` if it is to open, when the caller shows the window.
    pub fn toggle(&self) -> bool {
        let open = !self.shown.fetch_xor(true, Ordering::AcqRel);
        // Before the caller shows the window, so the first frame the bar paints starts afresh.
        if open {
            self.open.store(true, Ordering::Release);
        } else {
            self.close.store(true, Ordering::Release);
        }
        open
    }

    /// The bar closed itself: the next press opens it.
    pub fn closed(&self) {
        self.shown.store(false, Ordering::Release);
    }

    /// Takes what was asked since the bar last looked. Presses that came in between cancel out:
    /// the bar opens again only if the last of them opened it.
    pub fn take(&self) -> BarRequest {
        let close = self.close.swap(false, Ordering::AcqRel);
        let open = self.open.swap(false, Ordering::AcqRel) && self.shown.load(Ordering::Acquire);
        BarRequest { close, open }
    }
}

/// What the background thread asks of the app.
pub enum Event {
    ShowMain,
    Quit,
    /// The result of registering the hotkey: `Err` holds the reason it does not work.
    HotkeyRegistered(Result<(), String>),
}

#[cfg(windows)]
pub use platform::{Background, activate_running_instance, focus_bar, hide_bar, show_bar};

/// Other platforms have no quick search: `start` fails, so the window just closes as usual.
#[cfg(not(windows))]
pub use stub::{Background, activate_running_instance, focus_bar, hide_bar, show_bar};

#[cfg(not(windows))]
mod stub {
    use super::{BarSignals, Event, Hotkey};
    use eframe::egui;
    use std::sync::Arc;

    pub struct Background {
        pub events: crossbeam_channel::Receiver<Event>,
    }

    impl Background {
        pub fn start(_hotkey: Hotkey, _ctx: egui::Context, _signals: Arc<BarSignals>) -> std::io::Result<Background> {
            Err(std::io::ErrorKind::Unsupported.into())
        }

        pub fn set_hotkey(&self, _hotkey: Hotkey) {}
    }

    pub fn activate_running_instance() -> bool {
        false
    }

    pub fn hide_bar() {}

    pub fn show_bar() {}

    pub fn focus_bar() -> bool {
        true
    }
}

#[cfg(windows)]
mod platform {
    use super::{BarSignals, Event, Hotkey};
    use crate::quick_bar;
    use crossbeam_channel::{Receiver, Sender, unbounded};
    use eframe::egui;
    use std::cell::{Cell, RefCell};
    use std::io;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromWindow};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{HOT_KEY_MODIFIERS, MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey};
    use windows_sys::Win32::UI::Shell::{
        NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_SETVERSION, NIN_SELECT, NINF_KEY, NOTIFYICON_VERSION_4,
        NOTIFYICONDATAW, Shell_NotifyIconW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AllowSetForegroundWindow, AppendMenuW, CW_USEDEFAULT, ChangeWindowMessageFilterEx, CreatePopupMenu, CreateWindowExW,
        DefWindowProcW, DestroyMenu, DestroyWindow, DispatchMessageW, FindWindowExW, FindWindowW, GetCursorPos,
        GetForegroundWindow, GetMessageW, GetSystemMetrics, GetWindowRect, GetWindowThreadProcessId, HICON, IMAGE_ICON,
        IsWindowVisible, LoadImageW, MF_STRING, MSG, MSGFLT_ALLOW, PostMessageW, PostQuitMessage, RegisterClassW,
        RegisterWindowMessageW, SM_CXSMICON, SM_CYSMICON, SW_HIDE, SW_SHOWNORMAL, SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE, SWP_NOSIZE,
        SWP_NOZORDER, SetForegroundWindow, SetWindowPos, ShowWindowAsync, TPM_RETURNCMD, TrackPopupMenu,
        TranslateMessage, WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_DESTROY, WM_HOTKEY, WM_NULL, WNDCLASSW, WS_OVERLAPPED,
    };

    const CLASS_NAME: &str = "FileCraftBackgroundWindow";
    const HOTKEY_ID: i32 = 1;
    const TRAY_ID: u32 = 1;
    const WM_TRAY: u32 = WM_APP + 1;
    const WM_SET_HOTKEY: u32 = WM_APP + 2;
    const WM_SHOW_MAIN: u32 = WM_APP + 3;
    /// As in `shellapi.h`; `windows-sys` has only its two parts.
    const NIN_KEYSELECT: u32 = NIN_SELECT | NINF_KEY;
    /// `Background::drop`'s request to end the thread. It is not WM_CLOSE, which anyone can send:
    /// `taskkill` sends that to every top-level window, and it means "exit the program".
    const WM_SHUTDOWN: u32 = WM_APP + 4;
    /// How long dropping `Background` waits for the thread to remove the tray icon. The main thread
    /// handles no messages meanwhile, and a thread blocked on one of its windows would never finish.
    const SHUTDOWN_WAIT: Duration = Duration::from_secs(1);
    const MENU_OPEN: usize = 1;
    const MENU_SEARCH: usize = 2;
    const MENU_EXIT: usize = 3;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    /// The state of the background window, kept by the thread that owns it.
    struct State {
        ctx: egui::Context,
        events: Sender<Event>,
        next_hotkey: Arc<Mutex<Option<Hotkey>>>,
        signals: Arc<BarSignals>,
        taskbar_created: u32,
        registered: Cell<bool>,
    }

    thread_local! {
        static STATE: RefCell<Option<Rc<State>>> = const { RefCell::new(None) };
    }

    /// Runs `f` with the state, holding no borrow of it: Win32 calls made by `f` can re-enter the
    /// window procedure (showing a window sends this thread's windows activation messages).
    fn with_state(f: impl FnOnce(&State)) {
        let state = STATE.with_borrow(Option::clone);
        if let Some(state) = state {
            f(&state);
        }
    }

    impl State {
        fn send(&self, event: Event) {
            let _ = self.events.send(event);
            self.ctx.request_repaint();
        }

        fn register(&self, hwnd: HWND, hotkey: Hotkey) {
            // SAFETY: `hwnd` is the window this thread created; a hotkey that is not registered
            // only makes the unregister call fail, which is harmless.
            let registered = unsafe {
                if self.registered.get() {
                    UnregisterHotKey(hwnd, HOTKEY_ID);
                }
                RegisterHotKey(hwnd, HOTKEY_ID, hotkey.modifiers as HOT_KEY_MODIFIERS | MOD_NOREPEAT, hotkey.key) != 0
            };
            self.registered.set(registered);
            let result = if registered {
                Ok(())
            } else {
                Err(format!("{hotkey} is not available: {}", io::Error::last_os_error()))
            };
            self.send(Event::HotkeyRegistered(result));
        }

        fn toggle_bar(&self) {
            let Some(bar) = find_bar() else { return };
            // The main thread owns the bar, so everything here is asynchronous: a synchronous call
            // would wait for that thread to handle its messages, stalling this one while the UI is
            // busy. The requests are queued in order, so the window is placed before it shows. The
            // bar takes the foreground itself once it is on screen (`focus_bar`): here the show
            // has only been queued.
            if self.signals.toggle() {
                // SAFETY: plain Win32 calls on a window of this process; a window that has just
                // been destroyed only makes them fail.
                unsafe {
                    place_bar(bar);
                    ShowWindowAsync(bar, SW_SHOWNORMAL);
                }
            }
            self.ctx.request_repaint_of(quick_bar::viewport_id());
        }
    }

    /// The bar's window, found by title and checked to be ours: `eframe` does not hand out the
    /// handles of its secondary windows.
    fn find_bar() -> Option<HWND> {
        let title = wide(quick_bar::TITLE);
        let mut after: HWND = std::ptr::null_mut();
        loop {
            // SAFETY: `title` is NUL-terminated and outlives the call; `after` is null or a window
            // this loop was just given.
            let hwnd = unsafe { FindWindowExW(std::ptr::null_mut(), after, std::ptr::null(), title.as_ptr()) };
            if hwnd.is_null() {
                return None;
            }
            let mut pid = 0;
            // SAFETY: `pid` is a valid place to write the process id to.
            let same_process = unsafe {
                GetWindowThreadProcessId(hwnd, &mut pid);
                pid == GetCurrentProcessId()
            };
            if same_process {
                return Some(hwnd);
            }
            after = hwnd;
        }
    }

    /// Hides the bar, if it exists. The bar calls this while holding its own lock, so the window
    /// is hidden from the message loop rather than now: a synchronous call that made the event
    /// loop run the bar's UI again would wait for that lock forever. It is still the bar's thread
    /// that hides it, which is what returns the foreground to the previous window.
    pub fn hide_bar() {
        if let Some(bar) = find_bar() {
            // SAFETY: plain Win32 call on a window of this process.
            unsafe {
                ShowWindowAsync(bar, SW_HIDE);
            }
        }
    }

    /// Shows the bar again, after the bar's own thread has queued hiding it in the same frame:
    /// queued behind that, this undoes it where the hotkey thread's earlier show would not.
    pub fn show_bar() {
        if let Some(bar) = find_bar() {
            // SAFETY: plain Win32 call on a window of this process.
            unsafe {
                ShowWindowAsync(bar, SW_SHOWNORMAL);
            }
        }
    }

    /// Gives the bar the foreground, once it is on screen; `false` while it is not yet. Called on
    /// the bar's own thread, which owns the window, so the visibility it sees is current. The
    /// hotkey still lets this process take the foreground then.
    pub fn focus_bar() -> bool {
        let Some(bar) = find_bar() else { return true };
        // SAFETY: plain Win32 calls on a window of this process.
        unsafe {
            if IsWindowVisible(bar) == 0 {
                return false;
            }
            SetForegroundWindow(bar);
        }
        true
    }

    /// Centers the bar near the top of the monitor the user is working on.
    ///
    /// # Safety
    /// `bar` must be a window handle.
    unsafe fn place_bar(bar: HWND) {
        // SAFETY: both structs are zeroable and valid to write to; `cbSize` is set as required.
        unsafe {
            let monitor = MonitorFromWindow(GetForegroundWindow(), MONITOR_DEFAULTTOPRIMARY);
            let mut info: MONITORINFO = std::mem::zeroed();
            info.cbSize = size_of::<MONITORINFO>() as u32;
            let mut rect: RECT = std::mem::zeroed();
            if GetMonitorInfoW(monitor, &mut info) == 0 || GetWindowRect(bar, &mut rect) == 0 {
                return;
            }
            let work = info.rcWork;
            let x = work.left + ((work.right - work.left) - (rect.right - rect.left)) / 2;
            let y = work.top + (work.bottom - work.top) / 5;
            let flags = SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS;
            SetWindowPos(bar, std::ptr::null_mut(), x, y, 0, 0, flags);
        }
    }

    /// A filled-in `NOTIFYICONDATAW` for our icon.
    fn tray_data(hwnd: HWND) -> NOTIFYICONDATAW {
        // SAFETY: the struct is plain data, for which all zeroes is valid.
        let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hwnd;
        data.uID = TRAY_ID;
        data
    }

    fn add_tray_icon(hwnd: HWND) {
        let mut data = tray_data(hwnd);
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        data.uCallbackMessage = WM_TRAY;
        data.hIcon = small_icon();
        for (slot, unit) in data.szTip.iter_mut().zip("FileCraft".encode_utf16()) {
            *slot = unit;
        }
        // SAFETY: `data` is fully initialized for the flags it sets, and the version is the union
        // field NIM_SETVERSION reads. Without a taskbar the calls fail; the icon is added again when
        // the taskbar announces itself. Version 4 is what reports the keyboard's selections.
        unsafe {
            Shell_NotifyIconW(NIM_ADD, &data);
            data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            Shell_NotifyIconW(NIM_SETVERSION, &data);
        }
    }

    /// The small app icon; resource id 1 is the one `build.rs` embeds.
    fn small_icon() -> HICON {
        // SAFETY: plain Win32 calls; a null result just leaves the tray icon blank.
        unsafe {
            let (cx, cy) = (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON));
            LoadImageW(GetModuleHandleW(std::ptr::null()), 1 as _, IMAGE_ICON, cx, cy, 0) as HICON
        }
    }

    fn tray_menu(hwnd: HWND) {
        let items = [(MENU_OPEN, "Open FileCraft"), (MENU_SEARCH, "Quick search"), (MENU_EXIT, "Exit")];
        // SAFETY: the strings outlive the calls that read them. Taking the foreground first is what
        // Windows requires for the menu to close when the user clicks elsewhere, and the posted
        // message is its documented companion. The menu is destroyed on this thread.
        let choice = unsafe {
            let menu = CreatePopupMenu();
            if menu.is_null() {
                return;
            }
            for (id, label) in items {
                AppendMenuW(menu, MF_STRING, id, wide(label).as_ptr());
            }
            let mut at = POINT { x: 0, y: 0 };
            GetCursorPos(&mut at);
            SetForegroundWindow(hwnd);
            let choice = TrackPopupMenu(menu, TPM_RETURNCMD, at.x, at.y, 0, hwnd, std::ptr::null());
            PostMessageW(hwnd, WM_NULL, 0, 0);
            DestroyMenu(menu);
            choice as usize
        };
        // After the menu's message loop has ended, since that loop runs this window's procedure.
        with_state(|state| match choice {
            MENU_OPEN => state.send(Event::ShowMain),
            MENU_SEARCH => state.toggle_bar(),
            MENU_EXIT => state.send(Event::Quit),
            _ => {}
        });
    }

    unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        let mut taskbar_created = 0;
        with_state(|state| taskbar_created = state.taskbar_created);
        match msg {
            WM_HOTKEY => with_state(|state| state.toggle_bar()),
            // With icon version 4, the low word of `lparam` is the event.
            WM_TRAY => match lparam as u32 & 0xFFFF {
                NIN_SELECT | NIN_KEYSELECT => with_state(|state| state.send(Event::ShowMain)),
                WM_CONTEXTMENU => tray_menu(hwnd),
                _ => {}
            },
            // From outside, such as `taskkill`: exit properly, rather than losing this window and
            // leaving a process nobody can reach.
            WM_CLOSE => with_state(|state| state.send(Event::Quit)),
            // SAFETY: `hwnd` is this thread's own window.
            WM_SHUTDOWN => unsafe {
                DestroyWindow(hwnd);
            },
            WM_SET_HOTKEY => with_state(|state| {
                let next = state.next_hotkey.lock().unwrap().take();
                if let Some(hotkey) = next {
                    state.register(hwnd, hotkey);
                }
            }),
            WM_SHOW_MAIN => with_state(|state| state.send(Event::ShowMain)),
            WM_DESTROY => {
                // SAFETY: `hwnd` is the window being destroyed, which still exists here, and the
                // data identifies the icon it added.
                unsafe {
                    Shell_NotifyIconW(NIM_DELETE, &tray_data(hwnd));
                    PostQuitMessage(0);
                }
            }
            _ if msg == taskbar_created && msg != 0 => add_tray_icon(hwnd),
            // SAFETY: forwards the arguments this procedure was called with.
            _ => return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
        0
    }

    /// Creates the hidden window. It is an ordinary top-level window, not a message-only one,
    /// because only top-level windows hear that the taskbar was restarted and can be found by
    /// `activate_running_instance`.
    fn create_window() -> io::Result<HWND> {
        let class = wide(CLASS_NAME);
        // SAFETY: the class name is NUL-terminated and outlives both calls; a class that is
        // already registered (the quick search was switched off and on) only fails to register.
        let hwnd = unsafe {
            let instance = GetModuleHandleW(std::ptr::null());
            let wc = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                lpszClassName: class.as_ptr(),
                ..std::mem::zeroed()
            };
            RegisterClassW(&wc);
            CreateWindowExW(
                0,
                class.as_ptr(),
                class.as_ptr(),
                WS_OVERLAPPED,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                instance,
                std::ptr::null(),
            )
        };
        if hwnd.is_null() {
            return Err(io::Error::last_os_error());
        }
        // Lets a second start reach an instance running as administrator. This one message only
        // shows the window; Windows blocks the others from programs with fewer rights.
        // SAFETY: `hwnd` is the window just created; the extra-information pointer may be null.
        unsafe {
            ChangeWindowMessageFilterEx(hwnd, WM_SHOW_MAIN, MSGFLT_ALLOW, std::ptr::null_mut());
        }
        Ok(hwnd)
    }

    /// The background window and its thread. Dropping it removes the hotkey and the tray icon.
    pub struct Background {
        /// The window handle as a number: it is only passed to `PostMessageW`, which any thread
        /// may call, also with a window that no longer exists.
        hwnd: isize,
        next_hotkey: Arc<Mutex<Option<Hotkey>>>,
        /// Disconnects if the thread ever ends without being asked to.
        pub events: Receiver<Event>,
        stopped: std::sync::mpsc::Receiver<()>,
    }

    impl Background {
        pub fn start(hotkey: Hotkey, ctx: egui::Context, signals: Arc<BarSignals>) -> io::Result<Background> {
            let (events_tx, events) = unbounded();
            let next_hotkey = Arc::new(Mutex::new(None));
            let next_hotkey_thread = Arc::clone(&next_hotkey);
            let (started_tx, started) = std::sync::mpsc::channel();
            let (stopped_tx, stopped) = std::sync::mpsc::channel();
            std::thread::Builder::new().name("hotkey and tray".into()).spawn(move || {
                let hwnd = match create_window() {
                    Ok(hwnd) => hwnd,
                    Err(e) => {
                        let _ = started_tx.send(Err(e));
                        return;
                    }
                };
                // SAFETY: the name is NUL-terminated and outlives the call.
                let taskbar_created = unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) };
                // The app only notices that the thread has ended when it next runs its logic, which
                // a hidden window does not do unless asked to.
                let wake = ctx.clone();
                let state = State {
                    ctx,
                    events: events_tx,
                    next_hotkey: next_hotkey_thread,
                    signals,
                    taskbar_created,
                    registered: Cell::new(false),
                };
                state.register(hwnd, hotkey);
                STATE.set(Some(Rc::new(state)));
                // Before the icon: adding it waits for Explorer, which can take seconds while the
                // taskbar restarts, and the caller is the UI thread. Nothing here needs the icon.
                let _ = started_tx.send(Ok(hwnd as isize));
                add_tray_icon(hwnd);

                // SAFETY: the standard message loop; `msg` is zeroable and written by `GetMessageW`
                // before it is read. The loop ends with the window (`PostQuitMessage`), or when the
                // call fails.
                unsafe {
                    let mut msg: MSG = std::mem::zeroed();
                    while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                        TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
                STATE.set(None);
                let _ = stopped_tx.send(());
                wake.request_repaint();
            })?;
            let hwnd = started.recv().map_err(|_| io::Error::other("the hotkey thread stopped"))??;
            Ok(Background { hwnd, next_hotkey, events, stopped })
        }

        /// Replaces the hotkey. The outcome arrives as `Event::HotkeyRegistered`.
        pub fn set_hotkey(&self, hotkey: Hotkey) {
            *self.next_hotkey.lock().unwrap() = Some(hotkey);
            // SAFETY: posting to a window that no longer exists fails harmlessly.
            unsafe {
                PostMessageW(self.hwnd as HWND, WM_SET_HOTKEY, 0, 0);
            }
        }
    }

    impl Drop for Background {
        fn drop(&mut self) {
            // SAFETY: posting to a window that no longer exists fails harmlessly.
            unsafe {
                PostMessageW(self.hwnd as HWND, WM_SHUTDOWN, 0, 0);
            }
            let _ = self.stopped.recv_timeout(SHUTDOWN_WAIT);
        }
    }

    /// If FileCraft is already running in the background, asks it to show its window and returns
    /// `true`. Otherwise a second start would quietly load every index a second time.
    pub fn activate_running_instance() -> bool {
        let class = wide(CLASS_NAME);
        // SAFETY: `class` is NUL-terminated and outlives the call; the other calls take the window
        // just found, and fail harmlessly if it has gone in the meantime.
        unsafe {
            let hwnd = FindWindowW(class.as_ptr(), std::ptr::null());
            if hwnd.is_null() {
                return false;
            }
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd, &mut pid);
            // We were just started by the user, so we may pass the foreground on.
            AllowSetForegroundWindow(pid);
            PostMessageW(hwnd, WM_SHOW_MAIN, 0, 0) != 0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_formats_combinations() {
        for (text, shown) in [
            ("Alt+Space", "Alt+Space"),
            ("ctrl+shift+k", "Ctrl+Shift+K"),
            (" Win + F3 ", "Win+F3"),
            ("shift+control+7", "Ctrl+Shift+7"),
            ("F13", "F13"),
            ("Alt+`", "Alt+`"),
            ("Ctrl+pageup", "Ctrl+PageUp"),
        ] {
            assert_eq!(Hotkey::parse(text).unwrap().to_string(), shown);
        }
    }

    #[test]
    fn maps_to_virtual_key_codes() {
        assert_eq!(Hotkey::parse("Alt+Space").unwrap(), Hotkey { modifiers: MOD_ALT, key: 0x20 });
        assert_eq!(Hotkey::parse("Ctrl+a").unwrap(), Hotkey { modifiers: MOD_CONTROL, key: 0x41 });
        assert_eq!(Hotkey::parse("Win+F24").unwrap(), Hotkey { modifiers: MOD_WIN, key: 0x87 });
    }

    #[test]
    fn rejects_what_cannot_be_a_hotkey() {
        for text in ["", "Alt", "Ctrl+Alt", "Space", "k", "Alt+Nope", "Alt+K+L", "Alt+F25", "Alt+F0"] {
            assert!(Hotkey::parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn the_default_is_valid() {
        assert_eq!(Hotkey::parse(DEFAULT_HOTKEY).unwrap().to_string(), DEFAULT_HOTKEY);
    }

    /// A second press before the bar has even shown closes it, whatever the window's visibility.
    #[test]
    fn a_quick_second_press_closes_the_bar_again() {
        let signals = BarSignals::default();
        assert!(signals.toggle());
        assert!(!signals.toggle());
        assert_eq!(signals.take(), BarRequest { close: true, open: false });
    }

    #[test]
    fn closing_and_reopening_before_the_bar_looks_reopens_it() {
        let signals = BarSignals::default();
        signals.toggle();
        signals.take();
        assert!(!signals.toggle());
        assert!(signals.toggle());
        assert_eq!(signals.take(), BarRequest { close: true, open: true });
    }

    #[test]
    fn after_closing_itself_the_bar_opens_on_the_next_press() {
        let signals = BarSignals::default();
        signals.toggle();
        signals.take();
        signals.closed();
        assert!(signals.toggle());
        assert_eq!(signals.take(), BarRequest { close: false, open: true });
    }
}
