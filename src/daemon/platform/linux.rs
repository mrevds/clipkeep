//! Linux: XFixes clipboard events; password-manager hint via the TARGETS of the owner.

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
