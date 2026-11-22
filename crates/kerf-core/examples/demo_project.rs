//! Generate a demo project exercising most features:
//! `cargo run -p kerf-core --example demo_project -- <out-dir>`
//!
//! Writes `demo.wav` (32-bit float source) and `demo.kerf` (a project with
//! splits, a moved clip, fades, normalization, markers and a second track).

use std::f32::consts::TAU;
use std::path::PathBuf;

use kerf_core::edit::{self, NormalizeMode, NormalizeSettings};
use kerf_core::io::{write_wav, ExportSettings};
use kerf_core::{session, AudioBuffer, FadeShape, Project};

fn main() -> kerf_core::Result<()> {
    let dir: PathBuf = std::env::args().nth(1).unwrap_or_else(|| ".".into()).into();
    std::fs::create_dir_all(&dir)?;
    let sr = 48_000u32;

    // Three "songs" separated by silence, the last one too hot (peaks > 0 dBFS).
    let mut l = Vec::new();
    let mut r = Vec::new();
    for (song, (freq, amp)) in [(220.0f32, 0.4f32), (330.0, 0.25), (440.0, 1.4)]
        .iter()
        .enumerate()
    {
        let n = sr as usize * 4;
        for i in 0..n {
            let t = i as f32 / sr as f32;
            let env = (t * 3.0).sin().abs() * 0.7 + 0.3;
            let v = (TAU * freq * t).sin() * 0.6 + (TAU * freq * 2.01 * t).sin() * 0.3;
            l.push(v * amp * env);
            r.push((TAU * freq * 1.5 * t).sin() * amp * env * 0.8);
        }
        if song < 2 {
            l.extend(std::iter::repeat_n(0.0, sr as usize));
            r.extend(std::iter::repeat_n(0.0, sr as usize));
        }
    }
    let wav = dir.join("demo.wav");
    write_wav(
        &wav,
        &AudioBuffer::new(sr, vec![l, r]),
        ExportSettings::default(),
    )?;

    let mut p = Project::new(sr, 2);
    let t1 = session::import_file(&mut p, &wav, 0)?;
    let s = |secs: f64| (secs * sr as f64) as u64;

    edit::split_at(&mut p, &[t1], s(5.0))?;
    edit::split_at(&mut p, &[t1], s(10.0))?;
    edit::fade_in(&mut p, &[t1], 0, s(1.5), FadeShape::SCurve)?;
    edit::fade_out(&mut p, &[t1], s(12.0), s(14.0), FadeShape::EqualPower)?;
    edit::normalize(
        &mut p,
        &[t1],
        Some((s(10.0), s(14.0))),
        &NormalizeSettings {
            mode: NormalizeMode::Peak,
            target_db: -1.0,
            ..Default::default()
        },
    )?;
    // Slide the second song left so it overlaps the first: automatic crossfade.
    let second = p.track(t1).unwrap().clips[2].id;
    edit::move_clip(&mut p, second, t1, s(4.5))?;
    p.add_marker(0, "Intro");
    p.add_marker(s(5.0), "Second song");
    p.add_marker(s(10.0), "Finale");

    // A second track: copy of the first song, moved later and turned down.
    let cb = edit::copy(&p, &[t1], 0, s(4.0))?;
    let t2 = p.add_track("Overdub");
    edit::paste(&mut p, &cb, &[t2], s(6.0), edit::PasteMode::Mix)?;
    edit::apply_gain(&mut p, &[t2], None, -9.0)?;
    p.track_mut(t2).unwrap().pan = -0.4;
    // A clip-edge fade-out, as set by dragging the handle.
    let last = p.track(t2).unwrap().clips.last().unwrap().id;
    edit::set_clip_fade_out(&mut p, last, s(1.0), FadeShape::Linear)?;

    session::save(&p, &dir.join("demo.kerf"))?;
    println!("wrote {}", dir.join("demo.kerf").display());
    Ok(())
}
