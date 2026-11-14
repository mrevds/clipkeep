//! Linux: the GNOME shortcut starts a new `clipkeep show` each time. The first one listens on
//! a socket; a second one tells it to close and exits, so Super+V toggles the window.
use std::os::unix::net::{UnixListener, UnixStream};

pub struct Toggle(UnixListener);

pub fn claim() -> Result<Option<Toggle>, String> {
    let path = crate::paths::popup_socket_file();
    if UnixStream::connect(&path).is_ok() {
        return Ok(None);
    }
    let _ = std::fs::remove_file(&path);
    UnixListener::bind(&path)
        .map(|l| Some(Toggle(l)))
        .map_err(|e| e.to_string())
}

impl Toggle {
    pub fn listen(self, ctx: eframe::egui::Context) {
        std::thread::spawn(move || {
            if self.0.accept().is_ok() {
                ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Close);
                ctx.request_repaint();
            }
        });
    }
}

pub fn release() {
    let _ = std::fs::remove_file(crate::paths::popup_socket_file());
}
