//! Level measurement: sample peak, RMS and ITU-R BS.1770-4 / EBU R128
//! integrated loudness. Measurement is streaming, so arbitrarily long
//! material can be analysed in fixed memory.

use crate::util::gain_to_db;

/// Biquad in transposed direct form II, f64 for accuracy at low frequencies.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Biquad {
    b: [f64; 3],
    a: [f64; 3],
    z1: f64,
    z2: f64,
}

impl Biquad {
    fn new(b: [f64; 3], a: [f64; 3]) -> Self {
        Self {
            b,
            a,
            z1: 0.0,
            z2: 0.0,
        }
    }

    /// Second-order high-pass (RBJ cookbook, Butterworth Q).
    pub(crate) fn highpass(sample_rate: f64, hz: f64) -> Self {
        let w = 2.0 * std::f64::consts::PI * hz / sample_rate;
        let (cos, alpha) = (w.cos(), w.sin() / (2.0 * std::f64::consts::FRAC_1_SQRT_2));
        let a0 = 1.0 + alpha;
        Self::new(
            [
                (1.0 + cos) / 2.0 / a0,
                -(1.0 + cos) / a0,
                (1.0 + cos) / 2.0 / a0,
            ],
            [1.0, -2.0 * cos / a0, (1.0 - alpha) / a0],
        )
    }

    /// Second-order low-pass (RBJ cookbook, Butterworth Q).
    pub(crate) fn lowpass(sample_rate: f64, hz: f64) -> Self {
        let w = 2.0 * std::f64::consts::PI * hz / sample_rate;
        let (cos, alpha) = (w.cos(), w.sin() / (2.0 * std::f64::consts::FRAC_1_SQRT_2));
        let a0 = 1.0 + alpha;
        Self::new(
            [
                (1.0 - cos) / 2.0 / a0,
                (1.0 - cos) / a0,
                (1.0 - cos) / 2.0 / a0,
            ],
            [1.0, -2.0 * cos / a0, (1.0 - alpha) / a0],
        )
    }

    #[inline]
    pub(crate) fn process(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.z1;
        self.z1 = self.b[1] * x - self.a[1] * y + self.z2;
        self.z2 = self.b[2] * x - self.a[2] * y;
        y
    }
}

/// K-weighting filter pair (high shelf + RLB high-pass) for any sample rate.
/// Coefficients follow the analytic derivation used by libebur128, which
/// reproduces the BS.1770 reference coefficients exactly at 48 kHz.
pub(crate) fn k_weighting(sample_rate: f64) -> [Biquad; 2] {
    use std::f64::consts::PI;
    // Stage 1: high shelf.
    let f0 = 1681.974450955533;
    let g = 3.999843853973347;
    let q = 0.7071752369554196;
    let k = (PI * f0 / sample_rate).tan();
    let vh = 10f64.powf(g / 20.0);
    let vb = vh.powf(0.4996667741545416);
    let a0 = 1.0 + k / q + k * k;
    let shelf = Biquad::new(
        [
            (vh + vb * k / q + k * k) / a0,
            2.0 * (k * k - vh) / a0,
            (vh - vb * k / q + k * k) / a0,
        ],
        [1.0, 2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
    );
    // Stage 2: RLB high-pass.
    let f0 = 38.13547087602444;
    let q = 0.5003270373238773;
    let k = (PI * f0 / sample_rate).tan();
    let a0 = 1.0 + k / q + k * k;
    let hp = Biquad::new(
        [1.0, -2.0, 1.0],
        [1.0, 2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
    );
    [shelf, hp]
}

/// BS.1770 channel weight. Assumes SMPTE order (L R C LFE Ls Rs) for 5.1.
fn channel_weight(index: usize, channels: usize) -> f64 {
    if channels == 6 {
        [1.0, 1.0, 1.0, 0.0, 1.41, 1.41][index]
    } else {
        1.0
    }
}

/// Loudness of a mean-square energy (BS.1770).
fn energy_to_lufs(z: f64) -> f64 {
    -0.691 + 10.0 * z.log10()
}

/// Turns samples into 100 ms K-weighted energies, the building block of
/// every BS.1770 / EBU R128 measure. Real-time safe (no allocation), so the
/// audio thread can feed a live meter with it.
#[derive(Clone)]
pub struct SegmentFilter {
    filters: Vec<[Biquad; 2]>,
    weights: Vec<f64>,
    segment_len: usize,
    pos: usize,
    acc: f64,
}

impl SegmentFilter {
    pub fn new(sample_rate: u32, channels: usize) -> Self {
        let channels = channels.max(1);
        let sr = sample_rate as f64;
        Self {
            filters: (0..channels).map(|_| k_weighting(sr)).collect(),
            weights: (0..channels).map(|i| channel_weight(i, channels)).collect(),
            segment_len: ((sr * 0.1).round() as usize).max(1),
            pos: 0,
            acc: 0.0,
        }
    }

    pub fn channels(&self) -> usize {
        self.filters.len()
    }

    /// Feed one sample of channel `ch` of the current frame.
    #[inline]
    pub fn sample(&mut self, ch: usize, s: f32) {
        let [f1, f2] = &mut self.filters[ch];
        let k = f2.process(f1.process(s as f64));
        self.acc += self.weights[ch] * k * k;
    }

    /// Finish the current frame; returns a segment's energy every 100 ms.
    #[inline]
    pub fn end_frame(&mut self) -> Option<f64> {
        self.pos += 1;
        if self.pos < self.segment_len {
            return None;
        }
        let z = self.acc / self.segment_len as f64;
        self.pos = 0;
        self.acc = 0.0;
        Some(z)
    }
}

/// Loudness statistics over a run of 100 ms segment energies (from
/// [`SegmentFilter`]): EBU R128 momentary (400 ms), short-term (3 s),
/// integrated and loudness range.
#[derive(Clone, Debug, Default)]
pub struct Loudness {
    segments: Vec<f64>,
}

/// Segments in a momentary (400 ms) and a short-term (3 s) window.
const MOMENTARY: usize = 4;
const SHORT_TERM: usize = 30;

impl Loudness {
    pub fn push(&mut self, segment_energy: f64) {
        self.segments.push(segment_energy);
    }

    pub fn clear(&mut self) {
        self.segments.clear();
    }

    /// Measured time in seconds.
    pub fn seconds(&self) -> f64 {
        self.segments.len() as f64 * 0.1
    }

    fn windows(&self, n: usize) -> impl Iterator<Item = f64> + '_ {
        self.segments
            .windows(n)
            .map(move |w| w.iter().sum::<f64>() / n as f64)
    }

    fn last(&self, n: usize) -> Option<f64> {
        let w = self.segments.get(self.segments.len().checked_sub(n)?..)?;
        let z = w.iter().sum::<f64>() / n as f64;
        (z > 0.0).then(|| energy_to_lufs(z))
    }

    /// Momentary loudness of the last 400 ms (LUFS).
    pub fn momentary(&self) -> Option<f64> {
        self.last(MOMENTARY)
    }

    /// Short-term loudness of the last 3 s (LUFS).
    pub fn short_term(&self) -> Option<f64> {
        self.last(SHORT_TERM)
    }

    fn max_of(&self, n: usize) -> Option<f64> {
        self.windows(n)
            .filter(|&z| z > 0.0)
            .max_by(f64::total_cmp)
            .map(energy_to_lufs)
    }

    /// Loudest momentary loudness (LUFS).
    pub fn max_momentary(&self) -> Option<f64> {
        self.max_of(MOMENTARY)
    }

    /// Loudest short-term loudness (LUFS).
    pub fn max_short_term(&self) -> Option<f64> {
        self.max_of(SHORT_TERM)
    }

    /// Integrated loudness in LUFS with absolute (-70) and relative (-10 LU)
    /// gating. `None` if shorter than one 400 ms block or entirely gated out.
    pub fn integrated(&self) -> Option<f64> {
        gated_mean(self.windows(MOMENTARY), 10.0).map(energy_to_lufs)
    }

    /// Loudness range (EBU Tech 3342) in LU: the spread between the 10th and
    /// 95th percentile of short-term loudness, after gating at -70 LUFS and
    /// 20 LU below the mean. `None` for less than 3 s of audible material.
    pub fn range(&self) -> Option<f64> {
        let abs: Vec<f64> = self
            .windows(SHORT_TERM)
            .filter(|&z| z > 0.0 && energy_to_lufs(z) > -70.0)
            .collect();
        if abs.is_empty() {
            return None;
        }
        let mean = abs.iter().sum::<f64>() / abs.len() as f64;
        let gate = energy_to_lufs(mean) - 20.0;
        let mut l: Vec<f64> = abs
            .into_iter()
            .map(energy_to_lufs)
            .filter(|&l| l > gate)
            .collect();
        l.sort_by(f64::total_cmp);
        let pct = |p: f64| l[((l.len() - 1) as f64 * p).round() as usize];
        Some(pct(0.95) - pct(0.10))
    }
}

/// Mean energy of `blocks` after the absolute (-70 LUFS) gate and a relative
/// gate `relative_lu` below the absolute-gated mean.
fn gated_mean(blocks: impl Iterator<Item = f64>, relative_lu: f64) -> Option<f64> {
    let abs: Vec<f64> = blocks
        .filter(|&z| z > 0.0 && energy_to_lufs(z) > -70.0)
        .collect();
    if abs.is_empty() {
        return None;
    }
    let mean = abs.iter().sum::<f64>() / abs.len() as f64;
    let gate = energy_to_lufs(mean) - relative_lu;
    let (sum, n) = abs
        .iter()
        .filter(|&&z| energy_to_lufs(z) > gate)
        .fold((0.0, 0usize), |(s, n), &z| (s + z, n + 1));
    (n > 0).then(|| sum / n as f64)
}

/// True-peak detector (BS.1770-4 Annex 2): oversamples 4× (2× at 96 kHz and
/// up, not at all from 192 kHz) with a 12-tap-per-phase interpolator to
/// find peaks that fall between samples.
#[derive(Clone)]
pub struct TruePeak {
    /// Interpolator taps, oldest sample first: `taps[j][phase]` weighs the
    /// `j`th sample of the window. The phases are computed side by side
    /// (SIMD lanes), with unused phases left zero.
    taps: [[f32; MAX_PHASES]; TAPS_PER_PHASE],
    /// Per channel: the last `TAPS_PER_PHASE - 1` samples, then room for a
    /// chunk, so each window is a contiguous slice.
    work: Vec<Vec<f32>>,
    peak: [f32; MAX_PHASES],
    /// Largest gain any phase can give (sum of its absolute taps): output
    /// never exceeds the loudest input sample in the window times this.
    bound: f32,
}

const TAPS_PER_PHASE: usize = 12;
const MAX_PHASES: usize = 4;
const TAIL: usize = TAPS_PER_PHASE - 1;

impl TruePeak {
    pub fn new(sample_rate: u32, channels: usize) -> Self {
        let factor = match sample_rate {
            0..96_000 => 4,
            96_000..192_000 => 2,
            _ => 1,
        };
        let mut taps = [[0.0; MAX_PHASES]; TAPS_PER_PHASE];
        if factor == 1 {
            taps[TAIL][0] = 1.0;
        } else {
            for (p, phase) in interpolator(factor).iter().enumerate() {
                // `phase[k]` weighs the sample `k` steps back.
                for (k, &t) in phase.iter().enumerate() {
                    taps[TAIL - k][p] = t;
                }
            }
        }
        let bound = (0..MAX_PHASES)
            .map(|p| taps.iter().map(|t| t[p].abs()).sum::<f32>())
            .fold(0.0, f32::max);
        Self {
            taps,
            work: vec![vec![0.0; TAIL]; channels.max(1)],
            peak: [0.0; MAX_PHASES],
            bound,
        }
    }

    /// Feed consecutive samples of channel `ch`.
    pub fn push(&mut self, ch: usize, samples: &[f32]) {
        let work = &mut self.work[ch];
        work.truncate(TAIL);
        work.extend_from_slice(samples);
        let mut peak = self.peak;
        // In blocks: skip those too quiet to raise the peak (most of them,
        // once the loud parts are found).
        const BLOCK: usize = 64;
        let windows = work.len().saturating_sub(TAIL);
        for start in (0..windows).step_by(BLOCK) {
            let span = &work[start..(start + BLOCK + TAIL).min(work.len())];
            let loudest = span.iter().fold(0.0f32, |m, x| m.max(x.abs()));
            let current = peak.into_iter().fold(0.0, f32::max);
            if loudest * self.bound <= current {
                continue;
            }
            for w in span.windows(TAPS_PER_PHASE) {
                let mut y = [0.0f32; MAX_PHASES];
                for (taps, &x) in self.taps.iter().zip(w) {
                    for p in 0..MAX_PHASES {
                        y[p] += taps[p] * x;
                    }
                }
                for p in 0..MAX_PHASES {
                    peak[p] = peak[p].max(y[p].abs());
                }
            }
        }
        self.peak = peak;
        let n = work.len();
        work.copy_within(n - TAIL.., 0);
    }

    /// Highest true peak so far (linear).
    pub fn peak(&self) -> f32 {
        self.peak.into_iter().fold(0.0, f32::max)
    }
}

/// Windowed-sinc (Kaiser, β = 4) interpolator for `factor`× oversampling,
/// split into polyphase branches whose taps each sum to 1. Flat within
/// ±0.05 dB to 0.35 × the sample rate, images 34 dB down.
fn interpolator(factor: usize) -> Vec<[f32; TAPS_PER_PHASE]> {
    fn bessel_i0(x: f64) -> f64 {
        let (mut sum, mut term) = (1.0, 1.0);
        for k in 1..30 {
            term *= (x / 2.0 / k as f64).powi(2);
            sum += term;
        }
        sum
    }
    let n = TAPS_PER_PHASE * factor;
    let beta = 4.0;
    let h: Vec<f64> = (0..n)
        .map(|i| {
            let x = (i as f64 - (n - 1) as f64 / 2.0) / factor as f64;
            let sinc = if x == 0.0 {
                1.0
            } else {
                (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x)
            };
            let r = 2.0 * i as f64 / (n - 1) as f64 - 1.0;
            sinc * bessel_i0(beta * (1.0 - r * r).sqrt()) / bessel_i0(beta)
        })
        .collect();
    (0..factor)
        .map(|p| {
            let taps: Vec<f64> = (0..TAPS_PER_PHASE).map(|k| h[p + k * factor]).collect();
            let sum: f64 = taps.iter().sum();
            std::array::from_fn(|k| (taps[k] / sum) as f32)
        })
        .collect()
}

/// Streaming level meter: sample peak, RMS, clipping, loudness and
/// optionally true peak. Feed planar chunks with [`Meter::push`].
pub struct Meter {
    channels: usize,
    filter: SegmentFilter,
    loudness: Loudness,
    true_peak: Option<TruePeak>,
    peak: f32,
    sum_sq: f64,
    samples: u64,
    clipped: u64,
}

impl Meter {
    pub fn new(sample_rate: u32, channels: usize) -> Self {
        let channels = channels.max(1);
        Self {
            channels,
            filter: SegmentFilter::new(sample_rate, channels),
            loudness: Loudness::default(),
            true_peak: None,
            peak: 0.0,
            sum_sq: 0.0,
            samples: 0,
            clipped: 0,
        }
    }

    /// Also measure the true peak (about three times the work).
    pub fn with_true_peak(mut self, sample_rate: u32) -> Self {
        self.true_peak = Some(TruePeak::new(sample_rate, self.channels));
        self
    }

    /// Feed `frames` frames from planar channel slices.
    pub fn push<C: AsRef<[f32]>>(&mut self, chunk: &[C], frames: usize) {
        assert!(chunk.len() >= self.channels);
        for i in 0..frames {
            for (ch, data) in chunk.iter().enumerate().take(self.channels) {
                let s = data.as_ref()[i];
                let a = s.abs();
                if a > self.peak {
                    self.peak = a;
                }
                if a > 1.0 {
                    self.clipped += 1;
                }
                let sd = s as f64;
                self.sum_sq += sd * sd;
                self.filter.sample(ch, s);
            }
            if let Some(z) = self.filter.end_frame() {
                self.loudness.push(z);
            }
        }
        if let Some(tp) = &mut self.true_peak {
            for (ch, data) in chunk.iter().enumerate().take(self.channels) {
                tp.push(ch, &data.as_ref()[..frames]);
            }
        }
        self.samples += (frames * self.channels) as u64;
    }

    /// Absolute sample peak (linear).
    pub fn peak(&self) -> f32 {
        self.peak
    }

    pub fn peak_db(&self) -> f64 {
        gain_to_db(self.peak as f64)
    }

    /// True peak in dBTP, if enabled with [`with_true_peak`](Self::with_true_peak).
    /// Never below the sample peak.
    pub fn true_peak_db(&self) -> Option<f64> {
        self.true_peak
            .as_ref()
            .map(|tp| gain_to_db(tp.peak().max(self.peak) as f64))
    }

    /// Samples above 0 dBFS (they clip in integer formats).
    pub fn clipped_samples(&self) -> u64 {
        self.clipped
    }

    /// RMS across all channels (linear).
    pub fn rms(&self) -> f64 {
        if self.samples == 0 {
            0.0
        } else {
            (self.sum_sq / self.samples as f64).sqrt()
        }
    }

    pub fn rms_db(&self) -> f64 {
        gain_to_db(self.rms())
    }

    /// Integrated loudness in LUFS; see [`Loudness::integrated`].
    pub fn integrated_lufs(&self) -> Option<f64> {
        self.loudness.integrated()
    }

    /// Momentary, short-term and range measures.
    pub fn loudness(&self) -> &Loudness {
        &self.loudness
    }
}

/// Convenience: measure a whole buffer.
pub fn measure(buf: &crate::AudioBuffer) -> Meter {
    let mut m = Meter::new(buf.sample_rate(), buf.num_channels());
    m.push(buf.channels(), buf.frames());
    m
}

/// Measure `start..end` of the mix of `opts`' tracks, with true peak.
/// `progress` gets 0..=1 and returns `false` to cancel.
pub fn measure_range(
    project: &crate::Project,
    start: crate::Frame,
    end: crate::Frame,
    opts: &crate::render::RenderOptions,
    progress: crate::render::Progress,
) -> crate::Result<Meter> {
    let mut meter = Meter::new(project.sample_rate, project.channels as usize)
        .with_true_peak(project.sample_rate);
    let total = end.saturating_sub(start).max(1) as f64;
    let mut done = 0usize;
    crate::render::for_each_chunk(project, start, end, opts, |chunk, n| {
        meter.push(chunk, n);
        done += n;
        Ok(progress(done as f64 / total))
    })?;
    Ok(meter)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AudioBuffer;

    fn sine(sr: u32, freq: f64, amp: f64, secs: f64, channels: usize) -> AudioBuffer {
        let n = (sr as f64 * secs) as usize;
        let ch: Vec<f32> = (0..n)
            .map(|i| {
                (amp * (2.0 * std::f64::consts::PI * freq * i as f64 / sr as f64).sin()) as f32
            })
            .collect();
        AudioBuffer::new(sr, vec![ch; channels])
    }

    #[test]
    fn peak_and_rms_of_sine() {
        let m = measure(&sine(48_000, 1000.0, 0.5, 1.0, 2));
        assert!((m.peak() - 0.5).abs() < 1e-3);
        assert!((m.rms() - 0.5 / 2f64.sqrt()).abs() < 1e-3);
    }

    /// EBU Tech 3341 test case 1: stereo 1 kHz sine at -23 dBFS → -23.0 LUFS (±0.1).
    #[test]
    fn ebu_3341_case_1_at_multiple_rates() {
        let amp = 10f64.powf(-23.0 / 20.0);
        for sr in [44_100, 48_000, 96_000] {
            let lufs = measure(&sine(sr, 1000.0, amp, 20.0, 2))
                .integrated_lufs()
                .unwrap();
            assert!((lufs - -23.0).abs() < 0.1, "{sr} Hz: {lufs}");
        }
    }

    /// EBU Tech 3341 test case 2: -33 dBFS → -33.0 LUFS.
    #[test]
    fn ebu_3341_case_2() {
        let amp = 10f64.powf(-33.0 / 20.0);
        let lufs = measure(&sine(48_000, 1000.0, amp, 20.0, 2))
            .integrated_lufs()
            .unwrap();
        assert!((lufs - -33.0).abs() < 0.1, "{lufs}");
    }

    #[test]
    fn silence_has_no_loudness() {
        let m = measure(&AudioBuffer::silent(48_000, 2, 96_000));
        assert_eq!(m.integrated_lufs(), None);
        assert_eq!(m.peak(), 0.0);
    }

    #[test]
    fn chunked_equals_whole() {
        // Rising, so the true peak keeps moving and blocks aren't skipped.
        let n = 3 * 48_000;
        let ch: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f64 / 48_000.0;
                (0.3 * (i as f64 / n as f64) * (2.0 * std::f64::consts::PI * 9_000.0 * t).sin())
                    as f32
            })
            .collect();
        let buf = AudioBuffer::new(48_000, vec![ch.clone(), ch]);
        let whole = with_tp(&buf);
        let mut chunked = Meter::new(48_000, 2).with_true_peak(48_000);
        let mut pos = 0;
        while pos < buf.frames() {
            let n = 1013.min(buf.frames() - pos);
            let slices: Vec<&[f32]> = buf.channels().iter().map(|c| &c[pos..pos + n]).collect();
            chunked.push(&slices, n);
            pos += n;
        }
        assert_eq!(whole.integrated_lufs(), chunked.integrated_lufs());
        assert_eq!(whole.peak(), chunked.peak());
        assert_eq!(whole.true_peak_db(), chunked.true_peak_db());
        assert!(whole.true_peak_db().unwrap() > whole.peak_db());
    }

    fn sine_phase(sr: u32, freq: f64, amp: f64, phase: f64, secs: f64) -> AudioBuffer {
        let n = (sr as f64 * secs) as usize;
        let ch: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f64 / sr as f64;
                (amp * (2.0 * std::f64::consts::PI * freq * t + phase).sin()) as f32
            })
            .collect();
        AudioBuffer::new(sr, vec![ch.clone(), ch])
    }

    fn with_tp(buf: &AudioBuffer) -> Meter {
        let mut m =
            Meter::new(buf.sample_rate(), buf.num_channels()).with_true_peak(buf.sample_rate());
        m.push(buf.channels(), buf.frames());
        m
    }

    /// A quarter-rate sine sampled 45° off its crests: samples reach only
    /// 0.707 of the real peak (-3 dB), which true peak recovers.
    #[test]
    fn true_peak_between_samples() {
        for sr in [44_100, 48_000] {
            let amp = 0.5;
            let m = with_tp(&sine_phase(
                sr,
                sr as f64 / 4.0,
                amp,
                std::f64::consts::FRAC_PI_4,
                1.0,
            ));
            let real = gain_to_db(amp);
            assert!(
                (m.peak_db() - (real - 3.01)).abs() < 0.05,
                "{sr}: {}",
                m.peak_db()
            );
            let tp = m.true_peak_db().unwrap();
            assert!((tp - real).abs() < 0.2, "{sr} Hz: true peak {tp} vs {real}");
        }
        // 96 kHz oversamples only 2×: still exact for audible frequencies.
        let tone = sine_phase(96_000, 12_000.0, 0.5, std::f64::consts::FRAC_PI_8, 1.0);
        let m = with_tp(&tone);
        assert!(m.peak_db() < gain_to_db(0.5) - 0.6);
        assert!((m.true_peak_db().unwrap() - gain_to_db(0.5)).abs() < 0.2);
        // Low frequencies: true peak equals the sample peak.
        let m = with_tp(&sine_phase(48_000, 997.0, 0.5, 0.3, 1.0));
        assert!((m.true_peak_db().unwrap() - m.peak_db()).abs() < 0.05);
        assert_eq!(
            measure(&sine(48_000, 1000.0, 0.5, 1.0, 2)).true_peak_db(),
            None
        );
    }

    #[test]
    fn momentary_and_short_term_of_steady_tone() {
        let amp = 10f64.powf(-20.0 / 20.0);
        let m = measure(&sine(48_000, 1000.0, amp, 5.0, 2));
        let l = m.loudness();
        for v in [
            l.momentary(),
            l.short_term(),
            l.max_momentary(),
            l.max_short_term(),
        ] {
            assert!((v.unwrap() - -20.0).abs() < 0.1, "{v:?}");
        }
        let short = measure(&sine(48_000, 1000.0, amp, 2.0, 2));
        assert_eq!(short.loudness().short_term(), None, "needs 3 s");
        assert!(short.loudness().momentary().is_some());
    }

    /// EBU Tech 3342 cases 1 and 2: 20 s at -20 LUFS then 20 s at -30 (or
    /// -15) gives a loudness range of 10 (or 5) LU, ±1.
    #[test]
    fn ebu_3342_loudness_range() {
        for (second, want) in [(-30.0, 10.0), (-15.0, 5.0)] {
            let a = sine(48_000, 1000.0, 10f64.powf(-20.0 / 20.0), 20.0, 2);
            let b = sine(48_000, 1000.0, 10f64.powf(second / 20.0), 20.0, 2);
            let mut m = Meter::new(48_000, 2);
            m.push(a.channels(), a.frames());
            m.push(b.channels(), b.frames());
            let lra = m.loudness().range().unwrap();
            assert!((lra - want).abs() < 1.0, "{second}: LRA {lra}");
        }
        let steady = measure(&sine(48_000, 1000.0, 0.1, 10.0, 2));
        assert!(steady.loudness().range().unwrap() < 0.1);
    }

    #[test]
    fn clipped_samples_counted() {
        let m = measure(&sine(48_000, 1000.0, 1.2, 0.1, 1));
        assert!(m.clipped_samples() > 0);
        assert_eq!(
            measure(&sine(48_000, 1000.0, 0.9, 0.1, 1)).clipped_samples(),
            0
        );
    }
}
