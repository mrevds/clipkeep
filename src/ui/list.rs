//! The clip list: text rows, image rows with thumbnails, masked password rows.

use super::*;

impl Popup {
    /// Draws the scrolling list of hits; returns what a click asked for.
    pub(super) fn clip_list(&mut self, ui: &mut egui::Ui) -> Option<Action> {
        let mut action = None;
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
                        let mut label = if row < 9 {
                            format!("{}  ", row + 1)
                        } else {
                            "    ".to_owned()
                        };
                        let secret = h.kind == Kind::Secret as u8;
                        if secret {
                            label.push_str("🔒 ");
                        }
                        label.push_str(&h.preview);
                        let label = if secret {
                            egui::RichText::new(label).color(theme::MUTED).italics()
                        } else {
                            egui::RichText::new(label)
                        };
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
        action
    }

    /// One image row: thumbnail on the left, description next to it.
    pub(super) fn image_row(
        &mut self,
        ui: &mut egui::Ui,
        h: &Hit,
        selected: bool,
    ) -> egui::Response {
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
}
