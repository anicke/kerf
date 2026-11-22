//! Level measurement: sample peak, RMS and ITU-R BS.1770-4 / EBU R128
//! integrated loudness. Measurement is streaming, so arbitrarily long
//! material can be analysed in fixed memory.

use crate::util::gain_to_db;

/// Biquad in transposed direct form II, f64 for accuracy at low frequencies.
#[derive(Clone, Copy, Debug)]
struct Biquad {
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

    #[inline]
    fn process(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.z1;
        self.z1 = self.b[1] * x - self.a[1] * y + self.z2;
        self.z2 = self.b[2] * x - self.a[2] * y;
        y
    }
}

/// K-weighting filter pair (high shelf + RLB high-pass) for any sample rate.
/// Coefficients follow the analytic derivation used by libebur128, which
/// reproduces the BS.1770 reference coefficients exactly at 48 kHz.
fn k_weighting(sample_rate: f64) -> [Biquad; 2] {
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

/// Streaming level meter. Feed planar chunks with [`Meter::push`].
pub struct Meter {
    channels: usize,
    filters: Vec<[Biquad; 2]>,
    weights: Vec<f64>,
    peak: f32,
    sum_sq: f64,
    samples: u64,
    // Loudness gating: weighted mean-square energy per 100 ms segment.
    segment_len: usize,
    segment_pos: usize,
    segment_acc: f64,
    segments: Vec<f64>,
}

impl Meter {
    pub fn new(sample_rate: u32, channels: usize) -> Self {
        let channels = channels.max(1);
        let sr = sample_rate as f64;
        Self {
            channels,
            filters: (0..channels).map(|_| k_weighting(sr)).collect(),
            weights: (0..channels).map(|i| channel_weight(i, channels)).collect(),
            peak: 0.0,
            sum_sq: 0.0,
            samples: 0,
            segment_len: ((sr * 0.1).round() as usize).max(1),
            segment_pos: 0,
            segment_acc: 0.0,
            segments: Vec::new(),
        }
    }

    /// Feed `frames` frames from planar channel slices.
    pub fn push<C: AsRef<[f32]>>(&mut self, chunk: &[C], frames: usize) {
        assert!(chunk.len() >= self.channels);
        for i in 0..frames {
            let mut weighted = 0.0;
            for (ch, data) in chunk.iter().enumerate().take(self.channels) {
                let s = data.as_ref()[i];
                let a = s.abs();
                if a > self.peak {
                    self.peak = a;
                }
                let sd = s as f64;
                self.sum_sq += sd * sd;
                let [f1, f2] = &mut self.filters[ch];
                let k = f2.process(f1.process(sd));
                weighted += self.weights[ch] * k * k;
            }
            self.segment_acc += weighted;
            self.segment_pos += 1;
            if self.segment_pos == self.segment_len {
                self.segments
                    .push(self.segment_acc / self.segment_len as f64);
                self.segment_acc = 0.0;
                self.segment_pos = 0;
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

    /// Integrated loudness in LUFS with absolute (-70) and relative (-10 LU)
    /// gating. `None` if shorter than one 400 ms block or entirely gated out.
    pub fn integrated_lufs(&self) -> Option<f64> {
        let blocks: Vec<f64> = self
            .segments
            .windows(4)
            .map(|w| w.iter().sum::<f64>() / 4.0)
            .collect();
        let loudness = |z: f64| -0.691 + 10.0 * z.log10();
        let abs_gated: Vec<f64> = blocks
            .into_iter()
            .filter(|&z| z > 0.0 && loudness(z) > -70.0)
            .collect();
        if abs_gated.is_empty() {
            return None;
        }
        let mean = abs_gated.iter().sum::<f64>() / abs_gated.len() as f64;
        let relative_gate = loudness(mean) - 10.0;
        let rel_gated: Vec<f64> = abs_gated
            .into_iter()
            .filter(|&z| loudness(z) > relative_gate)
            .collect();
        if rel_gated.is_empty() {
            return None;
        }
        Some(loudness(
            rel_gated.iter().sum::<f64>() / rel_gated.len() as f64,
        ))
    }
}

/// Convenience: measure a whole buffer.
pub fn measure(buf: &crate::AudioBuffer) -> Meter {
    let mut m = Meter::new(buf.sample_rate(), buf.num_channels());
    m.push(buf.channels(), buf.frames());
    m
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
        let buf = sine(48_000, 440.0, 0.3, 3.0, 2);
        let whole = measure(&buf);
        let mut chunked = Meter::new(48_000, 2);
        let mut pos = 0;
        while pos < buf.frames() {
            let n = 1013.min(buf.frames() - pos);
            let slices: Vec<&[f32]> = buf.channels().iter().map(|c| &c[pos..pos + n]).collect();
            chunked.push(&slices, n);
            pos += n;
        }
        assert_eq!(whole.integrated_lufs(), chunked.integrated_lufs());
        assert_eq!(whole.peak(), chunked.peak());
    }
}
