//! Decoding audio files and writing WAV.

use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

use crate::buffer::AudioBuffer;
use crate::error::{Error, Result};
use crate::store::Samples;

/// File extensions offered in import dialogs.
pub const IMPORT_EXTENSIONS: &[&str] = &[
    "wav", "wave", "flac", "mp3", "ogg", "oga", "opus", "m4a", "mp4", "aac", "aif", "aiff", "caf",
    "mka", "webm",
];

/// Decodes a file packet by packet into planar f32 chunks, so arbitrarily
/// long files can be processed in constant memory.
pub struct StreamDecoder {
    path: PathBuf,
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    /// The rate of the decoded samples (which may differ from the header's).
    sample_rate: u32,
    /// The rate the container states.
    header_rate: Option<u32>,
    channels: usize,
    /// Length in frames at the container's rate.
    header_frames: Option<u64>,
    interleaved: Vec<f32>,
    planar: Vec<Vec<f32>>,
    /// First chunk, decoded during `open` to learn the stream format.
    pending: bool,
    done: bool,
}

impl StreamDecoder {
    pub fn open(path: &Path) -> Result<Self> {
        let derr = |message: String| Error::Decode {
            path: path.to_path_buf(),
            message,
        };
        let file = File::open(path)?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        let format = symphonia::default::get_probe()
            .probe(
                &hint,
                mss,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .map_err(|e| derr(e.to_string()))?;
        let track = format
            .default_track(TrackType::Audio)
            .ok_or_else(|| derr("no audio track".into()))?;
        let track_id = track.id;
        let header_frames = track.num_frames;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or_else(|| derr("unsupported codec".into()))?
            .clone();
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(&params, &AudioDecoderOptions::default())
            .map_err(|e| derr(e.to_string()))?;
        let mut me = Self {
            path: path.to_path_buf(),
            format,
            decoder,
            track_id,
            // Taken from the first decoded packet: containers can disagree
            // with the codec (HE-AAC states the full rate, but a decoder
            // without SBR delivers half of it).
            sample_rate: 0,
            header_rate: params.sample_rate.filter(|&r| r > 0),
            channels: params.channels.as_ref().map_or(0, |c| c.count()),
            header_frames,
            interleaved: Vec::new(),
            planar: Vec::new(),
            pending: false,
            done: false,
        };
        // Decode the first packet so the real output format is known up front.
        me.pending = me.decode_next()?;
        if !me.pending || me.sample_rate == 0 || me.channels == 0 {
            return Err(derr("file contains no audio".into()));
        }
        Ok(me)
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Total length in decoded frames, if the container states it (for progress).
    pub fn total_frames(&self) -> Option<u64> {
        let frames = self.header_frames?;
        Some(match self.header_rate {
            Some(h) if h != self.sample_rate => {
                (frames as u128 * self.sample_rate as u128 / h as u128) as u64
            }
            _ => frames,
        })
    }

    /// A warning when the file holds more than can be decoded: the
    /// container states a higher rate than the decoder delivers. This is
    /// HE-AAC (AAC with SBR), whose upper frequencies are reconstructed by
    /// an extension the built-in decoder doesn't support; the audio plays
    /// at the right speed but lacks content above half the decoded rate.
    pub fn limitation(&self) -> Option<String> {
        let header = self.header_rate?;
        (header > self.sample_rate).then(|| {
            format!(
                "{} is HE-AAC: decoded at {} Hz instead of {} Hz, so frequencies above {:.1} kHz are missing",
                self.path
                    .file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
                self.sample_rate,
                header,
                self.sample_rate as f64 / 2000.0
            )
        })
    }

    fn derr(&self, message: String) -> Error {
        Error::Decode {
            path: self.path.clone(),
            message,
        }
    }

    /// Decode the next packet into `self.planar`. Returns `false` at the end.
    fn decode_next(&mut self) -> Result<bool> {
        loop {
            if self.done {
                return Ok(false);
            }
            let packet = match self.format.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => {
                    self.done = true;
                    return Ok(false);
                }
                Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    self.done = true;
                    return Ok(false);
                }
                Err(e) => return Err(self.derr(e.to_string())),
            };
            if packet.track_id != self.track_id {
                continue;
            }
            let decoded = match self.decoder.decode(&packet) {
                Ok(buf) => buf,
                // Corrupt packet: skip it, as players do.
                Err(SymError::DecodeError(_)) => continue,
                Err(e) => return Err(self.derr(e.to_string())),
            };
            let spec = decoded.spec();
            let nch = spec.channels().count().max(1);
            if self.sample_rate == 0 {
                self.sample_rate = spec.rate();
            } else if spec.rate() != self.sample_rate {
                return Err(self.derr("sample rate changed mid-stream".into()));
            }
            if self.channels == 0 {
                self.channels = nch;
            }
            if nch != self.channels {
                return Err(self.derr("channel count changed mid-stream".into()));
            }
            decoded.copy_to_vec_interleaved(&mut self.interleaved);
            let frames = self.interleaved.len() / nch;
            if frames == 0 {
                continue;
            }
            self.planar.resize(nch, Vec::new());
            for (c, ch) in self.planar.iter_mut().enumerate() {
                ch.clear();
                ch.extend(self.interleaved.iter().skip(c).step_by(nch));
            }
            return Ok(true);
        }
    }

    /// The next chunk of planar audio, or `None` at the end of the stream.
    pub fn next_chunk(&mut self) -> Result<Option<&[Vec<f32>]>> {
        let has = if self.pending {
            self.pending = false;
            true
        } else {
            self.decode_next()?
        };
        Ok(has.then_some(&self.planar[..]))
    }
}

/// What can't be decoded faithfully from `path`, if anything (see
/// [`StreamDecoder::limitation`]). Cheap: decodes only the first packet.
pub fn decode_limitation(path: &Path) -> Option<String> {
    StreamDecoder::open(path).ok()?.limitation()
}

/// Decode an entire audio file to 32-bit float at its native sample rate.
///
/// Integer PCM up to 24 bits and 32-bit float data convert losslessly.
pub fn decode_file(path: &Path) -> Result<AudioBuffer> {
    let mut dec = StreamDecoder::open(path)?;
    let (rate, nch) = (dec.sample_rate(), dec.channels());
    let mut channels = vec![Vec::new(); nch];
    while let Some(chunk) = dec.next_chunk()? {
        for (dst, src) in channels.iter_mut().zip(chunk) {
            dst.extend_from_slice(src);
        }
    }
    Ok(AudioBuffer::new(rate, channels))
}

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

    #[test]
    fn rate_and_length_come_from_the_decoded_audio() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.wav");
        write_wav(
            &path,
            &AudioBuffer::new(24_000, vec![vec![0.25; 24_000]; 2]),
            ExportSettings::default(),
        )
        .unwrap();
        let dec = StreamDecoder::open(&path).unwrap();
        assert_eq!(dec.sample_rate(), 24_000);
        assert_eq!(dec.total_frames(), Some(24_000));
        assert_eq!(dec.limitation(), None);
        assert_eq!(decode_limitation(&path), None);
    }

    /// HE-AAC (AAC + SBR): the container states twice the rate the decoder
    /// delivers. Decoding must use the delivered rate (correct speed and
    /// length) and say what's missing. Needs a real file, e.g. a radio
    /// recording: `KERF_HE_AAC_SAMPLE=show.m4a cargo test -- --ignored he_aac`.
    #[test]
    #[ignore]
    fn he_aac_plays_at_the_right_speed() {
        let path = std::env::var_os("KERF_HE_AAC_SAMPLE").expect("set KERF_HE_AAC_SAMPLE");
        let path = Path::new(&path);
        let mut dec = StreamDecoder::open(path).unwrap();
        let rate = dec.sample_rate();
        let expected = dec.total_frames().unwrap();
        let mut frames = 0u64;
        while let Some(c) = dec.next_chunk().unwrap() {
            frames += c[0].len() as u64;
        }
        // The decoded length matches the container's duration (to a frame of 2048).
        assert!(
            frames.abs_diff(expected) <= 2048,
            "{frames} vs {expected} at {rate} Hz"
        );
        assert!(dec.limitation().unwrap().contains("HE-AAC"));
    }
}
