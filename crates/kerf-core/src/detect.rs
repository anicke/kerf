//! Finding where songs start in recordings of live music.
//!
//! Live recordings have no real silence between songs: the gaps hold
//! applause, crowd noise, tuning and talking. So instead of looking for
//! silence, Kerf looks for where the *music* stops and starts, using two
//! measurements taken every 100 ms:
//!
//! * **Loudness** (ITU-R BS.1770 K-weighted), compared with the recording's
//!   own typical song level, so quiet and loud shows work alike without a
//!   fixed threshold.
//! * **Spectral flatness**: music is pitched and structured (a peaky
//!   spectrum), applause and crowd noise are close to broadband noise (a flat
//!   spectrum). This separates a loud ovation from the next song.
//!
//! Spoken introductions count as part of the next song (the usual live-album
//! convention): quiet speech is pitched sound, not crowd noise.
//!
//! [`analyze`] does the expensive pass once; [`find_boundaries`] then runs in
//! microseconds, so settings can be adjusted with a live preview.

use realfft::RealFftPlanner;

use crate::analysis::k_weighting;
use crate::cache::Progress;
use crate::error::{Error, Result};
use crate::project::{Project, TrackId};
use crate::render::{self, RenderOptions};
use crate::Frame;

/// Analysis resolution.
pub const HOP_SECS: f64 = 0.1;
const FFT_LEN: usize = 2048;
/// Spectral flatness is measured in this band (Hz), where music is tonal and
/// crowd noise is broadband; rumble and hiss outside it are ignored.
const FLATNESS_BAND: (f64, f64) = (200.0, 6000.0);
/// Below this level a hop counts as silent.
const FLOOR_LUFS: f32 = -100.0;

/// Measurements of a range, one value per hop (100 ms).
pub struct Analysis {
    pub start: Frame,
    pub end: Frame,
    /// Frames per hop.
    pub hop: Frame,
    /// Loudness over 400 ms, centred on each hop (LUFS). Locates boundaries precisely.
    pub momentary: Vec<f32>,
    /// Loudness over 3 s, centred on each hop (LUFS). Finds gaps robustly.
    pub short_term: Vec<f32>,
    /// Spectral flatness per hop, 0 (pure tone) to 1 (white noise).
    pub flatness: Vec<f32>,
    /// Flatness averaged over 1 s.
    pub flatness_smooth: Vec<f32>,
    /// Typical loudness of the songs: the 80th percentile of short-term loudness.
    pub reference: f32,
}

impl Analysis {
    pub fn len(&self) -> usize {
        self.short_term.len()
    }

    pub fn is_empty(&self) -> bool {
        self.short_term.is_empty()
    }

    /// Timeline frame at the start of hop `k`.
    pub fn frame_of(&self, k: usize) -> Frame {
        (self.start + k as Frame * self.hop).min(self.end)
    }
}

/// Measure `start..end` of the given tracks (as heard: post-fader, mute respected).
#[allow(clippy::needless_range_loop)] // one frame across all channels per step
pub fn analyze(
    p: &Project,
    tracks: &[TrackId],
    start: Frame,
    end: Frame,
    progress: Progress,
) -> Result<Analysis> {
    if end <= start {
        return Err(Error::InvalidRange);
    }
    let sr = p.sample_rate;
    let channels = p.channels as usize;
    let hop = ((sr as f64 * HOP_SECS).round() as usize).max(1);

    let mut filters: Vec<_> = (0..channels).map(|_| k_weighting(sr as f64)).collect();
    let mut energy = Vec::new(); // K-weighted mean square per hop
    let mut flatness = Vec::new();
    let mut acc = 0.0f64;
    let mut in_hop = 0usize;

    // Mono ring buffer feeding the FFT, plus scratch.
    let mut ring = vec![0.0f32; FFT_LEN];
    let mut ring_pos = 0usize;
    let fft = RealFftPlanner::<f32>::new().plan_fft_forward(FFT_LEN);
    let mut fft_in = fft.make_input_vec();
    let mut fft_out = fft.make_output_vec();
    let mut fft_scratch = fft.make_scratch_vec();
    let window: Vec<f32> = (0..FFT_LEN)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / FFT_LEN as f32).cos())
        .collect();
    let bin = |hz: f64| (hz * FFT_LEN as f64 / sr as f64) as usize;
    let lo = bin(FLATNESS_BAND.0).max(1);
    let hi = bin(FLATNESS_BAND.1).min(FFT_LEN / 2).max(lo + 1);

    let total = (end - start) as f64;
    let mut done = 0usize;
    render::for_each_chunk(
        p,
        start,
        end,
        &RenderOptions::tracks(tracks.to_vec()),
        |chunk, n| {
            for i in 0..n {
                let mut mono = 0.0f32;
                for (c, f) in filters.iter_mut().enumerate() {
                    let s = chunk[c][i];
                    mono += s;
                    let [shelf, highpass] = f;
                    let k = highpass.process(shelf.process(s as f64));
                    acc += k * k;
                }
                ring[ring_pos] = mono / channels as f32;
                ring_pos = (ring_pos + 1) % FFT_LEN;
                in_hop += 1;
                if in_hop == hop {
                    energy.push(acc / hop as f64);
                    acc = 0.0;
                    in_hop = 0;
                    // FFT of the most recent FFT_LEN samples, oldest first.
                    for (j, x) in fft_in.iter_mut().enumerate() {
                        *x = ring[(ring_pos + j) % FFT_LEN] * window[j];
                    }
                    let _ = fft.process_with_scratch(&mut fft_in, &mut fft_out, &mut fft_scratch);
                    flatness.push(spectral_flatness(&fft_out[lo..hi]));
                }
            }
            done += n;
            Ok(progress(done as f64 / total))
        },
    )?;

    let loud = |z: f64| {
        if z > 1e-10 {
            (-0.691 + 10.0 * z.log10()) as f32
        } else {
            FLOOR_LUFS
        }
    };
    let momentary: Vec<f32> = moving_average(&energy, 2, 1)
        .into_iter()
        .map(loud)
        .collect();
    let short_term: Vec<f32> = moving_average(&energy, 15, 14)
        .into_iter()
        .map(loud)
        .collect();
    let flat64: Vec<f64> = flatness.iter().map(|&f| f as f64).collect();
    let flatness_smooth = moving_average(&flat64, 5, 4)
        .into_iter()
        .map(|f| f as f32)
        .collect();

    let mut audible: Vec<f32> = short_term.iter().copied().filter(|&l| l > -70.0).collect();
    audible.sort_by(f32::total_cmp);
    let reference = audible
        .get(audible.len() * 4 / 5)
        .copied()
        .unwrap_or(FLOOR_LUFS);

    Ok(Analysis {
        start,
        end,
        hop: hop as Frame,
        momentary,
        short_term,
        flatness,
        flatness_smooth,
        reference,
    })
}

/// Geometric over arithmetic mean of the power spectrum (silence counts as flat).
fn spectral_flatness(bins: &[realfft::num_complex::Complex<f32>]) -> f32 {
    let n = bins.len() as f64;
    let (mut log_sum, mut sum) = (0.0f64, 0.0f64);
    for b in bins {
        let p = (b.re as f64).powi(2) + (b.im as f64).powi(2) + 1e-20;
        log_sum += p.ln();
        sum += p;
    }
    let mean = sum / n;
    if mean < 1e-14 {
        return 1.0;
    }
    ((log_sum / n).exp() / mean).clamp(0.0, 1.0) as f32
}

/// Centred moving average over `before` values before and `after` after each index.
fn moving_average(v: &[f64], before: usize, after: usize) -> Vec<f64> {
    let mut prefix = Vec::with_capacity(v.len() + 1);
    prefix.push(0.0);
    for x in v {
        prefix.push(prefix.last().unwrap() + x);
    }
    (0..v.len())
        .map(|k| {
            let a = k.saturating_sub(before);
            let b = (k + after + 1).min(v.len());
            (prefix[b] - prefix[a]) / (b - a) as f64
        })
        .collect()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Placement {
    /// Where the music resumes, minus the pre-roll (live-album convention:
    /// applause stays with the previous track).
    #[default]
    MusicStart,
    /// The quietest moment of the gap.
    QuietestPoint,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DetectSettings {
    /// If set, place exactly this many songs (the strongest gaps win).
    pub expected_songs: Option<usize>,
    /// 0..1: how readily a gap counts as a song change (ignored with `expected_songs`).
    pub sensitivity: f32,
    pub min_song_secs: f64,
    pub min_gap_secs: f64,
    /// Place the marker this much before the music starts.
    pub pre_roll_secs: f64,
    pub placement: Placement,
    /// Also put a marker at the start of the range (names the first track).
    pub marker_at_start: bool,
}

impl Default for DetectSettings {
    fn default() -> Self {
        Self {
            expected_songs: None,
            sensitivity: 0.5,
            min_song_secs: 60.0,
            min_gap_secs: 2.0,
            pre_roll_secs: 0.5,
            placement: Placement::MusicStart,
            marker_at_start: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Boundary {
    /// Where the next song's marker goes.
    pub position: Frame,
    /// How clearly this is a song change, 0..1.
    pub strength: f32,
    /// The gap between songs.
    pub gap_start: Frame,
    pub gap_end: Frame,
}

/// How much a moment sounds like "between songs", 0..1.
///
/// Noise-like sound (applause, crowd) is enough on its own, however loud: an
/// ovation can match the songs' level. Quietness only adds to it and can never
/// reach the threshold alone, so a soft passage *inside* a song (a breakdown,
/// a sparse intro) is not mistaken for a gap unless it is also noise-like.
fn gapness(reference: f32, loudness: f32, flatness: f32) -> f32 {
    let noisy = ((flatness - 0.08) / 0.22).clamp(0.0, 1.0);
    let quiet = ((reference - loudness - 6.0) / 18.0).clamp(0.0, 1.0);
    (noisy + 0.45 * quiet).min(1.0)
}

const GAP: f32 = 0.5;

/// Song boundaries (sorted by position), not including the range start.
pub fn find_boundaries(a: &Analysis, s: &DetectSettings) -> Vec<Boundary> {
    let n = a.len();
    if n == 0 || a.reference <= FLOOR_LUFS {
        return Vec::new();
    }
    let hops = |secs: f64| (secs / HOP_SECS).round() as usize;
    let g: Vec<f32> = (0..n)
        .map(|k| gapness(a.reference, a.short_term[k], a.flatness_smooth[k]))
        .collect();

    // Runs of gap hops, merging runs split by brief blips (a shout, a chord).
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut k = 0;
    while k < n {
        if g[k] > GAP {
            let s0 = k;
            while k < n && g[k] > GAP {
                k += 1;
            }
            match runs.last_mut() {
                Some(last) if s0 - last.1 < hops(1.0) => last.1 = k,
                _ => runs.push((s0, k)),
            }
        } else {
            k += 1;
        }
    }

    // Gaps touching the range edges are intros/outros, not song changes.
    let mut candidates: Vec<Boundary> = runs
        .into_iter()
        .filter(|&(s0, s1)| s1 - s0 >= hops(s.min_gap_secs).max(1) && s0 > 0 && s1 < n)
        .map(|(s0, s1)| {
            let mean = g[s0..s1].iter().sum::<f32>() / (s1 - s0) as f32;
            let duration = ((s1 - s0) as f64 * HOP_SECS) as f32;
            let strength = mean * (0.5 + 0.5 * (duration / 6.0).min(1.0));
            Boundary {
                position: place(a, s0, s1, s),
                strength,
                gap_start: a.frame_of(s0),
                gap_end: a.frame_of(s1),
            }
        })
        .collect();

    // Strongest first; keep songs at least `min_song_secs` long.
    candidates.sort_by(|x, y| y.strength.total_cmp(&x.strength));
    let min_song = (s.min_song_secs * a.hop as f64 / HOP_SECS) as Frame;
    let min_strength = 0.9 - 0.4 * s.sensitivity.clamp(0.0, 1.0);
    let wanted = s.expected_songs.map(|n| n.saturating_sub(1));
    let mut chosen: Vec<Boundary> = Vec::new();
    for c in candidates {
        if wanted.is_some_and(|w| chosen.len() >= w) {
            break;
        }
        if wanted.is_none() && c.strength < min_strength {
            break;
        }
        let fits = c.position >= a.start + min_song
            && c.position + min_song <= a.end
            && chosen
                .iter()
                .all(|b| b.position.abs_diff(c.position) >= min_song);
        if fits {
            chosen.push(c);
        }
    }
    chosen.sort_by_key(|b| b.position);
    chosen
}

/// Marker position for the gap `s0..s1` (in hops).
fn place(a: &Analysis, s0: usize, s1: usize, s: &DetectSettings) -> Frame {
    let n = a.len();
    let gap_start = a.frame_of(s0);
    match s.placement {
        Placement::QuietestPoint => {
            let k = (s0..s1)
                .min_by(|&x, &y| a.momentary[x].total_cmp(&a.momentary[y]))
                .unwrap_or(s0);
            a.frame_of(k)
        }
        Placement::MusicStart => {
            // The 3 s gap measure smears edges; find the precise onset with the
            // 400 ms measure: the first moment music holds for a full second.
            let hold = 10;
            let from = s1.saturating_sub(30).max(s0);
            let to = (s1 + 30).min(n);
            let music = |k: usize| gapness(a.reference, a.momentary[k], a.flatness[k]) <= GAP;
            let onset = (from..to)
                .find(|&k| (k..(k + hold).min(n)).all(music))
                .unwrap_or(s1);
            let pre_roll = (s.pre_roll_secs * a.hop as f64 / HOP_SECS) as Frame;
            a.frame_of(onset).saturating_sub(pre_roll).max(gap_start)
        }
    }
}

/// Marker names from a pasted setlist (one title per line; leading numbering
/// like "1.", "01 -" or "3)" is removed), padded with "Track NN".
pub fn marker_names(setlist: &str, count: usize) -> Vec<String> {
    let titles: Vec<String> = setlist
        .lines()
        .map(|l| {
            let t = l.trim();
            let rest = t.trim_start_matches(|c: char| c.is_ascii_digit());
            if rest.len() < t.len() {
                let rest = rest.trim_start_matches(['.', ')', '-', ':', ' ', '\t']);
                if !rest.is_empty() {
                    return rest.trim().to_string();
                }
            }
            t.to_string()
        })
        .filter(|t| !t.is_empty())
        .collect();
    (0..count)
        .map(|i| {
            titles
                .get(i)
                .cloned()
                .unwrap_or_else(|| format!("Track {:02}", i + 1))
        })
        .collect()
}

/// Replace earlier automatic markers in `start..=end` with markers at
/// `positions` named `names`. Markers you placed yourself are kept.
pub fn apply_markers(
    p: &mut Project,
    start: Frame,
    end: Frame,
    positions: &[Frame],
    names: &[String],
) -> Result<()> {
    p.markers
        .retain(|m| !(m.auto && m.position >= start && m.position <= end));
    for (i, &pos) in positions.iter().enumerate() {
        let name = names
            .get(i)
            .cloned()
            .unwrap_or_else(|| format!("Track {:02}", i + 1));
        p.add_auto_marker(pos, name);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AudioBuffer;

    const SR: u32 = 16_000;

    struct Noise(u64);
    impl Noise {
        fn next(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 40) as f32 / (1u64 << 23) as f32 - 1.0
        }
    }

    /// Pitched, harmonic "music" with a slow pulse.
    fn song(out: &mut Vec<f32>, secs: f64, root: f32, amp: f32) {
        let n = (secs * SR as f64) as usize;
        for i in 0..n {
            let t = i as f32 / SR as f32;
            let pulse = 0.7 + 0.3 * (2.0 * std::f32::consts::PI * 2.0 * t).sin().abs();
            let mut v = 0.0;
            for (h, f) in [root, root * 1.26, root * 1.5].iter().enumerate() {
                for k in 1..=4 {
                    v += (2.0 * std::f32::consts::PI * f * k as f32 * t).sin()
                        / (k * (h + 1)) as f32;
                }
            }
            out.push(v * amp * pulse * 0.25);
        }
    }

    fn applause(out: &mut Vec<f32>, secs: f64, amp: f32, rng: &mut Noise) {
        for _ in 0..(secs * SR as f64) as usize {
            out.push(rng.next() * amp);
        }
    }

    /// Four songs; gaps of applause (one followed by a spoken introduction);
    /// a quiet breakdown inside song 2 that must *not* become a boundary.
    fn live_show() -> (Project, Vec<f64>) {
        let mut rng = Noise(0x1234_5678);
        let mut a = Vec::new();
        let mut starts = Vec::new();
        let secs = |v: &Vec<f32>| v.len() as f64 / SR as f64;
        applause(&mut a, 4.0, 0.05, &mut rng); // crowd before the show
        starts.push(secs(&a));
        song(&mut a, 90.0, 220.0, 0.5);
        applause(&mut a, 9.0, 0.12, &mut rng); // loud ovation
        starts.push(secs(&a));
        song(&mut a, 50.0, 196.0, 0.5);
        song(&mut a, 8.0, 196.0, 0.04); // quiet breakdown, still tonal
        song(&mut a, 60.0, 196.0, 0.5);
        applause(&mut a, 4.0, 0.08, &mut rng);
        // Band member introducing the next song: belongs to that song's track.
        starts.push(secs(&a));
        song(&mut a, 5.0, 140.0, 0.015);
        song(&mut a, 75.0, 262.0, 0.5);
        applause(&mut a, 6.0, 0.1, &mut rng);
        starts.push(secs(&a));
        song(&mut a, 100.0, 247.0, 0.5);
        applause(&mut a, 10.0, 0.1, &mut rng); // final applause
        let mut p = Project::new(SR, 1);
        p.add_source_as_track("show", None, AudioBuffer::new(SR, vec![a]), 0);
        (p, starts)
    }

    fn analyse(p: &Project) -> Analysis {
        analyze(p, &p.all_track_ids(), 0, p.length(), &mut |_| true).unwrap()
    }

    fn assert_near(found: &[Boundary], expected_secs: &[f64], tolerance: f64) {
        let got: Vec<f64> = found
            .iter()
            .map(|b| b.position as f64 / SR as f64)
            .collect();
        assert_eq!(
            got.len(),
            expected_secs.len(),
            "found {got:?}, expected {expected_secs:?}"
        );
        for (g, e) in got.iter().zip(expected_secs) {
            assert!(
                (g - e).abs() <= tolerance,
                "found {got:?}, expected {expected_secs:?}"
            );
        }
    }

    #[test]
    fn finds_song_starts_in_a_live_show() {
        let (p, starts) = live_show();
        let a = analyse(&p);
        assert!(
            a.reference > -30.0 && a.reference < -5.0,
            "reference {}",
            a.reference
        );
        let s = DetectSettings::default();
        let found = find_boundaries(&a, &s);
        // Markers land half a second (the pre-roll) before each song after the first.
        let expected: Vec<f64> = starts[1..].iter().map(|t| t - s.pre_roll_secs).collect();
        assert_near(&found, &expected, 0.6);
    }

    #[test]
    fn expected_song_count_picks_the_strongest_gaps() {
        let (p, starts) = live_show();
        let a = analyse(&p);
        // Even at the lowest sensitivity, the count decides.
        let s = DetectSettings {
            expected_songs: Some(4),
            sensitivity: 0.0,
            ..Default::default()
        };
        let expected: Vec<f64> = starts[1..].iter().map(|t| t - 0.5).collect();
        assert_near(&find_boundaries(&a, &s), &expected, 0.6);
        // Asking for 2 songs keeps the single strongest gap.
        let two = find_boundaries(
            &a,
            &DetectSettings {
                expected_songs: Some(2),
                ..Default::default()
            },
        );
        assert_eq!(two.len(), 1);
    }

    #[test]
    fn minimum_song_length_and_placement() {
        let (p, starts) = live_show();
        let a = analyse(&p);
        // Songs must be 100 s: gaps closer than that to each other or the edges are dropped.
        let long = find_boundaries(
            &a,
            &DetectSettings {
                min_song_secs: 100.0,
                ..Default::default()
            },
        );
        assert!(long.len() < 3);
        for w in long.windows(2) {
            assert!(w[1].position - w[0].position >= 100 * SR as u64);
        }
        // Quietest-point placement lands inside each gap, before the song's music
        // (with a spoken intro, the quietest moment can be during the talking).
        let quiet = find_boundaries(
            &a,
            &DetectSettings {
                placement: Placement::QuietestPoint,
                ..Default::default()
            },
        );
        assert_eq!(quiet.len(), 3);
        for (b, start) in quiet.iter().zip(&starts[1..]) {
            assert!(b.position >= b.gap_start && b.position <= b.gap_end);
            assert!((b.position as f64 / SR as f64) < start + 5.0);
        }
    }

    #[test]
    fn silence_and_continuous_music_have_no_boundaries() {
        let mut p = Project::new(SR, 1);
        p.add_source_as_track("s", None, AudioBuffer::silent(SR, 1, SR as usize * 200), 0);
        assert!(find_boundaries(&analyse(&p), &DetectSettings::default()).is_empty());
        let mut a = Vec::new();
        song(&mut a, 240.0, 220.0, 0.5);
        let mut p = Project::new(SR, 1);
        p.add_source_as_track("m", None, AudioBuffer::new(SR, vec![a]), 0);
        assert!(find_boundaries(&analyse(&p), &DetectSettings::default()).is_empty());
    }

    #[test]
    fn setlist_parsing() {
        let names = marker_names("1. Intro\n\n02 - Second Song\n3) Third: Live\nEncore", 6);
        assert_eq!(
            names,
            [
                "Intro",
                "Second Song",
                "Third: Live",
                "Encore",
                "Track 05",
                "Track 06"
            ]
        );
        assert_eq!(
            marker_names("1999\n", 1),
            ["1999"],
            "a title that is only digits stays"
        );
    }

    #[test]
    fn rerunning_replaces_only_automatic_markers() {
        let mut p = Project::new(SR, 1);
        p.add_marker(500, "mine");
        apply_markers(
            &mut p,
            0,
            10_000,
            &[0, 1000, 2000],
            &["A".into(), "B".into()],
        )
        .unwrap();
        assert_eq!(p.markers.len(), 4);
        assert_eq!(
            p.markers
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>(),
            ["A", "mine", "B", "Track 03"]
        );
        apply_markers(&mut p, 0, 10_000, &[3000], &["C".into()]).unwrap();
        assert_eq!(
            p.markers
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>(),
            ["mine", "C"]
        );
    }
}
