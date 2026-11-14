//! The sync thread: every few seconds it uploads local changes, downloads other devices' changes
//! and merges them. It holds the history lock only while merging, never during network calls.

use super::State;
use crate::crypto::Cipher;
use crate::store::media::Media;
use crate::sync::settings::Settings;
use crate::sync::{Op, Syncer};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const INTERVAL: Duration = Duration::from_secs(15);

pub enum Msg {
    Op(Op),
    /// Sync right now (`clipkeep sync now`).
    Now,
}

#[derive(Default)]
pub struct Status {
    pub backend: String,
    pub last_ok: Option<Instant>,
    pub error: Option<String>,
    pub received: usize,
}

impl Status {
    pub fn describe(&self) -> String {
        let when = match self.last_ok {
            Some(t) => format!("last sync {}s ago", t.elapsed().as_secs()),
            None => "not synced yet".into(),
        };
        match &self.error {
            Some(e) => format!("{}: {when}, ERROR: {e}", self.backend),
            None => format!(
                "{}: {when}, {} changes received",
                self.backend, self.received
            ),
        }
    }
}

pub struct Handle {
    pub tx: Sender<Msg>,
    pub status: Arc<Mutex<Status>>,
}

/// Starts the thread. Returns `None` (after logging why) if sync cannot start.
pub fn start(
    state: Arc<Mutex<State>>,
    settings: &Settings,
    local: Cipher,
    media: Media,
    state_path: std::path::PathBuf,
) -> Option<Handle> {
    let setup = (|| {
        let backend = settings.backend()?;
        Syncer::new(
            backend,
            settings.master_key()?,
            Some(local),
            media,
            state_path,
            &settings.device,
        )
    })();
    let syncer = match setup {
        Ok(s) => s,
        Err(e) => {
            eprintln!("clipkeep: sync is off: {e}");
            return None;
        }
    };
    let status = Arc::new(Mutex::new(Status {
        backend: syncer.backend_name(),
        ..Status::default()
    }));
    let (tx, rx) = mpsc::channel();
    let thread_status = Arc::clone(&status);
    std::thread::spawn(move || run(syncer, state, rx, thread_status));
    Some(Handle { tx, status })
}

fn run(
    mut syncer: Syncer,
    state: Arc<Mutex<State>>,
    rx: Receiver<Msg>,
    status: Arc<Mutex<Status>>,
) {
    let mut outbox: Vec<Op> = Vec::new();
    let mut last = Instant::now() - INTERVAL;
    loop {
        let mut force = false;
        let mut handle = |msg: Msg, syncer: &mut Syncer, outbox: &mut Vec<Op>| match msg {
            Msg::Op(op) => {
                syncer.note(&op);
                outbox.push(op);
            }
            Msg::Now => force = true,
        };
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(msg) => handle(msg, &mut syncer, &mut outbox),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        while let Ok(msg) = rx.try_recv() {
            handle(msg, &mut syncer, &mut outbox);
        }
        if !force && last.elapsed() < INTERVAL {
            continue;
        }
        last = Instant::now();
        let result = cycle(&mut syncer, &state, &mut outbox);
        let mut st = status.lock().unwrap();
        match result {
            Ok(n) => {
                st.last_ok = Some(Instant::now());
                st.error = None;
                st.received += n;
            }
            Err(e) => st.error = Some(e.to_string()),
        }
    }
}

fn cycle(
    syncer: &mut Syncer,
    state: &Arc<Mutex<State>>,
    outbox: &mut Vec<Op>,
) -> std::io::Result<usize> {
    if syncer.needs_snapshot() {
        let ops = Syncer::snapshot(&mut state.lock().unwrap().store);
        syncer.push(&ops)?;
        syncer.mark_snapshot_done()?;
    }
    if !outbox.is_empty() {
        syncer.push(outbox)?;
        outbox.clear();
    }
    let mut received = 0;
    for batch in syncer.fetch()?.batches() {
        let mut s = state.lock().unwrap();
        received += syncer.apply(batch, &mut s.store)?;
        s.touched();
    }
    Ok(received)
}
