//! Comparing two versions of audio sample by sample: the null test.
//!
//! Either side is a [`Reader`]: a project rendered as heard ([`ProjectReader`])
//! or a decoded file ([`DecoderReader`]). [`compare`] streams both, subtracts
//! them and reports whether they are identical, how large the difference is
//! (peak and RMS, and relative to the signal: the depth of the null) and where
//! it is. This proves an edit left everything else untouched, or that an
//! export matches the project.
//!
//! With [`CompareOptions::align`], the second side is first shifted by the
//! whole number of frames that best lines it up with the first (found by
//! cross-correlating their openings), so a copy with extra lead-in or an
//! encoder delay can still null.

use realfft::num_complex::Complex;
use realfft::RealFftPlanner;

use crate::decode::Decoder;
use crate::error::{Error, Result};
use crate::project::Project;
use crate::render::{mix_into, Progress, RenderOptions, CHUNK};
use crate::Frame;

/// A stream of planar 32-bit float audio.
pub trait Reader: Send {
    fn sample_rate(&self) -> u32;
    fn channels(&self) -> usize;
    /// Frames still to come, if known (for progress).
    fn remaining(&self) -> Option<u64>;
    /// Fill the start of every channel of `out` (each at least `max` long)
    /// with up to `max` frames; returns how many, 0 at the end.
    fn read(&mut self, out: &mut [Vec<f32>], max: usize) -> Result<usize>;
}

/// A range of a project, rendered as heard.
pub struct ProjectReader {
    project: Project,
    opts: RenderOptions,
    pos: Frame,
    end: Frame,
    chunks: usize,
}

impl ProjectReader {
    /// `project` is cheap to clone: its audio is shared.
    pub fn new(project: Project, start: Frame, end: Frame, opts: RenderOptions) -> Self {
        Self {
            project,
            opts,
            pos: start,
            end: end.max(start),
            chunks: 0,
        }
    }
}

impl Reader for ProjectReader {
    fn sample_rate(&self) -> u32 {
        self.project.sample_rate
    }

    fn channels(&self) -> usize {
        self.project.channels as usize
    }

    fn remaining(&self) -> Option<u64> {
        Some(self.end - self.pos)
    }

    fn read(&mut self, out: &mut [Vec<f32>], max: usize) -> Result<usize> {
        let n = max.min((self.end - self.pos) as usize);
        if n == 0 {
            return Ok(0);
        }
        mix_into(&self.project, self.pos, n, out, &self.opts);
        self.pos += n as Frame;
        // As in render::for_each_chunk: long passes release mapped audio.
        self.chunks += 1;
        if self.chunks.is_multiple_of(64) {
            crate::store::release_mapped_memory();
        }
        Ok(n)
    }
}

/// A decoded audio file.
pub struct DecoderReader {
    dec: Box<dyn Decoder>,
    /// Decoded frames not yet handed out.
    pending: Vec<Vec<f32>>,
    pending_pos: usize,
    delivered: u64,
}

impl DecoderReader {
    pub fn new(dec: Box<dyn Decoder>) -> Self {
        let channels = dec.channels();
        Self {
            dec,
            pending: vec![Vec::new(); channels],
            pending_pos: 0,
            delivered: 0,
        }
    }
}

impl Reader for DecoderReader {
    fn sample_rate(&self) -> u32 {
        self.dec.sample_rate()
    }

    fn channels(&self) -> usize {
        self.dec.channels()
    }

    fn remaining(&self) -> Option<u64> {
        self.dec
            .total_frames()
            .map(|t| t.saturating_sub(self.delivered))
    }

    fn read(&mut self, out: &mut [Vec<f32>], max: usize) -> Result<usize> {
        while self.pending_pos >= self.pending.first().map_or(0, Vec::len) {
            match self.dec.next_chunk()? {
                None => return Ok(0),
                Some(chunk) => {
                    for (p, c) in self.pending.iter_mut().zip(chunk) {
                        p.clear();
                        p.extend_from_slice(c);
                    }
                    self.pending_pos = 0;
                }
            }
        }
        let n = max.min(self.pending[0].len() - self.pending_pos);
        for (o, p) in out.iter_mut().zip(&self.pending) {
            o[..n].copy_from_slice(&p[self.pending_pos..self.pending_pos + n]);
        }
        self.pending_pos += n;
        self.delivered += n as u64;
        Ok(n)
    }
}

/// No audio at all, e.g. a project before anything was imported. Compared
/// with something, it is that much silence missing: every sample differs.
pub struct Empty {
    rate: u32,
    channels: usize,
}

impl Empty {
    pub fn new(rate: u32, channels: usize) -> Self {
        Self { rate, channels }
    }
}

impl Reader for Empty {
    fn sample_rate(&self) -> u32 {
        self.rate
    }

    fn channels(&self) -> usize {
        self.channels
    }

    fn remaining(&self) -> Option<u64> {
        Some(0)
    }

    fn read(&mut self, _: &mut [Vec<f32>], _: usize) -> Result<usize> {
        Ok(0)
    }
}

/// Frames read ahead (for alignment), handed out again before the rest.
struct Prefixed {
    prefix: Vec<Vec<f32>>,
    pos: usize,
    inner: Box<dyn Reader>,
}

impl Reader for Prefixed {
    fn sample_rate(&self) -> u32 {
        self.inner.sample_rate()
    }

    fn channels(&self) -> usize {
        self.inner.channels()
    }

    fn remaining(&self) -> Option<u64> {
        let left = (self.prefix[0].len() - self.pos) as u64;
        self.inner.remaining().map(|r| r + left)
    }

    fn read(&mut self, out: &mut [Vec<f32>], max: usize) -> Result<usize> {
        let left = self.prefix[0].len() - self.pos;
        if left == 0 {
            return self.inner.read(out, max);
        }
        let n = max.min(left);
        for (o, p) in out.iter_mut().zip(&self.prefix) {
            o[..n].copy_from_slice(&p[self.pos..self.pos + n]);
        }
        self.pos += n;
        Ok(n)
    }
}

/// At most `frames` frames of a reader.
struct Limited {
    inner: Box<dyn Reader>,
    left: u64,
}

impl Reader for Limited {
    fn sample_rate(&self) -> u32 {
        self.inner.sample_rate()
    }

    fn channels(&self) -> usize {
        self.inner.channels()
    }

    fn remaining(&self) -> Option<u64> {
        Some(
            self.inner
                .remaining()
                .map_or(self.left, |r| r.min(self.left)),
        )
    }

    fn read(&mut self, out: &mut [Vec<f32>], max: usize) -> Result<usize> {
        let n = self.inner.read(out, max.min(self.left as usize))?;
        self.left -= n as u64;
        Ok(n)
    }
}

/// Frames `start..end` of a reader (`end`: to the end).
pub fn slice(mut r: Box<dyn Reader>, start: u64, end: Option<u64>) -> Result<Box<dyn Reader>> {
    skip(r.as_mut(), start)?;
    Ok(match end {
        Some(end) => Box::new(Limited {
            inner: r,
            left: end.saturating_sub(start),
        }),
        None => r,
    })
}

/// Read until `max` frames or the end; returns how many.
fn fill(r: &mut dyn Reader, out: &mut [Vec<f32>], max: usize) -> Result<usize> {
    let mut got = 0;
    let mut scratch = vec![vec![0.0f32; max]; out.len()];
    while got < max {
        let n = r.read(&mut scratch, max - got)?;
        if n == 0 {
            break;
        }
        for (o, s) in out.iter_mut().zip(&scratch) {
            o[got..got + n].copy_from_slice(&s[..n]);
        }
        got += n;
    }
    Ok(got)
}

/// Read and discard up to `frames` frames.
fn skip(r: &mut dyn Reader, mut frames: u64) -> Result<()> {
    let mut scratch = vec![vec![0.0f32; CHUNK]; r.channels()];
    while frames > 0 {
        let n = r.read(&mut scratch, CHUNK.min(frames as usize))?;
        if n == 0 {
            break;
        }
        frames -= n as u64;
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompareOptions {
    /// Shift the second side to line up with the first before comparing.
    pub align: bool,
    /// Largest shift alignment looks for, in frames.
    pub max_offset: u64,
    /// Differences at or below this (linear, e.g. 10^(-90/20)) count as equal;
    /// 0 means any difference counts.
    pub threshold: f32,
    /// Differences closer than this many frames are reported as one region.
    pub region_gap: u64,
}

impl CompareOptions {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            align: false,
            max_offset: sample_rate as u64,
            threshold: 0.0,
            region_gap: sample_rate as u64 / 10,
        }
    }
}

/// How the second side was shifted.
#[derive(Clone, Debug, PartialEq)]
pub struct Alignment {
    /// Frames the second side starts later than the first (negative: earlier).
    /// That many frames were skipped at the start of the later one.
    pub offset: i64,
    /// How well the openings matched at that shift, 0..1 (1: the same signal).
    pub correlation: f64,
    /// Whether the match was good enough to use; if not, no shift was applied.
    pub found: bool,
}

/// Difference figures of one channel.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChannelDiff {
    /// Largest |a - b| (linear).
    pub peak: f32,
    /// RMS of a - b over the compared length (linear).
    pub rms: f64,
    /// Samples where |a - b| exceeds the threshold.
    pub differing_samples: u64,
}

/// A stretch where the two sides differ.
#[derive(Clone, Debug, PartialEq)]
pub struct DiffRegion {
    /// Frames from the start of the first side (after any alignment skip).
    pub start: Frame,
    pub end: Frame,
    /// Largest |a - b| in it.
    pub peak: f32,
}

/// Regions kept in a report; [`CompareReport::region_count`] counts them all.
pub const MAX_REGIONS: usize = 1000;

#[derive(Clone, Debug, PartialEq)]
pub struct CompareReport {
    pub sample_rate: u32,
    pub channels: usize,
    pub alignment: Option<Alignment>,
    /// Frames of each side compared (after any alignment skip). Where one is
    /// shorter, the rest of the longer one is compared against silence.
    pub frames_a: u64,
    pub frames_b: u64,
    /// Same length and every sample equal.
    pub identical: bool,
    /// Frames where any channel differs by more than the threshold.
    pub differing_frames: u64,
    pub first_difference: Option<Frame>,
    pub last_difference: Option<Frame>,
    /// Over all channels.
    pub peak: f32,
    pub rms: f64,
    /// RMS of each side, for the null depth.
    pub rms_a: f64,
    pub rms_b: f64,
    pub per_channel: Vec<ChannelDiff>,
    /// The first [`MAX_REGIONS`] regions.
    pub regions: Vec<DiffRegion>,
    pub region_count: usize,
}

impl CompareReport {
    /// How far below the first side's level the difference lies (dB, negative;
    /// `None` when there is no difference or the first side is silent). -96
    /// is about the noise of 16-bit audio.
    pub fn null_depth_db(&self) -> Option<f64> {
        (self.rms > 0.0 && self.rms_a > 0.0).then(|| 20.0 * (self.rms / self.rms_a).log10())
    }

    /// No difference above the threshold, over the same length.
    pub fn nulls(&self) -> bool {
        self.differing_frames == 0 && self.frames_a == self.frames_b
    }
}

/// Compare `a` and `b` sample by sample. They must have the same sample rate
/// and channel count.
pub fn compare(
    a: Box<dyn Reader>,
    b: Box<dyn Reader>,
    opts: &CompareOptions,
    progress: Progress,
) -> Result<CompareReport> {
    let (rate, channels) = (a.sample_rate(), a.channels());
    if b.sample_rate() != rate {
        return Err(Error::Mismatch(format!(
            "can't compare {rate} Hz audio with {} Hz audio",
            b.sample_rate()
        )));
    }
    if b.channels() != channels {
        return Err(Error::Mismatch(format!(
            "can't compare {channels}-channel audio with {}-channel audio",
            b.channels()
        )));
    }
    let (mut a, mut b, alignment) = if opts.align {
        let (a, b, al) = align(a, b, opts.max_offset, rate)?;
        (a, b, Some(al))
    } else {
        (a, b, None)
    };

    let total = a.remaining().max(b.remaining()).unwrap_or(0).max(1) as f64;
    let mut ba = vec![vec![0.0f32; CHUNK]; channels];
    let mut bb = vec![vec![0.0f32; CHUNK]; channels];
    let mut per_channel = vec![ChannelDiff::default(); channels];
    let mut sum_sq = vec![0.0f64; channels];
    let (mut sq_a, mut sq_b) = (0.0f64, 0.0f64);
    let (mut frames_a, mut frames_b) = (0u64, 0u64);
    let mut equal = true;
    let mut differing_frames = 0u64;
    let (mut first, mut last) = (None, None);
    let mut regions: Vec<DiffRegion> = Vec::new();
    let mut region_count = 0usize;
    let mut open: Option<DiffRegion> = None;
    let mut pos: Frame = 0;
    loop {
        let na = fill(a.as_mut(), &mut ba, CHUNK)?;
        let nb = fill(b.as_mut(), &mut bb, CHUNK)?;
        let n = na.max(nb);
        if n == 0 {
            break;
        }
        // The side that ended is compared as silence.
        for ch in &mut ba {
            ch[na..n].fill(0.0);
        }
        for ch in &mut bb {
            ch[nb..n].fill(0.0);
        }
        frames_a += na as u64;
        frames_b += nb as u64;
        for i in 0..n {
            let mut frame_peak = 0.0f32;
            let mut differs = false;
            for ch in 0..channels {
                let (x, y) = (ba[ch][i], bb[ch][i]);
                sq_a += (x as f64) * (x as f64);
                sq_b += (y as f64) * (y as f64);
                if x == y {
                    continue;
                }
                equal = false;
                let d = (x - y).abs();
                let c = &mut per_channel[ch];
                c.peak = c.peak.max(d);
                sum_sq[ch] += (d as f64) * (d as f64);
                if d > opts.threshold {
                    c.differing_samples += 1;
                    differs = true;
                    frame_peak = frame_peak.max(d);
                }
            }
            if !differs {
                continue;
            }
            let f = pos + i as Frame;
            differing_frames += 1;
            first.get_or_insert(f);
            last = Some(f);
            match &mut open {
                Some(r) if f - r.end <= opts.region_gap => {
                    r.end = f + 1;
                    r.peak = r.peak.max(frame_peak);
                }
                _ => {
                    if let Some(r) = open.take() {
                        region_count += 1;
                        if regions.len() < MAX_REGIONS {
                            regions.push(r);
                        }
                    }
                    open = Some(DiffRegion {
                        start: f,
                        end: f + 1,
                        peak: frame_peak,
                    });
                }
            }
        }
        pos += n as Frame;
        if !progress((pos as f64 / total).min(1.0)) {
            return Err(Error::Cancelled);
        }
    }
    if let Some(r) = open {
        region_count += 1;
        if regions.len() < MAX_REGIONS {
            regions.push(r);
        }
    }

    let len = pos.max(1) as f64;
    for (c, s) in per_channel.iter_mut().zip(&sum_sq) {
        c.rms = (s / len).sqrt();
    }
    let samples = len * channels.max(1) as f64;
    Ok(CompareReport {
        sample_rate: rate,
        channels,
        alignment,
        frames_a,
        frames_b,
        identical: equal && frames_a == frames_b,
        differing_frames,
        first_difference: first,
        last_difference: last,
        peak: per_channel.iter().fold(0.0, |m, c| m.max(c.peak)),
        rms: (sum_sq.iter().sum::<f64>() / samples).sqrt(),
        rms_a: (sq_a / samples).sqrt(),
        rms_b: (sq_b / samples).sqrt(),
        per_channel,
        regions,
        region_count,
    })
}

/// Seconds of each opening cross-correlated to find the shift.
const ALIGN_WINDOW_SECS: u64 = 10;
/// Below this, the openings are too unlike for a shift to mean anything.
const MIN_CORRELATION: f64 = 0.5;

type Aligned = (Box<dyn Reader>, Box<dyn Reader>, Alignment);

/// Find the shift of `b` against `a` and skip the extra frames of the later one.
fn align(
    mut a: Box<dyn Reader>,
    mut b: Box<dyn Reader>,
    max_offset: u64,
    rate: u32,
) -> Result<Aligned> {
    let channels = a.channels();
    let len = (ALIGN_WINDOW_SECS * rate as u64 + max_offset) as usize;
    let mut pa = vec![vec![0.0f32; len]; channels];
    let mut pb = vec![vec![0.0f32; len]; channels];
    let na = fill(a.as_mut(), &mut pa, len)?;
    let nb = fill(b.as_mut(), &mut pb, len)?;
    let mono = |p: &[Vec<f32>], n: usize| -> Vec<f32> {
        (0..n).map(|i| p.iter().map(|c| c[i]).sum()).collect()
    };
    let (offset, correlation) = best_lag(&mono(&pa, na), &mono(&pb, nb), max_offset as usize);
    for p in pa.iter_mut() {
        p.truncate(na);
    }
    for p in pb.iter_mut() {
        p.truncate(nb);
    }
    let mut a: Box<dyn Reader> = Box::new(Prefixed {
        prefix: pa,
        pos: 0,
        inner: a,
    });
    let mut b: Box<dyn Reader> = Box::new(Prefixed {
        prefix: pb,
        pos: 0,
        inner: b,
    });
    let found = correlation >= MIN_CORRELATION;
    if found {
        if offset > 0 {
            skip(b.as_mut(), offset as u64)?;
        } else {
            skip(a.as_mut(), offset.unsigned_abs())?;
        }
    }
    let alignment = Alignment {
        offset: if found { offset } else { 0 },
        correlation: (correlation.clamp(0.0, 1.0) * 1000.0).round() / 1000.0,
        found,
    };
    Ok((a, b, alignment))
}

/// The lag `l` (|l| <= `max_lag`) maximising sum a[n] * b[n + l], and that
/// correlation normalised by the energy of the overlapping parts (1 for the
/// same signal). Computed with FFTs.
fn best_lag(a: &[f32], b: &[f32], max_lag: usize) -> (i64, f64) {
    if a.is_empty() || b.is_empty() {
        return (0, 0.0);
    }
    let size = (a.len() + b.len()).next_power_of_two();
    let mut planner = RealFftPlanner::<f32>::new();
    let fwd = planner.plan_fft_forward(size);
    let inv = planner.plan_fft_inverse(size);
    let spectrum = |x: &[f32]| -> Vec<Complex<f32>> {
        let mut buf = fwd.make_input_vec();
        buf[..x.len()].copy_from_slice(x);
        let mut out = fwd.make_output_vec();
        fwd.process(&mut buf, &mut out).expect("FFT sizes match");
        out
    };
    let (fa, fb) = (spectrum(a), spectrum(b));
    // conj(A) * B transforms back to the cross-correlation r[l] = sum a[n] b[n + l].
    let mut prod: Vec<Complex<f32>> = fa.iter().zip(&fb).map(|(x, y)| x.conj() * y).collect();
    let mut r = inv.make_output_vec();
    inv.process(&mut prod, &mut r).expect("FFT sizes match");

    // Running energies, to normalise by the overlap at each lag.
    let prefix = |x: &[f32]| -> Vec<f64> {
        std::iter::once(0.0)
            .chain(x.iter().scan(0.0f64, |s, &v| {
                *s += (v as f64) * (v as f64);
                Some(*s)
            }))
            .collect()
    };
    let (ea, eb) = (prefix(a), prefix(b));
    let max_lag = max_lag as i64;
    let lags = (-max_lag).max(-(a.len() as i64) + 1)..=max_lag.min(b.len() as i64 - 1);
    let mut best = (0i64, 0.0f64, f64::MIN);
    for lag in lags {
        let raw = r[lag.rem_euclid(size as i64) as usize] as f64 / size as f64;
        // Overlap: a[n] with b[n + lag] for n in [n0, n1).
        let n0 = (-lag).max(0) as usize;
        let n1 = (a.len() as i64).min(b.len() as i64 - lag) as usize;
        if n1 <= n0 {
            continue;
        }
        let e = ((ea[n1] - ea[n0])
            * (eb[(n1 as i64 + lag) as usize] - eb[(n0 as i64 + lag) as usize]))
            .sqrt();
        if e <= 0.0 {
            continue;
        }
        if raw > best.2 {
            best = (lag, raw / e, raw);
        }
    }
    (best.0, best.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reader over audio in memory, handed out in uneven pieces.
    struct Mem {
        data: Vec<Vec<f32>>,
        pos: usize,
        rate: u32,
    }

    fn mem(data: Vec<Vec<f32>>) -> Box<dyn Reader> {
        Box::new(Mem {
            data,
            pos: 0,
            rate: 48_000,
        })
    }

    impl Reader for Mem {
        fn sample_rate(&self) -> u32 {
            self.rate
        }
        fn channels(&self) -> usize {
            self.data.len()
        }
        fn remaining(&self) -> Option<u64> {
            Some((self.data[0].len() - self.pos) as u64)
        }
        fn read(&mut self, out: &mut [Vec<f32>], max: usize) -> Result<usize> {
            // Odd piece sizes exercise the refilling.
            let n = max.min(7919).min(self.data[0].len() - self.pos);
            for (o, d) in out.iter_mut().zip(&self.data) {
                o[..n].copy_from_slice(&d[self.pos..self.pos + n]);
            }
            self.pos += n;
            Ok(n)
        }
    }

    /// Noise-like music stand-in: deterministic, uncorrelated with itself shifted.
    fn signal(frames: usize, seed: u32) -> Vec<f32> {
        let mut x = seed.wrapping_mul(2_654_435_761).max(1);
        (0..frames)
            .map(|i| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                let noise = (x as f32 / u32::MAX as f32) * 2.0 - 1.0;
                0.3 * noise + 0.2 * (i as f32 * 0.01 * seed as f32).sin()
            })
            .collect()
    }

    fn stereo(frames: usize) -> Vec<Vec<f32>> {
        vec![signal(frames, 1), signal(frames, 2)]
    }

    fn opts() -> CompareOptions {
        CompareOptions::new(48_000)
    }

    fn run(a: Vec<Vec<f32>>, b: Vec<Vec<f32>>, o: &CompareOptions) -> CompareReport {
        compare(mem(a), mem(b), o, &mut |_| true).unwrap()
    }

    #[test]
    fn identical_audio_nulls_exactly() {
        let s = stereo(200_000);
        let r = run(s.clone(), s, &opts());
        assert!(r.identical && r.nulls());
        assert_eq!((r.frames_a, r.frames_b), (200_000, 200_000));
        assert_eq!((r.peak, r.rms, r.differing_frames), (0.0, 0.0, 0));
        assert!(r.regions.is_empty());
        assert_eq!(r.null_depth_db(), None);
    }

    #[test]
    fn a_changed_stretch_is_found_exactly() {
        let a = stereo(300_000);
        let mut b = a.clone();
        for v in &mut b[1][100_000..100_500] {
            *v *= 0.5;
        }
        b[0][250_000] += 0.25;
        let r = run(a.clone(), b, &opts());
        assert!(!r.identical);
        assert_eq!(r.first_difference, Some(100_000));
        assert_eq!(r.last_difference, Some(250_000));
        assert_eq!(r.region_count, 2);
        assert_eq!(
            (r.regions[0].start, r.regions[0].end),
            (100_000, 100_500),
            "sample-exact"
        );
        assert_eq!((r.regions[1].start, r.regions[1].end), (250_000, 250_001));
        assert!((r.regions[1].peak - 0.25).abs() < 1e-6);
        assert_eq!(r.per_channel[0].differing_samples, 1);
        // Halving changes all 500 samples, bar any that are exactly 0.
        assert!(r.per_channel[1].differing_samples > 490);
        assert!(r.null_depth_db().unwrap() < -20.0);
    }

    #[test]
    fn nearby_differences_merge_into_one_region() {
        let a = stereo(100_000);
        let mut b = a.clone();
        b[0][10_000] += 0.1;
        b[0][10_000 + 4_800] += 0.1; // exactly the gap: still one region
        b[0][30_000] += 0.1;
        let r = run(a, b, &opts());
        assert_eq!(r.region_count, 2);
        assert_eq!((r.regions[0].start, r.regions[0].end), (10_000, 14_801));
    }

    #[test]
    fn the_threshold_ignores_tiny_differences() {
        let a = stereo(50_000);
        // Quantise to 16 bits: differences around -100 dBFS everywhere.
        let b: Vec<Vec<f32>> = a
            .iter()
            .map(|c| c.iter().map(|v| (v * 32768.0).round() / 32768.0).collect())
            .collect();
        let exact = run(a.clone(), b.clone(), &opts());
        assert!(!exact.identical && !exact.nulls());
        assert!(exact.peak <= 0.5 / 32768.0 + 1e-9);
        let depth = exact.null_depth_db().unwrap();
        // Quantisation noise (about -101 dBFS) under a signal of about -13 dBFS.
        assert!((-95.0..-80.0).contains(&depth), "{depth}");

        let mut o = opts();
        o.threshold = 10f32.powf(-90.0 / 20.0);
        let loose = run(a, b, &o);
        assert!(loose.nulls() && !loose.identical);
        assert!(loose.regions.is_empty());
        assert!(loose.peak > 0.0, "the figures still show the difference");
    }

    #[test]
    fn a_longer_side_differs_by_its_tail() {
        let a = stereo(100_000);
        let mut b = a.clone();
        for c in &mut b {
            c.extend(std::iter::repeat_n(0.0, 1000)); // trailing silence
        }
        let r = run(a.clone(), b, &opts());
        assert!(!r.identical, "lengths differ");
        assert_eq!(r.differing_frames, 0, "silence matches silence");
        assert!(!r.nulls());
        assert_eq!((r.frames_a, r.frames_b), (100_000, 101_000));

        let mut c = a.clone();
        for ch in &mut c {
            ch.truncate(90_000);
        }
        let r = run(a, c, &opts());
        assert_eq!(r.first_difference, Some(90_000), "the missing end shows");
    }

    #[test]
    fn mismatched_formats_are_refused() {
        let a = stereo(1000);
        let err = compare(
            mem(a.clone()),
            mem(vec![a[0].clone()]),
            &opts(),
            &mut |_| true,
        )
        .unwrap_err();
        assert!(err.to_string().contains("1-channel"), "{err}");
        let other = Box::new(Mem {
            data: a.clone(),
            pos: 0,
            rate: 44_100,
        });
        let err = compare(mem(a), other, &opts(), &mut |_| true).unwrap_err();
        assert!(err.to_string().contains("44100 Hz"), "{err}");
    }

    #[test]
    fn alignment_finds_a_delay_either_way() {
        let a = stereo(48_000 * 12);
        for delay in [1234usize, 1] {
            // b has `delay` frames of lead-in.
            let b: Vec<Vec<f32>> = a
                .iter()
                .map(|c| {
                    let mut v = vec![0.0; delay];
                    v.extend_from_slice(c);
                    v
                })
                .collect();
            let mut o = opts();
            o.align = true;
            let r = run(a.clone(), b.clone(), &o);
            let al = r.alignment.clone().unwrap();
            assert!(al.found);
            assert_eq!(al.offset, delay as i64);
            assert!(al.correlation > 0.99, "{al:?}");
            assert!(r.identical, "{r:?}");

            // The other way round: a starts later.
            let r = run(b, a.clone(), &o);
            assert_eq!(r.alignment.unwrap().offset, -(delay as i64));
            assert!(r.identical);
        }
    }

    #[test]
    fn alignment_gives_up_on_unrelated_audio() {
        let a = vec![signal(48_000 * 3, 7)];
        let b = vec![signal(48_000 * 3, 8)];
        let mut o = opts();
        o.align = true;
        let r = run(a, b, &o);
        let al = r.alignment.clone().unwrap();
        assert!(!al.found && al.offset == 0, "{al:?}");
        assert!(!r.nulls());

        let silent = vec![vec![0.0; 48_000]];
        let r = run(silent.clone(), silent, &o);
        assert!(!r.alignment.unwrap().found);
        assert!(r.identical);
    }

    #[test]
    fn cancelling_stops() {
        let s = stereo(300_000);
        let err = compare(mem(s.clone()), mem(s), &opts(), &mut |_| false).unwrap_err();
        assert!(matches!(err, Error::Cancelled));
    }
}
