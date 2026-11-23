//! Running operations on a document an editor owns (the app's open project),
//! without ever editing it at the same time as the user.
//!
//! The document lives on the editor's UI thread; the [`EditorExecutor`]
//! talks to it with [`EditorRequest`]s that the UI thread answers between
//! frames:
//!
//! 1. take a [`Snapshot`] of the project (cheap: audio is shared);
//! 2. run the operation on the snapshot here, on a worker thread, so long
//!    analyses and exports never freeze the editor;
//! 3. if it changed the project, ask the UI thread to **commit** the result
//!    as one undo step (labelled e.g. "AI: Split at 1:00"), which it does
//!    only if the user hasn't edited in the meantime; otherwise start over.
//!
//! Undo, redo, history and save act on the real document directly
//! ([`EditorRequest::Direct`], see [`run_direct`]).

use std::path::PathBuf;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kerf_core::edit::Clipboard;
use kerf_core::ops::{ErrorCode, OpError, Request, Response, Session};
use kerf_core::{AudioCache, Document, Project};

use crate::Executor;

/// Undo labels of assistant edits start with this.
pub const LABEL_PREFIX: &str = "AI: ";

/// How long to wait for the editor to answer.
const TIMEOUT: Duration = Duration::from_secs(120);

/// The editor's project at one moment.
pub struct Snapshot {
    pub project: Project,
    pub revision: u64,
    pub path: Option<PathBuf>,
    pub dirty: bool,
}

impl Snapshot {
    pub fn of(doc: &Document) -> Self {
        Self {
            project: doc.project().clone(),
            revision: doc.revision(),
            path: doc.path.clone(),
            dirty: doc.is_dirty(),
        }
    }
}

/// The project changed since the snapshot; nothing was committed.
#[derive(Debug)]
pub struct Conflict;

/// Messages the editor's UI thread must answer.
pub enum EditorRequest {
    /// Reply with [`Snapshot::of`] the open document.
    Snapshot(Sender<Snapshot>),
    /// If the document is still at `base_revision`, replace its project with
    /// `project` as one undo step labelled `label`; otherwise reply
    /// [`Conflict`].
    Commit {
        base_revision: u64,
        project: Project,
        label: String,
        reply: Sender<Result<(), Conflict>>,
    },
    /// Run `request` on the document itself (undo, redo, history, save):
    /// reply with [`run_direct`].
    Direct {
        request: Request,
        reply: Sender<Result<Response, OpError>>,
    },
}

/// Run a request on the editor's document in place (for
/// [`EditorRequest::Direct`]). Edits made this way are labelled too.
pub fn run_direct(
    doc: &mut Document,
    cache: Option<Arc<AudioCache>>,
    request: Request,
) -> Result<Response, OpError> {
    let mut s = Session::from_document(std::mem::take(doc), cache);
    s.set_label_prefix(LABEL_PREFIX);
    let r = s.execute(request);
    *doc = s.into_document();
    r
}

/// An [`Executor`] working on an editor's document.
pub struct EditorExecutor {
    to_editor: Mutex<Sender<EditorRequest>>,
    /// Wakes the editor so it handles the message promptly (e.g. a repaint).
    wake: Box<dyn Fn() + Send + Sync>,
    cache: Option<Arc<AudioCache>>,
    clipboard: Mutex<Option<Clipboard>>,
}

impl EditorExecutor {
    pub fn new(
        to_editor: Sender<EditorRequest>,
        wake: impl Fn() + Send + Sync + 'static,
        cache: Option<Arc<AudioCache>>,
    ) -> Self {
        Self {
            to_editor: Mutex::new(to_editor),
            wake: Box::new(wake),
            cache,
            clipboard: Mutex::new(None),
        }
    }

    fn ask<T>(&self, make: impl FnOnce(Sender<T>) -> EditorRequest) -> Result<T, OpError> {
        let gone = || OpError::new(ErrorCode::Unsupported, "Kerf closed the project");
        let (tx, rx) = channel();
        self.to_editor
            .lock()
            .unwrap()
            .send(make(tx))
            .map_err(|_| gone())?;
        (self.wake)();
        rx.recv_timeout(TIMEOUT).map_err(|e| match e {
            std::sync::mpsc::RecvTimeoutError::Timeout => {
                OpError::new(ErrorCode::Cancelled, "Kerf didn't respond in time")
            }
            _ => gone(),
        })
    }
}

impl Executor for EditorExecutor {
    fn execute(
        &self,
        request: Request,
        progress: &mut dyn FnMut(f64) -> bool,
    ) -> Result<Response, OpError> {
        match request {
            Request::Open { .. } | Request::New { .. } => {
                return Err(OpError::new(
                    ErrorCode::Unsupported,
                    "the assistant works on the project open in Kerf; ask the user to open \
                     another file there",
                ))
            }
            Request::Undo | Request::Redo | Request::History | Request::Save { .. } => {
                return self.ask(|reply| EditorRequest::Direct { request, reply })?;
            }
            _ => {}
        }
        for _ in 0..3 {
            let snap = self.ask(EditorRequest::Snapshot)?;
            let mut doc = Document::new(snap.project);
            doc.path = snap.path;
            if snap.dirty {
                doc.mark_unsaved();
            }
            let mut s = Session::from_document(doc, self.cache.clone());
            s.set_label_prefix(LABEL_PREFIX);
            s.set_clipboard(self.clipboard.lock().unwrap().clone());
            let result = s.execute_with_progress(request.clone(), progress);
            *self.clipboard.lock().unwrap() = s.take_clipboard();
            let response = result?;
            let doc = s.into_document();
            let Some(label) = doc.undo_label().map(String::from) else {
                return Ok(response); // read-only, or nothing changed
            };
            let committed = self.ask(|reply| EditorRequest::Commit {
                base_revision: snap.revision,
                project: doc.project().clone(),
                label,
                reply,
            })?;
            if committed.is_ok() {
                return Ok(response);
            }
            // The user edited meanwhile: redo the operation on the new state.
        }
        Err(OpError::new(
            ErrorCode::Cancelled,
            "the project kept changing while this ran; try again",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kerf_core::AudioBuffer;
    use std::sync::mpsc::Receiver;

    /// A stand-in for the editor's UI thread: owns the document and answers.
    fn editor(
        mut doc: Document,
        rx: Receiver<EditorRequest>,
        user_edit_before_first_commit: bool,
    ) -> std::thread::JoinHandle<Document> {
        std::thread::spawn(move || {
            let mut interfere = user_edit_before_first_commit;
            for msg in rx {
                match msg {
                    EditorRequest::Snapshot(r) => {
                        let _ = r.send(Snapshot::of(&doc));
                    }
                    EditorRequest::Commit {
                        base_revision,
                        project,
                        label,
                        reply,
                    } => {
                        if interfere {
                            // The user adds a marker just before the commit.
                            interfere = false;
                            doc.apply("User marker", |p| Ok(p.add_marker(10, "user")))
                                .unwrap();
                        }
                        let r = if doc.revision() == base_revision {
                            doc.apply(label, |p| {
                                *p = project;
                                Ok(())
                            })
                            .unwrap();
                            Ok(())
                        } else {
                            Err(Conflict)
                        };
                        let _ = reply.send(r);
                    }
                    EditorRequest::Direct { request, reply } => {
                        let _ = reply.send(run_direct(&mut doc, None, request));
                    }
                }
            }
            doc
        })
    }

    fn doc() -> Document {
        let mut p = Project::new(1000, 1);
        p.add_source_as_track("t", None, AudioBuffer::new(1000, vec![vec![0.5; 4000]]), 0);
        Document::new(p)
    }

    fn run(ex: &EditorExecutor, json: serde_json::Value) -> Result<Response, OpError> {
        ex.execute(serde_json::from_value(json).unwrap(), &mut |_| true)
    }

    #[test]
    fn edits_land_as_labelled_undo_steps() {
        let (tx, rx) = channel();
        let ui = editor(doc(), rx, false);
        let ex = EditorExecutor::new(tx, || {}, None);
        run(&ex, serde_json::json!({"op": "split", "at": 2})).unwrap();
        run(
            &ex,
            serde_json::json!({"op": "add_marker", "at": 1, "name": "A"}),
        )
        .unwrap();
        let info = run(&ex, serde_json::json!({"op": "info"})).unwrap();
        assert!(info.summary.contains("unsaved changes"));
        let h = run(&ex, serde_json::json!({"op": "history"})).unwrap();
        let kerf_core::ops::Data::History(h) = h.data else {
            panic!()
        };
        assert_eq!(
            h.undo,
            ["AI: Split at 0:02.000", "AI: Add marker \"A\" at 0:01.000"]
        );
        run(&ex, serde_json::json!({"op": "undo"})).unwrap();
        assert!(run(&ex, serde_json::json!({"op": "open", "path": "x.wav"})).is_err());
        drop(ex);
        let d = ui.join().unwrap();
        assert!(
            d.project().markers.is_empty(),
            "the undo reached the real document"
        );
        assert_eq!(d.project().tracks[0].clips.len(), 2);
    }

    #[test]
    fn a_user_edit_in_between_is_kept_and_the_operation_redone() {
        let (tx, rx) = channel();
        let ui = editor(doc(), rx, true);
        let ex = EditorExecutor::new(tx, || {}, None);
        run(&ex, serde_json::json!({"op": "split", "at": 2})).unwrap();
        drop(ex);
        let d = ui.join().unwrap();
        assert_eq!(
            d.history().collect::<Vec<_>>(),
            ["User marker", "AI: Split at 0:02.000"]
        );
        assert_eq!(d.project().markers.len(), 1, "the user's edit survived");
        assert_eq!(d.project().tracks[0].clips.len(), 2);
    }

    #[test]
    fn the_clipboard_persists_between_operations() {
        let (tx, rx) = channel();
        let ui = editor(doc(), rx, false);
        let ex = EditorExecutor::new(tx, || {}, None);
        run(&ex, serde_json::json!({"op": "copy", "start": 0, "end": 1})).unwrap();
        run(&ex, serde_json::json!({"op": "paste", "at": 4})).unwrap();
        drop(ex);
        assert_eq!(ui.join().unwrap().project().length(), 5000);
    }

    #[test]
    fn a_closed_editor_is_an_error_not_a_hang() {
        let (tx, rx) = channel();
        drop(rx);
        let ex = EditorExecutor::new(tx, || {}, None);
        let e = run(&ex, serde_json::json!({"op": "info"})).unwrap_err();
        assert!(e.message.contains("closed"));
    }
}
