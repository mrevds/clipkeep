//! Windows: message-only window with AddClipboardFormatListener + RegisterHotKey.

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
            None => {
                Event::HotkeyFailed(format!("{} already taken by another app", names.join(", ")))
            }
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
