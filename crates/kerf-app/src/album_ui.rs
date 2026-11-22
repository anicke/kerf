//! The Album window: details and cover, the track list with levels, and
//! album-wide loudness.

use eframe::egui;
use kerf_core::album::{self, AlbumAnalysis};
use kerf_core::render::{self, Region};
use kerf_core::util::format_time_as;
use kerf_core::{Frame, Project, Track};

use crate::app::KerfApp;

/// What the audio of the album depends on; analysis is redone when it changes
/// (typing album details does not invalidate it).
#[derive(Clone, PartialEq)]
pub struct AudioFingerprint {
    tracks: Vec<Track>,
    master_gain: f32,
    boundaries: Vec<Frame>,
    channels: u16,
}

impl AudioFingerprint {
    pub fn of(p: &Project) -> Self {
        Self {
            tracks: p.tracks.clone(),
            master_gain: p.master_gain,
            boundaries: p.markers.iter().map(|m| m.position).collect(),
            channels: p.channels,
        }
    }
}

pub struct AlbumState {
    pub analysis: Option<(AudioFingerprint, AlbumAnalysis)>,
    pub analyzing: bool,
    target_lufs: f64,
    ceiling: Option<f64>,
}

impl Default for AlbumState {
    fn default() -> Self {
        Self {
            analysis: None,
            analyzing: false,
            target_lufs: -16.0,
            ceiling: Some(-1.0),
        }
    }
}

pub enum Outcome {
    None,
    Analyze,
    ApplyGain(f64),
    ResetGain,
    ChooseCover,
    Export,
    Close,
}

/// Common loudness targets.
const TARGETS: [(f64, &str); 4] = [
    (-14.0, "-14 LUFS (streaming services)"),
    (-16.0, "-16 LUFS (dynamic music, live albums)"),
    (-18.0, "-18 LUFS (very dynamic music)"),
    (-23.0, "-23 LUFS (broadcast, EBU R128)"),
];

pub fn ui(app: &mut KerfApp, ui: &mut egui::Ui, st: &mut AlbumState) -> Outcome {
    let mut out = Outcome::None;
    let p = app.project().clone();
    let sr = p.sample_rate;
    let fmt = app.prefs.time_format;
    let regions: Vec<Region> = render::regions_from_markers(&p, p.length());
    let fresh = st
        .analysis
        .as_ref()
        .filter(|(fp, a)| *fp == AudioFingerprint::of(&p) && a.tracks.len() == regions.len());
    if fresh.is_none() && !st.analyzing && !regions.is_empty() {
        out = Outcome::Analyze; // keep levels up to date automatically
    }
    let levels = fresh.map(|(_, a)| a.clone());

    // --- Details --------------------------------------------------------------
    ui.heading("Details");
    let mut info = p.album.clone();
    let mut changed = false;
    egui::Grid::new("album-details")
        .num_columns(2)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            for (label, field) in [
                ("Artist", &mut info.artist),
                ("Album", &mut info.title),
                ("Year", &mut info.year),
                ("Genre", &mut info.genre),
                ("Comment", &mut info.comment),
            ] {
                ui.label(label);
                changed |= ui
                    .add(egui::TextEdit::singleline(field).desired_width(320.0))
                    .changed();
                ui.end_row();
            }
            ui.label("Cover");
            ui.horizontal(|ui| {
                let name = info
                    .cover
                    .as_ref()
                    .and_then(|c| c.file_name())
                    .map(|n| n.to_string_lossy().into_owned());
                ui.label(egui::RichText::new(name.unwrap_or_else(|| "none".into())).weak());
                if ui.button("Choose…").clicked() {
                    out = Outcome::ChooseCover;
                }
                if info.cover.is_some() && ui.button("Remove").clicked() {
                    info.cover = None;
                    changed = true;
                }
            });
            ui.end_row();
        });
    if changed {
        let _ = app.doc.apply_merged("Album details", "album-details", |p| {
            p.album = info;
            Ok(())
        });
    }

    // --- Tracks ----------------------------------------------------------------
    ui.separator();
    ui.heading(format!("Tracks ({})", regions.len()));
    if p.markers.is_empty() {
        ui.colored_label(
            ui.visuals().warn_fg_color,
            "No markers yet: the whole recording is one track. Use Markers → Detect Song Starts…",
        );
    }
    egui::ScrollArea::vertical()
        .max_height(220.0)
        .show(ui, |ui| {
            egui::Grid::new("album-tracks")
                .num_columns(6)
                .striped(true)
                .spacing([10.0, 4.0])
                .show(ui, |ui| {
                    for h in ["#", "Title", "Start", "Length", "Loudness", "Peak"] {
                        ui.label(egui::RichText::new(h).weak());
                    }
                    ui.end_row();
                    for (i, r) in regions.iter().enumerate() {
                        ui.label(format!("{:02}", i + 1));
                        match p.markers.iter().find(|m| m.position == r.start) {
                            Some(m) => {
                                let id = m.id;
                                let mut name = m.name.clone();
                                if ui
                                    .add_sized([220.0, 20.0], egui::TextEdit::singleline(&mut name))
                                    .changed()
                                {
                                    let _ = app.doc.apply_merged(
                                        "Rename track",
                                        &format!("marker-name-{}", id.0),
                                        |p| {
                                            if let Some(m) = p.marker_mut(id) {
                                                m.name = name;
                                            }
                                            Ok(())
                                        },
                                    );
                                }
                            }
                            None => {
                                // The audio before the first marker has no marker to hold a title.
                                if ui
                                    .button("Name this track")
                                    .on_hover_text("Adds a marker at the start")
                                    .clicked()
                                {
                                    let pos = r.start;
                                    app.edit("Add marker", |p| Ok(p.add_marker(pos, "Intro")));
                                }
                            }
                        }
                        ui.label(egui::RichText::new(format_time_as(r.start, sr, fmt)).monospace());
                        ui.label(
                            egui::RichText::new(format_time_as(r.end - r.start, sr, fmt))
                                .monospace(),
                        );
                        match levels.as_ref().and_then(|a| a.tracks.get(i)) {
                            Some(t) => {
                                ui.label(t.lufs.map_or("–".into(), |l| format!("{l:.1} LUFS")));
                                let peak = egui::RichText::new(format!("{:.1} dBFS", t.peak_db));
                                ui.label(if t.peak_db > -0.1 {
                                    peak.color(ui.visuals().error_fg_color)
                                } else {
                                    peak
                                });
                            }
                            None => {
                                ui.label("…");
                                ui.label("…");
                            }
                        }
                        ui.end_row();
                    }
                });
        });

    // --- Loudness ------------------------------------------------------------------
    ui.separator();
    ui.heading("Album loudness");
    ui.label(
        egui::RichText::new(
            "One gain for the whole album, so quiet songs stay quieter than loud ones, as they were played.",
        )
        .weak()
        .small(),
    );
    let current_db = album::master_gain_db(&p);
    match &levels {
        Some(a) => {
            ui.label(format!(
                "Measured: {} · peak {:.1} dBFS · album gain now {:+.1} dB",
                a.lufs.map_or("–".into(), |l| format!("{l:.1} LUFS")),
                a.peak_db,
                current_db
            ));
        }
        None => {
            ui.label(if st.analyzing {
                "Measuring…"
            } else {
                "Not measured yet."
            });
        }
    }
    egui::Grid::new("album-loudness")
        .num_columns(2)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            ui.label("Target");
            ui.horizontal(|ui| {
                let label = TARGETS
                    .iter()
                    .find(|t| t.0 == st.target_lufs)
                    .map_or("Custom", |t| t.1);
                egui::ComboBox::from_id_salt("album-target")
                    .selected_text(label)
                    .show_ui(ui, |ui| {
                        for (v, l) in TARGETS {
                            ui.selectable_value(&mut st.target_lufs, v, l);
                        }
                    });
                ui.add(
                    egui::DragValue::new(&mut st.target_lufs)
                        .range(-40.0..=-5.0)
                        .speed(0.1)
                        .suffix(" LUFS"),
                );
            });
            ui.end_row();
            ui.label("Peak ceiling");
            ui.horizontal(|ui| {
                let mut on = st.ceiling.is_some();
                ui.checkbox(&mut on, "");
                let mut v = st.ceiling.unwrap_or(-1.0);
                ui.add_enabled(
                    on,
                    egui::DragValue::new(&mut v)
                        .range(-20.0..=0.0)
                        .speed(0.1)
                        .suffix(" dBFS"),
                );
                st.ceiling = on.then_some(v);
            });
            ui.end_row();
        });
    let gain = levels
        .as_ref()
        .and_then(|a| album::album_gain_db(a, st.target_lufs, st.ceiling));
    ui.horizontal(|ui| {
        let label = match gain {
            Some(g) => format!("Apply album gain ({g:+.1} dB)"),
            None => "Apply album gain".into(),
        };
        if ui
            .add_enabled(
                gain.is_some_and(|g| g.abs() >= 0.05),
                egui::Button::new(label),
            )
            .clicked()
        {
            out = Outcome::ApplyGain(gain.unwrap());
        }
        if ui
            .add_enabled(p.master_gain != 1.0, egui::Button::new("Reset album gain"))
            .clicked()
        {
            out = Outcome::ResetGain;
        }
    });
    if let (Some(g), Some(a), Some(c)) = (gain, &levels, st.ceiling) {
        if a.lufs.is_some_and(|l| l + g < st.target_lufs - 0.05) {
            ui.label(
                egui::RichText::new(format!(
                    "Limited by the {c:.1} dBFS peak ceiling: the album will be {:.1} LUFS.",
                    a.lufs.unwrap() + g
                ))
                .weak()
                .small(),
            );
        }
    }

    ui.separator();
    ui.horizontal(|ui| {
        if ui
            .button("Export Album…")
            .on_hover_text("Export split at markers, tagged")
            .clicked()
        {
            out = Outcome::Export;
        }
        if ui.button("Close").clicked() {
            out = Outcome::Close;
        }
    });
    out
}
