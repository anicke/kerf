//! Storage for source audio: in memory, or memory-mapped from cache files so
//! that RAM use does not grow with the amount of material.

use std::fs::File;
use std::path::Path;
use std::sync::{Arc, Mutex, Once, Weak};
use std::time::Duration;

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
    Mapped(Vec<Arc<Mmap>>),
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
            let map = Arc::new(unsafe { Mmap::map(&file)? });
            register(&map);
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

// ---------------------------------------------------------------------------
// Keeping mapped audio out of the process's memory
// ---------------------------------------------------------------------------
//
// Pages of a mapping stay counted in the process's memory once read, so
// playing or exporting an hour of audio would make Kerf appear to use the
// whole file's size in RAM (the OS can reclaim them, but it shows, and it
// crowds out other caching). Every mapping is registered here and released
// regularly: a background thread does so every second, and long sequential
// reads (`render::for_each_chunk`) do so as they go, since they can read far
// more than a second's worth. (The cache's `Lease` registry tracks which
// entries are in use; this one holds the mappings themselves.)

static MAPPED: Mutex<Vec<Weak<Mmap>>> = Mutex::new(Vec::new());

/// How often the background thread releases mapped pages.
const RELEASE_EVERY: Duration = Duration::from_secs(1);

fn register(map: &Arc<Mmap>) {
    MAPPED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(Arc::downgrade(map));
    static TRIMMER: Once = Once::new();
    TRIMMER.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("kerf-release-pages".into())
            .spawn(|| loop {
                std::thread::sleep(RELEASE_EVERY);
                release_mapped_memory();
            });
    });
}

/// Drop every mapped audio page from this process's memory. Later reads
/// fetch them again (usually from the OS file cache).
pub fn release_mapped_memory() {
    // One pass: forget dropped mappings, release live ones. Holding the lock
    // meanwhile is fine: releasing is fast, and only `register` waits on it.
    MAPPED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|w| match w.upgrade() {
            Some(map) => {
                release(&map);
                true
            }
            None => false,
        });
}

#[cfg(unix)]
fn release(map: &Mmap) {
    use memmap2::UncheckedAdvice;
    // SAFETY: the mapping is read-only and shared (`Mmap::map`) over a cache
    // file that is never modified after it is written. MADV_DONTNEED only
    // removes the pages from this process; the next access faults them back
    // in from the file with identical contents, so slices borrowed from the
    // mapping never observe a change.
    let _ = unsafe { map.unchecked_advise(UncheckedAdvice::DontNeed) };
}

#[cfg(windows)]
fn release(map: &Mmap) {
    unsafe extern "system" {
        fn VirtualUnlock(address: *const std::ffi::c_void, size: usize) -> i32;
    }
    // Unlocking pages that aren't locked removes them from the working set
    // (documented behaviour); they are paged back in from the file on access.
    // SAFETY: the range is exactly this live mapping.
    unsafe {
        VirtualUnlock(map.as_ptr().cast(), map.len());
    }
}

#[cfg(not(any(unix, windows)))]
fn release(_map: &Mmap) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Write each channel to its own raw f32 file in `dir`; returns the paths.
    fn write_channels(dir: &Path, data: &[Vec<f32>]) -> Vec<std::path::PathBuf> {
        data.iter()
            .enumerate()
            .map(|(c, d)| {
                let p = dir.join(format!("c{c}"));
                File::create(&p)
                    .unwrap()
                    .write_all(bytemuck::cast_slice(d))
                    .unwrap();
                p
            })
            .collect()
    }

    #[test]
    fn mapped_matches_memory() {
        let dir = tempfile::tempdir().unwrap();
        let data = [vec![0.5f32, -1.0, 2.0], vec![1e-7, 0.0, -0.25]];
        let paths = write_channels(dir.path(), &data);
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

    #[test]
    fn released_pages_read_back_identically() {
        let dir = tempfile::tempdir().unwrap();
        let data: Vec<f32> = (0..300_000).map(|i| (i as f32 * 0.001).sin()).collect();
        let paths = write_channels(dir.path(), std::slice::from_ref(&data));
        let mapped = SourceAudio::map_files(48_000, data.len(), &paths, None).unwrap();
        assert_eq!(mapped.channel(0), &data[..]);
        release_mapped_memory();
        assert_eq!(mapped.channel(0), &data[..]);
    }
}
