//! Writing WAV; decoding lives in [`crate::decode`] (re-exported here).

pub use crate::decode::{decode_file, decode_limitation};

use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::store::Samples;

/// File extensions offered in import dialogs. Those the built-in decoder
/// can't read (AC-3, WMA, APE, WavPack…) need FFmpeg (see [`crate::decode`]).
pub const IMPORT_EXTENSIONS: &[&str] = &[
    "wav", "wave", "flac", "mp3", "ogg", "oga", "opus", "m4a", "mp4", "aac", "aif", "aiff", "caf",
    "mka", "webm", "ac3", "eac3", "wma", "ape", "wv", "dts", "amr", "m4b", "mov", "mkv",
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SampleFormat {
    Int16,
    Int24,
    Int32,
    #[default]
    Float32,
}

impl SampleFormat {
    pub const ALL: [SampleFormat; 4] = [Self::Int16, Self::Int24, Self::Int32, Self::Float32];

    pub fn label(self) -> &'static str {
        match self {
            Self::Int16 => "16-bit PCM",
            Self::Int24 => "24-bit PCM",
            Self::Int32 => "32-bit PCM",
            Self::Float32 => "32-bit float",
        }
    }

    pub fn bits(self) -> u16 {
        match self {
            Self::Int16 => 16,
            Self::Int24 => 24,
            Self::Int32 | Self::Float32 => 32,
        }
    }

    pub fn is_float(self) -> bool {
        self == Self::Float32
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportSettings {
    pub format: SampleFormat,
    /// TPDF dither when reducing to integer formats.
    pub dither: bool,
    /// Convert to this sample rate on export (`None`: the project rate).
    #[serde(default)]
    pub sample_rate: Option<u32>,
    /// Convert to this many channels on export (`None`: the project's).
    /// Mono becomes stereo by duplication; stereo becomes mono by averaging.
    #[serde(default)]
    pub channels: Option<u16>,
    /// File type (WAV or FLAC).
    #[serde(default)]
    pub container: crate::encode::Container,
}

impl ExportSettings {
    /// Red Book audio CD: 16-bit, 44.1 kHz stereo, dithered.
    pub const AUDIO_CD: ExportSettings = ExportSettings {
        format: SampleFormat::Int16,
        dither: true,
        sample_rate: Some(44_100),
        channels: Some(2),
        container: crate::encode::Container::Wav,
    };

    /// Lossless album: FLAC 24-bit at the project's rate and channels.
    pub const FLAC_ALBUM: ExportSettings = ExportSettings {
        format: SampleFormat::Int24,
        dither: true,
        sample_rate: None,
        channels: None,
        container: crate::encode::Container::Flac,
    };

    /// Output sample rate for a project at `project_rate`.
    pub fn rate_for(&self, project_rate: u32) -> u32 {
        self.sample_rate.unwrap_or(project_rate)
    }

    /// Output channel count for a project with `project_channels`.
    pub fn channels_for(&self, project_channels: u16) -> u16 {
        self.channels.unwrap_or(project_channels).max(1)
    }

    /// Whether these settings produce Red Book CD audio from a project.
    pub fn is_audio_cd(&self, project_rate: u32, project_channels: u16) -> bool {
        self.container == crate::encode::Container::Wav
            && self.format == SampleFormat::Int16
            && self.dither
            && self.rate_for(project_rate) == 44_100
            && self.channels_for(project_channels) == 2
    }
}

impl Default for ExportSettings {
    fn default() -> Self {
        Self {
            format: SampleFormat::Float32,
            dither: true,
            sample_rate: None,
            channels: None,
            container: crate::encode::Container::Wav,
        }
    }
}

/// Small, fast PRNG for dither (xorshift64*).
struct Rng(u64);

impl Rng {
    fn next_f64(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let v = self.0.wrapping_mul(0x2545_F491_4F6C_DD1D);
        (v >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Streaming WAV writer. Writes to a temporary file that is renamed into place
/// on [`finalize`](Self::finalize), so failed or cancelled exports never leave
/// a truncated file behind.
pub struct WavWriter {
    inner: Option<hound::WavWriter<BufWriter<File>>>,
    settings: ExportSettings,
    channels: usize,
    rng: Rng,
    tmp_path: PathBuf,
    path: PathBuf,
}

impl WavWriter {
    pub fn create(
        path: &Path,
        sample_rate: u32,
        channels: u16,
        settings: ExportSettings,
    ) -> Result<Self> {
        let spec = hound::WavSpec {
            channels,
            sample_rate,
            bits_per_sample: settings.format.bits(),
            sample_format: if settings.format.is_float() {
                hound::SampleFormat::Float
            } else {
                hound::SampleFormat::Int
            },
        };
        let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
        tmp_name.push(".partial");
        let tmp_path = path.with_file_name(tmp_name);
        let inner =
            hound::WavWriter::create(&tmp_path, spec).map_err(|e| Error::Encode(e.to_string()))?;
        Ok(Self {
            inner: Some(inner),
            settings,
            channels: channels as usize,
            rng: Rng(0x9E37_79B9_7F4A_7C15),
            tmp_path,
            path: path.to_path_buf(),
        })
    }

    pub fn write_planar<C: AsRef<[f32]>>(&mut self, chunk: &[C], frames: usize) -> Result<()> {
        let w = self.inner.as_mut().expect("writer used after finalize");
        let enc = |e: hound::Error| Error::Encode(e.to_string());
        match self.settings.format {
            SampleFormat::Float32 => {
                for i in 0..frames {
                    for ch in chunk.iter().take(self.channels) {
                        w.write_sample(ch.as_ref()[i]).map_err(enc)?;
                    }
                }
            }
            fmt => {
                let full_scale = (1i64 << (fmt.bits() - 1)) as f64;
                let (min, max) = (-full_scale, full_scale - 1.0);
                for i in 0..frames {
                    for ch in chunk.iter().take(self.channels) {
                        let mut v = ch.as_ref()[i] as f64 * full_scale;
                        if self.settings.dither {
                            // Triangular PDF, ±1 LSB peak.
                            v += self.rng.next_f64() - self.rng.next_f64();
                        }
                        let q = v.round().clamp(min, max) as i32;
                        w.write_sample(q).map_err(enc)?;
                    }
                }
            }
        }
        Ok(())
    }

    pub fn finalize(mut self) -> Result<()> {
        let w = self.inner.take().unwrap();
        w.finalize().map_err(|e| Error::Encode(e.to_string()))?;
        fs::rename(&self.tmp_path, &self.path)?;
        Ok(())
    }

    /// Discard the partially written file.
    pub fn abort(mut self) {
        drop(self.inner.take());
        let _ = fs::remove_file(&self.tmp_path);
    }
}

impl Drop for WavWriter {
    fn drop(&mut self) {
        if self.inner.take().is_some() {
            let _ = fs::remove_file(&self.tmp_path);
        }
    }
}

/// Write a whole buffer as WAV (convenience for tests and saving media).
pub fn write_wav(
    path: &Path,
    audio: &(impl Samples + ?Sized),
    settings: ExportSettings,
) -> Result<()> {
    let mut w = WavWriter::create(
        path,
        audio.sample_rate(),
        audio.num_channels() as u16,
        settings,
    )?;
    // Chunked so mapped audio is streamed rather than paged in all at once.
    let channels = audio.channel_slices();
    let mut pos = 0;
    while pos < audio.frames() {
        let n = 65_536.min(audio.frames() - pos);
        let chunk: Vec<&[f32]> = channels.iter().map(|c| &c[pos..pos + n]).collect();
        w.write_planar(&chunk, n)?;
        pos += n;
    }
    w.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::buffer::AudioBuffer;

    fn test_signal() -> AudioBuffer {
        // Includes values above full scale and tiny values: float must keep both exactly.
        let l: Vec<f32> = (0..4800).map(|i| ((i as f32) * 0.01).sin() * 1.7).collect();
        let r: Vec<f32> = (0..4800)
            .map(|i| ((i as f32) * 0.02).cos() * 1e-6)
            .collect();
        AudioBuffer::new(48_000, vec![l, r])
    }

    #[test]
    fn float32_roundtrip_is_bit_exact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.wav");
        let buf = test_signal();
        write_wav(
            &path,
            &buf,
            ExportSettings {
                format: SampleFormat::Float32,
                dither: false,
                ..Default::default()
            },
        )
        .unwrap();
        let back = decode_file(&path).unwrap();
        assert_eq!(back, buf);
        assert!(!dir.path().join("f.wav.partial").exists());
    }

    #[test]
    fn int24_roundtrip_within_one_lsb() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("i.wav");
        let l: Vec<f32> = (0..4800).map(|i| ((i as f32) * 0.01).sin() * 0.9).collect();
        let buf = AudioBuffer::new(44_100, vec![l]);
        write_wav(
            &path,
            &buf,
            ExportSettings {
                format: SampleFormat::Int24,
                dither: false,
                ..Default::default()
            },
        )
        .unwrap();
        let back = decode_file(&path).unwrap();
        assert_eq!(back.sample_rate(), 44_100);
        let lsb = 1.0 / (1 << 23) as f32;
        for (a, b) in back.channel(0).iter().zip(buf.channel(0)) {
            assert!((a - b).abs() <= lsb, "{a} vs {b}");
        }
    }

    #[test]
    fn int16_clips_and_dithers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.wav");
        let buf = AudioBuffer::new(48_000, vec![vec![2.0, -2.0, 0.0, 0.0]]);
        write_wav(
            &path,
            &buf,
            ExportSettings {
                format: SampleFormat::Int16,
                dither: true,
                ..Default::default()
            },
        )
        .unwrap();
        let back = decode_file(&path).unwrap();
        assert!(back.channel(0)[0] > 0.999);
        assert_eq!(back.channel(0)[1], -1.0);
        assert!(back.channel(0)[2].abs() <= 1.0 / 32768.0 + 1e-9);
    }
}
