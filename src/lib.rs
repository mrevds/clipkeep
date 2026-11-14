//! clipkeep: fast clipboard history for Linux, Windows and macOS.
//!
//! - `store`:  history on disk (text, images, search, groups); no OS or network code
//! - `crypto`: ciphers, the local key and the password vault
//! - `daemon`: background process: clipboard watcher, IPC server, hotkey, auto-paste
//! - `ui`:     the popup window
//! - `sync`:   encrypted sync between devices
//! - `secret`: is a clip a password or a key (kept in memory only, forgotten after a minute)
//! - `cli`:    commands and per-OS setup

pub mod cli;
pub mod config;
pub mod crypto;
pub mod daemon;
pub mod paths;
pub mod secret;
pub mod store;
pub mod sync;
pub mod ui;
