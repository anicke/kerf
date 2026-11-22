//! Crash recovery: periodic snapshots of unsaved work.
//!
//! While a project has unsaved changes, a snapshot is written every
//! `interval` to the recovery folder (`~/.local/share/kerf/recovery` on
//! Linux; `KERF_RECOVERY_DIR` overrides it). Snapshots are small: audio is
//! referenced, rendered audio stays in the cache. A running Kerf refreshes the
//! timestamp of its snapshot each interval; on start-up, snapshots that have
//! gone stale belong to a session that crashed and are offered for recovery.
//! A clean exit, a save or discarding changes removes the snapshot.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kerf_core::{session, Document};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Meta {
    /// Where the project was saved, if it ever was.
    original: Option<PathBuf>,
    saved_at: u64,
}

/// A snapshot left behind by a session that did not close cleanly.
#[derive(Clone, Debug)]
pub struct Orphan {
    pub snapshot: PathBuf,
    pub original: Option<PathBuf>,
    pub saved_at: SystemTime,
}

impl Orphan {
    pub fn title(&self) -> String {
        self.original
            .as_ref()
            .and_then(|p| p.file_stem())
            .map_or("Untitled".into(), |s| s.to_string_lossy().into_owned())
    }

    /// Delete the snapshot, its metadata and any media written for it.
    pub fn discard(&self) {
        remove_snapshot(&self.snapshot);
    }
}

fn meta_path(snapshot: &Path) -> PathBuf {
    snapshot.with_extension("json")
}

fn media_dir(snapshot: &Path) -> PathBuf {
    let stem = snapshot
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    snapshot.with_file_name(format!("{stem} Media"))
}

fn remove_snapshot(snapshot: &Path) {
    let _ = std::fs::remove_file(meta_path(snapshot)); // marker first
    let _ = std::fs::remove_file(snapshot);
    let _ = std::fs::remove_dir_all(media_dir(snapshot));
}

pub struct Recovery {
    dir: PathBuf,
    snapshot: PathBuf,
    interval: Duration,
    last_tick: Instant,
    written_revision: Option<u64>,
}

impl Recovery {
    pub fn new(interval_secs: u64) -> Option<Self> {
        let dir = match std::env::var_os("KERF_RECOVERY_DIR") {
            Some(d) => PathBuf::from(d),
            None => dirs::data_local_dir()?.join("kerf").join("recovery"),
        };
        std::fs::create_dir_all(&dir).ok()?;
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let snapshot = dir.join(format!("session-{}-{nanos}.kerf", std::process::id()));
        Some(Self {
            dir,
            snapshot,
            interval: Duration::from_secs(interval_secs),
            last_tick: Instant::now(),
            written_revision: None,
        })
    }

    pub fn set_interval(&mut self, secs: u64) {
        self.interval = Duration::from_secs(secs);
        if secs == 0 {
            self.clear();
        }
    }

    /// Snapshots from other sessions that stopped refreshing them.
    pub fn orphans(&self) -> Vec<Orphan> {
        // Generous: a live session refreshes at least every few minutes.
        let stale = Duration::from_secs(self.interval.as_secs().max(60) * 3);
        let now = SystemTime::now();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut out: Vec<Orphan> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .filter_map(|meta_file| {
                let snapshot = meta_file.with_extension("kerf");
                if snapshot == self.snapshot || !snapshot.exists() {
                    return None;
                }
                let modified = std::fs::metadata(&meta_file)
                    .and_then(|m| m.modified())
                    .ok()?;
                if now.duration_since(modified).unwrap_or_default() < stale {
                    return None; // another Kerf is running and owns it
                }
                let meta: Meta = serde_json::from_slice(&std::fs::read(&meta_file).ok()?).ok()?;
                Some(Orphan {
                    snapshot,
                    original: meta.original,
                    saved_at: UNIX_EPOCH + Duration::from_secs(meta.saved_at),
                })
            })
            .collect();
        out.sort_by_key(|o| std::cmp::Reverse(o.saved_at));
        out
    }

    /// Call every frame: writes a snapshot when there are unsaved changes and
    /// the interval has passed; removes it once everything is saved.
    pub fn tick(&mut self, doc: &Document) {
        if self.interval.is_zero() || self.last_tick.elapsed() < self.interval {
            return;
        }
        self.last_tick = Instant::now();
        self.write_now(doc);
    }

    fn write_now(&mut self, doc: &Document) {
        if !doc.is_dirty() || doc.project().tracks.is_empty() {
            self.clear();
            return;
        }
        // Also rewrite if the snapshot vanished (e.g. removed by another session).
        if self.written_revision != Some(doc.revision()) || !self.snapshot.exists() {
            if session::save_recovery(doc.project(), &self.snapshot).is_err() {
                return;
            }
            self.written_revision = Some(doc.revision());
        }
        // Writing the metadata doubles as the "still alive" heartbeat.
        let meta = Meta {
            original: doc.path.clone(),
            saved_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        };
        if let Ok(json) = serde_json::to_vec(&meta) {
            let _ = std::fs::write(meta_path(&self.snapshot), json);
        }
    }

    /// Remove this session's snapshot (clean exit, saved, discarded).
    pub fn clear(&mut self) {
        if self.written_revision.take().is_some() || self.snapshot.exists() {
            remove_snapshot(&self.snapshot);
        }
    }

    #[cfg(test)]
    fn force_tick(&mut self, doc: &Document) {
        self.write_now(doc);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kerf_core::{AudioBuffer, Project};

    fn dirty_doc() -> Document {
        let mut d = Document::default();
        d.apply("Import", |p: &mut Project| {
            Ok(p.add_source_as_track(
                "t",
                None,
                AudioBuffer::new(48_000, vec![vec![0.1; 100], vec![0.1; 100]]),
                0,
            ))
        })
        .unwrap();
        d
    }

    fn age(path: &Path, secs: u64) {
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(secs))
            .unwrap();
    }

    #[test]
    fn snapshot_lifecycle_and_orphan_detection() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY of env use: tests in this module are the only users of this variable.
        unsafe { std::env::set_var("KERF_RECOVERY_DIR", dir.path()) };
        let mut a = Recovery::new(60).unwrap();
        let b = Recovery::new(60).unwrap();

        let mut doc = dirty_doc();
        a.force_tick(&doc);
        assert!(a.snapshot.exists());
        assert!(
            b.orphans().is_empty(),
            "a fresh snapshot belongs to a running session"
        );

        // Session `a` stops refreshing (crashed): it becomes an orphan for `b`.
        age(&meta_path(&a.snapshot), 3600);
        let orphans = b.orphans();
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].title(), "Untitled");
        let recovered = session::load(&orphans[0].snapshot, None).unwrap();
        assert_eq!(recovered.tracks.len(), 1);
        orphans[0].discard();
        assert!(b.orphans().is_empty());

        // Saving (no longer dirty) removes the snapshot.
        a.force_tick(&doc);
        assert!(a.snapshot.exists());
        doc.mark_saved();
        a.force_tick(&doc);
        assert!(!a.snapshot.exists());
    }
}
