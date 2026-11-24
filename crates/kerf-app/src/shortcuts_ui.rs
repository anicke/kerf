//! The Keyboard Shortcuts dialog: every command with its keys; add a key by
//! pressing it, remove one with its ✕, reset one command or all.

use eframe::egui::{self, Key};

use crate::keymap::{self, Binding, Keymap, COMMANDS};
use crate::prefs::Prefs;

#[derive(Default)]
pub struct Editor {
    filter: String,
    /// Command waiting for a key press.
    recording: Option<&'static str>,
    note: Option<String>,
}

/// Show the editor; changes are applied and saved at once.
pub fn ui(ui: &mut egui::Ui, ed: &mut Editor, prefs: &mut Prefs, keys: &mut Keymap) {
    let mut change: Option<(&'static str, Vec<Binding>)> = None;

    if let Some(id) = ed.recording {
        match take_key_press(ui.ctx()) {
            Some(b) if b == Binding::new(egui::Modifiers::NONE, Key::Escape) => {
                ed.recording = None;
            }
            Some(b) => {
                let cmd = keymap::command(id).unwrap();
                let mut cur: Vec<Binding> = keys.bindings(cmd.action).collect();
                if !cur.contains(&b) {
                    cur.push(b);
                }
                change = Some((id, cur));
                ed.recording = None;
            }
            None => {}
        }
    }

    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut ed.filter)
                .hint_text("Search commands or keys")
                .desired_width(260.0),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let changed = !prefs.shortcuts.is_empty();
            if ui
                .add_enabled(changed, egui::Button::new("Reset all"))
                .on_hover_text("Go back to Kerf's default shortcuts")
                .clicked()
            {
                prefs.shortcuts.clear();
                ed.note = Some("All shortcuts are back to their defaults.".into());
                *keys = Keymap::new(&prefs.shortcuts);
                let _ = prefs.save();
            }
        });
    });
    let note = ed.recording.map_or(ed.note.clone(), |id| {
        Some(format!(
            "Press the keys for “{}” (Esc cancels).",
            keymap::command(id).unwrap().name
        ))
    });
    if let Some(n) = note {
        ui.label(egui::RichText::new(n).weak());
    }
    ui.separator();

    let filter = ed.filter.to_lowercase();
    egui::ScrollArea::vertical()
        .max_height(480.0)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            egui::Grid::new("shortcuts")
                .num_columns(3)
                .min_col_width(0.0)
                .spacing([16.0, 4.0])
                .show(ui, |ui| {
                    let mut group = "";
                    for c in COMMANDS {
                        let bindings: Vec<Binding> = keys.bindings(c.action).collect();
                        if !filter.is_empty()
                            && !c.name.to_lowercase().contains(&filter)
                            && !bindings
                                .iter()
                                .any(|b| b.label().to_lowercase().contains(&filter))
                        {
                            continue;
                        }
                        if c.group != group {
                            group = c.group;
                            ui.label(egui::RichText::new(group).strong());
                            ui.end_row();
                        }
                        let custom = prefs.shortcuts.contains_key(c.id);
                        let mut name = egui::RichText::new(c.name);
                        if custom {
                            name = name.italics();
                        }
                        ui.horizontal(|ui| {
                            ui.set_min_width(260.0);
                            ui.label(name).on_hover_text(if custom {
                                "Changed from the default"
                            } else {
                                ""
                            });
                        });
                        ui.horizontal(|ui| {
                            ui.set_min_width(220.0);
                            for b in &bindings {
                                if ui
                                    .add(egui::Button::new(format!("{}  ✕", b.label())).small())
                                    .on_hover_text("Remove this shortcut")
                                    .clicked()
                                {
                                    let rest =
                                        bindings.iter().copied().filter(|x| x != b).collect();
                                    change = Some((c.id, rest));
                                }
                            }
                        });
                        ui.horizontal(|ui| {
                            let recording = ed.recording == Some(c.id);
                            let add = egui::Button::selectable(
                                recording,
                                if recording { "Press keys…" } else { "+" },
                            )
                            .small();
                            if ui.add(add).on_hover_text("Add a shortcut").clicked() {
                                ed.recording = if recording { None } else { Some(c.id) };
                                ed.note = None;
                            }
                            if custom
                                && ui
                                    .small_button("↺")
                                    .on_hover_text("Back to the default")
                                    .clicked()
                            {
                                let defaults = c
                                    .defaults
                                    .iter()
                                    .filter_map(|s| Binding::parse(s))
                                    .collect();
                                change = Some((c.id, defaults));
                            }
                        });
                        ui.end_row();
                    }
                });
        });

    if let Some((id, new)) = change {
        let losers = keymap::assign(&mut prefs.shortcuts, id, new);
        ed.note = (!losers.is_empty()).then(|| format!("Taken from: {}.", losers.join(", ")));
        *keys = Keymap::new(&prefs.shortcuts);
        if let Err(e) = prefs.save() {
            ed.note = Some(format!("Could not save preferences: {e}"));
        }
    }
}

/// The first key pressed this frame (with its modifiers), swallowing all key
/// input so nothing else reacts to it while recording.
fn take_key_press(ctx: &egui::Context) -> Option<Binding> {
    ctx.input_mut(|i| {
        let mods = i.modifiers;
        let mut found = None;
        i.events.retain(|e| {
            let b = match e {
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => Binding::new(*modifiers, *key),
                // Ctrl+C/X/V arrive as clipboard events.
                egui::Event::Copy => Binding::new(mods, Key::C),
                egui::Event::Cut => Binding::new(mods, Key::X),
                egui::Event::Paste(_) => Binding::new(mods, Key::V),
                egui::Event::Key { .. } | egui::Event::Text(_) => return false,
                _ => return true,
            };
            found.get_or_insert(b);
            false
        });
        found
    })
}
