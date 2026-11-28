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
//! Radio and TV broadcasts are compressed so hard that neither cue works:
//! the gaps are as loud as the songs, and distorted guitars are as noise-like
//! as applause. Three more measurements cover them:
//!
//! * **Bass**: a band has kick drum and bass guitar; crowd noise and talk
//!   have little below 150 Hz.
//! * **Tonal persistence**: music's harmonics hold from one moment to the
//!   next; crowd noise and speech change constantly.
//! * **Talk** ([`crate::speech`]): presenters and band banter between songs.
//!
//! Missing bass together with low persistence, or talk, marks a gap. Either
//! alone doesn't (a bass-less ballad passage, a busy arrangement). When the
//! loudness and noise cues fire across most of a recording they carry no
//! information, and count for less.
//!
//! Spoken introductions count as part of the next song (the usual live-album
//! convention): the marker goes where the talk leading into the music
//! starts. Talk at the very start (a presenter's introduction) or end of a
//! recording becomes its own "Intro" or "Outro" marker instead of a song.
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
/// Bass is measured in this band, relative to the band above it.
const BASS_BAND: (f64, f64) = (40.0, 150.0);
const MID_BAND: (f64, f64) = (150.0, 4000.0);
/// Tonal persistence compares spectra this many hops apart (300 ms).
const PERSIST_LAG: usize = 3;
/// Width (Hz) of the smoothing that separates a spectrum's fine structure
/// (harmonics) from its overall shape.
const FINE_HZ: f64 = 200.0;

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
    /// Bass (40–150 Hz) relative to 150–4000 Hz, dB, median over 2 s.
    pub bass_db: Vec<f32>,
    /// Correlation of the spectrum's fine structure with 300 ms earlier,
    /// median over 2 s: high for music, low for crowd noise and speech.
    pub persistence: Vec<f32>,
    /// How much each hop sounds like talk, 0..1 (see [`crate::speech::scores`]),
    /// within stretches of talk; 0 elsewhere.
    pub speech: Vec<f32>,
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
    let (bass_lo, bass_hi) = (bin(BASS_BAND.0).max(1), bin(BASS_BAND.1).max(2));
    let (mid_lo, mid_hi) = (bass_hi, bin(MID_BAND.1).min(FFT_LEN / 2).max(bass_hi + 1));
    let fine_half = ((FINE_HZ * FFT_LEN as f64 / sr as f64) / 2.0)
        .round()
        .max(1.0) as usize;
    let mut bass = Vec::new();
    let mut persistence = Vec::new();
    let mut fine_ring: Vec<Vec<f32>> = vec![Vec::new(); PERSIST_LAG];
    let mut log_mid = vec![0f32; mid_hi - mid_lo];
    let mut fine = vec![0f32; mid_hi - mid_lo];

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
                    let power = |r: std::ops::Range<usize>| {
                        fft_out[r].iter().map(|c| c.norm_sqr() as f64).sum::<f64>()
                    };
                    let (b, m) = (power(bass_lo..bass_hi), power(mid_lo..mid_hi));
                    bass.push((10.0 * ((b + 1e-12) / (m + 1e-12)).log10()) as f32);
                    // Fine structure: the log spectrum minus its local average.
                    for (j, v) in log_mid.iter_mut().enumerate() {
                        *v = (fft_out[mid_lo + j].norm_sqr() + 1e-12).ln();
                    }
                    let mut sum: f32 = log_mid[..fine_half.min(log_mid.len())].iter().sum();
                    let mut count = fine_half.min(log_mid.len());
                    for j in 0..log_mid.len() {
                        if j + fine_half < log_mid.len() {
                            sum += log_mid[j + fine_half];
                            count += 1;
                        }
                        if j > fine_half {
                            sum -= log_mid[j - fine_half - 1];
                            count -= 1;
                        }
                        fine[j] = log_mid[j] - sum / count as f32;
                    }
                    let k = energy.len() - 1;
                    let earlier = &fine_ring[k % PERSIST_LAG];
                    persistence.push(if earlier.len() == fine.len() {
                        correlation(&fine, earlier)
                    } else {
                        0.0
                    });
                    fine_ring[k % PERSIST_LAG].clone_from(&fine);
                }
            }
            done += n;
            Ok(progress(0.7 * done as f64 / total))
        },
    )?;

    // Talk, from a second pass over the audio.
    let sp = crate::speech::features(p, tracks, start, end, &mut |f| progress(0.7 + 0.3 * f))?;
    // Only stretches `find_speech` would report: a moment where a loud song
    // ends into quieter noise can look like a syllable, but not like talk.
    let scores = crate::speech::scores(&sp);
    let mut speech = vec![0f32; energy.len()];
    for r in crate::speech::find(&sp, crate::speech::MIN_SPEECH_SECS) {
        let (a, b) = (
            ((r.start - start) / sp.hop) as usize,
            ((r.end - start) / sp.hop) as usize,
        );
        for k in a..b.min(speech.len()) {
            speech[k] = scores.get(k).copied().unwrap_or(0.0);
        }
    }

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

    let two_secs = (1.0 / HOP_SECS) as usize;
    Ok(Analysis {
        start,
        end,
        hop: hop as Frame,
        momentary,
        short_term,
        flatness,
        flatness_smooth,
        reference,
        bass_db: median_filter(&bass, two_secs),
        persistence: median_filter(&persistence, two_secs),
        speech,
    })
}

/// Pearson correlation of two equally long vectors.
fn correlation(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len() as f32;
    let (ma, mb) = (a.iter().sum::<f32>() / n, b.iter().sum::<f32>() / n);
    let (mut ab, mut aa, mut bb) = (0f32, 0f32, 0f32);
    for (x, y) in a.iter().zip(b) {
        let (x, y) = (x - ma, y - mb);
        ab += x * y;
        aa += x * x;
        bb += y * y;
    }
    if aa <= 0.0 || bb <= 0.0 {
        0.0
    } else {
        ab / (aa * bb).sqrt()
    }
}

/// Median of each value with `half` neighbours either side.
pub(crate) fn median_filter(v: &[f32], half: usize) -> Vec<f32> {
    let mut buf = Vec::with_capacity(2 * half + 1);
    (0..v.len())
        .map(|i| {
            buf.clear();
            buf.extend_from_slice(&v[i.saturating_sub(half)..(i + half + 1).min(v.len())]);
            buf.sort_by(f32::total_cmp);
            buf[buf.len() / 2]
        })
        .collect()
}

/// Geometric over arithmetic mean of the power spectrum (silence counts as flat).
pub(crate) fn spectral_flatness(bins: &[realfft::num_complex::Complex<f32>]) -> f32 {
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

/// What a boundary starts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BoundaryKind {
    /// The next song.
    #[default]
    Song,
    /// The first song, after an introduction that is mostly talk (a
    /// presenter): the range start begins an "Intro" instead of a song.
    AfterIntro,
    /// Closing talk after the last song: an "Outro", not a song.
    Outro,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Boundary {
    /// Where the next song's marker goes.
    pub position: Frame,
    pub kind: BoundaryKind,
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

/// Between-songs evidence from bass, persistence and talk, 0..1: missing
/// bass *and* unsteady harmonics (crowd noise, talk), or talk itself.
fn broadcast_cues(a: &Analysis, k: usize) -> f32 {
    let no_bass = ((-3.0 - a.bass_db[k]) / 9.0).clamp(0.0, 1.0);
    let unsteady = ((0.13 - a.persistence[k]) / 0.06).clamp(0.0, 1.0);
    let cues = no_bass.min(unsteady).max(a.speech[k]);
    // Weak hints (a little bass missing, a moment of talk-like sound) don't count.
    ((cues - 0.3) / 0.5).clamp(0.0, 1.0)
}

/// The gap measure: loudness and noise (weighted by how informative they are
/// in this recording) plus bass, persistence and talk.
struct Gaps<'a> {
    a: &'a Analysis,
    /// Weight of the loudness/noise cue.
    old_weight: f32,
}

impl<'a> Gaps<'a> {
    fn new(a: &'a Analysis) -> Self {
        // A cue that fires across most of the recording (hard-compressed
        // broadcasts of loud, distorted music) tells songs and gaps apart
        // poorly, so it counts for less there.
        let audible: Vec<usize> = (0..a.len()).filter(|&k| a.short_term[k] > -70.0).collect();
        let firing = audible
            .iter()
            .filter(|&&k| gapness(a.reference, a.short_term[k], a.flatness_smooth[k]) > GAP)
            .count();
        let share = firing as f32 / audible.len().max(1) as f32;
        Self {
            a,
            old_weight: (1.0 - (share - 0.3) / 0.3).clamp(0.4, 1.0),
        }
    }

    /// Gap measure of hop `k`: robust (3 s loudness, smoothed flatness) or
    /// precise (400 ms loudness, flatness of the hop itself).
    fn at(&self, k: usize, precise: bool) -> f32 {
        let a = self.a;
        let old = if precise {
            gapness(a.reference, a.momentary[k], a.flatness[k])
        } else {
            gapness(a.reference, a.short_term[k], a.flatness_smooth[k])
        };
        (self.old_weight * old + 0.6 * broadcast_cues(a, k)).min(1.0)
    }
}

/// Share of hops `from..to` that are talk.
fn talk_share(a: &Analysis, from: usize, to: usize) -> f32 {
    let to = to.min(a.len());
    if to <= from {
        return 0.0;
    }
    a.speech[from..to].iter().filter(|&&v| v > 0.5).count() as f32 / (to - from) as f32
}

/// Hop index of a timeline frame.
fn hop_of(a: &Analysis, f: Frame) -> usize {
    (f.saturating_sub(a.start) / a.hop.max(1)) as usize
}

/// An intro or outro must be mostly talk and within this much of the edge.
const EDGE_TALK_SECS: f64 = 300.0;
/// ...and at least this long (a scrap of the next programme isn't an outro).
const MIN_EDGE_SECS: f64 = 5.0;

/// Song boundaries (sorted by position), not including the range start.
pub fn find_boundaries(a: &Analysis, s: &DetectSettings) -> Vec<Boundary> {
    let n = a.len();
    if n == 0 || a.reference <= FLOOR_LUFS {
        return Vec::new();
    }
    let hops = |secs: f64| (secs / HOP_SECS).round() as usize;
    let gaps = Gaps::new(a);
    let g: Vec<f32> = (0..n).map(|k| gaps.at(k, false)).collect();

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

    // Gaps touching the range edges can only be a spoken intro or outro,
    // never a song change.
    let mut candidates: Vec<(Boundary, Frame)> = runs
        .into_iter()
        .filter(|&(s0, s1)| s1 - s0 >= hops(s.min_gap_secs).max(1) && s0 < s1 && s1 <= n)
        .map(|(s0, s1)| {
            let mean = g[s0..s1].iter().sum::<f32>() / (s1 - s0) as f32;
            let duration = ((s1 - s0) as f64 * HOP_SECS) as f32;
            let strength = mean * (0.5 + 0.5 * (duration / 6.0).min(1.0));
            let (position, onset) = place(a, &gaps, s0, s1, s);
            (
                Boundary {
                    position,
                    kind: BoundaryKind::Song,
                    strength,
                    gap_start: a.frame_of(s0),
                    gap_end: a.frame_of(s1),
                },
                onset,
            )
        })
        .collect();

    let min_strength = 0.9 - 0.4 * s.sensitivity.clamp(0.0, 1.0);
    let edge = hops(EDGE_TALK_SECS);

    // A presenter's introduction: mostly talk from the range start up to a
    // gap near it. The first song starts where the music does.
    let intro = candidates
        .iter()
        .position(|(b, _)| {
            let to = hop_of(a, b.gap_end);
            // Mostly talk from the start is the evidence; talk alone over a
            // music bed doesn't make a strong gap.
            to <= edge && to >= hops(MIN_EDGE_SECS) && talk_share(a, 0, to) >= 0.5
        })
        .map(|i| {
            let (mut b, onset) = candidates.remove(i);
            b.kind = BoundaryKind::AfterIntro;
            b.position = onset;
            b
        });
    // Closing talk: mostly talk from a gap near the range end to the end.
    let outro = candidates
        .iter()
        .rposition(|(b, _)| {
            let from = hop_of(a, b.gap_start);
            n - from <= edge && n - from >= hops(MIN_EDGE_SECS) && talk_share(a, from, n) >= 0.5
        })
        .map(|i| {
            let (mut b, _) = candidates.remove(i);
            b.kind = BoundaryKind::Outro;
            // The talk starts it; applause before it stays with the last song.
            let from = hop_of(a, b.gap_start);
            let to = hop_of(a, b.gap_end).max(from);
            if let Some(k) = (from..to).find(|&k| a.speech[k] > 0.5) {
                b.position = a.frame_of(k);
            }
            b
        });

    // Strongest first; keep songs at least `min_song_secs` long.
    let mut candidates: Vec<Boundary> = candidates
        .into_iter()
        .map(|(b, _)| b)
        .filter(|b| b.gap_start > a.start && b.gap_end < a.frame_of(n))
        .collect();
    candidates.sort_by(|x, y| y.strength.total_cmp(&x.strength));
    let min_song = (s.min_song_secs * a.hop as f64 / HOP_SECS) as Frame;
    let first = intro.as_ref().map_or(a.start, |b| b.position);
    let last = outro.as_ref().map_or(a.end, |b| b.position);
    let wanted = s.expected_songs.map(|n| n.saturating_sub(1));
    let mut chosen: Vec<Boundary> = Vec::new();
    for c in candidates {
        if wanted.is_some_and(|w| chosen.len() >= w) {
            break;
        }
        if wanted.is_none() && c.strength < min_strength {
            break;
        }
        let fits = c.position >= first + min_song
            && c.position + min_song <= last
            && chosen
                .iter()
                .all(|b| b.position.abs_diff(c.position) >= min_song);
        if fits {
            chosen.push(c);
        }
    }
    chosen.extend(intro);
    chosen.extend(outro);
    chosen.sort_by_key(|b| b.position);
    chosen
}

/// Marker position for the gap `s0..s1` (in hops), and where the music
/// resumes.
fn place(a: &Analysis, gaps: &Gaps, s0: usize, s1: usize, s: &DetectSettings) -> (Frame, Frame) {
    let n = a.len();
    let gap_start = a.frame_of(s0);
    // The 3 s gap measure smears edges; find the precise onset with the
    // 400 ms measure: the first moment music holds for a full second.
    let hold = 10;
    let from = s1.saturating_sub(30).max(s0);
    let to = (s1 + 30).min(n);
    let music = |k: usize| gaps.at(k, true) <= GAP;
    let onset = (from..to)
        .find(|&k| (k..(k + hold).min(n)).all(music))
        .unwrap_or(s1);
    let pre_roll = (s.pre_roll_secs * a.hop as f64 / HOP_SECS) as Frame;
    let at_onset = a.frame_of(onset).saturating_sub(pre_roll).max(gap_start);
    let position = match s.placement {
        Placement::QuietestPoint => {
            let k = (s0..s1)
                .min_by(|&x, &y| a.momentary[x].total_cmp(&a.momentary[y]))
                .unwrap_or(s0);
            a.frame_of(k)
        }
        Placement::MusicStart => match lead_in_talk(a, s0, onset) {
            // Talk leading into the song belongs to it.
            Some(k) => a.frame_of(k).saturating_sub(pre_roll).max(gap_start),
            None => at_onset,
        },
    };
    (position, at_onset)
}

/// Start (hop) of talk that leads into the music at `onset`: talk ending at
/// most 5 s before it, with pauses (cheering between sentences) under 8 s,
/// not before `from`.
fn lead_in_talk(a: &Analysis, from: usize, onset: usize) -> Option<usize> {
    let talk = |k: usize| a.speech.get(k).is_some_and(|&v| v > 0.5);
    let last = (from..onset).rev().take(50).find(|&k| talk(k))?;
    let mut start = last;
    let mut k = last;
    while k > from {
        k -= 1;
        if talk(k) {
            start = k;
        } else if start - k >= 80 {
            break;
        }
    }
    Some(start)
}

/// Names for the markers of [`find_boundaries`]'s result: "Intro" and
/// "Outro" for talk at the edges, setlist titles (see [`marker_names`]) for
/// the songs. With `marker_at_start` the first name is for the range start.
pub fn segment_names(found: &[Boundary], setlist: &str, marker_at_start: bool) -> Vec<String> {
    let intro = found
        .first()
        .is_some_and(|b| b.kind == BoundaryKind::AfterIntro);
    let songs = usize::from(!intro)
        + found
            .iter()
            .filter(|b| b.kind != BoundaryKind::Outro)
            .count();
    let mut titles = marker_names(setlist, songs).into_iter();
    let mut names = vec![if intro {
        "Intro".to_string()
    } else {
        titles.next().unwrap_or_default()
    }];
    for b in found {
        names.push(match b.kind {
            BoundaryKind::Outro => "Outro".to_string(),
            _ => titles.next().unwrap_or_default(),
        });
    }
    if !marker_at_start {
        names.remove(0);
    }
    names
}

/// Marker names from a pasted setlist (one title per line; leading numbering
/// like "1.", "01 -", "3)" or "07 " is removed, but not a number that starts
/// the title, as in "7 Days to the Wolves"), padded with "Track NN".
pub fn marker_names(setlist: &str, count: usize) -> Vec<String> {
    let titles: Vec<String> = setlist
        .lines()
        .map(|l| strip_numbering(l.trim()).to_string())
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

/// A title without its list numbering.
fn strip_numbering(t: &str) -> &str {
    let rest = t.trim_start_matches(|c: char| c.is_ascii_digit());
    let digits = &t[..t.len() - rest.len()];
    if digits.is_empty() || rest.is_empty() {
        return t;
    }
    let after = rest.trim_start_matches([' ', '\t']);
    // "1." "3)" "02 -" "4:" — punctuation marks numbering.
    if let Some(title) = after.strip_prefix(['.', ')', '-', ':']) {
        let title = title
            .trim_start_matches(['.', ')', '-', ':', ' ', '\t'])
            .trim();
        if !title.is_empty() {
            return title;
        }
    }
    // "07 Song" or a tab: a zero-padded number or a tab before the title.
    let padded = digits.len() > 1 && digits.starts_with('0');
    if (padded || rest.starts_with('\t')) && !after.is_empty() {
        return after.trim();
    }
    t
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

    /// A presenter talks over crowd noise, the band plays two songs with an
    /// ovation between them, then the singer introduces the third.
    fn broadcast_show() -> (Project, Vec<f64>) {
        assert_eq!(SR, crate::speech::tests::SR);
        let mut rng = Noise(0x2468);
        let mut a = Vec::new();
        let secs = |v: &Vec<f32>| v.len() as f64 / SR as f64;
        let talk = |a: &mut Vec<f32>, s: f64, seed, rng: &mut Noise| {
            for v in crate::speech::tests::talk(s, 0.5, seed) {
                a.push(v + rng.next() * 0.01);
            }
        };
        talk(&mut a, 25.0, 1, &mut rng);
        let mut starts = vec![secs(&a)];
        song(&mut a, 90.0, 220.0, 0.5);
        applause(&mut a, 8.0, 0.1, &mut rng);
        starts.push(secs(&a));
        song(&mut a, 90.0, 196.0, 0.5);
        applause(&mut a, 4.0, 0.08, &mut rng);
        starts.push(secs(&a)); // the singer's introduction belongs to song 3
        talk(&mut a, 8.0, 2, &mut rng);
        song(&mut a, 90.0, 262.0, 0.5);
        applause(&mut a, 6.0, 0.1, &mut rng);
        let mut p = Project::new(SR, 1);
        p.add_source_as_track("show", None, AudioBuffer::new(SR, vec![a]), 0);
        (p, starts)
    }

    #[test]
    fn spoken_intro_and_talk_leading_into_a_song() {
        let (p, starts) = broadcast_show();
        let a = analyse(&p);
        let found = find_boundaries(&a, &DetectSettings::default());
        let at = |b: &Boundary| b.position as f64 / SR as f64;
        assert_eq!(found.len(), 3, "{found:?}");
        assert_eq!(found[0].kind, BoundaryKind::AfterIntro);
        assert!(
            (at(&found[0]) - starts[0]).abs() < 1.5,
            "first song: {found:?}"
        );
        assert_eq!(found[1].kind, BoundaryKind::Song);
        assert!((at(&found[1]) - starts[1]).abs() < 1.5, "{found:?}");
        assert!(
            (at(&found[2]) - starts[2]).abs() < 1.5,
            "marker where the talk starts, not the music: {found:?}"
        );
        assert_eq!(
            segment_names(&found, "A\nB\nC", true),
            ["Intro", "A", "B", "C"]
        );
        // Counting songs doesn't count the intro.
        let s = DetectSettings {
            expected_songs: Some(2),
            ..Default::default()
        };
        let two = find_boundaries(&a, &s);
        assert_eq!(two.len(), 2, "{two:?}");
        assert_eq!(two[0].kind, BoundaryKind::AfterIntro);
    }

    #[test]
    fn presenter_over_a_music_bed_is_an_intro() {
        // No crowd noise: the talk alone has to mark the intro.
        let mut a = Vec::new();
        let mut bed = Vec::new();
        song(&mut bed, 30.0, 110.0, 0.06);
        for (t, b) in crate::speech::tests::talk(30.0, 0.5, 5)
            .into_iter()
            .zip(bed)
        {
            a.push(t + b);
        }
        song(&mut a, 90.0, 220.0, 0.5);
        let mut rng = Noise(9);
        applause(&mut a, 6.0, 0.1, &mut rng);
        song(&mut a, 90.0, 196.0, 0.5);
        let mut p = Project::new(SR, 1);
        p.add_source_as_track("show", None, AudioBuffer::new(SR, vec![a]), 0);
        let found = find_boundaries(&analyse(&p), &DetectSettings::default());
        assert_eq!(
            found.first().map(|b| b.kind),
            Some(BoundaryKind::AfterIntro),
            "{found:?}"
        );
        let at = found[0].position as f64 / SR as f64;
        assert!((29.0..=31.5).contains(&at), "{found:?}");
    }

    #[test]
    fn names_for_edges() {
        let b = |kind| Boundary {
            position: 0,
            kind,
            strength: 1.0,
            gap_start: 0,
            gap_end: 0,
        };
        let found = [b(BoundaryKind::Song), b(BoundaryKind::Outro)];
        assert_eq!(
            segment_names(&found, "One\nTwo", true),
            ["One", "Two", "Outro"]
        );
        assert_eq!(segment_names(&found, "One\nTwo", false), ["Two", "Outro"]);
        let found = [b(BoundaryKind::AfterIntro), b(BoundaryKind::Song)];
        assert_eq!(
            segment_names(&found, "One", true),
            ["Intro", "One", "Track 02"]
        );
        assert_eq!(segment_names(&[], "One", true), ["One"]);
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
        assert_eq!(
            marker_names(
                "7 Days to the Wolves\n99 Luftballons\n07 Nemo\n8\tAmaranth\n9 - Run",
                5
            ),
            [
                "7 Days to the Wolves",
                "99 Luftballons",
                "Nemo",
                "Amaranth",
                "Run"
            ],
            "a number that starts a title stays"
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
