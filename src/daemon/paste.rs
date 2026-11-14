//! Auto-paste: after a clip is chosen, press Ctrl+V in the app that gets focus back.
//!
//! - Linux: a virtual keyboard on `/dev/uinput` (works on Wayland and X11; the logged-in user
//!   normally has access through the `uaccess` ACL). Created once by the daemon, because the
//!   compositor needs a moment to pick up a new input device.
//! - Windows: `SendInput`.
//! - macOS: not yet (needs the Accessibility permission).

use std::io;

#[cfg(target_os = "linux")]
pub struct Paster(std::fs::File);

#[cfg(target_os = "linux")]
mod uinput {
    use std::ffi::{c_int, c_ulong};

    extern "C" {
        pub fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    }

    // _IOW('U', n, size) / _IO('U', n) from <linux/uinput.h>.
    pub const UI_DEV_CREATE: c_ulong = 0x5501;
    pub const UI_DEV_SETUP: c_ulong = 0x405C_5503;
    pub const UI_SET_EVBIT: c_ulong = 0x4004_5564;
    pub const UI_SET_KEYBIT: c_ulong = 0x4004_5565;

    pub const EV_SYN: u16 = 0;
    pub const EV_KEY: u16 = 1;
    pub const KEY_LEFTCTRL: u16 = 29;
    pub const KEY_V: u16 = 47;

    #[repr(C)]
    pub struct InputId {
        pub bustype: u16,
        pub vendor: u16,
        pub product: u16,
        pub version: u16,
    }

    #[repr(C)]
    pub struct UinputSetup {
        pub id: InputId,
        pub name: [u8; 80],
        pub ff_effects_max: u32,
    }

    /// `struct input_event` on 64-bit Linux: timeval (2 × i64) + type + code + value.
    pub fn event(kind: u16, code: u16, value: i32) -> [u8; 24] {
        let mut e = [0u8; 24];
        e[16..18].copy_from_slice(&kind.to_ne_bytes());
        e[18..20].copy_from_slice(&code.to_ne_bytes());
        e[20..24].copy_from_slice(&value.to_ne_bytes());
        e
    }
}

#[cfg(target_os = "linux")]
impl Paster {
    pub fn new() -> io::Result<Paster> {
        use std::os::fd::AsRawFd;
        use uinput::*;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/uinput")?;
        let fd = file.as_raw_fd();
        let mut setup = UinputSetup {
            id: InputId {
                bustype: 0x06, /* BUS_VIRTUAL */
                vendor: 0x636b,
                product: 0x0001,
                version: 1,
            },
            name: [0; 80],
            ff_effects_max: 0,
        };
        setup.name[..17].copy_from_slice(b"clipkeep-keyboard");
        let check = |r: std::ffi::c_int| {
            if r < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        };
        // SAFETY: ioctls on our own uinput fd with the argument types the kernel expects.
        unsafe {
            check(ioctl(fd, UI_SET_EVBIT, EV_KEY as std::ffi::c_int))?;
            check(ioctl(fd, UI_SET_EVBIT, EV_SYN as std::ffi::c_int))?;
            check(ioctl(fd, UI_SET_KEYBIT, KEY_LEFTCTRL as std::ffi::c_int))?;
            check(ioctl(fd, UI_SET_KEYBIT, KEY_V as std::ffi::c_int))?;
            check(ioctl(fd, UI_DEV_SETUP, &setup as *const UinputSetup))?;
            check(ioctl(fd, UI_DEV_CREATE))?;
        }
        Ok(Paster(file))
    }

    pub fn ctrl_v(&mut self) -> io::Result<()> {
        use std::io::Write;
        use uinput::*;
        let step = |f: &mut std::fs::File, code: u16, down: bool| -> io::Result<()> {
            f.write_all(&event(EV_KEY, code, down as i32))?;
            f.write_all(&event(EV_SYN, 0, 0))?;
            std::thread::sleep(std::time::Duration::from_millis(8));
            Ok(())
        };
        step(&mut self.0, KEY_LEFTCTRL, true)?;
        step(&mut self.0, KEY_V, true)?;
        step(&mut self.0, KEY_V, false)?;
        step(&mut self.0, KEY_LEFTCTRL, false)
    }
}

#[cfg(windows)]
pub struct Paster;

#[cfg(windows)]
impl Paster {
    pub fn new() -> io::Result<Paster> {
        Ok(Paster)
    }

    pub fn ctrl_v(&mut self) -> io::Result<()> {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
            SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_CONTROL,
        };
        let key = |vk: u16, up: bool| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: if up { KEYEVENTF_KEYUP } else { 0 },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let inputs = [
            key(VK_CONTROL, false),
            key(b'V' as u16, false),
            key(b'V' as u16, true),
            key(VK_CONTROL, true),
        ];
        // SAFETY: a valid array of INPUT structs and its exact size.
        let sent = unsafe {
            SendInput(
                inputs.len() as u32,
                inputs.as_ptr(),
                std::mem::size_of::<INPUT>() as i32,
            )
        };
        if sent as usize == inputs.len() {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
pub struct Paster;

#[cfg(not(any(target_os = "linux", windows)))]
impl Paster {
    pub fn new() -> io::Result<Paster> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "auto-paste is not available on this OS yet",
        ))
    }

    pub fn ctrl_v(&mut self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "auto-paste is not available on this OS yet",
        ))
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    #[test]
    fn input_event_layout() {
        let e = super::uinput::event(1, 47, 1);
        assert_eq!(&e[..16], &[0; 16]);
        assert_eq!(u16::from_ne_bytes([e[16], e[17]]), 1);
        assert_eq!(u16::from_ne_bytes([e[18], e[19]]), 47);
        assert_eq!(i32::from_ne_bytes([e[20], e[21], e[22], e[23]]), 1);
        assert_eq!(std::mem::size_of::<super::uinput::UinputSetup>(), 92);
    }
}
