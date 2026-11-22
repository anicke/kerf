//! Storage for source audio: in memory, or memory-mapped from cache files so
//! that RAM use does not grow with the amount of material.

use std::fs::File;
use std::path::Path;

use memmap2::Mmap;

use crate::buffer::AudioBuffer;
use crate::cache::Lease;
use crate::error::Result;

#[cfg(target_endian = "big")]
compile_error!(
    "Kerf's audio cache files are little-endian f32; big-endian targets are not supported"
);

/// Read access to planar f32 audio, whatever its backing.
pub trait Samples {
    fn sample_rate(&self) -> u32;
    fn num_channels(&self) -> usize;
    fn frames(&self) -> usize;
    fn channel(&self, index: usize) -> &[f32];

    /// All channels as slices (handy for writers and meters).
    fn channel_slices(&self) -> Vec<&[f32]> {
        (0..self.num_channels()).map(|c| self.channel(c)).collect()
    }
}

impl Samples for AudioBuffer {
    fn sample_rate(&self) -> u32 {
        AudioBuffer::sample_rate(self)
    }
    fn num_channels(&self) -> usize {
        AudioBuffer::num_channels(self)
    }
    fn frames(&self) -> usize {
        AudioBuffer::frames(self)
    }
    fn channel(&self, index: usize) -> &[f32] {
        AudioBuffer::channel(self, index)
    }
}

enum Backing {
    Memory(Vec<Vec<f32>>),
    /// One read-only mapping per channel. Cache files are never modified after
    /// they are written, which is what makes reading them as slices sound.
    Mapped(Vec<Mmap>),
}

/// The immutable audio of a [`Source`](crate::Source).
pub struct SourceAudio {
    sample_rate: u32,
    frames: usize,
    backing: Backing,
    lease: Option<Lease>,
}

impl SourceAudio {
    pub fn from_buffer(buf: AudioBuffer) -> Self {
        Self {
            sample_rate: buf.sample_rate(),
            frames: buf.frames(),
            backing: Backing::Memory(buf.into_channels()),
            lease: None,
        }
    }

    /// Map per-channel files of raw little-endian f32 samples.
    /// `lease` keeps the cache entry marked in use for as long as this lives.
    pub fn map_files(
        sample_rate: u32,
        frames: usize,
        paths: &[impl AsRef<Path>],
        lease: Option<Lease>,
    ) -> Result<Self> {
        let mut maps = Vec::with_capacity(paths.len());
        for p in paths {
            let file = File::open(p.as_ref())?;
            if file.metadata()?.len() != frames as u64 * 4 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "cache file has the wrong size",
                )
                .into());
            }
            // SAFETY: cache files are written once, renamed into place and never
            // modified afterwards; Kerf only ever maps them read-only.
            let map = unsafe { Mmap::map(&file)? };
            maps.push(map);
        }
        Ok(Self {
            sample_rate,
            frames,
            backing: Backing::Mapped(maps),
            lease,
        })
    }

    /// The cache entry this audio is mapped from, if any.
    pub fn cache_key(&self) -> Option<&str> {
        self.lease.as_ref().map(Lease::key)
    }

    pub fn is_mapped(&self) -> bool {
        matches!(self.backing, Backing::Mapped(_))
    }

    /// Bytes of sample data held in RAM (mapped audio is paged by the OS).
    pub fn resident_bytes(&self) -> usize {
        match &self.backing {
            Backing::Memory(ch) => ch.iter().map(|c| c.len() * 4).sum(),
            Backing::Mapped(_) => 0,
        }
    }

    /// Copy into an in-memory buffer (for small ranges or tests).
    pub fn to_buffer(&self) -> AudioBuffer {
        AudioBuffer::new(
            self.sample_rate,
            self.channel_slices().iter().map(|c| c.to_vec()).collect(),
        )
    }
}

impl Samples for SourceAudio {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn num_channels(&self) -> usize {
        match &self.backing {
            Backing::Memory(ch) => ch.len(),
            Backing::Mapped(m) => m.len(),
        }
    }
    fn frames(&self) -> usize {
        self.frames
    }
    fn channel(&self, index: usize) -> &[f32] {
        match &self.backing {
            Backing::Memory(ch) => &ch[index],
            Backing::Mapped(m) => {
                if self.frames == 0 {
                    &[]
                } else {
                    // Mappings are page-aligned, so the cast is always aligned.
                    bytemuck::cast_slice(&m[index][..self.frames * 4])
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn mapped_matches_memory() {
        let dir = tempfile::tempdir().unwrap();
        let data = [vec![0.5f32, -1.0, 2.0], vec![1e-7, 0.0, -0.25]];
        let paths: Vec<_> = (0..2).map(|c| dir.path().join(format!("c{c}"))).collect();
        for (p, d) in paths.iter().zip(&data) {
            File::create(p)
                .unwrap()
                .write_all(bytemuck::cast_slice(d))
                .unwrap();
        }
        let mapped = SourceAudio::map_files(48_000, 3, &paths, None).unwrap();
        assert!(mapped.is_mapped());
        assert_eq!(mapped.resident_bytes(), 0);
        assert_eq!(mapped.channel(0), &data[0][..]);
        assert_eq!(mapped.channel(1), &data[1][..]);
        let mem = SourceAudio::from_buffer(AudioBuffer::new(48_000, data.to_vec()));
        assert_eq!(mem.to_buffer(), mapped.to_buffer());
        assert!(
            SourceAudio::map_files(48_000, 4, &paths, None).is_err(),
            "size mismatch rejected"
        );
    }
}
