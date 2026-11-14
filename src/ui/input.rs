//! Keyboard shortcuts, what they do, and the group editor at the bottom.

use super::*;

pub(super) enum Action {
    Copy(u32),
    Pin(u32),
    Delete(u32),
    EditGroup(u32),
    Close,
}

impl Popup {
    pub(super) fn keyboard(&mut self, ui: &mut egui::Ui) -> Option<Action> {
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

    pub(super) fn apply(&mut self, ctx: &egui::Context, action: Action) {
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
    pub(super) fn group_editor(&mut self, ui: &mut egui::Ui) {
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
}
