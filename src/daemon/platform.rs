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
mod linux;
#[cfg(target_os = "linux")]
use self::linux as imp;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use self::windows as imp;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use self::macos as imp;
