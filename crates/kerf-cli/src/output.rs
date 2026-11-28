//! Printing results: readable text, or JSON.

use std::path::Path;

use kerf_core::ops::{Data, Request, Response};
use serde_json::{json, Value};

use crate::Outcome;

/// The `op` name of a request, as in JSON (`detect_songs`).
pub fn op_name(r: &Request) -> String {
    serde_json::to_value(r)
        .ok()
        .and_then(|v| v["op"].as_str().map(String::from))
        .unwrap_or_default()
}

/// `{"input": …, "ok": …, "steps": [{"op": …, "ok": …, "result"|"error": …}]}`
pub fn json(input: Option<&Path>, outcomes: &[Outcome]) -> Value {
    let steps: Vec<Value> = outcomes
        .iter()
        .map(|o| {
            let mut v = kerf_core::ops::envelope(&o.result);
            v["op"] = json!(op_name(&o.request));
            v
        })
        .collect();
    json!({
        "input": input,
        "ok": outcomes.iter().all(|o| o.result.is_ok()),
        "steps": steps,
    })
}

pub fn human(input: Option<&Path>, outcomes: &[Outcome], header: bool) {
    if header {
        if let Some(i) = input {
            println!("== {}", i.display());
        }
    }
    for o in outcomes {
        match &o.result {
            Ok(r) => print_response(r),
            Err(e) => eprintln!("error: {}: {e}", op_name(&o.request)),
        }
    }
    if header {
        println!();
    }
}

fn print_response(r: &Response) {
    println!("{}", r.summary);
    match &r.data {
        Data::Project(p) => {
            let a = &p.album;
            for (k, v) in [
                ("Artist", &a.artist),
                ("Album", &a.title),
                ("Year", &a.year),
                ("Genre", &a.genre),
            ] {
                if !v.is_empty() {
                    println!("  {k}: {v}");
                }
            }
            if let Some(g) = p.master_gain_db.filter(|g| g.abs() > 0.005) {
                println!("  Master gain: {g:+.2} dB");
            }
            for t in &p.tracks {
                let mut flags = Vec::new();
                if t.muted {
                    flags.push("muted");
                }
                if t.solo {
                    flags.push("solo");
                }
                println!(
                    "  Track {} \"{}\": {} clip(s){}",
                    t.id,
                    t.name,
                    t.clips.len(),
                    if flags.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", flags.join(", "))
                    }
                );
                for c in &t.clips {
                    println!(
                        "    clip {:>4}  {:>12} – {:<12} {}",
                        c.id, c.start.time, c.end.time, c.source
                    );
                }
            }
            if !p.markers.is_empty() {
                println!("  Markers:");
                for m in &p.markers {
                    println!(
                        "    {:>4}  {:>12}  {}{}",
                        m.id,
                        m.position.time,
                        m.name,
                        if m.auto { "  (detected)" } else { "" }
                    );
                }
            }
        }
        Data::Songs(s) => {
            println!("  Typical song level: {:.1} LUFS", s.reference_lufs);
            for (i, song) in s.songs.iter().enumerate() {
                println!(
                    "  {:>2}. {:>12}  {}{}",
                    i + 1,
                    song.position.time,
                    song.name,
                    song.confidence.map_or(String::new(), |c| format!(
                        "  (confidence {:.0}%)",
                        c * 100.0
                    ))
                );
            }
        }
        Data::Album(a) => {
            println!(
                "  {:>2}  {:<32} {:>12} {:>12} {:>10} {:>10}",
                "#", "Title", "Start", "Length", "LUFS", "Peak"
            );
            for t in &a.tracks {
                println!(
                    "  {:>2}  {:<32} {:>12} {:>12} {:>10} {:>10}  {}",
                    t.number,
                    t.title,
                    t.start.time,
                    t.length.time,
                    t.lufs.map_or("–".into(), |v| format!("{v:.1}")),
                    t.peak_db.map_or("–".into(), |v| format!("{v:.1}")),
                    t.flags.join(", ")
                );
            }
        }
        Data::Normalized { tracks } => {
            for t in tracks {
                println!(
                    "  Track {}: measured {}, gain {}",
                    t.track,
                    t.measured.map_or("–".into(), |v| format!("{v:.1}")),
                    t.gain_db
                        .map_or("skipped".into(), |v| format!("{v:+.2} dB"))
                );
            }
        }
        Data::Export(e) => {
            for f in &e.files {
                let span = match (&f.start, &f.length) {
                    (Some(s), Some(l)) => format!("{:>12} +{:<12}", s.time, l.time),
                    _ => format!("{:26}", ""),
                };
                println!(
                    "  {span} {}{}",
                    f.path.display(),
                    if f.exists && !e.written {
                        "  (exists)"
                    } else {
                        ""
                    }
                );
            }
            for w in &e.warnings {
                println!("  warning: {w}");
            }
        }
        Data::History(h) => {
            for (i, l) in h.undo.iter().enumerate() {
                println!("  {:>3}. {l}", i + 1);
            }
            if let Some(r) = &h.redo {
                println!("  next redo: {r}");
            }
        }
        Data::Silences(sil) => {
            for r in &sil.ranges {
                println!(
                    "  {:>12} – {:<12} ({})",
                    r.start.time, r.end.time, r.length.time
                );
            }
            if sil.truncated {
                println!("  …");
            }
        }
        Data::Speech(sp) => {
            for r in &sp.ranges {
                println!(
                    "  {:>12} – {:<12} ({})  confidence {:.2}",
                    r.start.time, r.end.time, r.length.time, r.confidence
                );
            }
        }
        Data::Waveform(w) => {
            for m in &w.markers {
                println!("  x = {:>4}  {:>12}  {}", m.x, m.position.time, m.name);
            }
            println!("  {}", w.png_base64);
        }
        Data::Spectrogram(sp) => {
            for m in &sp.markers {
                println!("  x = {:>4}  {:>12}  {}", m.x, m.position.time, m.name);
            }
            println!("  {}", sp.png_base64);
        }
        Data::Compare(c) => {
            let show = |v: Option<f64>| v.map_or("–".into(), |v| format!("{v:.1}"));
            println!("  a: {} ({})", c.a, c.length_a.time);
            println!("  b: {} ({})", c.b, c.length_b.time);
            if c.identical {
                return;
            }
            println!(
                "  difference: peak {} dBFS, RMS {} dBFS, null depth {} dB",
                show(c.peak_db),
                show(c.rms_db),
                show(c.null_depth_db)
            );
            for ch in &c.channels {
                println!(
                    "  channel {}: peak {} dBFS, {} samples differ",
                    ch.channel,
                    show(ch.peak_db),
                    ch.differing_samples
                );
            }
            const SHOWN: usize = 20;
            for r in c.regions.iter().take(SHOWN) {
                println!(
                    "  {:>12} – {:<12} ({}, peak {} dBFS)",
                    r.start.time,
                    r.end.time,
                    r.length.time,
                    show(r.peak_db)
                );
            }
            if c.region_count > SHOWN.min(c.regions.len()) {
                println!(
                    "  … {} more region(s)",
                    c.region_count - SHOWN.min(c.regions.len())
                );
            }
        }
        Data::None
        | Data::Levels(_)
        | Data::Track { .. }
        | Data::Marker { .. }
        | Data::Markers { .. }
        | Data::AlbumLoudness(_) => {}
    }
}
