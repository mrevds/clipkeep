//! The Win+V-style window: search box, tabs (All / Pinned / Links / your groups), clip list.
//! It holds no history itself: every search goes to the daemon on a worker thread,
//! so the window opens instantly even with a huge history and typing never blocks.

use crate::daemon::ipc::{self, Command, GroupRow};
use crate::store::text::Kind;
use crate::store::{Hit, View};
use eframe::egui::{self, Key, Modifiers};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

const RESULTS: usize = 200;

type Results = Arc<Mutex<Option<(u64, Result<Vec<Hit>, String>)>>>;

pub fn run(query: String) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    let Some(toggle) = toggle::claim()?
    else {
        return Ok(()); // a window was already open: we just closed it
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("clipkeep")
            .with_app_id("clipkeep")
            .with_inner_size([600.0, 500.0])
            .with_resizable(false)
            .with_decorations(false)
            .with_always_on_top()
            .with_icon(theme::window_icon().unwrap_or_default()),
        ..Default::default()
    };
    let paste = Arc::new(AtomicBool::new(false));
    let paste_flag = Arc::clone(&paste);
    let result = eframe::run_native(
        "clipkeep",
        options,
        Box::new(move |cc| {
            #[cfg(target_os = "linux")]
            toggle.listen(cc.egui_ctx.clone());
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(Popup::new(cc.egui_ctx.clone(), query, paste_flag)))
        }),
    )
    .map_err(|e| e.to_string());
    #[cfg(target_os = "linux")]
    toggle::release();
    // The window is gone now, so Ctrl+V lands in the app the user came from.
    if paste.load(Ordering::Relaxed) && crate::config::Config::load().auto_paste {
        if let Err(e) = ipc::request(&Command::Paste) {
            eprintln!("clipkeep: {e}");
        }
    }
    result
}

mod sync_screen;
mod theme;
#[cfg(target_os = "linux")]
mod toggle;
mod workers;

struct GroupEdit {
    id: u32,
    name: String,
}

struct Popup {
    query: String,
    tab: usize,
    groups: Vec<GroupRow>,
    hits: Vec<Hit>,
    loaded: bool,
    gen: u64,
    jobs: Sender<(u64, View, String)>,
    results: Results,
    selected: usize,
    scroll_to_selected: bool,
    was_focused: bool,
    frames: u32,
    error: Option<String>,
    sync: Option<sync_screen::SyncScreen>,
    edit: Option<GroupEdit>,
    paste_after_close: Arc<AtomicBool>,
    ctx: egui::Context,
    logo: Option<egui::TextureHandle>,
    thumbs: HashMap<u32, Option<egui::TextureHandle>>,
    thumbs_asked: HashSet<u32>,
    thumb_jobs: Sender<u32>,
    thumb_results: Receiver<(u32, Option<egui::ColorImage>)>,
    /// Debug: `CLIPKEEP_SCREENSHOT=out.ppm clipkeep show` saves one frame and exits.
    screenshot_to: Option<std::path::PathBuf>,
}

enum Action {
    Copy(u32),
    Pin(u32),
    Delete(u32),
    EditGroup(u32),
    Close,
}

impl Popup {
    fn new(ctx: egui::Context, query: String, paste_after_close: Arc<AtomicBool>) -> Self {
        let (jobs, rx) = mpsc::channel();
        let results: Results = Arc::default();
        let out = Arc::clone(&results);
        let (thumb_jobs, thumb_rx) = mpsc::channel();
        let (thumb_tx, thumb_results) = mpsc::channel();
        let thumb_ctx = ctx.clone();
        let ctx_for_self = ctx.clone();
        std::thread::spawn(move || workers::thumb_worker(thumb_rx, thumb_tx, thumb_ctx));
        std::thread::spawn(move || workers::search_worker(rx, out, ctx));
        let mut p = Popup {
            query,
            tab: std::env::var("CLIPKEEP_TAB")
                .ok()
                .and_then(|t| t.parse().ok())
                .unwrap_or(0),
            groups: Vec::new(),
            hits: Vec::new(),
            loaded: false,
            gen: 0,
            jobs,
            results,
            selected: 0,
            scroll_to_selected: false,
            was_focused: false,
            frames: 0,
            error: None,
            sync: None,
            edit: None,
            paste_after_close,
            ctx: ctx_for_self.clone(),
            logo: theme::logo_image()
                .map(|img| ctx_for_self.load_texture("logo", img, egui::TextureOptions::LINEAR)),
            thumbs: HashMap::new(),
            thumbs_asked: HashSet::new(),
            thumb_jobs,
            thumb_results,
            screenshot_to: std::env::var_os("CLIPKEEP_SCREENSHOT").map(Into::into),
        };
        p.reload_groups();
        p.refresh();
        if std::env::var_os("CLIPKEEP_SYNC_STATE").is_some()
            || std::env::var_os("CLIPKEEP_SYNC").is_some()
        {
            p.open_sync(&p.ctx.clone());
        }
        p
    }

    fn tabs(&self) -> Vec<(String, View)> {
        let mut t = vec![
            ("All".to_owned(), View::All),
            ("📌 Pinned".to_owned(), View::Pinned),
            ("🔗 Links".to_owned(), View::Links),
            ("🖼 Images".to_owned(), View::Images),
        ];
        t.extend(
            self.groups
                .iter()
                .map(|g| (format!("# {} {}", g.name, g.count), View::Group(g.id))),
        );
        t
    }

    fn view(&self) -> View {
        self.tabs().get(self.tab).map(|t| t.1).unwrap_or(View::All)
    }

    fn group_name(&self, id: u16) -> Option<&str> {
        self.groups
            .iter()
            .find(|g| g.id == id)
            .map(|g| g.name.as_str())
    }

    fn refresh(&mut self) {
        self.gen += 1;
        let _ = self.jobs.send((self.gen, self.view(), self.query.clone()));
    }

    fn reload_groups(&mut self) {
        match ipc::request(&Command::Groups) {
            Ok(lines) => self.groups = lines.iter().filter_map(|l| ipc::parse_group(l)).collect(),
            Err(e) => self.error = Some(format!("daemon: {e}")),
        }
        if self.tab >= self.tabs().len() {
            self.tab = 0;
        }
    }

    fn send(&mut self, cmd: Command) -> bool {
        match ipc::request(&cmd) {
            Ok(_) => true,
            Err(e) => {
                self.error = Some(format!("daemon: {e}"));
                false
            }
        }
    }

    fn take_results(&mut self) {
        let Some((gen, result)) = self.results.lock().unwrap().take() else {
            return;
        };
        if gen != self.gen {
            return; // an older query; a newer answer is on its way
        }
        match result {
            Ok(hits) => {
                self.hits = hits;
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
        self.loaded = true;
        if self.selected >= self.hits.len() {
            self.selected = self.hits.len().saturating_sub(1);
        }
    }

    fn take_thumbs(&mut self, ctx: &egui::Context) {
        while let Ok((id, image)) = self.thumb_results.try_recv() {
            let tex = image.map(|img| {
                ctx.load_texture(format!("thumb{id}"), img, egui::TextureOptions::LINEAR)
            });
            self.thumbs.insert(id, tex);
        }
    }

    /// One image row: thumbnail on the left, description next to it.
    fn image_row(&mut self, ui: &mut egui::Ui, h: &Hit, selected: bool) -> egui::Response {
        const ROW_H: f32 = 56.0;
        if self.thumbs_asked.insert(h.id) {
            let _ = self.thumb_jobs.send(h.id);
        }
        let (rect, resp) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), ROW_H),
            egui::Sense::click(),
        );
        let visuals = ui.visuals();
        if selected {
            ui.painter()
                .rect_filled(rect, 4.0, visuals.selection.bg_fill);
        } else if resp.hovered() {
            ui.painter()
                .rect_filled(rect, 4.0, visuals.widgets.hovered.weak_bg_fill);
        }
        let mut text_x = rect.left() + 8.0;
        if let Some(Some(tex)) = self.thumbs.get(&h.id) {
            let size = tex.size_vec2();
            let k = ((ROW_H - 8.0) / size.y).min(160.0 / size.x).min(1.0);
            let r = egui::Rect::from_min_size(
                egui::pos2(rect.left() + 8.0, rect.center().y - size.y * k / 2.0),
                size * k,
            );
            ui.painter().image(
                tex.id(),
                r,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }
        text_x += self
            .thumbs
            .get(&h.id)
            .and_then(|t| t.as_ref())
            .map_or(0.0, |t| {
                (t.size_vec2().x
                    * ((ROW_H - 8.0) / t.size_vec2().y)
                        .min(160.0 / t.size_vec2().x)
                        .min(1.0))
                .max(0.0)
            })
            + 14.0;
        let mut label = h.preview.clone();
        if h.pinned {
            label.push_str("  📌");
        }
        if let Some(g) = self.group_name(h.group) {
            label.push_str(&format!("  #{g}"));
        }
        ui.painter().text(
            egui::pos2(text_x, rect.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(14.0),
            ui.visuals().text_color(),
        );
        resp
    }

    fn open_sync(&mut self, ctx: &egui::Context) {
        self.sync = Some(sync_screen::SyncScreen::open(ctx.clone()));
        // Browser and password prompts need a normal window: not on top, and it must survive losing focus.
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
            egui::WindowLevel::Normal,
        ));
    }

    fn close_sync(&mut self, ctx: &egui::Context) {
        self.sync = None;
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
            egui::WindowLevel::AlwaysOnTop,
        ));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    fn switch_tab(&mut self, delta: isize) {
        let n = self.tabs().len() as isize;
        self.tab = (self.tab as isize + delta).rem_euclid(n) as usize;
        self.selected = 0;
        self.refresh();
    }

    fn keyboard(&mut self, ui: &mut egui::Ui) -> Option<Action> {
        let n = self.hits.len();
        let current = self.hits.get(self.selected).map(|h| h.id);
        let mut action = None;
        let mut tab_delta = 0;
        ui.input_mut(|i| {
            if i.consume_key(Modifiers::NONE, Key::Escape) {
                action = Some(Action::Close);
            }
            let mut sel = None;
            if i.consume_key(Modifiers::NONE, Key::ArrowDown) && n > 0 {
                sel = Some((self.selected + 1).min(n - 1));
            }
            if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                sel = Some(self.selected.saturating_sub(1));
            }
            if i.consume_key(Modifiers::NONE, Key::PageDown) && n > 0 {
                sel = Some((self.selected + 8).min(n - 1));
            }
            if i.consume_key(Modifiers::NONE, Key::PageUp) {
                sel = Some(self.selected.saturating_sub(8));
            }
            if let Some(sel) = sel {
                self.selected = sel;
                self.scroll_to_selected = true;
            }
            if i.consume_key(Modifiers::SHIFT, Key::Tab) {
                tab_delta = -1;
            }
            if i.consume_key(Modifiers::NONE, Key::Tab) {
                tab_delta = 1;
            }
            if i.consume_key(Modifiers::NONE, Key::Enter) {
                action = current.map(Action::Copy);
            }
            if i.consume_key(Modifiers::COMMAND, Key::P) {
                action = current.map(Action::Pin);
            }
            if i.consume_key(Modifiers::COMMAND, Key::G) {
                action = current.map(Action::EditGroup);
            }
            if i.consume_key(Modifiers::SHIFT, Key::Delete) {
                action = current.map(Action::Delete);
            }
            const DIGITS: [Key; 9] = [
                Key::Num1,
                Key::Num2,
                Key::Num3,
                Key::Num4,
                Key::Num5,
                Key::Num6,
                Key::Num7,
                Key::Num8,
                Key::Num9,
            ];
            for (k, key) in DIGITS.iter().enumerate() {
                if i.consume_key(Modifiers::COMMAND, *key) {
                    if let Some(h) = self.hits.get(k) {
                        action = Some(Action::Copy(h.id));
                    }
                }
            }
        });
        if tab_delta != 0 {
            self.switch_tab(tab_delta);
        }
        action
    }

    fn apply(&mut self, ctx: &egui::Context, action: Action) {
        match action {
            Action::Close => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Action::Copy(id) => {
                if self.send(Command::Copy(id)) {
                    self.paste_after_close.store(true, Ordering::Relaxed);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
            Action::Pin(id) => {
                if self.send(Command::Pin(id)) {
                    self.refresh();
                }
            }
            Action::Delete(id) => {
                if self.send(Command::Delete(id)) {
                    self.refresh();
                }
            }
            Action::EditGroup(id) => {
                let current = self
                    .hits
                    .iter()
                    .find(|h| h.id == id)
                    .map(|h| h.group)
                    .unwrap_or(0);
                let name = self.group_name(current).unwrap_or("").to_owned();
                self.edit = Some(GroupEdit { id, name });
            }
        }
    }

    /// Bottom bar while choosing a group: type a name (new or existing) or click one.
    fn group_editor(&mut self, ui: &mut egui::Ui) {
        let Some(edit) = &mut self.edit else { return };
        let (enter, esc) = ui.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::Enter),
                i.consume_key(Modifiers::NONE, Key::Escape),
            )
        });
        let mut chosen: Option<String> = enter.then(|| edit.name.clone());
        ui.horizontal(|ui| {
            ui.label("Group:");
            let field = ui.add(
                egui::TextEdit::singleline(&mut edit.name)
                    .hint_text("name, Enter to save, empty = no group")
                    .desired_width(260.0),
            );
            field.request_focus();
            for g in &self.groups {
                if ui.small_button(&g.name).clicked() {
                    chosen = Some(g.name.clone());
                }
            }
        });
        if esc {
            self.edit = None;
        } else if let Some(name) = chosen {
            let id = edit.id;
            self.edit = None;
            if self.send(Command::SetGroup(id, name)) {
                self.reload_groups();
                self.refresh();
            }
        }
    }

    fn debug_screenshot(&mut self, ctx: &egui::Context) {
        let Some(path) = self.screenshot_to.clone() else {
            return;
        };
        ctx.request_repaint();
        if !self.loaded {
            return;
        }
        self.frames += 1;
        if self.frames == 3 {
            let c = ctx.clone();
            let callback = egui::ScreenshotCallback::new(move |img| {
                if let Err(e) = save_ppm(&path, &img) {
                    eprintln!("clipkeep: screenshot failed: {e}");
                }
                c.send_viewport_cmd(egui::ViewportCommand::Close);
                c.request_repaint();
            });
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(callback));
        }
    }
}

fn save_ppm(path: &std::path::Path, img: &egui::ColorImage) -> std::io::Result<()> {
    let [w, h] = img.size;
    let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
    for px in &img.pixels {
        out.extend_from_slice(&[px.r(), px.g(), px.b()]);
    }
    std::fs::write(path, out)
}

impl eframe::App for Popup {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.debug_screenshot(&ctx);
        self.take_results();
        self.take_thumbs(&ctx);

        // Ask for focus once (Windows/macOS start us from the daemon), then close
        // when the user clicks somewhere else, like Win+V does.
        match ui.input(|i| i.viewport().focused) {
            Some(true) => self.was_focused = true,
            Some(false)
                if self.was_focused && self.screenshot_to.is_none() && self.sync.is_none() =>
            {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            _ if !self.was_focused => ctx.send_viewport_cmd(egui::ViewportCommand::Focus),
            _ => {}
        }

        if let Some(screen) = &mut self.sync {
            let mut back = false;
            let logo = self.logo.clone();
            egui::CentralPanel::default()
                .frame(theme::window_frame())
                .show(ui, |ui| back = screen.show(ui, logo.as_ref()));
            if back {
                self.close_sync(&ctx);
            }
            return;
        }

        let mut action = if self.edit.is_none() {
            self.keyboard(ui)
        } else {
            None
        };

        egui::CentralPanel::default().frame(theme::window_frame()).show(ui, |ui| {
            let mut open_sync = false;
            let search = ui
                .horizontal(|ui| {
                    if let Some(logo) = &self.logo {
                        ui.add(egui::Image::new((logo.id(), egui::vec2(28.0, 28.0))));
                    }
                    let w = ui.available_width() - 84.0;
                    let search = ui.add(
                        egui::TextEdit::singleline(&mut self.query)
                            .hint_text("Search…")
                            .desired_width(w),
                    );
                    open_sync = ui.button("Sync").clicked();
                    search
                })
                .inner;
            if open_sync {
                self.open_sync(&ctx);
            }
            if self.edit.is_none() {
                search.request_focus();
            }
            if search.changed() {
                self.selected = 0;
                self.refresh();
            }

            ui.horizontal_wrapped(|ui| {
                for (i, (label, _)) in self.tabs().into_iter().enumerate() {
                    if ui.selectable_label(i == self.tab, label).clicked() && i != self.tab {
                        self.tab = i;
                        self.selected = 0;
                        self.refresh();
                    }
                }
            });
            ui.add_space(2.0);

            let footer_h = 26.0;
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .max_height(ui.available_height() - footer_h)
                .show(ui, |ui| {
                    if self.loaded && self.hits.is_empty() {
                        ui.add_space(30.0);
                        ui.vertical_centered(|ui| {
                            ui.label(theme::muted(if self.query.is_empty() && self.tab == 0 {
                                "History is empty. Copy something and it shows up here."
                            } else {
                                "Nothing found."
                            }));
                        });
                    }
                    for row in 0..self.hits.len() {
                        let h = self.hits[row].clone();
                        let selected = row == self.selected;
                        let resp = if h.kind == Kind::Image as u8 {
                            self.image_row(ui, &h, selected)
                        } else {
                        let mut label = if row < 9 { format!("{}  ", row + 1) } else { "    ".to_owned() };
                        label.push_str(&h.preview);
                        let mut tags = String::new();
                        if let Some(g) = self.group_name(h.group) {
                            tags.push_str(&format!("#{g} "));
                        }
                        if h.kind == Kind::Link as u8 {
                            tags.push('🔗');
                        }
                        if h.pinned {
                            tags.push('📌');
                        }
                        ui.add(
                            egui::Button::selectable(selected, label)
                                .right_text(egui::RichText::new(tags).weak())
                                .truncate()
                                .min_size(egui::vec2(ui.available_width(), 26.0)),
                        )
                        };
                        if selected && self.scroll_to_selected {
                            resp.scroll_to_me(None);
                        }
                        if resp.clicked() {
                            action = Some(Action::Copy(h.id));
                        }
                        if resp.secondary_clicked() {
                            action = Some(Action::Pin(h.id));
                        }
                    }
                });
            self.scroll_to_selected = false;

            ui.separator();
            if self.edit.is_some() {
                self.group_editor(ui);
            } else {
                match &self.error {
                    Some(e) => ui.colored_label(theme::ERR, e),
                    None => ui.label(theme::muted("Enter copy · Ctrl+1…9 quick · Tab tabs · Ctrl+P pin · Ctrl+G group · Shift+Del delete").size(12.0)),
                };
            }
        });

        if let Some(action) = action {
            self.apply(&ctx, action);
        }
    }
}
