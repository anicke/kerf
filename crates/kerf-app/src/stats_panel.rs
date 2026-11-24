//! The Statistics panel: levels and loudness of the selection (or the whole
//! project), measured in the background whenever it changes, and a live
//! loudness meter of what is playing.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::Arc;

use eframe::egui;
use kerf_core::analysis::Loudness;
use kerf_core::render::RenderOptions;
use kerf_core::util::format_time_as;
use kerf_core::{Frame, Project, TrackId};

use crate::app::KerfApp;

/// What a measurement was taken of.
#[derive(Clone, Debug, PartialEq)]
struct Key {
    revision: u64,
    start: Frame,
    end: Frame,
    tracks: Vec<TrackId>,
    /// The range is the selection (not the whole project).
    selection: bool,
}

#[derive(Clone, Debug)]
struct Measured {
    peak_db: f64,
    true_peak_db: Option<f64>,
    rms_db: f64,
    lufs: Option<f64>,
    max_momentary: Option<f64>,
    max_short_term: Option<f64>,
    range_lu: Option<f64>,
    clipped: u64,
}

struct Pending {
    key: Key,
    rx: Receiver<Result<Measured, String>>,
    cancel: Arc<AtomicBool>,
    progress: Arc<AtomicU32>,
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

#[derive(Default)]
pub struct Stats {
    shown: Option<(Key, Result<Measured, String>)>,
    pending: Option<Pending>,
    /// Loudness of what has played since playback last started.
    live: Loudness,
    was_playing: bool,
}

impl Stats {
    /// Collect the live meter's input; call every frame, panel open or not.
    pub fn poll_live(&mut self, player: &mut crate::audio::Player) {
        let playing = player.is_playing();
        if playing && !self.was_playing {
            self.live.clear();
        }
        self.was_playing = playing;
        let live = &mut self.live;
        player.take_loudness(|z| {
            if playing {
                live.push(z);
            }
        });
    }

    /// Whether `key` is neither shown nor being measured.
    fn needs(&self, key: &Key) -> bool {
        !(self.shown.as_ref().is_some_and(|(k, _)| k == key)
            || self.pending.as_ref().is_some_and(|p| p.key == *key))
    }

    /// Measure `key` in the background, abandoning any other measurement.
    fn request(&mut self, ctx: &egui::Context, project: Arc<Project>, key: Key) {
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU32::new(0));
        let (c, pr, k, ctx) = (cancel.clone(), progress.clone(), key.clone(), ctx.clone());
        std::thread::spawn(move || {
            let opts = RenderOptions::tracks(k.tracks);
            let r = kerf_core::analysis::measure_range(&project, k.start, k.end, &opts, &mut |f| {
                pr.store((f as f32).to_bits(), Ordering::Relaxed);
                !c.load(Ordering::Relaxed)
            });
            let r = r.map_err(|e| e.to_string()).map(|m| {
                let l = m.loudness();
                Measured {
                    peak_db: m.peak_db(),
                    true_peak_db: m.true_peak_db(),
                    rms_db: m.rms_db(),
                    lufs: m.integrated_lufs(),
                    max_momentary: l.max_momentary(),
                    max_short_term: l.max_short_term(),
                    range_lu: l.range(),
                    clipped: m.clipped_samples(),
                }
            });
            let _ = tx.send(r);
            ctx.request_repaint();
        });
        self.pending = Some(Pending {
            key,
            rx,
            cancel,
            progress,
        });
    }

    fn poll(&mut self) {
        let Some(p) = &self.pending else { return };
        match p.rx.try_recv() {
            Ok(r) => {
                let p = self.pending.take().unwrap();
                self.shown = Some((p.key.clone(), r));
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => self.pending = None,
        }
    }
}

pub fn show(app: &mut KerfApp, ui: &mut egui::Ui) {
    let ctx = ui.ctx().clone();
    let len = app.project().length();
    let (start, end, selection) = match app.sel.range.filter(|(a, b)| b > a) {
        Some((a, b)) => (a, b.min(len), true),
        None => (0, len, false),
    };
    let key = Key {
        revision: app.doc.revision(),
        start,
        end,
        tracks: app.target_tracks(),
        selection,
    };
    app.stats.poll();
    // Wait for drags (selection, faders) to finish before measuring.
    if end > start && !ctx.input(|i| i.pointer.any_down()) && app.stats.needs(&key) {
        let project = Arc::new(app.project().clone());
        app.stats.request(&ctx, project, key.clone());
    }
    if app.stats.pending.is_some() {
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }

    ui.horizontal(|ui| {
        ui.heading("Statistics");
    });
    ui.separator();

    let sr = app.project().sample_rate;
    let tf = app.prefs.time_format;
    let n_tracks = app.project().tracks.len();
    let what = if selection {
        format!(
            "Selection {} – {}",
            format_time_as(start, sr, tf),
            format_time_as(end, sr, tf)
        )
    } else {
        "Whole project".to_string()
    };
    let tracks = match key.tracks.len() {
        n if n == n_tracks => String::new(),
        1 => " · 1 track".into(),
        n => format!(" · {n} tracks"),
    };
    ui.label(egui::RichText::new(format!("{what}{tracks}")).strong());
    if end > start {
        ui.label(
            egui::RichText::new(format!("Length {}", format_time_as(end - start, sr, tf))).weak(),
        );
    }

    let stats = &app.stats;
    let stale = stats.shown.as_ref().is_none_or(|(k, _)| *k != key);
    if len == 0 {
        ui.label(egui::RichText::new("No audio yet.").weak());
    } else if let Some(p) = &stats.pending {
        let f = f32::from_bits(p.progress.load(Ordering::Relaxed));
        ui.add(egui::ProgressBar::new(f).desired_height(4.0));
    } else {
        ui.add_space(4.0);
    }

    match &stats.shown {
        Some((_, Ok(m))) if len > 0 => levels_grid(ui, m, stale),
        Some((_, Err(e))) if !stale => {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
        _ => {}
    }

    ui.add_space(8.0);
    ui.separator();
    live_meter(ui, &stats.live, app.player.is_playing());
}

fn levels_grid(ui: &mut egui::Ui, m: &Measured, stale: bool) {
    let fmt = |v: Option<f64>, unit: &str| match v.filter(|v| v.is_finite()) {
        Some(v) => format!("{v:.1} {unit}"),
        None => "–".into(),
    };
    let warn = ui.visuals().warn_fg_color;
    let rows: [(&str, String, &str, bool); 8] = [
        (
            "Peak",
            fmt(Some(m.peak_db), "dBFS"),
            "Highest sample",
            m.peak_db > 0.0,
        ),
        (
            "True peak",
            fmt(m.true_peak_db, "dBTP"),
            "Highest level between samples too (BS.1770), as a DAC or lossy encoder will see it. \
             Streaming services ask for -1 dBTP or lower.",
            m.true_peak_db.is_some_and(|v| v > -1.0),
        ),
        ("RMS", fmt(Some(m.rms_db), "dBFS"), "Average level", false),
        (
            "Loudness",
            fmt(m.lufs, "LUFS"),
            "Integrated loudness (EBU R128 / BS.1770)",
            false,
        ),
        (
            "Max momentary",
            fmt(m.max_momentary, "LUFS"),
            "Loudest 400 ms",
            false,
        ),
        (
            "Max short-term",
            fmt(m.max_short_term, "LUFS"),
            "Loudest 3 seconds",
            false,
        ),
        (
            "Loudness range",
            fmt(m.range_lu, "LU"),
            "How much the loudness varies (EBU Tech 3342): high for dynamic music, low for compressed",
            false,
        ),
        (
            "Clipped samples",
            m.clipped.to_string(),
            "Samples above 0 dBFS: they clip when exported to an integer format such as 16-bit WAV",
            m.clipped > 0,
        ),
    ];
    egui::Grid::new("stats_grid")
        .num_columns(2)
        .spacing([16.0, 4.0])
        .show(ui, |ui| {
            for (name, value, tip, flag) in rows {
                ui.label(name).on_hover_text(tip);
                let mut t = egui::RichText::new(value).monospace();
                if flag {
                    t = t.color(warn);
                }
                if stale {
                    t = t.weak();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(t).on_hover_text(tip)
                });
                ui.end_row();
            }
        });
}

/// Momentary and short-term bars with their values, and the integrated
/// loudness since playback started.
fn live_meter(ui: &mut egui::Ui, live: &Loudness, playing: bool) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Live loudness").strong());
        if !playing {
            ui.label(egui::RichText::new("(not playing)").weak());
        }
    });
    let (m, s) = if playing {
        (live.momentary(), live.short_term())
    } else {
        (None, None)
    };
    meter_row(ui, "M", "Momentary (400 ms)", m);
    meter_row(ui, "S", "Short-term (3 s)", s);
    let i = live.integrated();
    ui.horizontal(|ui| {
        ui.label("Integrated");
        ui.label(egui::RichText::new(i.map_or("–".into(), |v| format!("{v:.1} LUFS"))).monospace());
        if live.seconds() > 0.0 {
            ui.label(egui::RichText::new(format!("over {:.0} s", live.seconds())).weak());
        }
    })
    .response
    .on_hover_text("Since playback last started");
}

/// Meter scale in LUFS.
const SCALE: (f64, f64) = (-54.0, 0.0);

fn meter_row(ui: &mut egui::Ui, label: &str, tip: &str, v: Option<f64>) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(label).monospace())
            .on_hover_text(tip);
        let w = (ui.available_width() - 90.0).max(40.0);
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, 10.0), egui::Sense::hover());
        resp.on_hover_text(format!(
            "{tip}; marks at -23 (EBU R128) and -14 LUFS (streaming)"
        ));
        let p = ui.painter();
        let x = |l: f64| {
            let t = ((l - SCALE.0) / (SCALE.1 - SCALE.0)).clamp(0.0, 1.0) as f32;
            rect.left() + t * rect.width()
        };
        p.rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
        if let Some(v) = v {
            let fill = if v > -9.0 {
                egui::Color32::from_rgb(230, 190, 60)
            } else {
                egui::Color32::from_rgb(80, 200, 120)
            };
            let bar = egui::Rect::from_min_max(rect.min, egui::pos2(x(v), rect.max.y));
            p.rect_filled(bar, 2.0, fill);
        }
        for mark in [-23.0, -14.0] {
            let mx = x(mark);
            p.line_segment(
                [egui::pos2(mx, rect.top()), egui::pos2(mx, rect.bottom())],
                egui::Stroke::new(1.0, ui.visuals().text_color().gamma_multiply(0.6)),
            );
        }
        ui.label(
            egui::RichText::new(v.map_or("–".into(), |v| format!("{v:>5.1} LUFS"))).monospace(),
        );
    });
}
