//! A project plus its undo/redo history and save state.

use std::path::PathBuf;

use crate::error::Result;
use crate::project::Project;

/// Default number of undo steps kept. Snapshots share audio, so each step
/// costs only the size of the clip/track metadata.
pub const DEFAULT_UNDO_LIMIT: usize = 1000;

#[derive(Clone)]
struct Snapshot {
    label: String,
    project: Project,
    revision: u64,
    /// Set while a continuous gesture (e.g. a fader drag) is extending this step.
    merge_key: Option<String>,
}

pub struct Document {
    project: Project,
    revision: u64,
    next_revision: u64,
    saved_revision: u64,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    undo_limit: usize,
    /// Project file this document was loaded from / saved to.
    pub path: Option<PathBuf>,
}

impl Default for Document {
    fn default() -> Self {
        Self::new(Project::default())
    }
}

impl Document {
    pub fn new(project: Project) -> Self {
        Self {
            project,
            revision: 0,
            next_revision: 1,
            saved_revision: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            undo_limit: DEFAULT_UNDO_LIMIT,
            path: None,
        }
    }

    pub fn project(&self) -> &Project {
        &self.project
    }

    /// Changes with each edit/undo/redo; use to invalidate caches.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Run an edit as one undoable step. On error the project is left
    /// untouched. Edits that change nothing do not create an undo step.
    pub fn apply<T>(
        &mut self,
        label: impl Into<String>,
        edit: impl FnOnce(&mut Project) -> Result<T>,
    ) -> Result<T> {
        let before = self.project.clone();
        match edit(&mut self.project) {
            Ok(v) => {
                if self.project != before {
                    self.push_undo(label.into(), before);
                }
                Ok(v)
            }
            Err(e) => {
                self.project = before;
                Err(e)
            }
        }
    }

    /// Like [`apply`](Self::apply), but consecutive calls with the same
    /// `merge_key` collapse into one undo step until [`end_merge`](Self::end_merge).
    /// Use for continuous gestures such as dragging a fader.
    pub fn apply_merged<T>(
        &mut self,
        label: impl Into<String>,
        merge_key: &str,
        edit: impl FnOnce(&mut Project) -> Result<T>,
    ) -> Result<T> {
        let extending = self.redo.is_empty()
            && self
                .undo
                .last()
                .is_some_and(|s| s.merge_key.as_deref() == Some(merge_key));
        let before = self.project.clone();
        match edit(&mut self.project) {
            Ok(v) => {
                if self.project != before {
                    if extending {
                        self.revision = self.next_revision;
                        self.next_revision += 1;
                    } else {
                        self.push_undo(label.into(), before);
                        self.undo.last_mut().unwrap().merge_key = Some(merge_key.to_string());
                    }
                }
                Ok(v)
            }
            Err(e) => {
                self.project = before;
                Err(e)
            }
        }
    }

    /// Finish a merged gesture; the next edit starts a new undo step.
    pub fn end_merge(&mut self) {
        if let Some(s) = self.undo.last_mut() {
            s.merge_key = None;
        }
    }

    fn push_undo(&mut self, label: String, before: Project) {
        self.end_merge();
        self.undo.push(Snapshot {
            label,
            project: before,
            revision: self.revision,
            merge_key: None,
        });
        if self.undo.len() > self.undo_limit {
            self.undo.remove(0);
        }
        self.redo.clear();
        self.revision = self.next_revision;
        self.next_revision += 1;
    }

    /// Undo the last edit; returns its label.
    pub fn undo(&mut self) -> Option<String> {
        let snap = self.undo.pop()?;
        let label = snap.label.clone();
        let current = Snapshot {
            label: snap.label,
            project: std::mem::replace(&mut self.project, snap.project),
            revision: self.revision,
            merge_key: None,
        };
        self.revision = snap.revision;
        self.redo.push(current);
        Some(label)
    }

    /// Redo the last undone edit; returns its label.
    pub fn redo(&mut self) -> Option<String> {
        let snap = self.redo.pop()?;
        let label = snap.label.clone();
        let current = Snapshot {
            label: snap.label,
            project: std::mem::replace(&mut self.project, snap.project),
            revision: self.revision,
            merge_key: None,
        };
        self.revision = snap.revision;
        self.undo.push(current);
        Some(label)
    }

    pub fn undo_label(&self) -> Option<&str> {
        self.undo.last().map(|s| s.label.as_str())
    }

    pub fn redo_label(&self) -> Option<&str> {
        self.redo.last().map(|s| s.label.as_str())
    }

    /// Labels of undoable steps, oldest first.
    pub fn history(&self) -> impl Iterator<Item = &str> {
        self.undo.iter().map(|s| s.label.as_str())
    }

    pub fn is_dirty(&self) -> bool {
        self.revision != self.saved_revision
    }

    pub fn mark_saved(&mut self) {
        self.saved_revision = self.revision;
    }

    /// Treat the current state as unsaved (e.g. work restored after a crash).
    pub fn mark_unsaved(&mut self) {
        self.saved_revision = u64::MAX;
    }

    pub fn set_undo_limit(&mut self, limit: usize) {
        self.undo_limit = limit.max(1);
        while self.undo.len() > self.undo_limit {
            self.undo.remove(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit;
    use crate::error::Error;
    use crate::AudioBuffer;

    fn doc() -> (Document, crate::TrackId) {
        let mut p = Project::new(1000, 1);
        let t = p.add_source_as_track("t", None, AudioBuffer::new(1000, vec![vec![0.5; 1000]]), 0);
        (Document::new(p), t)
    }

    #[test]
    fn undo_redo_roundtrip() {
        let (mut d, t) = doc();
        let p0 = d.project().clone();
        d.apply("Split", |p| edit::split_at(p, &[t], 500)).unwrap();
        let p1 = d.project().clone();
        d.apply("Delete", |p| edit::ripple_delete(p, &[t], 100, 200))
            .unwrap();
        let p2 = d.project().clone();
        assert!(d.is_dirty());
        assert_eq!(d.undo_label(), Some("Delete"));

        assert_eq!(d.undo().as_deref(), Some("Delete"));
        assert_eq!(d.project(), &p1);
        assert_eq!(d.undo().as_deref(), Some("Split"));
        assert_eq!(d.project(), &p0);
        assert!(!d.is_dirty(), "back at the saved state");
        assert_eq!(d.undo(), None);

        assert_eq!(d.redo().as_deref(), Some("Split"));
        assert_eq!(d.redo().as_deref(), Some("Delete"));
        assert_eq!(d.project(), &p2);
        assert_eq!(d.redo(), None);
    }

    #[test]
    fn new_edit_clears_redo() {
        let (mut d, t) = doc();
        d.apply("A", |p| edit::split_at(p, &[t], 500)).unwrap();
        d.undo();
        d.apply("B", |p| edit::split_at(p, &[t], 300)).unwrap();
        assert_eq!(d.redo_label(), None);
    }

    #[test]
    fn noop_and_failed_edits_leave_no_history() {
        let (mut d, t) = doc();
        d.apply("Split outside", |p| edit::split_at(p, &[t], 5000))
            .unwrap();
        assert_eq!(d.undo_label(), None);
        let before = d.project().clone();
        let r = d.apply("Bad", |p| {
            edit::split_at(p, &[t], 500)?;
            Err::<(), _>(Error::InvalidRange)
        });
        assert!(r.is_err());
        assert_eq!(d.project(), &before, "failed edit rolled back");
        assert_eq!(d.undo_label(), None);
        assert!(!d.is_dirty());
    }

    #[test]
    fn merged_gesture_is_one_step() {
        let (mut d, t) = doc();
        for g in [0.9f32, 0.8, 0.7] {
            d.apply_merged("Track gain", "gain", |p| {
                p.track_mut(t).unwrap().gain = g;
                Ok(())
            })
            .unwrap();
        }
        d.end_merge();
        d.apply_merged("Track gain", "gain", |p| {
            p.track_mut(t).unwrap().gain = 0.5;
            Ok(())
        })
        .unwrap();
        assert_eq!(d.history().count(), 2);
        d.undo();
        assert_eq!(d.project().track(t).unwrap().gain, 0.7);
        d.undo();
        assert_eq!(d.project().track(t).unwrap().gain, 1.0);
    }

    #[test]
    fn undo_limit() {
        let (mut d, t) = doc();
        d.set_undo_limit(3);
        for i in 1..10 {
            d.apply(format!("s{i}"), |p| edit::split_at(p, &[t], i * 50))
                .unwrap();
        }
        assert_eq!(d.history().collect::<Vec<_>>(), ["s7", "s8", "s9"]);
    }
}
