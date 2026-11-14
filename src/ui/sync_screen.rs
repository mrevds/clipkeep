//! The "Sync" screen of the popup: sign in with Google, choose the sync password, show the
//! recovery code, see the status. All slow work (browser sign-in, Argon2, network) runs on a
//! worker thread; the screen only reacts to messages from it.

use super::theme;
use crate::cli;
use crate::daemon::ipc::{self, Command};
use crate::sync::onboard::{self, Pending};
use crate::sync::settings::Settings;
use eframe::egui::{self, Key};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

enum State {
    /// Sync is off: explain and offer the sign-in button.
    Intro,
    /// Waiting for the user to finish in the browser.
    SigningIn,
    /// Signed in. `pending` knows whether a vault already exists (join) or not (create).
    Password {
        exists: bool,
        pending: Option<Arc<Pending>>,
    },
    /// Sync is on; this is the one time the recovery code is shown.
    Recovery(String),
    /// Sync is on.
    Connected,
    Failed(String),
}

enum Msg {
    SignedIn(Result<Pending, String>),
    Created(Result<String, String>),
    Joined(Result<(), String>),
    Disabled(Result<(), String>),
}

pub(super) struct SyncScreen {
    state: State,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    ctx: egui::Context,
    password: String,
    repeat: String,
    use_recovery: bool,
    show_password: bool,
    busy: bool,
    wrote_it_down: bool,
    confirm_off: bool,
    focus: bool,
    status: String,
    status_at: Option<Instant>,
}

impl SyncScreen {
    pub(super) fn open(ctx: egui::Context) -> SyncScreen {
        let (tx, rx) = mpsc::channel();
        let on = matches!(Settings::load(), Ok(Some(_)));
        let mut s = SyncScreen {
            state: if on { State::Connected } else { State::Intro },
            tx,
            rx,
            ctx,
            password: String::new(),
            repeat: String::new(),
            use_recovery: false,
            show_password: false,
            busy: false,
            wrote_it_down: false,
            confirm_off: false,
            focus: false,
            status: String::new(),
            status_at: None,
        };
        s.debug_state();
        s
    }

    /// Debug: `CLIPKEEP_SYNC_STATE=create|join|recovery|failed` jumps to a screen (for screenshots).
    fn debug_state(&mut self) {
        match std::env::var("CLIPKEEP_SYNC_STATE").as_deref() {
            Ok("create") => {
                self.state = State::Password {
                    exists: false,
                    pending: None,
                }
            }
            Ok("join") => {
                self.state = State::Password {
                    exists: true,
                    pending: None,
                }
            }
            Ok("recovery") => {
                self.state = State::Recovery(
                    "ABCD-EFGH-IJKL-MNOP-QRST-UVWX-YZ23-4567-ABCD-EFGH-IJKL-MNOP-QRST".into(),
                )
            }
            Ok("failed") => {
                self.state = State::Failed("cannot reach the storage: timed out".into())
            }
            _ => {}
        }
    }

    fn spawn(&mut self, job: impl FnOnce() -> Msg + Send + 'static) {
        self.busy = true;
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(job());
            ctx.request_repaint();
        });
    }

    fn fail(&mut self, e: String) {
        self.busy = false;
        self.state = State::Failed(e);
    }

    fn take_messages(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            self.busy = false;
            match msg {
                Msg::SignedIn(Ok(p)) => {
                    self.focus = true;
                    self.state = State::Password {
                        exists: p.exists(),
                        pending: Some(Arc::new(p)),
                    };
                    self.ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                Msg::SignedIn(Err(e)) | Msg::Created(Err(e)) | Msg::Disabled(Err(e)) => {
                    self.fail(e)
                }
                Msg::Created(Ok(code)) => {
                    self.clear_secrets();
                    self.state = State::Recovery(code);
                }
                Msg::Joined(Ok(())) => {
                    self.clear_secrets();
                    self.status_at = None;
                    self.state = State::Connected;
                }
                // A wrong password is not fatal: stay on the form and say so.
                Msg::Joined(Err(e)) => {
                    self.password.clear();
                    self.focus = true;
                    self.status = e;
                }
                Msg::Disabled(Ok(())) => {
                    self.confirm_off = false;
                    self.state = State::Intro;
                }
            }
        }
    }

    fn clear_secrets(&mut self) {
        self.password.clear();
        self.repeat.clear();
        self.status.clear();
        self.use_recovery = false;
        self.show_password = false;
    }

    fn refresh_status(&mut self) {
        if self
            .status_at
            .is_some_and(|t| t.elapsed() < Duration::from_secs(2))
        {
            return;
        }
        self.status_at = Some(Instant::now());
        self.status = match ipc::request(&Command::Stats) {
            Ok(lines) => lines
                .iter()
                .find_map(|l| l.strip_prefix("S\tsync\t"))
                .unwrap_or("starting…")
                .to_owned(),
            Err(e) => format!("daemon: {e}"),
        };
    }

    /// Draws the screen. Returns true when the user wants to go back to the clip list.
    pub(super) fn show(&mut self, ui: &mut egui::Ui, logo: Option<&egui::TextureHandle>) -> bool {
        self.take_messages();
        let mut back = false;
        let esc = ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape));
        if esc && !self.busy && !matches!(self.state, State::Recovery(_)) {
            back = true;
        }
        let (title, subtitle): (&str, &str) = match &self.state {
            State::Intro => (
                "Sync between your devices",
                "Your history follows you to every computer you sign in on. It is encrypted here, before it leaves this one.",
            ),
            State::SigningIn => (
                "Sign in with Google",
                "Finish signing in in the browser window that just opened.",
            ),
            State::Password { exists: true, .. } => (
                "Unlock your history",
                "Sync is already set up. Enter the same password here.",
            ),
            State::Password { exists: false, .. } => (
                "Choose a sync password",
                "It protects your history. You type it once on each new device.",
            ),
            State::Recovery(_) => (
                "Sync is on",
                "Save your recovery code now. It will not be shown again.",
            ),
            State::Connected => ("Sync is on", ""),
            State::Failed(_) => ("Something went wrong", ""),
        };
        page(ui, logo, title, subtitle, |ui| match &self.state {
            State::Intro => back |= self.intro(ui),
            State::SigningIn => {
                ui.add_space(6.0);
                ui.vertical_centered(|ui| {
                    ui.spinner();
                });
                ui.add_space(6.0);
                ui.vertical_centered(|ui| {
                    ui.label(theme::muted(
                        "No window? Open the link printed in the terminal.",
                    ));
                });
            }
            State::Password { .. } => back |= self.password_form(ui),
            State::Recovery(_) => back |= self.recovery(ui),
            State::Connected => back |= self.connected(ui),
            State::Failed(_) => back |= self.failed(ui),
        });
        back
    }

    fn intro(&mut self, ui: &mut egui::Ui) -> bool {
        let mut back = false;
        ui.add_space(4.0);
        card(ui, |ui| {
            point(
                ui,
                "Google only stores unreadable files in a hidden app folder.",
            );
            point(ui, "The key is your own password. Google never sees it.");
            point(ui, "Pins, groups, deletions and images sync too.");
        });
        ui.add_space(8.0);
        let w = ui.available_width();
        if ui
            .add_enabled(
                !self.busy,
                theme::primary("Sign in with Google").min_size(egui::vec2(w, 40.0)),
            )
            .clicked()
        {
            self.state = State::SigningIn;
            self.spawn(|| Msg::SignedIn(onboard::google_login().and_then(Pending::connect)));
        }
        ui.vertical_centered(|ui| {
            if ui.link("Not now").clicked() {
                back = true;
            }
        });
        back
    }

    /// A password box with a Show/Hide button.
    fn secret_field(&mut self, ui: &mut egui::Ui, which: u8, hide: bool) -> egui::Response {
        let w = ui.available_width() - 72.0;
        ui.horizontal(|ui| {
            let value = if which == 0 {
                &mut self.password
            } else {
                &mut self.repeat
            };
            let resp = ui.add(
                egui::TextEdit::singleline(value)
                    .password(hide && !self.show_password)
                    .margin(egui::vec2(10.0, 7.0))
                    .desired_width(w),
            );
            if which == 0 {
                let label = if self.show_password { "Hide" } else { "Show" };
                if ui
                    .add_sized([60.0, 32.0], egui::Button::new(label))
                    .clicked()
                {
                    self.show_password = !self.show_password;
                }
            }
            resp
        })
        .inner
    }

    fn password_form(&mut self, ui: &mut egui::Ui) -> bool {
        let State::Password { exists, pending } = &self.state else {
            return false;
        };
        let (exists, pending) = (*exists, pending.clone());
        let mut back = false;
        let mut submit = false;
        let hide = !(exists && self.use_recovery);

        ui.label(theme::muted(if exists && self.use_recovery {
            "Recovery code"
        } else {
            "Password"
        }));
        let first = self.secret_field(ui, 0, hide);
        if self.focus {
            first.request_focus();
            self.focus = false;
        }
        let mut enter = first.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        if !exists {
            ui.label(theme::muted("Repeat it"));
            let second = self.secret_field(ui, 1, true);
            enter |= second.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        }
        let problem = if exists || self.password.is_empty() {
            None
        } else if let Err(e) = crate::crypto::vault::check_password(&self.password) {
            Some(e)
        } else if !self.repeat.is_empty() && self.repeat != self.password {
            Some("The two passwords do not match".to_owned())
        } else {
            None
        };
        if let Some(p) = &problem {
            ui.colored_label(theme::ERR, p);
        } else if !exists {
            ui.label(theme::muted("At least 10 characters. A phrase of 3-4 words is easy to remember. Nobody can reset it for you.").size(12.5));
        }
        if !self.status.is_empty() {
            ui.colored_label(theme::ERR, &self.status);
        }
        let ready = !self.password.is_empty()
            && (exists || (self.password == self.repeat && problem.is_none()));
        ui.add_space(6.0);
        let w = ui.available_width();
        let label = if exists { "Unlock" } else { "Turn on sync" };
        if ui
            .add_enabled(
                ready && !self.busy,
                theme::primary(label).min_size(egui::vec2(w, 40.0)),
            )
            .clicked()
        {
            submit = true;
        }
        ui.vertical_centered(|ui| {
            if self.busy {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(theme::muted("Working, a few seconds…"));
                });
            } else if exists {
                let text = if self.use_recovery {
                    "Use the password instead"
                } else {
                    "Forgot it? Use the recovery code"
                };
                if ui.link(text).clicked() {
                    self.use_recovery = !self.use_recovery;
                    self.password.clear();
                    self.status.clear();
                    self.focus = true;
                }
            }
            if !self.busy && ui.link("Cancel").clicked() {
                back = true;
            }
        });
        if (submit || (enter && ready)) && !self.busy {
            if let Some(p) = pending {
                let secret = self.password.clone();
                if exists {
                    let recovery = self.use_recovery;
                    self.status.clear();
                    self.spawn(move || {
                        Msg::Joined(
                            p.join(&secret, recovery)
                                .and_then(|()| cli::restart_daemon()),
                        )
                    });
                } else {
                    self.spawn(move || {
                        Msg::Created(p.create(&secret).and_then(|code| {
                            cli::restart_daemon()?;
                            Ok(code)
                        }))
                    });
                }
            }
        }
        if back {
            // The sign-in token was never saved, so cancelling leaves nothing behind.
            self.clear_secrets();
            self.state = State::Intro;
        }
        false
    }

    fn recovery(&mut self, ui: &mut egui::Ui) -> bool {
        let State::Recovery(code) = &self.state else {
            return false;
        };
        let code = code.clone();
        ui.add_space(2.0);
        egui::Frame::new()
            .fill(egui::Color32::from_rgb(0x0A, 0x10, 0x0D))
            .stroke(egui::Stroke::new(1.0, theme::ACCENT_BRIGHT))
            .corner_radius(10.0)
            .inner_margin(egui::Margin::same(14))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                let groups: Vec<&str> = code.split('-').collect();
                for row in groups.chunks(7) {
                    ui.label(egui::RichText::new(row.join("  ")).monospace().size(15.0));
                }
            });
        ui.label(theme::muted("If you forget your password, this code is the only way back to your history. Keep it in a password manager or on paper.").size(12.5));
        ui.add_space(4.0);
        ui.checkbox(&mut self.wrote_it_down, "I saved it");
        let w = ui.available_width();
        if ui
            .add_enabled(
                self.wrote_it_down,
                theme::primary("Done").min_size(egui::vec2(w, 40.0)),
            )
            .clicked()
        {
            self.wrote_it_down = false;
            self.status_at = None;
            self.state = State::Connected;
        }
        false
    }

    fn connected(&mut self, ui: &mut egui::Ui) -> bool {
        self.refresh_status();
        ui.ctx().request_repaint_after(Duration::from_secs(2));
        let mut back = false;
        card(ui, |ui| {
            row(ui, "Status", &self.status);
            if let Ok(Some(s)) = Settings::load() {
                row(ui, "This device", &s.device);
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let half = (ui.available_width() - 8.0) / 2.0;
            if ui
                .add_sized([half, 38.0], theme::primary("Sync now"))
                .clicked()
            {
                let _ = ipc::request(&Command::Sync);
                self.status_at = None;
            }
            if ui
                .add_sized([half, 38.0], egui::Button::new("Back"))
                .clicked()
            {
                back = true;
            }
        });
        ui.add_space(14.0);
        ui.separator();
        if self.confirm_off {
            ui.label(theme::muted("Turn sync off on this device? Your local history stays. The encrypted copy stays in your Google Drive until you delete it there.").size(12.5));
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!self.busy, egui::Button::new("Yes, turn off"))
                    .clicked()
                {
                    self.spawn(|| Msg::Disabled(cli::sync_disable()));
                }
                if ui.button("Keep it on").clicked() {
                    self.confirm_off = false;
                }
                if self.busy {
                    ui.spinner();
                }
            });
        } else {
            ui.vertical_centered(|ui| {
                if ui.link("Turn off sync on this device").clicked() {
                    self.confirm_off = true;
                }
            });
        }
        back
    }

    fn failed(&mut self, ui: &mut egui::Ui) -> bool {
        let State::Failed(e) = &self.state else {
            return false;
        };
        let e = e.clone();
        let mut back = false;
        card(ui, |ui| {
            ui.colored_label(theme::ERR, e);
        });
        ui.add_space(4.0);
        let w = ui.available_width();
        if ui
            .add(theme::primary("Try again").min_size(egui::vec2(w, 40.0)))
            .clicked()
        {
            self.state = if matches!(Settings::load(), Ok(Some(_))) {
                State::Connected
            } else {
                State::Intro
            };
        }
        ui.vertical_centered(|ui| {
            if ui.link("Back").clicked() {
                back = true;
            }
        });
        back
    }
}

/// Centered column 440 px wide: logo, title, subtitle, then the body.
fn page(
    ui: &mut egui::Ui,
    logo: Option<&egui::TextureHandle>,
    title: &str,
    subtitle: &str,
    body: impl FnOnce(&mut egui::Ui),
) {
    let pad = ((ui.available_width() - 440.0) / 2.0).max(0.0) as i8;
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(pad, 0))
        .show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(4.0);
                if let Some(logo) = logo {
                    ui.add(egui::Image::new((logo.id(), egui::vec2(72.0, 72.0))));
                }
                ui.add_space(2.0);
                ui.label(egui::RichText::new(title).heading().strong());
                if !subtitle.is_empty() {
                    ui.add(egui::Label::new(theme::muted(subtitle)).halign(egui::Align::Center));
                }
            });
            ui.add_space(10.0);
            body(ui);
        });
}

fn card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(theme::RAISED)
        .stroke(egui::Stroke::new(1.0, theme::BORDER))
        .corner_radius(10.0)
        .inner_margin(egui::Margin::same(14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
}

fn point(ui: &mut egui::Ui, text: &str) {
    ui.horizontal_top(|ui| {
        ui.colored_label(theme::ACCENT_BRIGHT, "•");
        ui.add(egui::Label::new(text).wrap());
    });
}

fn row(ui: &mut egui::Ui, name: &str, value: &str) {
    ui.horizontal_top(|ui| {
        ui.add_sized([86.0, 18.0], egui::Label::new(theme::muted(name)));
        ui.add(egui::Label::new(value).wrap());
    });
}
