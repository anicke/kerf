//! Decoding through an external FFmpeg (`ffmpeg` and `ffprobe`), for what
//! the built-in decoder lacks: HE-AAC (SBR and parametric stereo), AC-3,
//! E-AC-3, WMA, APE, WavPack, DTS and more.
//!
//! `ffprobe` reports the stream format; `ffmpeg` then streams the audio as
//! raw 32-bit float, read chunk by chunk, so memory use stays constant. Its
//! decoders also trim encoder delay and padding (gapless playback data).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::thread::JoinHandle;

use super::Decoder;
use crate::error::{Error, Result};

/// Frames per chunk handed out by [`FfmpegDecoder`].
const CHUNK_FRAMES: usize = 32_768;

/// A located FFmpeg installation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ffmpeg {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

fn search_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(exe(name)))
        .find(|p| p.is_file())
}

fn command(program: &Path) -> Command {
    let mut c = Command::new(program);
    c.stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

impl Ffmpeg {
    /// Use the `ffmpeg` at `ffmpeg` (with `ffprobe` beside it, or on the
    /// PATH), or search the PATH for both when `None`.
    pub fn find(ffmpeg: Option<&Path>) -> Option<Self> {
        let ffmpeg = match ffmpeg {
            Some(p) => p.is_file().then(|| p.to_path_buf())?,
            None => search_path("ffmpeg")?,
        };
        let beside = ffmpeg.with_file_name(exe("ffprobe"));
        let ffprobe = if beside.is_file() {
            beside
        } else {
            search_path("ffprobe")?
        };
        Some(Self { ffmpeg, ffprobe })
    }

    /// The first line of `ffmpeg -version`, e.g. "ffmpeg version 7.1".
    pub fn version(&self) -> Option<String> {
        let out = command(&self.ffmpeg).arg("-version").output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let line = text.lines().next()?;
        // "ffmpeg version 7.1 Copyright (c) …" → "ffmpeg version 7.1"
        Some(
            line.split(" Copyright")
                .next()
                .unwrap_or(line)
                .trim()
                .to_string(),
        )
    }

    /// Start decoding the first audio stream of `path`.
    pub fn open(&self, path: &Path) -> Result<FfmpegDecoder> {
        let derr = |message: String| Error::Decode {
            path: path.to_path_buf(),
            message: format!("FFmpeg: {message}"),
        };
        let (rate, channels, duration) = self.probe(path).map_err(derr)?;
        let mut child = command(&self.ffmpeg)
            .args(["-nostdin", "-hide_banner", "-v", "error", "-i"])
            .arg(path)
            .args([
                "-map",
                "0:a:0",
                "-vn",
                "-sn",
                "-dn",
                "-f",
                "f32le",
                "-acodec",
                "pcm_f32le",
            ])
            .args([
                "-ar",
                &rate.to_string(),
                "-ac",
                &channels.to_string(),
                "pipe:1",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| derr(format!("could not run {}: {e}", self.ffmpeg.display())))?;
        let stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let errors = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = stderr.read_to_string(&mut s);
            s
        });
        let mut dec = FfmpegDecoder {
            path: path.to_path_buf(),
            child: Some(child),
            stdout,
            errors: Some(errors),
            sample_rate: rate,
            channels,
            total_frames: duration.map(|d| (d * rate as f64).round() as u64),
            bytes: Vec::new(),
            planar: vec![Vec::new(); channels],
            pending: false,
        };
        // Decode the first chunk now, so failures show up when opening.
        dec.pending = dec.read_chunk()?;
        if !dec.pending {
            return Err(derr("the file contains no audio".into()));
        }
        Ok(dec)
    }

    /// Sample rate, channel count and duration (seconds) of the first audio stream.
    fn probe(&self, path: &Path) -> std::result::Result<(u32, usize, Option<f64>), String> {
        let out = command(&self.ffprobe)
            .args(["-v", "error", "-select_streams", "a:0"])
            .args([
                "-show_entries",
                "stream=sample_rate,channels:format=duration",
            ])
            .args(["-of", "json"])
            .arg(path)
            .output()
            .map_err(|e| format!("could not run {}: {e}", self.ffprobe.display()))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(last_line(&err).unwrap_or("ffprobe failed").to_string());
        }
        let v: serde_json::Value =
            serde_json::from_slice(&out.stdout).map_err(|e| format!("ffprobe output: {e}"))?;
        let stream = v["streams"]
            .get(0)
            .ok_or_else(|| "no audio track".to_string())?;
        let rate = stream["sample_rate"]
            .as_str()
            .and_then(|r| r.parse::<u32>().ok())
            .filter(|&r| r > 0)
            .ok_or_else(|| "unknown sample rate".to_string())?;
        let channels = stream["channels"]
            .as_u64()
            .filter(|&c| c > 0)
            .ok_or_else(|| "unknown channel count".to_string())? as usize;
        let duration = v["format"]["duration"]
            .as_str()
            .and_then(|d| d.parse::<f64>().ok())
            .filter(|d| d.is_finite() && *d > 0.0);
        Ok((rate, channels, duration))
    }
}

fn last_line(s: &str) -> Option<&str> {
    s.lines().map(str::trim).rfind(|l| !l.is_empty())
}

/// Audio streamed from an `ffmpeg` process.
pub struct FfmpegDecoder {
    path: PathBuf,
    child: Option<Child>,
    stdout: ChildStdout,
    errors: Option<JoinHandle<String>>,
    sample_rate: u32,
    channels: usize,
    total_frames: Option<u64>,
    bytes: Vec<u8>,
    planar: Vec<Vec<f32>>,
    /// First chunk, read during `open`.
    pending: bool,
}

impl FfmpegDecoder {
    /// Read the next chunk into `self.planar`; `false` at the end.
    fn read_chunk(&mut self) -> Result<bool> {
        let frame_bytes = self.channels * 4;
        self.bytes.resize(CHUNK_FRAMES * frame_bytes, 0);
        let mut filled = 0;
        while filled < self.bytes.len() {
            match self.stdout.read(&mut self.bytes[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        let frames = filled / frame_bytes;
        if frames == 0 {
            self.finish()?;
            return Ok(false);
        }
        for (c, ch) in self.planar.iter_mut().enumerate() {
            ch.clear();
            ch.extend((0..frames).map(|i| {
                let o = (i * self.channels + c) * 4;
                f32::from_le_bytes(self.bytes[o..o + 4].try_into().unwrap())
            }));
        }
        Ok(true)
    }

    /// Wait for ffmpeg to exit and turn a failure into an error.
    fn finish(&mut self) -> Result<()> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        let status = child.wait()?;
        let errors = self
            .errors
            .take()
            .and_then(|h| h.join().ok())
            .unwrap_or_default();
        if status.success() {
            Ok(())
        } else {
            Err(Error::Decode {
                path: self.path.clone(),
                message: format!(
                    "FFmpeg: {}",
                    last_line(&errors).unwrap_or("decoding failed")
                ),
            })
        }
    }
}

impl Drop for FfmpegDecoder {
    fn drop(&mut self) {
        // Stopped early (cancelled, or an error elsewhere): end the process.
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Decoder for FfmpegDecoder {
    fn backend(&self) -> &'static str {
        "ffmpeg"
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> usize {
        self.channels
    }

    fn total_frames(&self) -> Option<u64> {
        self.total_frames
    }

    fn next_chunk(&mut self) -> Result<Option<&[Vec<f32>]>> {
        let has = if self.pending {
            self.pending = false;
            true
        } else {
            self.read_chunk()?
        };
        Ok(has.then_some(&self.planar[..]))
    }
}
