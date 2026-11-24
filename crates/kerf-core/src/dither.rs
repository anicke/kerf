//! Float → integer conversion for export: TPDF dither, optional noise
//! shaping, and no dither where nothing would be lost.
//!
//! Dither is skipped for stretches (a few milliseconds per channel) whose
//! samples already sit exactly on the target grid: digital silence stays
//! truly silent, and unedited 16-bit audio exported at 16 bits comes out
//! bit-identical to the source instead of with fresh noise added.

use serde::{Deserialize, Serialize};

/// How to dither when reducing to an integer format.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum Dither {
    /// Round to the nearest step (adds distortion on quiet material).
    None,
    /// Triangular (TPDF) dither, ±1 LSB: distortion-free, flat noise.
    #[default]
    Tpdf,
    /// TPDF with first-order noise shaping: less noise where hearing is
    /// most sensitive, more near the top of the band. 16-bit only.
    Light,
    /// TPDF with psychoacoustic (F-weighted, 9th-order) noise shaping:
    /// about 25 dB less noise around 3–4 kHz. 16-bit at 44.1 or 48 kHz;
    /// elsewhere it falls back to light shaping.
    Strong,
}

impl Dither {
    pub const ALL: [Dither; 4] = [Self::None, Self::Tpdf, Self::Light, Self::Strong];

    pub fn label(self) -> &'static str {
        match self {
            Self::None => "Off",
            Self::Tpdf => "TPDF",
            Self::Light => "TPDF + light noise shaping",
            Self::Strong => "TPDF + strong noise shaping",
        }
    }

    pub fn is_on(self) -> bool {
        self != Self::None
    }

    /// What is actually applied at `bits` and `sample_rate`: shaping only
    /// at 16 bits (and below), strong only at 44.1/48 kHz.
    pub fn effective(self, bits: u16, sample_rate: u32) -> Dither {
        match self {
            Self::Light | Self::Strong if bits > 16 => Self::Tpdf,
            Self::Strong if !matches!(sample_rate, 44_100 | 48_000) => Self::Light,
            d => d,
        }
    }

    fn shaping(self) -> &'static [f64] {
        match self {
            Self::None | Self::Tpdf => &[],
            // Noise transfer 1 - z⁻¹: no noise at DC, +6 dB at Nyquist.
            Self::Light => &[1.0],
            // Wannamaker's 9-tap F-weighted filter (as in SoX), designed for
            // 44.1 kHz and close to optimal at 48 kHz.
            Self::Strong => &[
                2.412, -3.370, 3.937, -4.174, 3.353, -2.205, 1.281, -0.569, 0.0847,
            ],
        }
    }
}

/// Earlier settings stored dither as a bool: accept `true`/`false` too.
impl<'de> Deserialize<'de> for Dither {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Bool(bool),
            Name(String),
        }
        match Repr::deserialize(d)? {
            Repr::Bool(true) => Ok(Self::Tpdf),
            Repr::Bool(false) => Ok(Self::None),
            Repr::Name(n) => match n.as_str() {
                "none" | "off" => Ok(Self::None),
                "tpdf" => Ok(Self::Tpdf),
                "light" => Ok(Self::Light),
                "strong" => Ok(Self::Strong),
                _ => Err(serde::de::Error::custom(format!(
                    "unknown dither {n:?}: use none, tpdf, light or strong"
                ))),
            },
        }
    }
}

/// Samples per decision on whether a stretch needs dither (~5 ms).
const TILE: usize = 256;
const MAX_TAPS: usize = 9;

/// Converts float samples to `bits`-bit integers per channel, keeping each
/// channel's noise-shaping state between calls.
pub struct Quantizer {
    full_scale: f64,
    dither: bool,
    shaping: &'static [f64],
    rng: u64,
    /// Per channel: recent quantisation errors, newest first.
    errors: Vec<[f64; MAX_TAPS]>,
}

impl Quantizer {
    pub fn new(bits: u16, dither: Dither, sample_rate: u32, channels: usize) -> Self {
        let d = dither.effective(bits, sample_rate);
        Self {
            full_scale: (1i64 << (bits - 1)) as f64,
            dither: d.is_on(),
            shaping: d.shaping(),
            rng: 0x9E37_79B9_7F4A_7C15,
            errors: vec![[0.0; MAX_TAPS]; channels.max(1)],
        }
    }

    fn uniform(&mut self) -> f64 {
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        (self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Append `input` (channel `ch`) to `out` as integers.
    pub fn quantize(&mut self, ch: usize, input: &[f32], out: &mut Vec<i32>) {
        let (lo, hi) = (-self.full_scale, self.full_scale - 1.0);
        for tile in input.chunks(TILE) {
            let exact = !self.dither
                || tile.iter().all(|&x| {
                    let v = x as f64 * self.full_scale;
                    v == v.round() && (lo..=hi).contains(&v)
                });
            if exact {
                // Nothing to lose (or dither off): plain rounding, and the
                // shaping filter starts afresh after it.
                out.extend(
                    tile.iter()
                        .map(|&x| (x as f64 * self.full_scale).round().clamp(lo, hi) as i32),
                );
                if self.dither {
                    self.errors[ch] = [0.0; MAX_TAPS];
                }
                continue;
            }
            for &x in tile {
                let e = self.errors[ch];
                let feedback: f64 = self.shaping.iter().zip(&e).map(|(h, e)| h * e).sum();
                let v = x as f64 * self.full_scale - feedback;
                let d = self.uniform() - self.uniform();
                let q = (v + d).round();
                // The error fed back excludes clipping, which would otherwise
                // make the shaping filter run away on overloaded passages.
                let err = (q - v).clamp(-4.0, 4.0);
                let errs = &mut self.errors[ch];
                errs.copy_within(0..MAX_TAPS - 1, 1);
                errs[0] = err;
                out.push(q.clamp(lo, hi) as i32);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(d: Dither, bits: u16, sr: u32, input: &[f32]) -> Vec<i32> {
        let mut q = Quantizer::new(bits, d, sr, 1);
        let mut out = Vec::new();
        for chunk in input.chunks(1000) {
            q.quantize(0, chunk, &mut out);
        }
        out
    }

    /// A quiet tone that is not on the 16-bit grid.
    fn tone(n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| 0.01 * (i as f32 * 0.0712).sin() + 1e-6)
            .collect()
    }

    #[test]
    fn exact_audio_and_silence_pass_through_untouched() {
        // 16-bit source material, already on the grid.
        let on_grid: Vec<f32> = (0..5000)
            .map(|i| ((i * 37 % 2001) as f32 - 1000.0) / 32768.0)
            .collect();
        for d in Dither::ALL {
            let out = run(d, 16, 44_100, &on_grid);
            let want: Vec<i32> = on_grid.iter().map(|x| (x * 32768.0) as i32).collect();
            assert_eq!(out, want, "{d:?}");
        }
        // Silence after audio stays digitally silent.
        let mut x = tone(3000);
        x.extend(std::iter::repeat_n(0.0, 3000));
        let out = run(Dither::Strong, 16, 44_100, &x);
        let tail = &out[3000 + TILE..];
        assert!(tail.iter().all(|&s| s == 0), "silence dithered");
        assert!(out[..3000]
            .iter()
            .any(|&s| s != (x[0] * 32768.0).round() as i32));
    }

    #[test]
    fn tpdf_error_is_unbiased_and_small() {
        let x = tone(50_000);
        let out = run(Dither::Tpdf, 16, 44_100, &x);
        let errs: Vec<f64> = x
            .iter()
            .zip(&out)
            .map(|(&a, &b)| b as f64 - a as f64 * 32768.0)
            .collect();
        let mean = errs.iter().sum::<f64>() / errs.len() as f64;
        let rms = (errs.iter().map(|e| e * e).sum::<f64>() / errs.len() as f64).sqrt();
        assert!(mean.abs() < 0.02, "mean {mean}");
        // TPDF + rounding: 1/6 + 1/12 LSB² → 0.5 LSB RMS.
        assert!((rms - 0.5).abs() < 0.03, "rms {rms}");
        assert!(errs.iter().all(|e| e.abs() <= 1.5));
    }

    /// Error power in a band, by a plain DFT over the band's bins.
    fn band_power(errs: &[f64], sr: f64, lo: f64, hi: f64) -> f64 {
        let n = errs.len();
        let (k0, k1) = ((lo / sr * n as f64) as usize, (hi / sr * n as f64) as usize);
        (k0..k1)
            .map(|k| {
                let w = 2.0 * std::f64::consts::PI * k as f64 / n as f64;
                let (mut re, mut im) = (0.0, 0.0);
                for (i, e) in errs.iter().enumerate() {
                    re += e * (w * i as f64).cos();
                    im -= e * (w * i as f64).sin();
                }
                re * re + im * im
            })
            .sum::<f64>()
            / (k1 - k0) as f64
    }

    #[test]
    fn noise_shaping_moves_noise_out_of_the_sensitive_band() {
        let sr = 44_100.0;
        let x = tone(4096);
        let err = |d| {
            let out = run(d, 16, 44_100, &x);
            x.iter()
                .zip(&out)
                .map(|(&a, &b)| b as f64 - a as f64 * 32768.0)
                .collect::<Vec<_>>()
        };
        let (flat, light, strong) = (err(Dither::Tpdf), err(Dither::Light), err(Dither::Strong));
        let db = |a: f64, b: f64| 10.0 * (a / b).log10();
        // 2–5 kHz, where hearing is most sensitive.
        let mid = |e: &[f64]| band_power(e, sr, 2_000.0, 5_000.0);
        let top = |e: &[f64]| band_power(e, sr, 18_000.0, 21_000.0);
        assert!(
            db(mid(&light), mid(&flat)) < -5.0,
            "light: {}",
            db(mid(&light), mid(&flat))
        );
        assert!(
            db(mid(&strong), mid(&flat)) < -15.0,
            "strong: {}",
            db(mid(&strong), mid(&flat))
        );
        assert!(db(top(&strong), top(&flat)) > 10.0, "noise moved up");
    }

    #[test]
    fn shaping_survives_clipping() {
        let x: Vec<f32> = (0..20_000).map(|i| 1.5 * (i as f32 * 0.01).sin()).collect();
        let out = run(Dither::Strong, 16, 44_100, &x);
        // Back below full scale, the output follows the input again.
        let quiet: Vec<f32> = x.iter().map(|v| v * 0.1).collect();
        let mut q = Quantizer::new(16, Dither::Strong, 44_100, 1);
        let mut o = Vec::new();
        q.quantize(0, &x, &mut o);
        q.quantize(0, &quiet, &mut o);
        let tail = &o[x.len() + 1000..];
        for (y, v) in tail.iter().zip(&quiet[1000..]) {
            assert!((*y as f64 - *v as f64 * 32768.0).abs() < 40.0);
        }
        assert_eq!(out.len(), x.len());
    }

    #[test]
    fn effective_mode_and_old_settings() {
        assert_eq!(Dither::Strong.effective(24, 44_100), Dither::Tpdf);
        assert_eq!(Dither::Strong.effective(16, 96_000), Dither::Light);
        assert_eq!(Dither::Strong.effective(16, 48_000), Dither::Strong);
        let parse = |s: &str| serde_json::from_str::<Dither>(s).unwrap();
        assert_eq!(parse("true"), Dither::Tpdf);
        assert_eq!(parse("false"), Dither::None);
        assert_eq!(parse("\"strong\""), Dither::Strong);
        assert!(serde_json::from_str::<Dither>("\"loud\"").is_err());
    }
}
