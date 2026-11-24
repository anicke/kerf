//! Writing audio files: WAV and FLAC, with tags for FLAC.
//!
//! FLAC is encoded frame by frame and streamed to disk, so even a two-hour
//! single-file album uses constant memory. The header (which records the total
//! length and an MD5 of the audio) is written as a placeholder first and
//! filled in when the file is finished.

use std::fs::{self, File};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use flacenc::component::{BitRepr, Stream};
use flacenc::error::Verify;
use flacenc::source::{Context, Fill, FrameBuf};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::io::{ExportSettings, SampleFormat, WavWriter};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Container {
    #[default]
    Wav,
    /// Lossless and compressed, with tags. 16 or 24-bit.
    Flac,
}

impl Container {
    pub const ALL: [Container; 2] = [Self::Wav, Self::Flac];

    pub fn label(self) -> &'static str {
        match self {
            Self::Wav => "WAV",
            Self::Flac => "FLAC",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Flac => "flac",
        }
    }

    /// Sample formats this container can hold.
    pub fn supports(self, f: SampleFormat) -> bool {
        match self {
            Self::Wav => true,
            Self::Flac => matches!(f, SampleFormat::Int16 | SampleFormat::Int24),
        }
    }
}

/// Descriptive tags written into exported files (FLAC Vorbis comments).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tags {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub date: String,
    pub genre: String,
    pub comment: String,
    /// Track number and total.
    pub track: Option<(u32, u32)>,
    pub isrc: String,
    /// Front cover image (JPEG or PNG).
    pub cover: Option<PathBuf>,
}

use crate::dither::Quantizer;

const FLAC_BLOCK: usize = 4096;

/// Streaming FLAC encoder writing to a temporary file renamed into place on
/// [`finalize`](Self::finalize).
pub struct FlacWriter {
    file: Option<BufWriter<File>>,
    stream: Stream,
    config: flacenc::error::Verified<flacenc::config::Encoder>,
    framebuf: FrameBuf,
    context: Context,
    pending: Vec<i32>,
    channels: usize,
    quantizer: Quantizer,
    /// Integer samples per channel, reused between chunks.
    ints: Vec<Vec<i32>>,
    tmp_path: PathBuf,
    path: PathBuf,
}

fn flac_err(e: impl std::fmt::Debug) -> Error {
    Error::Encode(format!("FLAC: {e:?}"))
}

impl FlacWriter {
    pub fn create(
        path: &Path,
        sample_rate: u32,
        channels: u16,
        settings: ExportSettings,
    ) -> Result<Self> {
        let bits = match settings.format {
            SampleFormat::Int16 => 16,
            SampleFormat::Int24 => 24,
            other => return Err(Error::Encode(format!("FLAC can't store {}", other.label()))),
        };
        let channels = channels as usize;
        let mut cfg = flacenc::config::Encoder::default();
        cfg.block_size = FLAC_BLOCK;
        let config = cfg.into_verified().map_err(flac_err)?;
        let mut stream = Stream::new(sample_rate as usize, channels, bits).map_err(flac_err)?;
        stream
            .stream_info_mut()
            .set_block_sizes(FLAC_BLOCK, FLAC_BLOCK)
            .map_err(flac_err)?;

        let tmp_path = partial_path(path);
        let mut file = BufWriter::new(File::create(&tmp_path)?);
        // Placeholder header; rewritten with the final length and MD5 at the end.
        file.write_all(&header_bytes(&stream)?)?;
        Ok(Self {
            file: Some(file),
            stream,
            config,
            framebuf: FrameBuf::with_size(channels, FLAC_BLOCK).map_err(flac_err)?,
            context: Context::new(bits, channels),
            pending: Vec::with_capacity(FLAC_BLOCK * channels * 2),
            channels,
            quantizer: Quantizer::new(bits as u16, settings.dither, sample_rate, channels),
            ints: vec![Vec::new(); channels],
            tmp_path,
            path: path.to_path_buf(),
        })
    }

    pub fn write_planar<C: AsRef<[f32]>>(&mut self, chunk: &[C], frames: usize) -> Result<()> {
        for (c, ch) in chunk.iter().take(self.channels).enumerate() {
            self.ints[c].clear();
            self.quantizer
                .quantize(c, &ch.as_ref()[..frames], &mut self.ints[c]);
        }
        for i in 0..frames {
            for ch in &self.ints {
                self.pending.push(ch[i]);
            }
        }
        let block = FLAC_BLOCK * self.channels;
        let mut done = 0;
        while self.pending.len() - done >= block {
            self.encode(done, done + block)?;
            done += block;
        }
        self.pending.drain(..done);
        Ok(())
    }

    fn encode(&mut self, from: usize, to: usize) -> Result<()> {
        let samples = &self.pending[from..to];
        self.framebuf.fill_interleaved(samples).map_err(flac_err)?;
        self.context.fill_interleaved(samples).map_err(flac_err)?;
        let number = self.context.current_frame_number().unwrap_or(0);
        let frame = flacenc::encode_fixed_size_frame(
            &self.config,
            &self.framebuf,
            number,
            self.stream.stream_info(),
        )
        .map_err(flac_err)?;
        let mut sink = flacenc::bitsink::ByteSink::new();
        frame.write(&mut sink).map_err(flac_err)?;
        self.file.as_mut().unwrap().write_all(sink.as_slice())?;
        Ok(())
    }

    /// Flush, fill in the header, add tags and move the file into place.
    pub fn finalize(mut self, tags: Option<&Tags>) -> Result<()> {
        if !self.pending.is_empty() {
            let n = self.pending.len();
            self.encode(0, n)?;
            self.pending.clear();
        }
        let info = self.stream.stream_info_mut();
        info.set_md5_digest(&self.context.md5_digest());
        info.set_total_samples(self.context.total_samples());
        let header = header_bytes(&self.stream)?;
        let mut file = self
            .file
            .take()
            .unwrap()
            .into_inner()
            .map_err(|e| e.into_error())?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&header)?;
        file.sync_all()?;
        drop(file);
        if let Some(tags) = tags {
            write_flac_tags(&self.tmp_path, tags)?;
        }
        fs::rename(&self.tmp_path, &self.path)?;
        Ok(())
    }
}

impl Drop for FlacWriter {
    fn drop(&mut self) {
        if self.file.take().is_some() {
            let _ = fs::remove_file(&self.tmp_path);
        }
    }
}

/// "fLaC" plus the STREAMINFO block (fixed size, so it can be rewritten in place).
fn header_bytes(stream: &Stream) -> Result<Vec<u8>> {
    let mut sink = flacenc::bitsink::ByteSink::new();
    stream.write(&mut sink).map_err(flac_err)?;
    Ok(sink.as_slice().to_vec())
}

fn partial_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".partial");
    path.with_file_name(name)
}

/// Write Vorbis comments and the cover picture into a FLAC file.
pub fn write_flac_tags(path: &Path, t: &Tags) -> Result<()> {
    let mut tag = metaflac::Tag::read_from_path(path).map_err(|e| Error::Encode(e.to_string()))?;
    {
        let vc = tag.vorbis_comments_mut();
        let mut set = |key: &str, value: &str| {
            if !value.is_empty() {
                vc.set(key, vec![value.to_string()]);
            }
        };
        set("TITLE", &t.title);
        set("ARTIST", &t.artist);
        set("ALBUM", &t.album);
        set("ALBUMARTIST", &t.album_artist);
        set("DATE", &t.date);
        set("GENRE", &t.genre);
        set("COMMENT", &t.comment);
        set("ISRC", &t.isrc);
        if let Some((n, total)) = t.track {
            set("TRACKNUMBER", &n.to_string());
            set("TRACKTOTAL", &total.to_string());
        }
        set("ENCODER", concat!("Kerf ", env!("CARGO_PKG_VERSION")));
    }
    if let Some(cover) = &t.cover {
        let data = fs::read(cover)?;
        let mime = match cover
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("png") => "image/png",
            _ => "image/jpeg",
        };
        tag.add_picture(mime, metaflac::block::PictureType::CoverFront, data);
    }
    tag.save().map_err(|e| Error::Encode(e.to_string()))
}

/// A file being exported: WAV or FLAC, chosen by [`ExportSettings::container`].
pub enum AudioWriter {
    Wav(Box<WavWriter>),
    Flac(Box<FlacWriter>),
}

impl AudioWriter {
    pub fn create(
        path: &Path,
        sample_rate: u32,
        channels: u16,
        settings: ExportSettings,
    ) -> Result<Self> {
        Ok(match settings.container {
            Container::Wav => Self::Wav(Box::new(WavWriter::create(
                path,
                sample_rate,
                channels,
                settings,
            )?)),
            Container::Flac => Self::Flac(Box::new(FlacWriter::create(
                path,
                sample_rate,
                channels,
                settings,
            )?)),
        })
    }

    pub fn write_planar<C: AsRef<[f32]>>(&mut self, chunk: &[C], frames: usize) -> Result<()> {
        match self {
            Self::Wav(w) => w.write_planar(chunk, frames),
            Self::Flac(w) => w.write_planar(chunk, frames),
        }
    }

    /// Finish the file. Tags are written for FLAC (WAV has no standard tags).
    pub fn finalize(self, tags: Option<&Tags>) -> Result<()> {
        match self {
            Self::Wav(w) => w.finalize(),
            Self::Flac(w) => w.finalize(tags),
        }
    }

    pub fn abort(self) {
        match self {
            Self::Wav(w) => w.abort(),
            Self::Flac(w) => drop(w),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{io, AudioBuffer};

    fn signal(frames: usize) -> AudioBuffer {
        let l: Vec<f32> = (0..frames)
            .map(|i| ((i as f32) * 0.013).sin() * 0.8)
            .collect();
        let r: Vec<f32> = (0..frames)
            .map(|i| ((i as f32) * 0.021).cos() * 0.4)
            .collect();
        AudioBuffer::new(44_100, vec![l, r])
    }

    #[test]
    fn flac_is_lossless_for_the_quantized_audio() {
        let dir = tempfile::tempdir().unwrap();
        for (format, bits) in [(SampleFormat::Int16, 16u16), (SampleFormat::Int24, 24)] {
            let path = dir.path().join(format!("t{bits}.flac"));
            let buf = signal(FLAC_BLOCK * 3 + 1234); // includes a partial last block
            let settings = ExportSettings {
                format,
                dither: crate::dither::Dither::None,
                container: Container::Flac,
                ..Default::default()
            };
            let mut w = FlacWriter::create(&path, 44_100, 2, settings).unwrap();
            // Feed in odd-sized chunks to exercise block buffering.
            let mut pos = 0;
            while pos < buf.frames() {
                let n = 1000.min(buf.frames() - pos);
                let chunk: Vec<&[f32]> = buf.channels().iter().map(|c| &c[pos..pos + n]).collect();
                w.write_planar(&chunk, n).unwrap();
                pos += n;
            }
            w.finalize(None).unwrap();
            assert!(!dir.path().join(format!("t{bits}.flac.partial")).exists());

            let back = io::decode_file(&path).unwrap();
            assert_eq!(
                (back.sample_rate(), back.num_channels(), back.frames()),
                (44_100, 2, buf.frames())
            );
            let mut q = Quantizer::new(bits, crate::dither::Dither::None, 44_100, 2);
            let scale = (1i64 << (bits - 1)) as f32;
            let ints: Vec<Vec<i32>> = (0..2)
                .map(|c| {
                    let mut v = Vec::new();
                    q.quantize(c, buf.channel(c), &mut v);
                    v
                })
                .collect();
            for (c, ch) in ints.iter().enumerate() {
                for (i, &v) in ch.iter().enumerate() {
                    let want = v as f32 / scale;
                    assert_eq!(back.channel(c)[i], want, "{bits}-bit frame {i} ch {c}");
                }
            }
        }
    }

    #[test]
    fn flac_tags_and_cover() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tagged.flac");
        let cover = dir.path().join("cover.png");
        fs::write(&cover, b"\x89PNG\r\n\x1a\nnot-really-a-png").unwrap();
        let settings = ExportSettings {
            format: SampleFormat::Int16,
            container: Container::Flac,
            ..Default::default()
        };
        let mut w = FlacWriter::create(&path, 44_100, 2, settings).unwrap();
        let buf = signal(10_000);
        w.write_planar(buf.channels(), buf.frames()).unwrap();
        let tags = Tags {
            title: "Encore".into(),
            artist: "The Band".into(),
            album: "Live at the Hall".into(),
            date: "2026".into(),
            track: Some((7, 12)),
            cover: Some(cover),
            ..Default::default()
        };
        w.finalize(Some(&tags)).unwrap();

        let tag = metaflac::Tag::read_from_path(&path).unwrap();
        let get = |k: &str| {
            tag.get_vorbis(k)
                .and_then(|mut v| v.next())
                .map(str::to_string)
        };
        assert_eq!(get("TITLE").as_deref(), Some("Encore"));
        assert_eq!(get("ALBUM").as_deref(), Some("Live at the Hall"));
        assert_eq!(get("TRACKNUMBER").as_deref(), Some("7"));
        assert_eq!(get("TRACKTOTAL").as_deref(), Some("12"));
        assert_eq!(tag.pictures().count(), 1);
        // Still decodes after tagging.
        assert_eq!(io::decode_file(&path).unwrap().frames(), 10_000);
    }

    #[test]
    fn flac_rejects_float() {
        let dir = tempfile::tempdir().unwrap();
        let settings = ExportSettings {
            container: Container::Flac,
            ..Default::default()
        };
        assert!(FlacWriter::create(&dir.path().join("x.flac"), 44_100, 2, settings).is_err());
    }
}
