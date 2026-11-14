//! macOS: NSPasteboard.changeCount polling + Carbon RegisterEventHotKey.

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
        0, 11, 8, 2, 14, 3, 5, 4, 34, 38, 40, 37, 46, 45, 31, 35, 12, 15, 1, 17, 32, 9, 13, 7, 16,
        6,
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
        let send_str: unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_char) -> *mut c_void =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
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
            None => Event::HotkeyFailed(format!("{} could not be registered", names.join(", "))),
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
