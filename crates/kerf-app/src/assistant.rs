//! AI assistants editing the open project, over MCP (opt-in in Preferences).
//!
//! An MCP client runs `kerf-mcp --app`, which connects to the host started
//! here. Operations run on snapshots off the UI thread (see
//! `kerf_mcp::editor`); this module answers their requests between frames,
//! so the assistant and the user never edit at the same time, and every
//! assistant edit is one undo step labelled "AI: …".

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;

use eframe::egui;
use kerf_core::AudioCache;
use kerf_mcp::editor::{run_direct, Conflict, EditorExecutor, EditorRequest, Snapshot};
use kerf_mcp::host::{default_connection_file, Client, Host};
use kerf_mcp::{KerfServer, Options, Sandbox};

use crate::app::{Action, KerfApp};

pub struct Assistant {
    host: Host,
    inbox: Receiver<EditorRequest>,
    /// Requests held back while the user is in the middle of a gesture.
    held: Vec<EditorRequest>,
    /// The folders it may use, for display.
    pub folders: Vec<PathBuf>,
}

const INTRO: &str = "\
You are working in the Kerf audio editor app, on the project the user has open, while they \
watch: every edit you make appears on their timeline at once and is one undo step labelled \
\"AI: …\" in their history. The user may edit too; your operations always see the current \
state. You can't open or close projects: ask the user to open the file they mean. Edits are \
undoable, but exports and saves write files, so confirm those first.";

/// Folders the assistant may read and write: those set in Preferences, or
/// the home folder.
pub fn folders(configured: &[PathBuf]) -> Vec<PathBuf> {
    let set: Vec<PathBuf> = configured.iter().filter(|p| p.is_dir()).cloned().collect();
    if !set.is_empty() {
        return set;
    }
    vec![dirs::home_dir().unwrap_or_else(|| std::env::current_dir().unwrap_or_default())]
}

/// How an MCP client starts the bridge: the program and its arguments. That is
/// `kerf-mcp` next to this program if it's there, otherwise from the PATH. Inside
/// an AppImage, whose files sit in a mount that changes on every start, it is the
/// AppImage itself, whose launcher runs the bundled `kerf-mcp` when asked to.
pub fn bridge_command() -> (String, Vec<String>) {
    bridge_for(
        std::env::var_os("APPIMAGE").map(PathBuf::from),
        std::env::current_exe().ok(),
    )
}

fn bridge_for(appimage: Option<PathBuf>, exe: Option<PathBuf>) -> (String, Vec<String>) {
    let app = vec!["--app".to_string()];
    if let Some(image) = appimage.filter(|p| p.is_file()) {
        let args = ["kerf-mcp".to_string()].into_iter().chain(app).collect();
        return (image.display().to_string(), args);
    }
    let name = if cfg!(windows) {
        "kerf-mcp.exe"
    } else {
        "kerf-mcp"
    };
    let cmd = exe
        .and_then(|e| e.parent().map(|d| d.join(name)))
        .filter(|p| p.is_file())
        .map_or_else(|| name.to_string(), |p| p.display().to_string());
    (cmd, app)
}

/// The client configuration to paste into Claude Desktop, Claude Code etc.
pub fn client_config() -> String {
    let (cmd, args) = bridge_command();
    let config = serde_json::json!({
        "mcpServers": { "kerf": { "command": cmd, "args": args } }
    });
    serde_json::to_string_pretty(&config).unwrap_or_default()
}

impl Assistant {
    pub fn start(
        ctx: &egui::Context,
        cache: Option<Arc<AudioCache>>,
        configured_folders: &[PathBuf],
    ) -> std::io::Result<Self> {
        let folders = folders(configured_folders);
        let sandbox = Sandbox::new(folders.clone())?;
        let (tx, inbox) = channel();
        let wake_ctx = ctx.clone();
        let executor = Arc::new(EditorExecutor::new(
            tx,
            move || wake_ctx.request_repaint(),
            cache,
        ));
        let options = Options {
            hidden_tools: vec!["open", "new"],
            intro: Some(INTRO.into()),
        };
        let repaint_ctx = ctx.clone();
        let host = Host::start(
            default_connection_file(),
            move || KerfServer::with_executor(sandbox.clone(), executor.clone(), options.clone()),
            move || repaint_ctx.request_repaint(),
        )?;
        Ok(Self {
            host,
            inbox,
            held: Vec::new(),
            folders,
        })
    }

    pub fn clients(&self) -> Vec<Client> {
        self.host.clients()
    }

    pub fn disconnect(&self) {
        self.host.disconnect_all();
    }
}

impl KerfApp {
    /// Turn assistant access on or off (restarting it if it was on).
    pub fn set_assistant(&mut self, ctx: &egui::Context, on: bool) {
        self.assistant = None; // stops a running host first
        if !on {
            return;
        }
        match Assistant::start(ctx, self.cache.clone(), &self.prefs.assistant_folders) {
            Ok(a) => self.assistant = Some(a),
            Err(e) => self.error(format!("Could not start assistant access: {e}")),
        }
    }

    /// Answer the assistant's requests. Called every frame.
    pub fn poll_assistant(&mut self, ctx: &egui::Context) {
        let Some(a) = &mut self.assistant else {
            return;
        };
        let mut pending: Vec<EditorRequest> = std::mem::take(&mut a.held);
        pending.extend(a.inbox.try_iter());
        if pending.is_empty() {
            return;
        }
        // Never change the project under a drag or a gesture in progress.
        let busy = self.drag.is_some() || ctx.input(|i| i.pointer.any_down());
        let mut held = Vec::new();
        for msg in pending {
            match msg {
                EditorRequest::Snapshot(reply) => {
                    let _ = reply.send(Snapshot::of(&self.doc));
                }
                msg @ (EditorRequest::Commit { .. } | EditorRequest::Direct { .. }) if busy => {
                    held.push(msg);
                }
                EditorRequest::Commit {
                    base_revision,
                    project,
                    label,
                    reply,
                } => {
                    if self.doc.revision() != base_revision {
                        let _ = reply.send(Err(Conflict));
                        continue;
                    }
                    let was_empty = self.project().is_empty();
                    self.edit(&label, |p| {
                        *p = project;
                        Ok(())
                    });
                    self.info(label);
                    if was_empty && !self.project().is_empty() {
                        self.perform(ctx, Action::ZoomFit);
                    }
                    let _ = reply.send(Ok(()));
                }
                EditorRequest::Direct { request, reply } => {
                    let before = self.doc.revision();
                    let r = run_direct(&mut self.doc, self.cache.clone(), request);
                    self.validate_selection();
                    if let Ok(resp) = &r {
                        let saved = resp.summary.starts_with("Saved");
                        if saved {
                            if let Some(p) = self.doc.path.clone() {
                                self.remember_recent(&p);
                            }
                        }
                        if self.doc.revision() != before || saved {
                            self.info(format!("AI: {}", resp.summary));
                        }
                    }
                    let _ = reply.send(r);
                }
            }
        }
        if let Some(a) = &mut self.assistant {
            a.held = held;
            if !a.held.is_empty() {
                ctx.request_repaint();
            }
        }
    }

    /// Status-bar indicator: who is connected, with a Disconnect button.
    pub fn assistant_indicator(&self, ui: &mut egui::Ui) {
        let Some(a) = &self.assistant else {
            return;
        };
        let clients = a.clients();
        if clients.is_empty() {
            ui.label(egui::RichText::new("🤖 Assistant: waiting").weak())
                .on_hover_text(format!(
                    "AI assistants can connect with this MCP client configuration:\n\n{}\n\n\
                     They may read and write files in: {}",
                    client_config(),
                    a.folders
                        .iter()
                        .map(|f| f.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            return;
        }
        if ui
            .small_button("Disconnect")
            .on_hover_text("End the assistant's session")
            .clicked()
        {
            a.disconnect();
        }
        let names: Vec<String> = clients.iter().map(|c| c.name.clone()).collect();
        ui.label(
            egui::RichText::new(format!("🤖 {} connected", names.join(", ")))
                .color(egui::Color32::from_rgb(120, 200, 140))
                .strong(),
        )
        .on_hover_text(
            "An AI assistant can edit this project. Its edits appear in Edit → Undo as \"AI: …\".",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_default_to_home_and_skip_missing_ones() {
        let dir = tempfile::tempdir().unwrap();
        let set = folders(&[dir.path().to_path_buf(), "/no/such/folder".into()]);
        assert_eq!(set, [dir.path().to_path_buf()]);
        assert_eq!(folders(&["/no/such/folder".into()]).len(), 1, "falls back");
    }

    #[test]
    fn client_config_is_valid_json() {
        let v: serde_json::Value = serde_json::from_str(&client_config()).unwrap();
        assert_eq!(v["mcpServers"]["kerf"]["args"][0], "--app");
    }

    #[test]
    fn bridge_runs_through_the_appimage_when_inside_one() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("Kerf.AppImage");
        std::fs::write(&image, b"").unwrap();
        let (cmd, args) = bridge_for(Some(image.clone()), None);
        assert_eq!(cmd, image.display().to_string());
        assert_eq!(args, ["kerf-mcp", "--app"]);

        // A stale APPIMAGE (no such file) falls back to kerf-mcp next to the program.
        let exe = dir.path().join("kerf");
        let mcp = dir.path().join(if cfg!(windows) {
            "kerf-mcp.exe"
        } else {
            "kerf-mcp"
        });
        std::fs::write(&mcp, b"").unwrap();
        let (cmd, args) = bridge_for(Some(dir.path().join("gone")), Some(exe));
        assert_eq!(cmd, mcp.display().to_string());
        assert_eq!(args, ["--app"]);
    }
}
