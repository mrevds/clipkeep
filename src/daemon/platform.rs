//! OS glue for the daemon: "the clipboard changed" and "the hotkey was pressed" events.
//! `run` blocks the calling (main) thread; macOS needs its event loop on the main thread.
//!
//! - Linux: XFixes selection events (also works on GNOME/KDE Wayland through XWayland).
//!   The hotkey is a desktop shortcut that runs `clipkeep show` (see `clipkeep setup`).
//! - Windows: a message-only window with `AddClipboardFormatListener` + `RegisterHotKey`.
//! - macOS: `NSPasteboard.changeCount` polled every 250 ms + a Carbon `RegisterEventHotKey`.

use crate::config::Hotkey;
use std::sync::mpsc::Sender;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Clipboard,
    Hotkey,
    HotkeyReady(String),
    HotkeyFailed(String),
}

pub use imp::{concealed, run};

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    use x11rb::connection::Connection;
    use x11rb::protocol::xfixes::{ConnectionExt as _, SelectionEventMask};
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, CreateWindowAux, WindowClass};
    use x11rb::protocol::Event as XEvent;
    use x11rb::rust_connection::RustConnection;

    /// A private X connection + invisible window to ask the clipboard owner for its TARGETS.
    struct Probe {
        conn: RustConnection,
        win: u32,
        clipboard: u32,
        targets: u32,
        prop: u32,
        hint: u32,
    }

    impl Probe {
        fn new() -> Result<Probe, Box<dyn std::error::Error>> {
            let (conn, screen) = x11rb::connect(None)?;
            let root = conn.setup().roots[screen].root;
            let win = conn.generate_id()?;
            conn.create_window(
                0,
                win,
                root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                0,
                &CreateWindowAux::new(),
            )?;
            let atom = |name: &[u8]| -> Result<u32, Box<dyn std::error::Error>> {
                Ok(conn.intern_atom(false, name)?.reply()?.atom)
            };
            let probe = Probe {
                clipboard: atom(b"CLIPBOARD")?,
                targets: atom(b"TARGETS")?,
                prop: atom(b"CLIPKEEP_TARGETS")?,
                // KeePassXC, KeePass and others offer this type with the value "secret".
                hint: atom(b"x-kde-passwordManagerHint")?,
                win,
                conn,
            };
            probe.conn.flush()?;
            Ok(probe)
        }

        fn concealed(&self) -> Result<bool, Box<dyn std::error::Error>> {
            let c = &self.conn;
            c.convert_selection(self.win, self.clipboard, self.targets, self.prop, 0u32)?;
            c.flush()?;
            let deadline = Instant::now() + Duration::from_millis(300);
            while Instant::now() < deadline {
                match c.poll_for_event()? {
                    Some(XEvent::SelectionNotify(e)) if e.requestor == self.win => {
                        if e.property == 0 {
                            return Ok(false); // owner refused: no targets
                        }
                        let reply = c
                            .get_property(true, self.win, self.prop, AtomEnum::ATOM, 0, 4096)?
                            .reply()?;
                        return Ok(reply
                            .value32()
                            .is_some_and(|mut atoms| atoms.any(|a| a == self.hint)));
                    }
                    Some(_) => {}
                    None => std::thread::sleep(Duration::from_millis(3)),
                }
            }
            Ok(false)
        }
    }

    /// True when the app that owns the clipboard marked it as a password.
    pub fn concealed() -> bool {
        static PROBE: Mutex<Option<Probe>> = Mutex::new(None);
        let mut probe = PROBE.lock().unwrap_or_else(|e| e.into_inner());
        if probe.is_none() {
            *probe = Probe::new().ok();
        }
        match probe.as_ref().map(Probe::concealed) {
            Some(Ok(v)) => v,
            Some(Err(_)) => {
                *probe = None; // reconnect next time
                false
            }
            None => false,
        }
    }

    pub fn run(tx: Sender<Event>, _hotkeys: &[Hotkey]) -> Result<(), String> {
        let go = || -> Result<(), Box<dyn std::error::Error>> {
            let (conn, screen) = x11rb::connect(None)?;
            let root = conn.setup().roots[screen].root;
            conn.xfixes_query_version(5, 0)?.reply()?;
            let clipboard = conn.intern_atom(false, b"CLIPBOARD")?.reply()?.atom;
            conn.xfixes_select_selection_input(
                root,
                clipboard,
                SelectionEventMask::SET_SELECTION_OWNER
                    | SelectionEventMask::SELECTION_WINDOW_DESTROY
                    | SelectionEventMask::SELECTION_CLIENT_CLOSE,
            )?;
            conn.flush()?;
            loop {
                if let x11rb::protocol::Event::XfixesSelectionNotify(_) = conn.wait_for_event()? {
                    if tx.send(Event::Clipboard).is_err() {
                        return Ok(());
                    }
                }
            }
        };
        go().map_err(|e| format!("X11 clipboard watcher failed ({e}); is DISPLAY set?"))
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::DataExchange::AddClipboardFormatListener;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        RegisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW,
        TranslateMessage, HWND_MESSAGE, MSG, WM_CLIPBOARDUPDATE, WM_HOTKEY, WNDCLASSW,
    };

    static TX: OnceLock<Sender<Event>> = OnceLock::new();

    /// True when the app that copied marked the data as private. Password managers add one of
    /// these formats (KeePass, Bitwarden, 1Password, Windows' own docs for clipboard history).
    pub fn concealed() -> bool {
        use windows_sys::Win32::System::DataExchange::{
            IsClipboardFormatAvailable, RegisterClipboardFormatW,
        };
        [
            "ExcludeClipboardContentFromMonitorProcessing",
            "Clipboard Viewer Ignore",
            // Apps set this to 0 to keep a copy out of clipboard history; nobody sets it to 1.
            "CanIncludeInClipboardHistory",
        ]
        .iter()
        .any(|name| {
            let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
            // SAFETY: valid NUL-terminated UTF-16 string; both calls only read.
            unsafe {
                let format = RegisterClipboardFormatW(wide.as_ptr());
                format != 0 && IsClipboardFormatAvailable(format) != 0
            }
        })
    }

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        if msg == WM_CLIPBOARDUPDATE {
            if let Some(tx) = TX.get() {
                let _ = tx.send(Event::Clipboard);
            }
            return 0;
        }
        DefWindowProcW(hwnd, msg, w, l)
    }

    fn mods(hk: &Hotkey) -> HOT_KEY_MODIFIERS {
        let mut m = MOD_NOREPEAT;
        if hk.ctrl {
            m |= MOD_CONTROL;
        }
        if hk.alt {
            m |= MOD_ALT;
        }
        if hk.shift {
            m |= MOD_SHIFT;
        }
        if hk.meta {
            m |= MOD_WIN;
        }
        m
    }

    pub fn run(tx: Sender<Event>, hotkeys: &[Hotkey]) -> Result<(), String> {
        let _ = TX.set(tx.clone());
        // SAFETY: plain Win32 calls; every pointer passed lives until the call returns
        // (the class name lives for the whole function, which never returns while the window exists).
        unsafe {
            let hinst = GetModuleHandleW(std::ptr::null());
            let class: Vec<u16> = "clipkeep-daemon\0".encode_utf16().collect();
            let mut wc: WNDCLASSW = std::mem::zeroed();
            wc.lpfnWndProc = Some(wndproc);
            wc.hInstance = hinst;
            wc.lpszClassName = class.as_ptr();
            if RegisterClassW(&wc) == 0 {
                return Err("RegisterClassW failed".into());
            }
            let hwnd = CreateWindowExW(
                0,
                class.as_ptr(),
                class.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                std::ptr::null_mut(),
                hinst,
                std::ptr::null(),
            );
            if hwnd.is_null() {
                return Err("CreateWindowExW failed".into());
            }
            if AddClipboardFormatListener(hwnd) == 0 {
                return Err("AddClipboardFormatListener failed".into());
            }
            let mut ready = None;
            for hk in hotkeys {
                if RegisterHotKey(hwnd, 1, mods(hk), hk.key as u32) != 0 {
                    ready = Some(hk.to_string());
                    break;
                }
            }
            let names: Vec<String> = hotkeys.iter().map(ToString::to_string).collect();
            let _ = tx.send(match ready {
                Some(name) => Event::HotkeyReady(name),
                None => Event::HotkeyFailed(format!(
                    "{} already taken by another app",
                    names.join(", ")
                )),
            });
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                if msg.message == WM_HOTKEY {
                    let _ = tx.send(Event::Hotkey);
                }
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use std::ffi::{c_char, c_void};
    use std::time::Duration;

    #[repr(C)]
    struct EventTypeSpec {
        class: u32,
        kind: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct EventHotKeyId {
        signature: u32,
        id: u32,
    }

    type Handler = extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> i32;

    #[link(name = "Carbon", kind = "framework")]
    extern "C" {
        fn GetApplicationEventTarget() -> *mut c_void;
        fn InstallEventHandler(
            target: *mut c_void,
            handler: Handler,
            count: u32,
            list: *const EventTypeSpec,
            user_data: *mut c_void,
            out_ref: *mut *mut c_void,
        ) -> i32;
        fn RegisterEventHotKey(
            key_code: u32,
            modifiers: u32,
            id: EventHotKeyId,
            target: *mut c_void,
            options: u32,
            out_ref: *mut *mut c_void,
        ) -> i32;
        fn RunApplicationEventLoop();
    }

    #[link(name = "AppKit", kind = "framework")]
    extern "C" {}

    #[link(name = "objc")]
    extern "C" {
        fn objc_getClass(name: *const c_char) -> *mut c_void;
        fn sel_registerName(name: *const c_char) -> *mut c_void;
        fn objc_msgSend();
    }

    const K_EVENT_CLASS_KEYBOARD: u32 = u32::from_be_bytes(*b"keyb");
    const K_EVENT_HOT_KEY_PRESSED: u32 = 5;
    const CMD: u32 = 1 << 8;
    const SHIFT: u32 = 1 << 9;
    const OPTION: u32 = 1 << 11;
    const CONTROL: u32 = 1 << 12;

    /// ANSI virtual key codes (kVK_ANSI_*).
    fn key_code(c: char) -> Option<u32> {
        const LETTERS: [u32; 26] = [
            0, 11, 8, 2, 14, 3, 5, 4, 34, 38, 40, 37, 46, 45, 31, 35, 12, 15, 1, 17, 32, 9, 13, 7,
            16, 6,
        ];
        const DIGITS: [u32; 10] = [29, 18, 19, 20, 21, 23, 22, 26, 28, 25];
        match c {
            'A'..='Z' => Some(LETTERS[(c as u8 - b'A') as usize]),
            '0'..='9' => Some(DIGITS[(c as u8 - b'0') as usize]),
            _ => None,
        }
    }

    fn change_count() -> isize {
        // SAFETY: objc_msgSend called through correctly typed pointers for
        // `+[NSPasteboard generalPasteboard]` (returns id) and `-changeCount` (returns NSInteger).
        unsafe {
            let send_id: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            let send_int: unsafe extern "C" fn(*mut c_void, *mut c_void) -> isize =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            let class = objc_getClass(c"NSPasteboard".as_ptr());
            if class.is_null() {
                return -1;
            }
            let pb = send_id(class, sel_registerName(c"generalPasteboard".as_ptr()));
            if pb.is_null() {
                return -1;
            }
            send_int(pb, sel_registerName(c"changeCount".as_ptr()))
        }
    }

    /// True when the app that copied marked the data as a password
    /// (`org.nspasteboard.ConcealedType`) or as not worth keeping (`TransientType`).
    pub fn concealed() -> bool {
        // SAFETY: objc_msgSend called through correctly typed pointers:
        // `+generalPasteboard` and `-types` return id, `+stringWithUTF8String:` takes a C string,
        // `-containsObject:` takes id and returns BOOL (one byte on both architectures).
        unsafe {
            let send_id: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            let send_str: unsafe extern "C" fn(
                *mut c_void,
                *mut c_void,
                *const c_char,
            ) -> *mut c_void = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            let send_has: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> u8 =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            let pb_class = objc_getClass(c"NSPasteboard".as_ptr());
            let str_class = objc_getClass(c"NSString".as_ptr());
            if pb_class.is_null() || str_class.is_null() {
                return false;
            }
            let pb = send_id(pb_class, sel_registerName(c"generalPasteboard".as_ptr()));
            if pb.is_null() {
                return false;
            }
            let types = send_id(pb, sel_registerName(c"types".as_ptr()));
            if types.is_null() {
                return false;
            }
            [
                c"org.nspasteboard.ConcealedType",
                c"org.nspasteboard.TransientType",
            ]
            .iter()
            .any(|t| {
                let name = send_str(
                    str_class,
                    sel_registerName(c"stringWithUTF8String:".as_ptr()),
                    t.as_ptr(),
                );
                !name.is_null()
                    && send_has(types, sel_registerName(c"containsObject:".as_ptr()), name) != 0
            })
        }
    }

    extern "C" fn on_hotkey(_: *mut c_void, _: *mut c_void, user: *mut c_void) -> i32 {
        // SAFETY: `user` is the leaked Box<Sender<Event>> installed in `run`; it is never freed.
        let tx = unsafe { &*(user as *const Sender<Event>) };
        let _ = tx.send(Event::Hotkey);
        0
    }

    pub fn run(tx: Sender<Event>, hotkeys: &[Hotkey]) -> Result<(), String> {
        let poll = tx.clone();
        std::thread::spawn(move || {
            let mut last = change_count();
            loop {
                std::thread::sleep(Duration::from_millis(250));
                let now = change_count();
                if now != last {
                    last = now;
                    if poll.send(Event::Clipboard).is_err() {
                        return;
                    }
                }
            }
        });
        // SAFETY: Carbon calls with valid pointers; the handler's user data is leaked on purpose.
        unsafe {
            let target = GetApplicationEventTarget();
            let user = Box::into_raw(Box::new(tx.clone())) as *mut c_void;
            let spec = EventTypeSpec {
                class: K_EVENT_CLASS_KEYBOARD,
                kind: K_EVENT_HOT_KEY_PRESSED,
            };
            InstallEventHandler(target, on_hotkey, 1, &spec, user, std::ptr::null_mut());
            let mut ready = None;
            for hk in hotkeys {
                let Some(code) = key_code(hk.key) else {
                    continue;
                };
                let mut mods = 0;
                if hk.meta {
                    mods |= CMD;
                }
                if hk.shift {
                    mods |= SHIFT;
                }
                if hk.alt {
                    mods |= OPTION;
                }
                if hk.ctrl {
                    mods |= CONTROL;
                }
                let id = EventHotKeyId {
                    signature: u32::from_be_bytes(*b"ckp1"),
                    id: 1,
                };
                let mut out = std::ptr::null_mut();
                if RegisterEventHotKey(code, mods, id, target, 0, &mut out) == 0 {
                    ready = Some(hk.to_string());
                    break;
                }
            }
            let names: Vec<String> = hotkeys.iter().map(ToString::to_string).collect();
            let _ = tx.send(match ready {
                Some(name) => Event::HotkeyReady(name),
                None => {
                    Event::HotkeyFailed(format!("{} could not be registered", names.join(", ")))
                }
            });
            RunApplicationEventLoop();
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn key_codes() {
            assert_eq!(super::key_code('V'), Some(9));
            assert_eq!(super::key_code('A'), Some(0));
            assert_eq!(super::key_code('0'), Some(29));
        }
    }
}
