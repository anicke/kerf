//! The History panel: every undo step in a list; click one to go back (or
//! forward) to it in one go.

use eframe::egui;

use crate::app::KerfApp;

pub fn show(app: &mut KerfApp, ui: &mut egui::Ui) {
    let states = app.doc.history_states();
    let pos = app.doc.history_position();
    let mut jump = None;
    let mut compare = None;

    ui.horizontal(|ui| {
        ui.heading("History");
        ui.label(egui::RichText::new(format!("{}", states.len() - 1)).weak());
    });
    ui.separator();

    // Bring the current step into view when it changes (edit, undo, redo…).
    let last_id = egui::Id::unique("history_panel_last_pos");
    let moved = ui.data(|d| d.get_temp::<usize>(last_id)) != Some(pos);
    ui.data_mut(|d| d.insert_temp(last_id, pos));

    egui::ScrollArea::vertical()
        .min_scrolled_height(0.0)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            for (i, s) in states.iter().enumerate() {
                let name = s.label.unwrap_or(if states.len() > 1 {
                    "Start"
                } else {
                    "Start (no edits yet)"
                });
                let mut text = egui::RichText::new(name);
                if i == pos {
                    text = text.strong();
                } else if s.label.is_none() || i > pos {
                    // The start, and undone steps that can be redone.
                    text = text.weak();
                }
                if i > pos {
                    text = text.italics();
                }
                let r = ui
                    .add(
                        egui::Button::selectable(i == pos, text)
                            .frame_when_inactive(i == pos)
                            .shortcut_text(if s.saved { "saved" } else { "" })
                            .truncate()
                            .min_size(egui::vec2(ui.available_width(), 0.0)),
                    )
                    .on_hover_text(match (i.cmp(&pos), s.saved) {
                        (std::cmp::Ordering::Less, _) => "Go back to this step",
                        (std::cmp::Ordering::Equal, false) => "The current state",
                        (std::cmp::Ordering::Equal, true) => "The current state, as saved",
                        (std::cmp::Ordering::Greater, _) => "Redo up to this step",
                    });
                if r.clicked() && i != pos {
                    jump = Some(i);
                }
                r.context_menu(|ui| {
                    if ui
                        .add_enabled(i != pos, egui::Button::new("Compare with Current State"))
                        .on_hover_text("Null test: what differs between this step and now?")
                        .clicked()
                    {
                        compare = Some(i);
                        ui.close();
                    }
                });
                if i == pos && moved {
                    r.scroll_to_me(Some(egui::Align::Center));
                }
            }
        });

    if let Some(i) = compare {
        let title = match states[i].label {
            Some(l) => format!("the state after \"{l}\""),
            None => "the start of the history".into(),
        };
        drop(states);
        let ctx = ui.ctx().clone();
        app.compare_with(&ctx, kerf_core::ops::Version::History(i), title);
        return;
    }
    if let Some(i) = jump {
        let label = states[i].label.map(String::from);
        drop(states);
        if app.doc.go_to(i) {
            app.validate_selection();
            match label {
                Some(l) => app.info(format!("Went to: {l}")),
                None => app.info("Went back to the start"),
            }
        }
    }
}
