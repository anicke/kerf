//! The "Detect Song Starts" dialog: settings, setlist and a live preview.

use eframe::egui;
use kerf_core::detect::{self, Analysis, DetectSettings, Placement};
use kerf_core::util::format_time;
use kerf_core::{Frame, TrackId};

pub struct DetectState {
    pub analysis: Analysis,
    pub tracks: Vec<TrackId>,
    /// Document revision the analysis was made from.
    pub revision: u64,
    pub settings: DetectSettings,
    pub know_count: bool,
    pub expected: usize,
    pub setlist: String,
    /// Proposed markers (drawn on the timeline while the dialog is open).
    pub preview: Vec<(Frame, String)>,
    dirty: bool,
}

pub enum Outcome {
    None,
    Apply {
        start: Frame,
        end: Frame,
        positions: Vec<Frame>,
        names: Vec<String>,
    },
    Reanalyze,
    Cancel,
}

impl DetectState {
    pub fn new(analysis: Analysis, tracks: Vec<TrackId>, revision: u64) -> Self {
        let mut s = Self {
            analysis,
            tracks,
            revision,
            settings: DetectSettings::default(),
            know_count: false,
            expected: 10,
            setlist: String::new(),
            preview: Vec::new(),
            dirty: true,
        };
        s.refresh();
        s
    }

    fn refresh(&mut self) {
        let mut settings = self.settings.clone();
        settings.expected_songs = self.know_count.then_some(self.expected.max(1));
        let found = detect::find_boundaries(&self.analysis, &settings);
        let mut positions: Vec<Frame> = Vec::new();
        if settings.marker_at_start {
            positions.push(self.analysis.start);
        }
        positions.extend(found.iter().map(|b| b.position));
        let names = detect::marker_names(&self.setlist, positions.len());
        self.preview = positions.into_iter().zip(names).collect();
        self.dirty = false;
    }

    /// Number of songs the current preview describes.
    fn songs(&self) -> usize {
        self.preview.len() + usize::from(!self.settings.marker_at_start)
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, sample_rate: u32, current_revision: u64) -> Outcome {
        let a = &self.analysis;
        ui.label(format!(
            "Analysed {} of {} track(s). Typical song level: {:.1} LUFS.",
            format_time(a.end - a.start, sample_rate),
            self.tracks.len(),
            a.reference
        ));
        if current_revision != self.revision {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                "The project has changed since the analysis.",
            );
            if ui.button("Analyse again").clicked() {
                return Outcome::Reanalyze;
            }
        }
        ui.separator();

        let mut changed = false;
        egui::Grid::new("detect-settings")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                ui.label("Songs");
                ui.vertical(|ui| {
                    changed |= ui
                        .radio_value(&mut self.know_count, false, "Detect automatically")
                        .changed();
                    ui.add_enabled_ui(!self.know_count, |ui| {
                        ui.horizontal(|ui| {
                            ui.label("Fewer");
                            changed |= ui
                                .add(
                                    egui::Slider::new(&mut self.settings.sensitivity, 0.0..=1.0)
                                        .show_value(false),
                                )
                                .changed();
                            ui.label("More");
                        });
                    });
                    ui.horizontal(|ui| {
                        changed |= ui
                            .radio_value(&mut self.know_count, true, "I know how many:")
                            .changed();
                        changed |= ui
                            .add_enabled(
                                self.know_count,
                                egui::DragValue::new(&mut self.expected).range(1..=200),
                            )
                            .changed();
                    });
                });
                ui.end_row();

                ui.label("Shortest song");
                changed |= ui
                    .add(
                        egui::DragValue::new(&mut self.settings.min_song_secs)
                            .range(10.0..=1200.0)
                            .speed(1.0)
                            .suffix(" s"),
                    )
                    .changed();
                ui.end_row();

                ui.label("Shortest gap");
                changed |= ui
                    .add(
                        egui::DragValue::new(&mut self.settings.min_gap_secs)
                            .range(0.3..=60.0)
                            .speed(0.1)
                            .suffix(" s"),
                    )
                    .changed();
                ui.end_row();

                ui.label("Marker goes");
                ui.vertical(|ui| {
                    changed |= ui
                        .radio_value(
                            &mut self.settings.placement,
                            Placement::MusicStart,
                            "Where the music starts",
                        )
                        .changed();
                    ui.add_enabled_ui(self.settings.placement == Placement::MusicStart, |ui| {
                        ui.horizontal(|ui| {
                            ui.label("     with pre-roll");
                            changed |= ui
                                .add(
                                    egui::DragValue::new(&mut self.settings.pre_roll_secs)
                                        .range(0.0..=5.0)
                                        .speed(0.05)
                                        .suffix(" s"),
                                )
                                .changed();
                        });
                    });
                    changed |= ui
                        .radio_value(
                            &mut self.settings.placement,
                            Placement::QuietestPoint,
                            "At the quietest point of the gap",
                        )
                        .changed();
                });
                ui.end_row();

                ui.label("");
                changed |= ui
                    .checkbox(
                        &mut self.settings.marker_at_start,
                        "Marker at the very start (names track 1)",
                    )
                    .changed();
                ui.end_row();
            });

        ui.separator();
        ui.label("Setlist (optional, one title per line):");
        changed |= ui
            .add(
                egui::TextEdit::multiline(&mut self.setlist)
                    .desired_rows(4)
                    .desired_width(f32::INFINITY)
                    .hint_text("1. Opening Song\n2. Second Song\n…"),
            )
            .changed();
        if changed || self.dirty {
            self.refresh();
        }

        ui.separator();
        let songs = self.songs();
        let mut heading = format!("{songs} song(s)");
        if self.know_count && songs < self.expected {
            heading.push_str(&format!(
                " — only found {} clear gap(s); add the rest by hand",
                songs.saturating_sub(1)
            ));
        }
        ui.label(egui::RichText::new(heading).strong());
        egui::ScrollArea::vertical()
            .max_height(150.0)
            .show(ui, |ui| {
                for (i, (pos, name)) in self.preview.iter().enumerate() {
                    ui.label(
                        egui::RichText::new(format!(
                            "{:02}  {:>10}  {name}",
                            i + 1,
                            format_time(*pos, sample_rate)
                        ))
                        .monospace(),
                    );
                }
            });
        ui.label(
            egui::RichText::new(
                "Markers from earlier detections are replaced; markers you placed yourself are kept. \
                 Drag any marker afterwards to fine-tune it.",
            )
            .weak()
            .small(),
        );

        let mut outcome = Outcome::None;
        ui.horizontal(|ui| {
            let label = format!("Add {} Marker(s)", self.preview.len());
            if ui
                .add_enabled(!self.preview.is_empty(), egui::Button::new(label))
                .clicked()
            {
                outcome = Outcome::Apply {
                    start: self.analysis.start,
                    end: self.analysis.end,
                    positions: self.preview.iter().map(|p| p.0).collect(),
                    names: self.preview.iter().map(|p| p.1.clone()).collect(),
                };
            }
            if ui.button("Cancel").clicked() {
                outcome = Outcome::Cancel;
            }
        });
        outcome
    }
}
