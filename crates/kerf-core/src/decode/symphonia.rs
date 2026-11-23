//! The built-in decoder, based on Symphonia.

use std::fs::File;
use std::path::{Path, PathBuf};

use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

use super::Decoder;
use crate::error::{Error, Result};

/// The built-in decoder (Symphonia): WAV, FLAC, MP3, Vorbis, AAC-LC, ALAC,
/// AIFF, CAF. Decodes packet by packet, in constant memory.
pub struct SymphoniaDecoder {
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

impl SymphoniaDecoder {
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
}

impl Decoder for SymphoniaDecoder {
    fn backend(&self) -> &'static str {
        "symphonia"
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> usize {
        self.channels
    }

    /// Total length in decoded frames, if the container states it (for progress).
    fn total_frames(&self) -> Option<u64> {
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
    fn limitation(&self) -> Option<String> {
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

    /// The next chunk of planar audio, or `None` at the end of the stream.
    fn next_chunk(&mut self) -> Result<Option<&[Vec<f32>]>> {
        let has = if self.pending {
            self.pending = false;
            true
        } else {
            self.decode_next()?
        };
        Ok(has.then_some(&self.planar[..]))
    }
}
