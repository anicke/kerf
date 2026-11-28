//! Finding talk: presenters, announcements and band banter between songs.
//!
//! Speech without music has a rhythm music rarely has: syllables about four
//! to eight times a second, separated by short pauses where the voice band
//! drops by 15 dB or more, with harmonic (voiced) sound at the peaks. Music,
//! singing included, keeps the voice band filled: sustained notes, chords and
//! reverb leave only shallow dips, especially after broadcast compression.
//! Crowd noise fills the pauses evenly but is noise-like, not harmonic.
//!
//! Measured every 100 ms over a 2 s window of a 10 ms envelope of the voice
//! band (300–3400 Hz):
//!
//! * **depth**: how far the envelope drops between syllables (its 90th minus
//!   10th percentile, in dB);
//! * **syllabic modulation**: the share of the envelope's fluctuation at
//!   syllable rates (2.5–8 Hz), and how evenly spread it is (speech) rather
//!   than one sharp beat (music);
//! * **voicing**: spectral flatness at the envelope's peaks, low for voiced
//!   speech, high for crowd noise.

use realfft::RealFftPlanner;

use crate::analysis::Biquad;
use crate::cache::Progress;
use crate::detect::median_filter;
use crate::error::{Error, Result};
use crate::project::{Project, TrackId};
use crate::render::{self, RenderOptions};
use crate::Frame;

/// Resolution of the results.
pub const HOP_SECS: f64 = 0.1;
/// Envelope resolution.
const ENV_SECS: f64 = 0.01;
/// Each hop looks at this much audio around it.
const WINDOW_SECS: f64 = 2.0;
const VOICE_BAND: (f64, f64) = (300.0, 3400.0);

/// Measurements per hop (100 ms).
pub struct Features {
    pub start: Frame,
    pub end: Frame,
    pub hop: Frame,
    pub depth_db: Vec<f32>,
    pub syllabic: Vec<f32>,
    pub peakiness: Vec<f32>,
    pub peak_flatness: Vec<f32>,
    /// Voice-band level (dBFS mean square over the window).
    pub level_db: Vec<f32>,
}

/// Measure `start..end` of the given tracks' mix.
pub fn features(
    p: &Project,
    tracks: &[TrackId],
    start: Frame,
    end: Frame,
    progress: Progress,
) -> Result<Features> {
    if end <= start {
        return Err(Error::InvalidRange);
    }
    let sr = p.sample_rate.max(1) as f64;
    let channels = p.channels.max(1) as usize;
    let env_len = ((sr * ENV_SECS).round() as usize).max(1);
    let mut hp = Biquad::highpass(sr, VOICE_BAND.0);
    let mut lp = Biquad::lowpass(sr, VOICE_BAND.1);

    // Flatness of each 10 ms frame's neighbourhood, from an FFT of ~21 ms.
    let n = ((sr * 0.0213) as usize).next_power_of_two().max(256);
    let fft = RealFftPlanner::<f32>::new().plan_fft_forward(n);
    let mut fin = fft.make_input_vec();
    let mut fout = fft.make_output_vec();
    let mut scratch = fft.make_scratch_vec();
    let window: Vec<f32> = (0..n)
        .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / n as f32).cos())
        .collect();
    let bin = |hz: f64| ((hz * n as f64 / sr) as usize).clamp(1, n / 2);
    let (lo, hi) = (bin(200.0), bin(4000.0));
    let mut ring = vec![0f32; n];
    let mut ring_pos = 0usize;

    let mut env = Vec::new(); // voice-band mean square per 10 ms
    let mut flat = Vec::new(); // flatness per 10 ms
    let mut acc = 0f64;
    let mut in_env = 0usize;
    let total = (end - start) as f64;
    let mut done = 0usize;
    render::for_each_chunk(
        p,
        start,
        end,
        &RenderOptions::tracks(tracks.to_vec()),
        |chunk, frames| {
            for i in 0..frames {
                let m: f32 =
                    chunk.iter().take(channels).map(|c| c[i]).sum::<f32>() / channels as f32;
                let v = lp.process(hp.process(m as f64));
                acc += v * v;
                ring[ring_pos] = m;
                ring_pos = (ring_pos + 1) % n;
                in_env += 1;
                if in_env == env_len {
                    env.push(acc / env_len as f64);
                    acc = 0.0;
                    in_env = 0;
                    for (j, x) in fin.iter_mut().enumerate() {
                        *x = ring[(ring_pos + j) % n] * window[j];
                    }
                    let f = if fft
                        .process_with_scratch(&mut fin, &mut fout, &mut scratch)
                        .is_ok()
                    {
                        crate::detect::spectral_flatness(&fout[lo..hi])
                    } else {
                        1.0
                    };
                    flat.push(f);
                }
            }
            done += frames;
            Ok(progress(done as f64 / total))
        },
    )?;

    let db: Vec<f32> = env
        .iter()
        .map(|&e| (10.0 * (e + 1e-12).log10()) as f32)
        .collect();
    let per_hop = (HOP_SECS / ENV_SECS).round() as usize;
    let half = (WINDOW_SECS / ENV_SECS / 2.0).round() as usize;
    let hops = db.len().div_ceil(per_hop);

    // Modulation spectrum of a window (one DFT size for all windows).
    let wlen = 2 * half;
    let mfft = RealFftPlanner::<f32>::new().plan_fft_forward(wlen);
    let mut min = mfft.make_input_vec();
    let mut mout = mfft.make_output_vec();
    let mut mscratch = mfft.make_scratch_vec();
    let env_rate = 1.0 / ENV_SECS;
    let mbin = |hz: f64| ((hz * wlen as f64 / env_rate).round() as usize).min(wlen / 2);

    let mut out = Features {
        start,
        end,
        hop: (sr * HOP_SECS).round() as Frame,
        depth_db: Vec::with_capacity(hops),
        syllabic: Vec::with_capacity(hops),
        peakiness: Vec::with_capacity(hops),
        peak_flatness: Vec::with_capacity(hops),
        level_db: Vec::with_capacity(hops),
    };
    let mut sorted = Vec::with_capacity(wlen);
    for k in 0..hops {
        let c = k * per_hop + per_hop / 2;
        let a = c.saturating_sub(half);
        let b = (c + half).min(db.len());
        let w = &db[a..b];
        sorted.clear();
        sorted.extend_from_slice(w);
        sorted.sort_by(f32::total_cmp);
        let pct = |q: f32| sorted[((sorted.len() - 1) as f32 * q) as usize];
        let (p10, p50, p90) = (pct(0.1), pct(0.5), pct(0.9));
        out.depth_db.push(p90 - p10);
        let mean_e: f64 = env[a..b].iter().sum::<f64>() / w.len() as f64;
        out.level_db.push((10.0 * (mean_e + 1e-12).log10()) as f32);

        // Envelope fluctuation by rate, ignoring the window's mean.
        let mean = w.iter().sum::<f32>() / w.len() as f32;
        for (j, x) in min.iter_mut().enumerate() {
            let v = w.get(j).copied().unwrap_or(mean) - mean;
            let t = std::f32::consts::TAU * j as f32 / wlen as f32;
            *x = v * (0.5 - 0.5 * t.cos());
        }
        let _ = mfft.process_with_scratch(&mut min, &mut mout, &mut mscratch);
        let pw: Vec<f32> = mout.iter().map(|c| c.norm_sqr()).collect();
        let band = |lo: f64, hi: f64| pw[mbin(lo)..=mbin(hi)].iter().sum::<f32>();
        let all = band(0.5, 25.0) + 1e-9;
        out.syllabic.push(band(2.5, 8.0) / all);
        let rhythm = &pw[mbin(1.0)..=mbin(10.0)];
        let peak = rhythm.iter().copied().fold(0f32, f32::max);
        let avg = rhythm.iter().sum::<f32>() / rhythm.len() as f32 + 1e-9;
        out.peakiness.push(peak / avg);

        // Flatness where the voice band is loud (the syllables).
        let mut f: Vec<f32> = (a..b).filter(|&j| db[j] >= p50).map(|j| flat[j]).collect();
        f.sort_by(f32::total_cmp);
        out.peak_flatness
            .push(f.get(f.len() / 2).copied().unwrap_or(1.0));
    }
    Ok(out)
}

/// A stretch of talk.
#[derive(Clone, Debug, PartialEq)]
pub struct SpeechRange {
    pub start: Frame,
    pub end: Frame,
    /// How clearly it is speech, 0.5..1.
    pub confidence: f32,
}

/// Pauses shorter than this join two stretches of talk into one.
const JOIN_SECS: f64 = 1.0;
/// Shorter stretches are not reported.
pub const MIN_SPEECH_SECS: f64 = 1.5;

/// How much each hop sounds like talk, 0..1; over 0.5 counts.
///
/// Both cues must hold: deep dips between syllables (over 11 dB in the
/// voice band) *and* voiced, harmonic peaks (flatness under 0.12), each
/// taken as the median over 2 s. Tuned on a 53-minute festival broadcast
/// against a transcript: it found the presenter (over crowd noise and over
/// the band's intro) and the singer's talk between songs, and flagged 0.2% of
/// the music.
pub fn scores(f: &Features) -> Vec<f32> {
    let half = (1.0 / HOP_SECS) as usize;
    let depth = median_filter(&f.depth_db, half);
    let flat = median_filter(&f.peak_flatness, half);
    depth
        .iter()
        .zip(&flat)
        .map(|(&d, &pf)| {
            let deep = ((d - 8.0) / 6.0).clamp(0.0, 1.0);
            let voiced = ((0.16 - pf) / 0.08).clamp(0.0, 1.0);
            deep.min(voiced)
        })
        .collect()
}

/// Stretches of talk, in order.
pub fn find(f: &Features, min_secs: f64) -> Vec<SpeechRange> {
    let s = scores(f);
    let frame = |k: usize| (f.start + k as Frame * f.hop).min(f.end);
    let join = (JOIN_SECS / HOP_SECS).round() as usize;
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut k = 0;
    while k < s.len() {
        if s[k] > 0.5 {
            let a = k;
            while k < s.len() && s[k] > 0.5 {
                k += 1;
            }
            match runs.last_mut() {
                Some(last) if a - last.1 < join => last.1 = k,
                _ => runs.push((a, k)),
            }
        } else {
            k += 1;
        }
    }
    let min_hops = (min_secs / HOP_SECS).round().max(1.0) as usize;
    runs.into_iter()
        .filter(|(a, b)| b - a >= min_hops)
        .map(|(a, b)| {
            let inside: Vec<f32> = s[a..b].iter().copied().filter(|&v| v > 0.5).collect();
            SpeechRange {
                start: frame(a),
                end: frame(b),
                confidence: inside.iter().sum::<f32>() / inside.len().max(1) as f32,
            }
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::AudioBuffer;

    pub(crate) const SR: u32 = 16_000;

    struct Noise(u64);
    impl Noise {
        fn next(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 40) as f32 / (1u64 << 23) as f32 - 1.0
        }
    }

    /// Talk-like sound: voiced syllables of 120–260 ms with gliding pitch
    /// and formant-like harmonics, short pauses between them and a longer
    /// one every few syllables.
    pub(crate) fn talk(secs: f64, amp: f32, seed: u64) -> Vec<f32> {
        let mut rng = Noise(seed);
        let n = (secs * SR as f64) as usize;
        let mut out = vec![0f32; n];
        let mut i = 0usize;
        let mut syll = 0;
        let mut phase = 0f32;
        while i < n {
            let len = ((0.12 + 0.07 * (rng.next() as f64 + 1.0)) * SR as f64) as usize;
            let f0a = 110.0 + 40.0 * (rng.next() + 1.0);
            let f0b = f0a * (1.0 + 0.25 * rng.next());
            for j in 0..len.min(n - i) {
                let t = j as f32 / len as f32;
                let f0 = f0a + (f0b - f0a) * t;
                phase += std::f32::consts::TAU * f0 / SR as f32;
                let env = (std::f32::consts::PI * t).sin();
                let mut v = 0.0;
                for h in 1..=20 {
                    let hz = f0 * h as f32;
                    // Two formant bumps (~500 Hz and ~1500 Hz).
                    let w = (-((hz - 500.0) / 250.0).powi(2)).exp()
                        + 0.6 * (-((hz - 1500.0) / 400.0).powi(2)).exp()
                        + 0.05;
                    v += w * (phase * h as f32).sin();
                }
                out[i + j] = v * env * amp * 0.3;
            }
            i += len;
            syll += 1;
            let pause = if syll % 5 == 0 { 0.35 } else { 0.06 };
            i += (pause * SR as f64) as usize;
        }
        out
    }

    /// Music-like sound: a sustained chord with a pulse, and sung notes held
    /// for half a second each on top.
    fn music(secs: f64, amp: f32) -> Vec<f32> {
        let n = (secs * SR as f64) as usize;
        (0..n)
            .map(|i| {
                let t = i as f32 / SR as f32;
                let pulse = 0.7 + 0.3 * (std::f32::consts::TAU * 2.0 * t).sin().abs();
                let note = [220.0, 247.0, 262.0, 294.0][(t * 2.0) as usize % 4];
                let mut v = 0.0;
                for (h, f) in [110.0f32, 138.6, 165.0].iter().enumerate() {
                    for k in 1..=6 {
                        v +=
                            (std::f32::consts::TAU * f * k as f32 * t).sin() / (k * (h + 1)) as f32;
                    }
                }
                for k in 1..=8 {
                    v += 0.8 * (std::f32::consts::TAU * note * k as f32 * t).sin() / k as f32;
                }
                v * amp * pulse * 0.15
            })
            .collect()
    }

    fn project(data: Vec<f32>) -> Project {
        let mut p = Project::new(SR, 1);
        p.add_source_as_track("t", None, AudioBuffer::new(SR, vec![data]), 0);
        p
    }

    fn found(data: Vec<f32>) -> Vec<(f64, f64)> {
        let p = project(data);
        let f = features(&p, &p.all_track_ids(), 0, p.length(), &mut |_| true).unwrap();
        find(&f, MIN_SPEECH_SECS)
            .iter()
            .map(|r| (r.start as f64 / SR as f64, r.end as f64 / SR as f64))
            .collect()
    }

    #[test]
    fn talk_between_music_is_found() {
        // 20 s music, 10 s talk, 20 s music.
        let mut a = music(20.0, 0.8);
        a.extend(talk(10.0, 0.8, 7));
        a.extend(music(20.0, 0.8));
        let r = found(a);
        assert_eq!(r.len(), 1, "{r:?}");
        let (s, e) = r[0];
        assert!(
            (19.0..=21.5).contains(&s) && (28.5..=31.0).contains(&e),
            "{r:?}"
        );
    }

    #[test]
    fn talk_over_crowd_noise_is_found_and_crowd_alone_is_not() {
        let mut rng = Noise(3);
        let crowd = |secs: f64, rng: &mut Noise| -> Vec<f32> {
            (0..(secs * SR as f64) as usize)
                .map(|_| rng.next() * 0.03)
                .collect()
        };
        let mut a = crowd(10.0, &mut rng);
        let t = talk(10.0, 0.8, 11);
        let c = crowd(10.0, &mut rng);
        a.extend(t.iter().zip(&c).map(|(x, y)| x + y));
        a.extend(crowd(10.0, &mut rng));
        let r = found(a);
        assert_eq!(r.len(), 1, "{r:?}");
        assert!(r[0].0 >= 9.0 && r[0].1 <= 21.0, "{r:?}");
    }

    #[test]
    fn music_alone_is_not_talk() {
        assert!(found(music(60.0, 0.8)).is_empty());
        assert!(found(vec![0.0; 10 * SR as usize]).is_empty(), "silence");
    }
}
