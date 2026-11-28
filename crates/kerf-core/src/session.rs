//! Importing audio and saving/loading `.kerf` project files.
//!
//! A project file is JSON describing tracks, clips and markers, plus the paths
//! of the audio files they reference. Audio that only exists in memory (e.g.
//! bounced tracks) is written as 32-bit float WAV into a `<name> Media` folder
//! beside the project on save, so nothing is ever lost to quantisation.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::cache::{AudioCache, Progress};
use crate::document::{Document, HistorySnapshots};
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
    /// Every source used by the current state or a state in the history.
    sources: Vec<SourceEntry>,
    #[serde(flatten)]
    state: StateEntry,
    /// The undo history, so it survives closing the project (and a
    /// command-line tool working on it one call at a time). Older Kerf
    /// versions ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    history: Option<HistoryFile>,
}

/// One state of the project: everything but the audio.
#[derive(Serialize, Deserialize)]
struct StateEntry {
    sample_rate: u32,
    channels: u16,
    ids: IdGen,
    tracks: Vec<Track>,
    markers: Vec<Marker>,
    #[serde(default = "default_true")]
    auto_crossfade: bool,
    #[serde(default)]
    album: crate::project::AlbumInfo,
    #[serde(default = "unity")]
    master_gain: f32,
}

impl StateEntry {
    fn of(p: &Project) -> Self {
        Self {
            sample_rate: p.sample_rate,
            channels: p.channels,
            ids: p.ids.clone(),
            tracks: p.tracks.clone(),
            markers: p.markers.clone(),
            auto_crossfade: p.auto_crossfade,
            album: p.album.clone(),
            master_gain: p.master_gain,
        }
    }

    /// The state as a project using `sources` (shared by every state).
    fn into_project(
        self,
        sources: &BTreeMap<SourceId, Arc<Source>>,
        cache: Option<Arc<AudioCache>>,
    ) -> Result<Project> {
        let mut project = Project::new(self.sample_rate, self.channels);
        project.ids = self.ids;
        project.auto_crossfade = self.auto_crossfade;
        project.album = self.album;
        project.master_gain = self.master_gain;
        project.cache = cache;
        project.sources = sources.clone();
        for id in sources.keys() {
            project.ids.ensure_above(id.0);
        }
        for t in &self.tracks {
            project.ids.ensure_above(t.id.0);
            for c in &t.clips {
                project.ids.ensure_above(c.id.0);
                if !sources.contains_key(&c.source) {
                    return Err(Error::ProjectFormat(format!(
                        "clip {} references a missing source",
                        c.id.0
                    )));
                }
            }
        }
        for m in &self.markers {
            project.ids.ensure_above(m.id.0);
        }
        project.tracks = self.tracks;
        project.markers = self.markers;
        for t in &mut project.tracks {
            t.sort_clips();
        }
        project.sort_markers();
        Ok(project)
    }

    fn used_sources(&self) -> impl Iterator<Item = SourceId> + '_ {
        self.tracks
            .iter()
            .flat_map(|t| t.clips.iter().map(|c| c.source))
    }
}

#[derive(Serialize, Deserialize)]
struct HistoryFile {
    /// Oldest first; each entry is the state before its edit.
    undo: Vec<HistoryEntry>,
    /// The next edit to redo last; each entry is the state after its edit.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    redo: Vec<HistoryEntry>,
}

#[derive(Serialize, Deserialize)]
struct HistoryEntry {
    label: String,
    #[serde(flatten)]
    state: StateEntry,
}

/// Undo steps kept in a saved project (the most recent ones). Each costs the
/// size of the project's track, clip and marker lists, never audio.
pub const SAVED_UNDO_LIMIT: usize = 100;

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

/// Save the project without its undo history. Sources referenced by clips
/// that have no file yet are written to the media folder first.
pub fn save(project: &Project, path: &Path) -> Result<()> {
    save_impl(project, None, path, false)
}

/// Save the document: the project plus its most recent undo history (up to
/// [`SAVED_UNDO_LIMIT`] steps, and anything that can be redone), so that
/// reopening it can still undo. Audio only used by the history is saved
/// like the project's own.
pub fn save_document(doc: &Document, path: &Path) -> Result<()> {
    let mut h = doc.snapshots();
    let skip = h.undo.len().saturating_sub(SAVED_UNDO_LIMIT);
    h.undo.drain(..skip);
    h.redo
        .drain(..h.redo.len().saturating_sub(SAVED_UNDO_LIMIT));
    save_impl(doc.project(), Some(&h), path, false)
}

/// Write a crash-recovery snapshot. Unlike [`save`] it never changes the
/// project: rendered audio is referenced by its cache entry (no copying), and
/// is only written out (to the snapshot's own media folder) when there is no
/// cache. A later real save still puts rendered audio in the project's Media.
pub fn save_recovery(project: &Project, path: &Path) -> Result<()> {
    save_impl(project, None, path, true)
}

fn save_impl(
    project: &Project,
    history: Option<&HistorySnapshots>,
    path: &Path,
    recovery: bool,
) -> Result<()> {
    let base = path.parent().unwrap_or(Path::new("."));
    let states: Vec<&Project> = std::iter::once(project)
        .chain(
            history
                .into_iter()
                .flat_map(|h| h.undo.iter().chain(&h.redo).map(|(_, p)| *p)),
        )
        .collect();
    // Each source as any state that uses it knows it.
    let mut used: BTreeMap<SourceId, Option<&Arc<Source>>> = BTreeMap::new();
    for p in &states {
        for c in p.tracks.iter().flat_map(|t| &t.clips) {
            let src = p.sources.get(&c.source);
            let e = used.entry(c.source).or_insert(None);
            if e.is_none() {
                *e = src;
            }
        }
    }

    let mut entries = Vec::new();
    for (id, src) in used {
        let src = src.ok_or(Error::NotFound("source"))?;
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
                        dither: crate::dither::Dither::None,
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

    let entry = |(label, p): &(&str, &Project)| HistoryEntry {
        label: label.to_string(),
        state: StateEntry::of(p),
    };
    let file = ProjectFile {
        format: FORMAT_TAG.into(),
        version: FORMAT_VERSION,
        sources: entries,
        state: StateEntry::of(project),
        history: history
            .filter(|h| !h.undo.is_empty() || !h.redo.is_empty())
            .map(|h| HistoryFile {
                undo: h.undo.iter().map(entry).collect(),
                redo: h.redo.iter().map(entry).collect(),
            }),
    };
    let json =
        serde_json::to_string_pretty(&file).map_err(|e| Error::ProjectFormat(e.to_string()))?;
    let tmp = path.with_extension("kerf.tmp");
    fs::write(&tmp, json)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Load a project (without its undo history), decoding all referenced audio
/// in parallel. With a cache, previously imported audio is mapped straight
/// from it without decoding.
pub fn load(path: &Path, cache: Option<Arc<AudioCache>>) -> Result<Project> {
    load_impl(path, cache, false).map(|d| d.project().clone())
}

/// Load a project with the undo history saved in it (see [`save_document`]).
/// If audio that only the history uses can't be found, the project still
/// opens, without its history.
pub fn load_document(path: &Path, cache: Option<Arc<AudioCache>>) -> Result<Document> {
    load_impl(path, cache, true)
}

fn load_impl(path: &Path, cache: Option<Arc<AudioCache>>, with_history: bool) -> Result<Document> {
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
    let history = file.history.filter(|_| with_history);
    let current: std::collections::BTreeSet<SourceId> = file.state.used_sources().collect();
    let base = path.parent().unwrap_or(Path::new("."));
    let resolve = |e: &SourceEntry| {
        let rel = base.join(&e.path);
        if rel.exists() {
            rel
        } else {
            e.absolute_path.clone()
        }
    };

    // Without the history, only the current state's audio is needed.
    let wanted = file
        .sources
        .into_iter()
        .filter(|e| history.is_some() || current.contains(&e.id));
    type Loaded = (SourceEntry, Option<PathBuf>, SourceAudio, PeakCache);
    let decoded: Vec<(SourceId, Result<Loaded>)> = std::thread::scope(|s| {
        let handles: Vec<_> = wanted
            .map(|e| {
                let p = resolve(&e);
                let rate = file.state.sample_rate;
                let cache = cache.as_deref();
                let id = e.id;
                let h = s.spawn(move || -> Result<Loaded> {
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
                });
                (id, h)
            })
            .collect();
        handles
            .into_iter()
            .map(|(id, h)| (id, h.join().expect("decoder thread panicked")))
            .collect()
    });

    let mut sources = BTreeMap::new();
    let mut history_complete = true;
    for (id, r) in decoded {
        match r {
            Ok((e, p, audio, peaks)) => {
                sources.insert(
                    e.id,
                    Arc::new(Source::with_peaks(e.id, e.name, p, audio, peaks)),
                );
            }
            // Audio only an earlier state used: open without the history.
            Err(_) if !current.contains(&id) => history_complete = false,
            Err(e) => return Err(e),
        }
    }

    let history = history.filter(|_| history_complete);
    if history.is_none() {
        sources.retain(|id, _| current.contains(id));
    }
    let project = file.state.into_project(&sources, cache.clone())?;
    let Some(h) = history else {
        return Ok(Document::new(project));
    };
    let states = |v: Vec<HistoryEntry>| {
        v.into_iter()
            .map(|e| Ok((e.label, e.state.into_project(&sources, cache.clone())?)))
            .collect::<Result<Vec<_>>>()
    };
    Ok(Document::with_history(
        project,
        states(h.undo)?,
        states(h.redo)?,
    ))
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

    /// A one-channel WAV of `secs` seconds at 48 kHz.
    fn wav(dir: &Path, name: &str, secs: usize) -> PathBuf {
        let path = dir.join(name);
        let data: Vec<f32> = (0..secs * 48_000)
            .map(|i| ((i as f32) * 0.01).sin() * 0.5)
            .collect();
        io::write_wav(
            &path,
            &AudioBuffer::new(48_000, vec![data]),
            ExportSettings::default(),
        )
        .unwrap();
        path
    }

    #[test]
    fn undo_history_survives_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let a = wav(dir.path(), "a.wav", 2);
        let b = wav(dir.path(), "b.wav", 1);
        let mut doc = Document::new(Project::new(48_000, 1));
        let t = doc.apply("Import", |p| import_file(p, &a, 0)).unwrap();
        doc.apply("Split", |p| edit::split_at(p, &[t], 1000))
            .unwrap();
        // Rendered audio that only an earlier state uses still has to be saved.
        doc.apply("Bounce", |p| edit::bounce_track(p, t, None).map(|_| ()))
            .unwrap();
        doc.apply("Clear", |p| {
            p.tracks[0].clips.clear();
            Ok(())
        })
        .unwrap();
        doc.apply("Import b", |p| import_file(p, &b, 0).map(|_| ()))
            .unwrap();
        doc.apply("Marker", |p| {
            p.add_marker(10, "M");
            Ok(())
        })
        .unwrap();
        doc.undo(); // "Marker" can be redone
        let path = dir.path().join("p.kerf");
        save_document(&doc, &path).unwrap();
        assert!(dir.path().join("p Media").is_dir(), "rendered audio saved");

        let mut d = load_document(&path, None).unwrap();
        assert!(!d.is_dirty());
        assert_eq!(
            d.history().collect::<Vec<_>>(),
            ["Import", "Split", "Bounce", "Clear", "Import b"]
        );
        assert_eq!(d.redo_label(), Some("Marker"));
        assert_eq!(d.project().tracks, doc.project().tracks);
        assert_eq!(d.redo().as_deref(), Some("Marker"));
        doc.redo();
        assert_eq!(d.project().markers.len(), 1);
        let o = RenderOptions::default();
        while d.history_position() > 3 {
            d.undo();
            doc.undo();
        }
        assert_eq!(d.history_position(), doc.history_position());
        assert_eq!(d.project().tracks, doc.project().tracks);
        assert_eq!(
            render(d.project(), 0, 96_000, &o),
            render(doc.project(), 0, 96_000, &o),
            "the bounced state sounds the same after reopening"
        );
        assert!(d.is_dirty());

        // Plain `load` ignores the history; old files without one still open.
        let p = load(&path, None).unwrap();
        assert_eq!(
            p.tracks,
            load_document(&path, None).unwrap().project().tracks
        );
        save(&p, &path).unwrap();
        assert!(!fs::read_to_string(&path).unwrap().contains("\"history\""));
        assert_eq!(load_document(&path, None).unwrap().history().count(), 0);
    }

    #[test]
    fn missing_history_audio_drops_only_the_history() {
        let dir = tempfile::tempdir().unwrap();
        let a = wav(dir.path(), "a.wav", 1);
        let b = wav(dir.path(), "b.wav", 1);
        let mut doc = Document::new(Project::new(48_000, 1));
        doc.apply("Import a", |p| import_file(p, &a, 0).map(|_| ()))
            .unwrap();
        doc.apply("Replace", |p| {
            p.tracks.clear();
            import_file(p, &b, 0).map(|_| ())
        })
        .unwrap();
        let path = dir.path().join("p.kerf");
        save_document(&doc, &path).unwrap();
        fs::remove_file(&a).unwrap();
        let d = load_document(&path, None).unwrap();
        assert_eq!(d.history().count(), 0);
        assert_eq!(d.project().tracks, doc.project().tracks);
        assert_eq!(d.project().sources.len(), 1);
    }

    #[test]
    fn saved_history_is_limited() {
        let dir = tempfile::tempdir().unwrap();
        let mut doc = Document::new(Project::new(48_000, 1));
        for i in 0..SAVED_UNDO_LIMIT + 5 {
            doc.apply(format!("M{i}"), |p| {
                p.add_marker(i as Frame, "m");
                Ok(())
            })
            .unwrap();
        }
        let path = dir.path().join("p.kerf");
        save_document(&doc, &path).unwrap();
        let d = load_document(&path, None).unwrap();
        assert_eq!(d.history().count(), SAVED_UNDO_LIMIT);
        assert_eq!(d.history().next(), Some("M5"), "the most recent steps");
    }
}
