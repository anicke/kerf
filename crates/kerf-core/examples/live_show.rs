//! Generate a synthetic live concert to try song-start detection on:
//! `cargo run --release -p kerf-core --example live_show -- <out.wav>`
//!
//! Six "songs" (pitched chords with a pulse and a quiet breakdown in one),
//! separated by applause of varying loudness, one gap followed by a spoken
//! introduction, with crowd noise before and after.

use std::f32::consts::TAU;
use std::path::PathBuf;

use kerf_core::io::{write_wav, ExportSettings};
use kerf_core::AudioBuffer;

const SR: u32 = 44_100;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 40) as f32 / (1u64 << 23) as f32 - 1.0
    }
}

fn song(
    l: &mut Vec<f32>,
    r: &mut Vec<f32>,
    secs: f32,
    root: f32,
    amp: f32,
    breakdown_at: Option<f32>,
) {
    let n = (secs * SR as f32) as usize;
    for i in 0..n {
        let t = i as f32 / SR as f32;
        let a = match breakdown_at {
            Some(b) if t > b && t < b + 10.0 => amp * 0.08,
            _ => amp,
        };
        let pulse = 0.65 + 0.35 * (TAU * 2.1 * t).sin().abs();
        let chord = [1.0, 1.26, 1.5, 2.0];
        let mut v = 0.0;
        for (j, ratio) in chord.iter().enumerate() {
            for h in 1..=5 {
                v += (TAU * root * ratio * h as f32 * t + j as f32).sin() / (h * (j + 1)) as f32;
            }
        }
        let fade = (t / 0.05).min(1.0) * ((secs - t) / 0.3).clamp(0.0, 1.0);
        l.push(v * a * pulse * fade * 0.2);
        r.push(v * a * pulse * fade * 0.18);
    }
}

fn crowd(l: &mut Vec<f32>, r: &mut Vec<f32>, secs: f32, amp: f32, rng: &mut Rng) {
    let n = (secs * SR as f32) as usize;
    let (mut lp_l, mut lp_r) = (0.0f32, 0.0f32);
    for i in 0..n {
        let t = i as f32 / SR as f32;
        let swell = (t / 1.0).min(1.0) * ((secs - t) / 1.5).clamp(0.0, 1.0);
        // Gently low-passed noise, a bit like a room full of clapping.
        lp_l += 0.5 * (rng.next() - lp_l);
        lp_r += 0.5 * (rng.next() - lp_r);
        l.push(lp_l * amp * swell);
        r.push(lp_r * amp * swell);
    }
}

fn main() -> kerf_core::Result<()> {
    let out: PathBuf = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "live_show.wav".into())
        .into();
    let mut rng = Rng(0x00C0_FFEE);
    let (mut l, mut r) = (Vec::new(), Vec::new());
    crowd(&mut l, &mut r, 6.0, 0.05, &mut rng);
    song(&mut l, &mut r, 95.0, 220.0, 0.6, None);
    crowd(&mut l, &mut r, 10.0, 0.25, &mut rng);
    song(&mut l, &mut r, 120.0, 196.0, 0.6, Some(55.0));
    crowd(&mut l, &mut r, 5.0, 0.12, &mut rng);
    song(&mut l, &mut r, 6.0, 140.0, 0.03, None); // "thank you, this next one…"
    song(&mut l, &mut r, 80.0, 262.0, 0.6, None);
    crowd(&mut l, &mut r, 7.0, 0.2, &mut rng);
    song(&mut l, &mut r, 110.0, 247.0, 0.55, None);
    crowd(&mut l, &mut r, 4.0, 0.1, &mut rng);
    song(&mut l, &mut r, 70.0, 175.0, 0.6, None);
    crowd(&mut l, &mut r, 12.0, 0.3, &mut rng);
    song(&mut l, &mut r, 100.0, 233.0, 0.6, None);
    crowd(&mut l, &mut r, 15.0, 0.25, &mut rng);
    write_wav(
        &out,
        &AudioBuffer::new(SR, vec![l, r]),
        ExportSettings::default(),
    )?;
    println!("wrote {}", out.display());
    Ok(())
}
