//! On-disk cache of decoded audio, memory-mapped for playback and editing.
//!
//! Each entry is one raw little-endian f32 file per channel (`<key>.c<N>.f32`),
//! the waveform peaks (`<key>.peaks`) and a small JSON file (`<key>.json`) that
//! is written last, so its presence marks a complete entry. Entries for files
//! on disk are keyed by path, size, modification time and target sample rate,
//! so a changed file is re-decoded automatically. Files are written under a
//! temporary name and renamed into place; a crash never leaves a half-written
//! entry that looks complete.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::io::StreamDecoder;
use crate::peaks::{PeakBuilder, PeakCache};
use crate::resample::StreamResampler;
use crate::store::SourceAudio;

/// Bump when the on-disk format changes; old entries are then ignored.
const CACHE_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct Meta {
    version: u32,
    sample_rate: u32,
    channels: usize,
    frames: usize,
}

/// Progress callback for long imports: receives 0..=1, returns `false` to cancel.
pub type Progress<'a> = &'a mut dyn FnMut(f64) -> bool;

type Registry = Arc<Mutex<HashMap<String, usize>>>;

#[derive(Debug)]
pub struct AudioCache {
    dir: PathBuf,
    /// Entries currently mapped by live sources (including ones only kept
    /// alive by undo history), with reference counts.
    in_use: Registry,
}

/// Held by every mapped [`SourceAudio`]: marks its cache entry as in use, so
/// [`AudioCache::clear_unused`] and [`AudioCache::prune`] never delete it.
#[derive(Debug)]
pub struct Lease {
    registry: Registry,
    key: String,
}

impl Lease {
    pub fn key(&self) -> &str {
        &self.key
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut map = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = map.get_mut(&self.key) {
            *n -= 1;
            if *n == 0 {
                map.remove(&self.key);
            }
        }
    }
}

/// Size of the cache, and how much of it the running app is using.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub entries: usize,
    pub bytes: u64,
    pub in_use_entries: usize,
    pub in_use_bytes: u64,
}

impl AudioCache {
    /// Use (and create) `dir` for cache files.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            in_use: Registry::default(),
        })
    }

    /// The platform cache location, e.g. `~/.cache/kerf/audio` on Linux,
    /// `~/Library/Caches/kerf/audio` on macOS, `%LOCALAPPDATA%\kerf\audio` on Windows.
    pub fn open_default() -> Result<Self> {
        let base = dirs::cache_dir().unwrap_or_else(std::env::temp_dir);
        Self::open(base.join("kerf").join("audio"))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn file(&self, key: &str, suffix: &str) -> PathBuf {
        self.dir.join(format!("{key}{suffix}"))
    }

    fn lease(&self, key: &str) -> Lease {
        *self
            .in_use
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key.to_string())
            .or_default() += 1;
        Lease {
            registry: self.in_use.clone(),
            key: key.to_string(),
        }
    }

    fn is_in_use(&self, key: &str) -> bool {
        self.in_use
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(key)
    }

    /// Complete files grouped by entry key, with their total size.
    fn entries(&self) -> Result<HashMap<String, u64>> {
        let mut out: HashMap<String, u64> = HashMap::new();
        for e in fs::read_dir(&self.dir)?.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.ends_with(".partial") {
                continue;
            }
            // Keys never contain '.', so the key is everything before the first one.
            let key = name.split('.').next().unwrap_or_default().to_string();
            *out.entry(key).or_default() += e.metadata().map(|m| m.len()).unwrap_or(0);
        }
        Ok(out)
    }

    /// Delete one entry's files (marker first, so a half-deleted entry is
    /// never mistaken for a complete one). Returns bytes freed.
    fn remove_entry(&self, key: &str) -> u64 {
        let _ = fs::remove_file(self.file(key, ".json"));
        let mut freed = 0;
        if let Ok(dir) = fs::read_dir(&self.dir) {
            for f in dir.flatten() {
                let n = f.file_name().to_string_lossy().into_owned();
                if n.starts_with(&format!("{key}.")) && !n.ends_with(".partial") {
                    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
                    if fs::remove_file(f.path()).is_ok() {
                        freed += len;
                    }
                }
            }
        }
        freed
    }

    pub fn stats(&self) -> Result<CacheStats> {
        let mut s = CacheStats::default();
        for (key, bytes) in self.entries()? {
            s.entries += 1;
            s.bytes += bytes;
            if self.is_in_use(&key) {
                s.in_use_entries += 1;
                s.in_use_bytes += bytes;
            }
        }
        Ok(s)
    }

    /// Delete every entry not used by this app right now (sources in the open
    /// project or its undo history are kept). Another running Kerf may still
    /// be using an entry; on Linux and macOS it keeps working and re-decodes
    /// next time, on Windows the delete fails and the entry is left. Returns
    /// bytes freed.
    pub fn clear_unused(&self) -> Result<u64> {
        let mut freed = 0;
        for key in self.entries()?.into_keys() {
            if !self.is_in_use(&key) {
                freed += self.remove_entry(&key);
            }
        }
        Ok(freed)
    }

    /// Decode `path` (resampling to `target_rate`, or keeping its own rate if
    /// `None`) into the cache, or reuse an existing entry.
    pub fn import(
        &self,
        path: &Path,
        target_rate: Option<u32>,
        progress: Progress,
    ) -> Result<(SourceAudio, PeakCache)> {
        let mut dec = StreamDecoder::open(path)?;
        let rate = target_rate.unwrap_or(dec.sample_rate());
        let key = file_key(path, rate)?;
        if let Some(hit) = self.load(&key) {
            return Ok(hit);
        }
        let mut writer = CacheWriter::new(self, &key, dec.channels(), rate)?;
        let mut rs = StreamResampler::new(dec.sample_rate(), rate, dec.channels())?;
        let total = dec.total_frames().filter(|&t| t > 0);
        let mut done = 0u64;
        let mut sink = |c: &[&[f32]]| writer.write(c);
        while let Some(chunk) = dec.next_chunk()? {
            let n = chunk[0].len();
            rs.push(chunk, n, &mut sink)?;
            done += n as u64;
            if let Some(t) = total {
                if !progress((done as f64 / t as f64).min(1.0)) {
                    return Err(Error::Cancelled);
                }
            }
        }
        rs.finish(&mut sink)?;
        writer.finish()
    }

    /// Start a new entry for generated audio (e.g. a render). Write it in
    /// chunks with [`CacheWriter::write`], then call [`CacheWriter::finish`].
    pub fn writer(&self, channels: usize, sample_rate: u32) -> Result<CacheWriter<'_>> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let mut h = Fnv::new();
        h.write(&nanos.to_le_bytes());
        h.write(&std::process::id().to_le_bytes());
        h.write(&COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
        CacheWriter::new(self, &format!("r{:016x}", h.0), channels, sample_rate)
    }

    /// Open a complete entry, refreshing its timestamp so pruning keeps it.
    fn load(&self, key: &str) -> Option<(SourceAudio, PeakCache)> {
        let meta_path = self.file(key, ".json");
        let meta: Meta = serde_json::from_slice(&fs::read(&meta_path).ok()?).ok()?;
        if meta.version != CACHE_VERSION {
            return None;
        }
        let paths: Vec<PathBuf> = (0..meta.channels)
            .map(|c| self.file(key, &format!(".c{c}.f32")))
            .collect();
        let audio =
            SourceAudio::map_files(meta.sample_rate, meta.frames, &paths, Some(self.lease(key)))
                .ok()?;
        let peaks_path = self.file(key, ".peaks");
        let peaks = PeakCache::load(&peaks_path, meta.channels, meta.frames).unwrap_or_else(|| {
            let p = PeakCache::build_from(&audio);
            let _ = p.save(&peaks_path);
            p
        });
        if let Ok(f) = File::options().write(true).open(&meta_path) {
            let _ = f.set_modified(SystemTime::now());
        }
        Some((audio, peaks))
    }

    /// Delete entries not opened for `max_age` (except ones in use now), and
    /// stale partial files. Returns the number of bytes freed.
    pub fn prune(&self, max_age: Duration) -> Result<u64> {
        let now = SystemTime::now();
        let age = |p: &Path| {
            fs::metadata(p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| now.duration_since(t).ok())
        };
        let mut freed = 0;
        for entry in fs::read_dir(&self.dir)?.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let stale_partial = name.ends_with(".partial")
                && age(&path).is_some_and(|a| a > Duration::from_secs(86_400));
            let expired = name.ends_with(".json") && age(&path).is_some_and(|a| a >= max_age);
            if stale_partial {
                freed += entry.metadata().map(|m| m.len()).unwrap_or(0);
                let _ = fs::remove_file(&path);
            } else if expired {
                let key = name.trim_end_matches(".json");
                if !self.is_in_use(key) {
                    freed += self.remove_entry(key);
                }
            }
        }
        Ok(freed)
    }
}

/// Streams planar audio into a new cache entry.
pub struct CacheWriter<'a> {
    cache: &'a AudioCache,
    key: String,
    tag: String,
    sample_rate: u32,
    files: Vec<BufWriter<File>>,
    peaks: PeakBuilder,
    frames: usize,
}

impl<'a> CacheWriter<'a> {
    fn new(cache: &'a AudioCache, key: &str, channels: usize, sample_rate: u32) -> Result<Self> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        // Unique temporary names so concurrent imports of one file don't collide.
        let tag = format!(
            "{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let mut files = Vec::with_capacity(channels);
        for c in 0..channels {
            let p = cache.file(key, &format!(".c{c}.f32.{tag}.partial"));
            files.push(BufWriter::with_capacity(1 << 20, File::create(p)?));
        }
        Ok(Self {
            cache,
            key: key.to_string(),
            tag,
            sample_rate,
            files,
            peaks: PeakBuilder::new(channels),
            frames: 0,
        })
    }

    pub fn write<C: AsRef<[f32]>>(&mut self, chunk: &[C]) -> Result<()> {
        let n = chunk.first().map_or(0, |c| c.as_ref().len());
        for (f, c) in self.files.iter_mut().zip(chunk) {
            f.write_all(bytemuck::cast_slice(&c.as_ref()[..n]))?;
        }
        self.peaks.push(chunk, n);
        self.frames += n;
        Ok(())
    }

    /// Like [`write`](Self::write) for the first `frames` frames of a chunk.
    pub fn write_frames<C: AsRef<[f32]>>(&mut self, chunk: &[C], frames: usize) -> Result<()> {
        let slices: Vec<&[f32]> = chunk
            .iter()
            .take(self.files.len())
            .map(|c| &c.as_ref()[..frames])
            .collect();
        self.write(&slices)
    }

    pub fn finish(mut self) -> Result<(SourceAudio, PeakCache)> {
        // Taking the files disarms the cleanup in `Drop`.
        let files = std::mem::take(&mut self.files);
        let peaks = std::mem::replace(&mut self.peaks, PeakBuilder::new(0));
        let (cache, key, tag, sample_rate, frames) = (
            self.cache,
            self.key.clone(),
            self.tag.clone(),
            self.sample_rate,
            self.frames,
        );
        let channels = files.len();
        let mut finals = Vec::with_capacity(channels);
        for (c, f) in files.into_iter().enumerate() {
            let file = f.into_inner().map_err(|e| e.into_error())?;
            file.sync_all()?;
            drop(file);
            let tmp = cache.file(&key, &format!(".c{c}.f32.{tag}.partial"));
            let fin = cache.file(&key, &format!(".c{c}.f32"));
            if let Err(e) = fs::rename(&tmp, &fin) {
                let _ = fs::remove_file(&tmp);
                // Another import finished the same entry first (and Windows won't
                // replace a mapped file): use theirs.
                if let Some(hit) = cache.load(&key) {
                    return Ok(hit);
                }
                return Err(e.into());
            }
            finals.push(fin);
        }
        let peaks = peaks.finish();
        peaks.save(&cache.file(&key, ".peaks"))?;
        let meta = Meta {
            version: CACHE_VERSION,
            sample_rate,
            channels,
            frames,
        };
        let meta_tmp = cache.file(&key, &format!(".json.{tag}.partial"));
        fs::write(
            &meta_tmp,
            serde_json::to_vec(&meta).map_err(|e| Error::ProjectFormat(e.to_string()))?,
        )?;
        fs::rename(&meta_tmp, cache.file(&key, ".json"))?;
        let audio = SourceAudio::map_files(sample_rate, frames, &finals, Some(cache.lease(&key)))?;
        Ok((audio, peaks))
    }
}

impl Drop for CacheWriter<'_> {
    fn drop(&mut self) {
        // Only reached with files still open on error/cancel paths.
        for c in 0..self.files.len() {
            let _ = fs::remove_file(
                self.cache
                    .file(&self.key, &format!(".c{c}.f32.{}.partial", self.tag)),
            );
        }
    }
}

/// FNV-1a: tiny, and stable across Rust versions (unlike `DefaultHasher`).
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }
}

fn file_key(path: &Path, rate: u32) -> Result<String> {
    let canonical = fs::canonicalize(path)?;
    let meta = fs::metadata(&canonical)?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .unwrap_or_default();
    let mut h = Fnv::new();
    h.write(canonical.to_string_lossy().as_bytes());
    h.write(&meta.len().to_le_bytes());
    h.write(&mtime.as_nanos().to_le_bytes());
    h.write(&rate.to_le_bytes());
    h.write(&CACHE_VERSION.to_le_bytes());
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem: String = stem
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(24)
        .collect();
    Ok(format!("{stem}-{:016x}", h.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::{self, ExportSettings};
    use crate::store::Samples;
    use crate::AudioBuffer;

    fn wav(dir: &Path, name: &str, rate: u32, frames: usize) -> PathBuf {
        let p = dir.join(name);
        let l: Vec<f32> = (0..frames)
            .map(|i| ((i as f32) * 0.01).sin() * 1.3)
            .collect();
        let r: Vec<f32> = l.iter().map(|v| -v * 0.5).collect();
        io::write_wav(
            &p,
            &AudioBuffer::new(rate, vec![l, r]),
            ExportSettings::default(),
        )
        .unwrap();
        p
    }

    #[test]
    fn import_maps_bit_exact_audio_and_reuses_entry() {
        let dir = tempfile::tempdir().unwrap();
        let cache = AudioCache::open(dir.path().join("cache")).unwrap();
        let path = wav(dir.path(), "a.wav", 48_000, 100_000);
        let (audio, peaks) = cache.import(&path, None, &mut |_| true).unwrap();
        assert!(audio.is_mapped());
        assert_eq!(
            audio.to_buffer(),
            io::decode_file(&path).unwrap(),
            "32-bit float survives the cache bit-exactly"
        );
        assert_eq!(
            peaks.min_max(&audio, 0, 0, 100_000),
            PeakCache::build_from(&audio).min_max(&audio, 0, 0, 100_000)
        );

        let entries = fs::read_dir(cache.dir()).unwrap().count();
        let (again, _) = cache.import(&path, None, &mut |_| true).unwrap();
        assert_eq!(
            fs::read_dir(cache.dir()).unwrap().count(),
            entries,
            "second import is a cache hit"
        );
        assert_eq!(again.to_buffer(), audio.to_buffer());
        assert!(!fs::read_dir(cache.dir()).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".partial")));
    }

    #[test]
    fn import_resamples_to_target_rate() {
        let dir = tempfile::tempdir().unwrap();
        let cache = AudioCache::open(dir.path().join("cache")).unwrap();
        let path = wav(dir.path(), "b.wav", 44_100, 44_100);
        let (audio, _) = cache.import(&path, Some(48_000), &mut |_| true).unwrap();
        assert_eq!((audio.sample_rate(), audio.frames()), (48_000, 48_000));
        let whole = crate::resample::resample(&io::decode_file(&path).unwrap(), 48_000).unwrap();
        let err = audio
            .channel(0)
            .iter()
            .zip(whole.channel(0))
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f32::max);
        assert!(err < 1e-4, "{err}");
    }

    #[test]
    fn changed_file_is_reimported_and_cancel_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let cache = AudioCache::open(dir.path().join("cache")).unwrap();
        let path = wav(dir.path(), "c.wav", 48_000, 1000);
        let (a, _) = cache.import(&path, None, &mut |_| true).unwrap();
        wav(dir.path(), "c.wav", 48_000, 2000);
        let (b, _) = cache.import(&path, None, &mut |_| true).unwrap();
        assert_eq!((a.frames(), b.frames()), (1000, 2000));

        let long = wav(dir.path(), "d.wav", 48_000, 480_000);
        let r = cache.import(&long, None, &mut |_| false);
        assert!(matches!(r, Err(Error::Cancelled)));
        assert!(!fs::read_dir(cache.dir()).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("d-")));
    }

    #[test]
    fn in_use_entries_survive_clear_unused() {
        let dir = tempfile::tempdir().unwrap();
        let cache = AudioCache::open(dir.path().join("cache")).unwrap();
        let a = wav(dir.path(), "a.wav", 48_000, 10_000);
        let b = wav(dir.path(), "b.wav", 48_000, 10_000);
        let (held, _) = cache.import(&a, None, &mut |_| true).unwrap();
        let (dropped, _) = cache.import(&b, None, &mut |_| true).unwrap();
        let (again, _) = cache.import(&a, None, &mut |_| true).unwrap(); // second lease on a

        let s = cache.stats().unwrap();
        assert_eq!((s.entries, s.in_use_entries), (2, 2));
        assert!(s.bytes >= 2 * 2 * 10_000 * 4);
        drop(dropped);
        let s = cache.stats().unwrap();
        assert_eq!(s.in_use_entries, 1);
        assert!(cache.clear_unused().unwrap() > 0);
        assert_eq!(
            cache.stats().unwrap().entries,
            1,
            "only the unused entry was removed"
        );
        assert_eq!(
            held.channel(0)[123],
            again.channel(0)[123],
            "held audio still readable"
        );

        drop(held);
        assert_eq!(
            cache.stats().unwrap().in_use_entries,
            1,
            "still leased by `again`"
        );
        assert_eq!(
            cache.prune(Duration::ZERO).unwrap(),
            0,
            "prune skips in-use entries"
        );
        drop(again);
        cache.clear_unused().unwrap();
        assert_eq!(cache.stats().unwrap(), CacheStats::default());
    }

    #[test]
    fn writer_and_prune() {
        let dir = tempfile::tempdir().unwrap();
        let cache = AudioCache::open(dir.path()).unwrap();
        let mut w = cache.writer(1, 48_000).unwrap();
        w.write(&[vec![0.25f32; 10]]).unwrap();
        w.write(&[vec![0.5f32; 5]]).unwrap();
        let (audio, _) = w.finish().unwrap();
        assert_eq!(audio.frames(), 15);
        assert_eq!(audio.channel(0)[12], 0.5);
        drop(audio);
        assert_eq!(
            cache.prune(Duration::from_secs(3600)).unwrap(),
            0,
            "fresh entries kept"
        );
        assert!(cache.prune(Duration::ZERO).unwrap() > 0);
        assert_eq!(fs::read_dir(cache.dir()).unwrap().count(), 0);
    }
}
