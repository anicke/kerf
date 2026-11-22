//! Importing audio and saving/loading `.kerf` project files.
//!
//! A project file is JSON describing tracks, clips and markers, plus the paths
//! of the audio files they reference. Audio that only exists in memory (e.g.
//! bounced tracks) is written as 32-bit float WAV into a `<name> Media` folder
//! beside the project on save, so nothing is ever lost to quantisation.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::cache::{AudioCache, Progress};
use crate::error::{Error, Result};
use crate::io::{self, ExportSettings, SampleFormat};
use crate::peaks::PeakCache;
use crate::project::{IdGen, Marker, Project, Source, SourceId, Track, TrackId};
use crate::resample::resample;
use crate::store::{Samples, SourceAudio};
use crate::util::sanitize_file_name;
use crate::Frame;

pub const PROJECT_EXTENSION: &str = "kerf";
const FORMAT_TAG: &str = "kerf-project";
const FORMAT_VERSION: u32 = 1;

/// Decode `path`, resampled to `rate` (or at its own rate if `None`), through
/// the cache when one is given (memory-mapped) or into memory otherwise.
pub fn prepare_audio(
    cache: Option<&AudioCache>,
    path: &Path,
    rate: Option<u32>,
    progress: Progress,
) -> Result<(SourceAudio, PeakCache)> {
    match cache {
        Some(c) => c.import(path, rate, progress),
        None => {
            let buf = io::decode_file(path)?;
            let buf = match rate {
                Some(r) => resample(&buf, r)?,
                None => buf,
            };
            let peaks = PeakCache::build(&buf);
            Ok((SourceAudio::from_buffer(buf), peaks))
        }
    }
}

/// Decode a file and add it to the project as a new track at `position`.
///
/// If the project is still empty it adopts the file's sample rate and channel
/// count, so editing a single file never resamples it.
pub fn import_file(project: &mut Project, path: &Path, position: Frame) -> Result<TrackId> {
    let rate = (!project.is_empty()).then_some(project.sample_rate);
    let (audio, peaks) = prepare_audio(project.cache.as_deref(), path, rate, &mut |_| true)?;
    if project.is_empty() {
        project.sample_rate = audio.sample_rate();
        project.channels = audio.num_channels().clamp(1, 8) as u16;
    }
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Audio".into());
    let sid = project.add_prepared_source(name.clone(), Some(path.to_path_buf()), audio, peaks);
    project
        .add_track_with_source(name, sid, position)
        .ok_or(Error::NotFound("source"))
}

#[derive(Serialize, Deserialize)]
struct SourceEntry {
    id: SourceId,
    name: String,
    /// Relative to the project file's folder when possible.
    #[serde(default)]
    path: PathBuf,
    /// Absolute fallback.
    #[serde(default)]
    absolute_path: PathBuf,
    /// Recovery files only: rendered audio still in the audio cache, mapped
    /// directly instead of being written out as a file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache_key: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct ProjectFile {
    format: String,
    version: u32,
    sample_rate: u32,
    channels: u16,
    ids: IdGen,
    sources: Vec<SourceEntry>,
    tracks: Vec<Track>,
    markers: Vec<Marker>,
    #[serde(default = "default_true")]
    auto_crossfade: bool,
    #[serde(default)]
    album: crate::project::AlbumInfo,
    #[serde(default = "unity")]
    master_gain: f32,
}

fn unity() -> f32 {
    1.0
}

fn default_true() -> bool {
    true
}

fn media_dir(project_path: &Path) -> PathBuf {
    let stem = project_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    project_path.with_file_name(format!("{stem} Media"))
}

/// Save the project. Sources referenced by clips that have no file yet are
/// written to the media folder first.
pub fn save(project: &Project, path: &Path) -> Result<()> {
    save_impl(project, path, false)
}

/// Write a crash-recovery snapshot. Unlike [`save`] it never changes the
/// project: rendered audio is referenced by its cache entry (no copying), and
/// is only written out (to the snapshot's own media folder) when there is no
/// cache. A later real save still puts rendered audio in the project's Media.
pub fn save_recovery(project: &Project, path: &Path) -> Result<()> {
    save_impl(project, path, true)
}

fn save_impl(project: &Project, path: &Path, recovery: bool) -> Result<()> {
    let base = path.parent().unwrap_or(Path::new("."));
    let used: std::collections::BTreeSet<SourceId> = project
        .tracks
        .iter()
        .flat_map(|t| t.clips.iter().map(|c| c.source))
        .collect();

    let mut entries = Vec::new();
    for id in used {
        let src = project.sources.get(&id).ok_or(Error::NotFound("source"))?;
        if recovery && src.path.get().is_none() {
            if let Some(key) = src.audio.cache_key() {
                entries.push(SourceEntry {
                    id,
                    name: src.name.clone(),
                    path: PathBuf::new(),
                    absolute_path: PathBuf::new(),
                    cache_key: Some(key.to_string()),
                });
                continue;
            }
        }
        let file = match src.path.get() {
            Some(p) => p.clone(),
            None => {
                let dir = media_dir(path);
                fs::create_dir_all(&dir)?;
                let file = dir.join(format!("{} {}.wav", id.0, sanitize_file_name(&src.name)));
                io::write_wav(
                    &file,
                    &src.audio,
                    ExportSettings {
                        format: SampleFormat::Float32,
                        dither: false,
                        ..Default::default()
                    },
                )?;
                if !recovery {
                    let _ = src.path.set(file.clone());
                }
                file
            }
        };
        let absolute_path = fs::canonicalize(&file).unwrap_or(file.clone());
        let canonical_base = fs::canonicalize(base).unwrap_or(base.to_path_buf());
        let rel = absolute_path
            .strip_prefix(&canonical_base)
            .map(Path::to_path_buf)
            .unwrap_or(absolute_path.clone());
        entries.push(SourceEntry {
            id,
            name: src.name.clone(),
            path: rel,
            absolute_path,
            cache_key: None,
        });
    }

    let file = ProjectFile {
        format: FORMAT_TAG.into(),
        version: FORMAT_VERSION,
        sample_rate: project.sample_rate,
        channels: project.channels,
        ids: project.ids.clone(),
        sources: entries,
        tracks: project.tracks.clone(),
        markers: project.markers.clone(),
        auto_crossfade: project.auto_crossfade,
        album: project.album.clone(),
        master_gain: project.master_gain,
    };
    let json =
        serde_json::to_string_pretty(&file).map_err(|e| Error::ProjectFormat(e.to_string()))?;
    let tmp = path.with_extension("kerf.tmp");
    fs::write(&tmp, json)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Load a project, decoding all referenced audio in parallel. With a cache,
/// previously imported audio is mapped straight from it without decoding.
pub fn load(path: &Path, cache: Option<Arc<AudioCache>>) -> Result<Project> {
    let text = fs::read_to_string(path)?;
    let file: ProjectFile =
        serde_json::from_str(&text).map_err(|e| Error::ProjectFormat(e.to_string()))?;
    if file.format != FORMAT_TAG {
        return Err(Error::ProjectFormat("not a Kerf project".into()));
    }
    if file.version > FORMAT_VERSION {
        return Err(Error::ProjectFormat(format!(
            "made by a newer Kerf (format {})",
            file.version
        )));
    }
    let base = path.parent().unwrap_or(Path::new("."));
    let resolve = |e: &SourceEntry| {
        let rel = base.join(&e.path);
        if rel.exists() {
            rel
        } else {
            e.absolute_path.clone()
        }
    };

    type Loaded = (SourceEntry, Option<PathBuf>, SourceAudio, PeakCache);
    let decoded: Vec<Result<Loaded>> = std::thread::scope(|s| {
        let handles: Vec<_> = file
            .sources
            .into_iter()
            .map(|e| {
                let p = resolve(&e);
                let rate = file.sample_rate;
                let cache = cache.as_deref();
                s.spawn(move || -> Result<Loaded> {
                    // Rendered audio from a recovery file: straight from the cache.
                    if let Some(key) = &e.cache_key {
                        return match cache.and_then(|c| c.open_entry(key)) {
                            Some((a, pk)) => Ok((e, None, a, pk)),
                            None => Err(Error::ProjectFormat(format!(
                                "rendered audio \"{}\" is no longer in the audio cache",
                                e.name
                            ))),
                        };
                    }
                    prepare_audio(cache, &p, Some(rate), &mut |_| true)
                        .map(|(a, pk)| (e, Some(p), a, pk))
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("decoder thread panicked"))
            .collect()
    });

    let mut project = Project::new(file.sample_rate, file.channels);
    project.ids = file.ids;
    project.auto_crossfade = file.auto_crossfade;
    project.album = file.album.clone();
    project.master_gain = file.master_gain;
    project.cache = cache;
    for r in decoded {
        let (e, p, audio, peaks) = r?;
        project.ids.ensure_above(e.id.0);
        project.sources.insert(
            e.id,
            Arc::new(Source::with_peaks(e.id, e.name, p, audio, peaks)),
        );
    }
    for t in &file.tracks {
        project.ids.ensure_above(t.id.0);
        for c in &t.clips {
            project.ids.ensure_above(c.id.0);
            if !project.sources.contains_key(&c.source) {
                return Err(Error::ProjectFormat(format!(
                    "clip {} references a missing source",
                    c.id.0
                )));
            }
        }
    }
    project.tracks = file.tracks;
    project.markers = file.markers;
    for t in &mut project.tracks {
        t.sort_clips();
    }
    project.sort_markers();
    Ok(project)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit;
    use crate::render::{render, RenderOptions};
    use crate::AudioBuffer;

    fn roundtrip(use_cache: bool) {
        let dir = tempfile::tempdir().unwrap();
        let cache =
            use_cache.then(|| Arc::new(AudioCache::open(dir.path().join("cache")).unwrap()));
        let wav = dir.path().join("in.wav");
        let data: Vec<f32> = (0..4800).map(|i| ((i as f32) * 0.05).sin() * 1.5).collect();
        io::write_wav(
            &wav,
            &AudioBuffer::new(48_000, vec![data]),
            ExportSettings::default(),
        )
        .unwrap();

        let mut p = Project::new(44_100, 2);
        p.cache = cache.clone();
        let t = import_file(&mut p, &wav, 100).unwrap();
        assert_eq!(
            (p.sample_rate, p.channels),
            (48_000, 1),
            "empty project adopts file format"
        );
        edit::split_at(&mut p, &[t], 1000).unwrap();
        edit::fade_out(&mut p, &[t], 3000, 4000, crate::FadeShape::SCurve).unwrap();
        let bounced = edit::bounce_track(&mut p, t, Some((2000, 4900))).unwrap();
        assert_eq!(
            p.sources[&bounced].audio.is_mapped(),
            use_cache,
            "renders go to the cache too"
        );
        assert!(p.sources.values().all(|s| s.audio.is_mapped() == use_cache));
        p.add_marker(1234, "Chorus");

        let proj_path = dir.path().join("song.kerf");
        save(&p, &proj_path).unwrap();
        assert!(dir.path().join("song Media").is_dir());

        let q = load(&proj_path, cache).unwrap();
        assert!(q.sources.values().all(|s| s.audio.is_mapped() == use_cache));
        assert_eq!(q.tracks, p.tracks);
        assert_eq!(q.markers, p.markers);
        let o = RenderOptions::default();
        assert_eq!(render(&q, 0, 5000, &o), render(&p, 0, 5000, &o));
    }

    #[test]
    fn save_load_roundtrip_in_memory() {
        roundtrip(false);
    }

    #[test]
    fn save_load_roundtrip_through_cache() {
        roundtrip(true);
    }

    #[test]
    fn import_resamples_into_existing_project() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("in.wav");
        io::write_wav(
            &wav,
            &AudioBuffer::new(44_100, vec![vec![0.1; 44_100]]),
            ExportSettings::default(),
        )
        .unwrap();
        let mut p = Project::new(48_000, 2);
        p.add_track("existing");
        let t = import_file(&mut p, &wav, 0).unwrap();
        assert_eq!(p.sample_rate, 48_000);
        assert_eq!(p.track(t).unwrap().end(), 48_000);
    }

    #[test]
    fn recovery_snapshot_maps_rendered_audio_and_leaves_project_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(AudioCache::open(dir.path().join("cache")).unwrap());
        let wav = dir.path().join("in.wav");
        let data: Vec<f32> = (0..9600).map(|i| ((i as f32) * 0.02).sin()).collect();
        io::write_wav(
            &wav,
            &AudioBuffer::new(48_000, vec![data]),
            ExportSettings::default(),
        )
        .unwrap();
        let mut p = Project::new(48_000, 1);
        p.cache = Some(cache.clone());
        let t = import_file(&mut p, &wav, 0).unwrap();
        edit::fade_in(&mut p, &[t], 0, 2000, crate::FadeShape::Linear).unwrap();
        let rendered = edit::bounce_track(&mut p, t, None).unwrap();

        let snap = dir.path().join("recovery").join("snap.kerf");
        fs::create_dir_all(snap.parent().unwrap()).unwrap();
        save_recovery(&p, &snap).unwrap();
        assert!(
            !dir.path().join("recovery").join("snap Media").exists(),
            "nothing copied"
        );
        assert!(
            p.sources[&rendered].path.get().is_none(),
            "project unchanged by the snapshot"
        );

        let q = load(&snap, Some(cache)).unwrap();
        assert!(
            q.sources[&rendered].path.get().is_none(),
            "still unsaved after recovery"
        );
        let o = RenderOptions::default();
        assert_eq!(render(&q, 0, 9600, &o), render(&p, 0, 9600, &o));

        // A real save afterwards writes the rendered audio into the project's Media.
        let real = dir.path().join("real.kerf");
        save(&q, &real).unwrap();
        assert!(dir.path().join("real Media").is_dir());
    }
}
