//! The Markers panel: every marker in a list, to jump, rename, select a song
//! or delete.

use eframe::egui;
use kerf_core::edit;
use kerf_core::util::format_time;

use crate::app::{Action, KerfApp};

pub fn show(app: &mut KerfApp, ui: &mut egui::Ui) {
    let ctx = ui.ctx().clone();
    let p = app.project().clone(); // cheap: audio is shared
    let sr = p.sample_rate;
    let here = if app.player.is_playing() {
        app.player.position()
    } else {
        app.cursor
    };
    // The marker of the song being played or under the cursor.
    let current = p.markers.iter().rposition(|m| m.position <= here);
    let mut action = None;

    ui.horizontal(|ui| {
        ui.heading("Markers");
        ui.label(egui::RichText::new(format!("{}", p.markers.len())).weak());
    });
    ui.horizontal(|ui| {
        if ui.button("Add at cursor").on_hover_text("M").clicked() {
            action = Some(Action::AddMarker);
        }
        if ui.button("Detect songs…").clicked() {
            action = Some(Action::DetectSongs);
        }
    });
    ui.separator();

    if p.markers.is_empty() {
        ui.label(
            egui::RichText::new("No markers yet. Press M to add one at the cursor, or detect song starts automatically.")
                .weak(),
        );
    }

    let stripe = ui.visuals().faint_bg_color;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for (i, m) in p.markers.iter().enumerate() {
                let id = m.id;
                let row = egui::Frame::new().fill(if i % 2 == 1 {
                    stripe
                } else {
                    egui::Color32::TRANSPARENT
                });
                row.show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let mut time = egui::RichText::new(format_time(m.position, sr)).monospace();
                        if Some(i) == current {
                            time = time.strong().color(ui.visuals().selection.stroke.color);
                        }
                        if ui
                            .add(egui::Button::new(time).frame(false))
                            .on_hover_text("Go to marker")
                            .clicked()
                        {
                            app.sel.range = None;
                            app.seek(m.position);
                            app.ensure_visible(m.position);
                        }
                        // Buttons on the right; the name takes the remaining width.
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .small_button("✕")
                                .on_hover_text("Delete marker")
                                .clicked()
                            {
                                app.edit("Delete marker", |p| edit::remove_marker(p, id));
                            }
                            if ui
                                .small_button("⇥")
                                .on_hover_text("Select from this marker to the next")
                                .clicked()
                            {
                                let end = p.markers.get(i + 1).map_or(p.length(), |n| n.position);
                                app.sel.tracks.clear();
                                app.sel.range = (end > m.position).then_some((m.position, end));
                                app.cursor = m.position;
                                app.ensure_visible(m.position);
                            }
                            let mut name = m.name.clone();
                            let hint = if m.auto {
                                "Detected automatically"
                            } else {
                                "Marker name"
                            };
                            let r = ui
                                .add(
                                    egui::TextEdit::singleline(&mut name)
                                        .desired_width(ui.available_width()),
                                )
                                .on_hover_text(hint);
                            if r.changed() {
                                let _ = app.doc.apply_merged(
                                    "Rename marker",
                                    &format!("marker-name-{}", id.0),
                                    |p| {
                                        if let Some(m) = p.marker_mut(id) {
                                            m.name = name;
                                        }
                                        Ok(())
                                    },
                                );
                            }
                        });
                    });
                });
            }
        });

    if let Some(a) = action {
        app.perform(&ctx, a);
    }
}
