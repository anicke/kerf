//! The Compare window: a null test of the project as it is now against an
//! earlier state in the history or a file (an export, another take, a saved
//! project). It shows whether they are identical, the size of the difference,
//! and where it is; click a region to select it on the timeline.

use eframe::egui;
use kerf_core::ops::{CompareInfo, Request, Response, Session, Version};
use kerf_core::Frame;

use crate::app::KerfApp;

pub struct CompareState {
    /// What the project is compared with.
    pub with: Version,
    /// How `with` is described (a history label, a file name).
    pub title: String,
    pub align: bool,
    /// Ignore differences below `threshold_db`.
    pub use_threshold: bool,
    pub threshold_db: f64,
    pub running: bool,
    /// The last result, and the document revision it was made at.
    pub result: Option<(Result<Done, String>, u64)>,
}

pub struct Done {
    pub summary: String,
    pub info: Box<CompareInfo>,
}

impl CompareState {
    pub fn new(with: Version, title: String) -> Self {
        Self {
            with,
            title,
            align: false,
            use_threshold: false,
            threshold_db: -90.0,
            running: false,
            result: None,
        }
    }

    /// The request for the current settings.
    pub fn request(&self) -> Request {
        Request::Compare {
            a: Version::Project,
            b: self.with.clone(),
            start: None,
            end: None,
            tracks: None,
            align: self.align,
            max_offset_secs: None,
            threshold_db: self.use_threshold.then_some(self.threshold_db),
        }
    }
}

/// Turn an operation's response into a result for the window.
pub fn done(r: Result<Response, kerf_core::ops::OpError>) -> Result<Done, String> {
    let r = r.map_err(|e| e.message)?;
    match r.data {
        kerf_core::ops::Data::Compare(info) => Ok(Done {
            summary: r.summary,
            info,
        }),
        _ => Err("unexpected response".into()),
    }
}

/// Compare on a worker thread: on a copy of the document, history and all
/// (cheap: audio is shared).
pub fn run(
    doc: kerf_core::Document,
    cache: Option<std::sync::Arc<kerf_core::AudioCache>>,
    request: Request,
    progress: &mut dyn FnMut(f64) -> bool,
) -> Result<Done, String> {
    let mut s = Session::from_document(doc, cache);
    done(s.execute_with_progress(request, progress))
}

pub enum Outcome {
    None,
    Run,
    /// Select this range on the timeline.
    Select(Frame, Frame),
}

pub fn ui(app: &KerfApp, ui: &mut egui::Ui, st: &mut CompareState) -> Outcome {
    let mut out = Outcome::None;
    ui.label(format!(
        "The project now, compared sample by sample with {}.",
        st.title
    ));
    ui.add_space(4.0);

    match &st.result {
        None if st.running => {
            ui.label(egui::RichText::new("Comparing…").weak());
        }
        None => {}
        Some((Err(e), _)) => {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
        Some((Ok(d), revision)) => {
            if *revision != app.doc.revision() {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    "The project changed since: compare again for an up-to-date result.",
                );
            }
            if let Some(range) = result(ui, d, app) {
                out = range;
            }
        }
    }

    ui.separator();
    ui.horizontal(|ui| {
        ui.checkbox(&mut st.align, "Line up first").on_hover_text(
            "Shift the other version by whole samples to match the start of the project \
                 (extra lead-in, encoder delay) before comparing",
        );
        ui.checkbox(&mut st.use_threshold, "Ignore differences below")
            .on_hover_text("e.g. -90 dBFS accepts 16-bit rounding and dither");
        ui.add_enabled(
            st.use_threshold,
            egui::DragValue::new(&mut st.threshold_db)
                .range(-150.0..=0.0)
                .speed(0.5)
                .suffix(" dBFS"),
        );
    });
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        if ui
            .add_enabled(!st.running, egui::Button::new("Compare Again"))
            .clicked()
        {
            out = Outcome::Run;
        }
    });
    out
}

/// The figures and regions of a result; returns a region the user clicked.
fn result(ui: &mut egui::Ui, d: &Done, app: &KerfApp) -> Option<Outcome> {
    let c = &d.info;
    let fmt = app.prefs.time_format;
    let sr = app.project().sample_rate;
    let time = |f: Frame| kerf_core::util::format_time_as(f, sr, fmt);
    let show = |v: Option<f64>| v.map_or("–".into(), |v| format!("{v:.1}"));
    let (text, color) = if c.identical {
        (
            "✔ Identical, bit for bit",
            ui.visuals().widgets.active.fg_stroke.color,
        )
    } else if c.nulls {
        (
            "✔ No difference above the threshold",
            ui.visuals().widgets.active.fg_stroke.color,
        )
    } else {
        ("✖ They differ", ui.visuals().warn_fg_color)
    };
    ui.label(egui::RichText::new(text).heading().color(color));
    ui.label(&d.summary);
    ui.add_space(4.0);

    egui::Grid::new("compare-figures")
        .num_columns(2)
        .spacing([12.0, 4.0])
        .show(ui, |ui| {
            ui.label("Lengths");
            ui.label(format!(
                "{}  /  {}",
                time(c.length_a.frames),
                time(c.length_b.frames)
            ));
            ui.end_row();
            if let Some(al) = &c.alignment {
                ui.label("Lined up");
                ui.label(if al.found {
                    format!(
                        "shifted by {} samples ({:+.3} s), match {:.0}%",
                        al.offset_frames,
                        al.offset_secs,
                        al.correlation * 100.0
                    )
                } else {
                    "no match found: compared as they are".into()
                });
                ui.end_row();
            }
            if !c.identical {
                ui.label("Difference");
                ui.label(format!(
                    "peak {} dBFS, RMS {} dBFS",
                    show(c.peak_db),
                    show(c.rms_db)
                ));
                ui.end_row();
                ui.label("Null depth");
                ui.label(format!("{} dB below the project", show(c.null_depth_db)))
                    .on_hover_text("How far the difference lies below the signal: -96 dB is about 16-bit rounding noise");
                ui.end_row();
                for ch in &c.channels {
                    ui.label(format!("Channel {}", ch.channel));
                    ui.label(format!(
                        "{} samples differ, peak {} dBFS",
                        ch.differing_samples,
                        show(ch.peak_db)
                    ));
                    ui.end_row();
                }
            }
        });

    if c.regions.is_empty() {
        return None;
    }
    ui.add_space(6.0);
    ui.label(
        egui::RichText::new(if c.truncated {
            format!(
                "Where they differ (first {} of {}), click to select:",
                c.regions.len(),
                c.region_count
            )
        } else {
            format!("Where they differ ({}), click to select:", c.region_count)
        })
        .strong(),
    );
    let mut picked = None;
    egui::ScrollArea::vertical()
        .max_height(200.0)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            for r in &c.regions {
                let text = format!(
                    "{} – {}   ({}, peak {} dBFS)",
                    time(r.start.frames),
                    time(r.end.frames),
                    time(r.length.frames),
                    show(r.peak_db)
                );
                if ui
                    .add(egui::Button::new(text).frame_when_inactive(false))
                    .clicked()
                {
                    picked = Some(Outcome::Select(r.start.frames, r.end.frames));
                }
            }
        });
    picked
}
