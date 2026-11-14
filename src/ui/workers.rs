//! Background threads of the popup: they talk to the daemon so the UI thread never blocks.

use super::{Results, RESULTS};
use crate::daemon::ipc::{self, Command};
use crate::store::View;
use eframe::egui;
use std::sync::mpsc::{Receiver, Sender};

pub(super) fn search_worker(rx: Receiver<(u64, View, String)>, out: Results, ctx: egui::Context) {
    while let Ok(mut job) = rx.recv() {
        while let Ok(newer) = rx.try_recv() {
            job = newer; // only the latest query matters
        }
        let (gen, view, query) = job;
        let result = ipc::request(&Command::Search {
            view,
            limit: RESULTS,
            query,
        })
        .map(|lines| lines.iter().filter_map(|l| ipc::parse_hit(l)).collect())
        .map_err(|e| format!("daemon: {e}"));
        *out.lock().unwrap() = Some((gen, result));
        ctx.request_repaint();
    }
}

/// Fetches thumbnails of image clips from the daemon, one at a time, newest request first.
pub(super) fn thumb_worker(
    rx: Receiver<u32>,
    out: Sender<(u32, Option<egui::ColorImage>)>,
    ctx: egui::Context,
) {
    while let Ok(id) = rx.recv() {
        let image = ipc::request(&Command::Thumb(id)).ok().and_then(|lines| {
            let b64 = lines.first()?.strip_prefix("T\t")?;
            let (w, h, rgba) = crate::store::media::decode_png(&ipc::base64_decode(b64)?).ok()?;
            Some(egui::ColorImage::from_rgba_unmultiplied(
                [w as usize, h as usize],
                &rgba,
            ))
        });
        if out.send((id, image)).is_err() {
            return;
        }
        ctx.request_repaint();
    }
}
