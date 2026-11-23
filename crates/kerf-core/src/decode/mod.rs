//! Decoding audio files, through interchangeable backends.
//!
//! A [`Decoder`] streams a file as planar 32-bit float chunks. Two backends
//! implement it:
//!
//! * the built-in decoder ([`symphonia`]), always available: WAV, FLAC, MP3,
//!   Vorbis, AAC-LC, ALAC, AIFF, CAF;
//! * an external FFmpeg ([`ffmpeg`]), when installed: everything else,
//!   including HE-AAC (common for radio recordings), AC-3, WMA, APE and
//!   WavPack.
//!
//! [`open`] uses the built-in decoder whenever it can decode a file fully,
//! and FFmpeg when it can't open the file or can only decode part of it
//! (HE-AAC's upper band). Which FFmpeg is used is set with [`configure`]
//! (the app's preferences) or the `KERF_FFMPEG` environment variable: a path
//! to `ffmpeg`, or `off`.

pub mod ffmpeg;
pub mod symphonia;

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::buffer::AudioBuffer;
use crate::error::{Error, Result};
pub use ffmpeg::Ffmpeg;
pub use symphonia::SymphoniaDecoder;

/// A decoded audio stream.
pub trait Decoder: Send {
    /// Which backend decodes it (`"symphonia"`, `"ffmpeg"`); part of cache keys.
    fn backend(&self) -> &'static str;
    /// Rate of the delivered samples.
    fn sample_rate(&self) -> u32;
    fn channels(&self) -> usize;
    /// Total length in frames, if known up front (for progress).
    fn total_frames(&self) -> Option<u64>;
    /// What this decoder can't reproduce of the file, if anything.
    fn limitation(&self) -> Option<String> {
        None
    }
    /// The next chunk (one `Vec` per channel), or `None` at the end.
    fn next_chunk(&mut self) -> Result<Option<&[Vec<f32>]>>;
}

/// Which FFmpeg to use.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FfmpegSetting {
    /// Search the PATH.
    #[default]
    Auto,
    /// This `ffmpeg` executable (with `ffprobe` beside it).
    Path(PathBuf),
    /// Never use FFmpeg.
    Off,
}

impl FfmpegSetting {
    /// `KERF_FFMPEG`: a path, or `off` / `0` / `none`. Overrides other settings.
    pub fn from_env() -> Option<Self> {
        let v = std::env::var_os("KERF_FFMPEG")?;
        let s = v.to_string_lossy();
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Self::Auto,
            "off" | "0" | "none" | "no" => Self::Off,
            _ => Self::Path(PathBuf::from(&v)),
        })
    }

    /// The FFmpeg this setting finds, if any.
    pub fn locate(&self) -> Option<Ffmpeg> {
        match self {
            Self::Auto => Ffmpeg::find(None),
            Self::Path(p) => Ffmpeg::find(Some(p)),
            Self::Off => None,
        }
    }
}

static FFMPEG: RwLock<Option<Option<Arc<Ffmpeg>>>> = RwLock::new(None);

/// Choose the FFmpeg to use (the `KERF_FFMPEG` environment variable, when
/// set, takes precedence). Returns what was found.
pub fn configure(setting: &FfmpegSetting) -> Option<Arc<Ffmpeg>> {
    let setting = FfmpegSetting::from_env().unwrap_or_else(|| setting.clone());
    let found = setting.locate().map(Arc::new);
    *FFMPEG.write().unwrap_or_else(|e| e.into_inner()) = Some(found.clone());
    found
}

/// The FFmpeg in use (located on first use if [`configure`] wasn't called).
pub fn ffmpeg() -> Option<Arc<Ffmpeg>> {
    if let Some(f) = FFMPEG.read().unwrap_or_else(|e| e.into_inner()).as_ref() {
        return f.clone();
    }
    configure(&FfmpegSetting::Auto)
}

/// Open `path` with the best available decoder.
pub fn open(path: &Path) -> Result<Box<dyn Decoder>> {
    open_with(path, ffmpeg().as_deref())
}

/// [`open`] with an explicit FFmpeg (or none).
pub fn open_with(path: &Path, ffmpeg: Option<&Ffmpeg>) -> Result<Box<dyn Decoder>> {
    let builtin = SymphoniaDecoder::open(path);
    if matches!(&builtin, Ok(d) if d.limitation().is_none()) {
        return builtin.map(|d| Box::new(d) as Box<dyn Decoder>);
    }
    let Some(ff) = ffmpeg else {
        return match builtin {
            Ok(d) => Ok(Box::new(d)),
            Err(Error::Decode { path, message }) => Err(Error::Decode {
                path,
                message: format!("{message} (installing FFmpeg may help)"),
            }),
            Err(e) => Err(e),
        };
    };
    match (ff.open(path), builtin) {
        (Ok(d), _) => Ok(Box::new(d)),
        // FFmpeg failed too: a partial decode is better than nothing.
        (Err(_), Ok(d)) => Ok(Box::new(d)),
        (Err(ff_err), Err(builtin_err)) => Err(match (builtin_err, ff_err) {
            (Error::Decode { path, message }, Error::Decode { message: m2, .. }) => Error::Decode {
                path,
                message: format!("{message}; {m2}"),
            },
            (e, _) => e,
        }),
    }
}

/// What can't be decoded faithfully from `path`, if anything. Cheap: only
/// the start of the file is decoded.
pub fn decode_limitation(path: &Path) -> Option<String> {
    let limitation = open(path).ok()?.limitation()?;
    Some(if ffmpeg().is_none() {
        format!("{limitation} (install FFmpeg for full quality)")
    } else {
        limitation
    })
}

/// Decode an entire audio file to 32-bit float at its native sample rate.
///
/// Integer PCM up to 24 bits and 32-bit float data convert losslessly.
pub fn decode_file(path: &Path) -> Result<AudioBuffer> {
    let mut dec = open(path)?;
    let (rate, nch) = (dec.sample_rate(), dec.channels());
    let mut channels = vec![Vec::new(); nch];
    while let Some(chunk) = dec.next_chunk()? {
        for (dst, src) in channels.iter_mut().zip(chunk) {
            dst.extend_from_slice(src);
        }
    }
    Ok(AudioBuffer::new(rate, channels))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::{write_wav, ExportSettings};

    fn drain(d: &mut dyn Decoder) -> Vec<Vec<f32>> {
        let mut out = vec![Vec::new(); d.channels()];
        while let Some(c) = d.next_chunk().unwrap() {
            for (o, s) in out.iter_mut().zip(c) {
                o.extend_from_slice(s);
            }
        }
        out
    }

    fn signal(frames: usize) -> Vec<Vec<f32>> {
        let l: Vec<f32> = (0..frames)
            .map(|i| ((i as f32) * 0.013).sin() * 0.7)
            .collect();
        let r: Vec<f32> = l.iter().map(|v| -v * 0.5).collect();
        vec![l, r]
    }

    /// FFmpeg, if installed here; tests needing it are skipped otherwise.
    fn ff() -> Option<Ffmpeg> {
        let f = Ffmpeg::find(None);
        if f.is_none() {
            eprintln!("FFmpeg not found: skipping");
        }
        f
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
        let dec = open_with(&path, None).unwrap();
        assert_eq!(dec.backend(), "symphonia");
        assert_eq!(dec.sample_rate(), 24_000);
        assert_eq!(dec.total_frames(), Some(24_000));
        assert_eq!(dec.limitation(), None);
    }

    #[test]
    fn complete_files_use_the_builtin_decoder_even_with_ffmpeg() {
        let Some(ff) = ff() else { return };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.wav");
        write_wav(
            &path,
            &AudioBuffer::new(48_000, signal(1000)),
            ExportSettings::default(),
        )
        .unwrap();
        assert_eq!(open_with(&path, Some(&ff)).unwrap().backend(), "symphonia");
    }

    #[test]
    fn ffmpeg_decodes_float_wav_bit_exactly() {
        let Some(ff) = ff() else { return };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.wav");
        let data = signal(100_000); // several chunks
        write_wav(
            &path,
            &AudioBuffer::new(44_100, data.clone()),
            ExportSettings::default(),
        )
        .unwrap();
        let mut d = ff.open(&path).unwrap();
        assert_eq!((d.sample_rate(), d.channels()), (44_100, 2));
        assert_eq!(d.total_frames(), Some(100_000));
        assert_eq!(drain(&mut d), data);
        assert!(ff.version().unwrap().starts_with("ffmpeg"));
    }

    #[test]
    fn formats_the_builtin_decoder_lacks_fall_back_to_ffmpeg() {
        let Some(ff) = ff() else { return };
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("a.wav");
        write_wav(
            &wav,
            &AudioBuffer::new(48_000, signal(48_000)),
            ExportSettings::default(),
        )
        .unwrap();
        // AC-3, which only FFmpeg decodes.
        let ac3 = dir.path().join("a.ac3");
        let made = std::process::Command::new(&ff.ffmpeg)
            .args(["-v", "error", "-y", "-i"])
            .arg(&wav)
            .args(["-c:a", "ac3", "-b:a", "192k"])
            .arg(&ac3)
            .status()
            .unwrap();
        assert!(made.success());

        let err = open_with(&ac3, None).err().unwrap().to_string();
        assert!(err.contains("installing FFmpeg may help"), "{err}");

        let mut d = open_with(&ac3, Some(&ff)).unwrap();
        assert_eq!(d.backend(), "ffmpeg");
        assert_eq!((d.sample_rate(), d.channels()), (48_000, 2));
        let out = drain(d.as_mut());
        // AC-3 is lossy and frame-based: the length is close, the sound similar.
        assert!(out[0].len().abs_diff(48_000) <= 1536, "{}", out[0].len());
        let peak = out[0].iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!((peak - 0.7).abs() < 0.05, "peak {peak}");
    }

    #[test]
    fn ffmpeg_failures_are_reported() {
        let Some(ff) = ff() else { return };
        let dir = tempfile::tempdir().unwrap();
        let junk = dir.path().join("junk.wav");
        std::fs::write(&junk, b"this is not audio at all").unwrap();
        let err = ff.open(&junk).err().unwrap().to_string();
        assert!(err.contains("FFmpeg"), "{err}");
        let err = open_with(&junk, Some(&ff)).err().unwrap().to_string();
        assert!(
            err.contains("FFmpeg"),
            "both decoders' reasons are given: {err}"
        );
    }

    #[test]
    fn stopping_early_ends_the_process() {
        let Some(ff) = ff() else { return };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("long.wav");
        write_wav(
            &path,
            &AudioBuffer::new(48_000, signal(480_000)),
            ExportSettings::default(),
        )
        .unwrap();
        let mut d = ff.open(&path).unwrap();
        d.next_chunk().unwrap();
        drop(d); // must not hang or leave ffmpeg running
    }

    #[test]
    fn settings_from_the_environment_value() {
        assert_eq!(FfmpegSetting::Off.locate(), None);
        assert_eq!(FfmpegSetting::Path("/no/such/ffmpeg".into()).locate(), None);
        let s: FfmpegSetting = serde_json::from_str(r#"{"path": "/opt/ffmpeg"}"#).unwrap();
        assert_eq!(s, FfmpegSetting::Path("/opt/ffmpeg".into()));
        assert_eq!(
            serde_json::to_string(&FfmpegSetting::Auto).unwrap(),
            r#""auto""#
        );
    }

    /// HE-AAC (AAC + SBR): the container states twice the rate the built-in
    /// decoder delivers. Built in: the delivered rate (right speed), with a
    /// limitation; with FFmpeg: the full rate. Needs a real file, e.g. a radio
    /// recording: `KERF_HE_AAC_SAMPLE=show.m4a cargo test -- --ignored he_aac`.
    #[test]
    #[ignore]
    fn he_aac_plays_at_the_right_speed() {
        let path = std::env::var_os("KERF_HE_AAC_SAMPLE").expect("set KERF_HE_AAC_SAMPLE");
        let path = Path::new(&path);
        let mut d = open_with(path, None).unwrap();
        let expected = d.total_frames().unwrap();
        let frames = drain(d.as_mut())[0].len() as u64;
        assert!(frames.abs_diff(expected) <= 4096, "{frames} vs {expected}");
        assert!(d.limitation().unwrap().contains("HE-AAC"));

        if let Some(ff) = Ffmpeg::find(None) {
            let full = open_with(path, Some(&ff)).unwrap();
            assert_eq!(full.backend(), "ffmpeg");
            assert_eq!(full.sample_rate(), 2 * d.sample_rate());
            assert_eq!(full.limitation(), None);
        }
    }
}
