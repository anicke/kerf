//! End-to-end: an MCP client drives the `kerf-mcp` binary over stdio.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use kerf_core::io::{write_wav, ExportSettings};
use kerf_core::AudioBuffer;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientConfig, GetPromptRequestParams, NumberOrString,
    ProgressNotificationParam, ProgressToken, ReadResourceRequestParams, RequestMetaObject,
};
use rmcp::service::{NotificationContext, RunningService};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::{ClientHandler, RoleClient, ServiceExt};
use serde_json::{json, Value};

/// Counts progress notifications.
#[derive(Clone, Default)]
struct Client {
    progress: Arc<AtomicUsize>,
}

impl ClientHandler for Client {
    async fn on_progress(
        &self,
        _p: ProgressNotificationParam,
        _c: NotificationContext<RoleClient>,
    ) {
        self.progress.fetch_add(1, Ordering::SeqCst);
    }

    fn get_info(&self) -> ClientConfig {
        ClientConfig::default()
    }
}

fn tone(path: &Path, secs: f64) {
    let sr = 44_100;
    let n = (secs * sr as f64) as usize;
    let ch: Vec<f32> = (0..n)
        .map(|i| (i as f32 * 220.0 * std::f32::consts::TAU / sr as f32).sin() * 0.3)
        .collect();
    write_wav(
        path,
        &AudioBuffer::new(sr, vec![ch.clone(), ch]),
        ExportSettings::default(),
    )
    .unwrap();
}

async fn start(allow: &Path, client: Client) -> RunningService<RoleClient, Client> {
    let transport = TokioChildProcess::new(
        tokio::process::Command::new(env!("CARGO_BIN_EXE_kerf-mcp")).configure(|c| {
            c.arg("--no-cache").arg("--allow").arg(allow);
        }),
    )
    .unwrap();
    client.serve(transport).await.unwrap()
}

async fn call(
    c: &RunningService<RoleClient, Client>,
    tool: &'static str,
    args: Value,
) -> CallToolResult {
    let params = CallToolRequestParams::new(tool).with_arguments(args.as_object().unwrap().clone());
    c.call_tool(params).await.unwrap()
}

fn text(r: &CallToolResult) -> String {
    r.content[0].as_text().unwrap().text.clone()
}

/// The JSON details after the summary line.
fn details(r: &CallToolResult) -> Value {
    let t = text(r);
    serde_json::from_str(t.split_once('\n').map_or("null", |x| x.1)).unwrap()
}

async fn ok(
    c: &RunningService<RoleClient, Client>,
    tool: &'static str,
    args: Value,
) -> CallToolResult {
    let r = call(c, tool, args.clone()).await;
    assert_ne!(r.is_error, Some(true), "{tool} {args} failed: {}", text(&r));
    r
}

#[tokio::test]
async fn edit_look_and_export_through_mcp() {
    let dir = tempfile::tempdir().unwrap();
    tone(&dir.path().join("take.wav"), 6.0);
    let client = Client::default();
    let progress = client.progress.clone();
    let c = start(dir.path(), client).await;

    let info = c.peer_info().unwrap();
    assert_eq!(info.server_info.as_ref().unwrap().name, "kerf");
    assert!(info.instructions.as_deref().unwrap().contains("dry_run"));
    let tools = c.list_all_tools().await.unwrap();
    assert!(tools.iter().any(|t| t.name == "waveform"));

    // Relative paths resolve inside the allowed folder.
    ok(&c, "open", json!({"path": "take.wav"})).await;
    let r = ok(&c, "split", json!({"at": "0:03"})).await;
    assert_eq!(text(&r), "Split at 0:03.000");
    ok(&c, "add_marker", json!({"at": 3, "name": "Second half"})).await;
    let info = details(&ok(&c, "info", json!({})).await);
    assert_eq!(info["tracks"][0]["clips"].as_array().unwrap().len(), 2);

    // The waveform comes back as an image plus its time scale.
    let w = ok(&c, "waveform", json!({"width": 600, "height": 150})).await;
    let img = w.content[1].as_image().expect("image content");
    assert_eq!(img.mime_type, "image/png");
    let d = details(&w);
    assert_eq!(d["markers"][0]["x"], 300);
    assert!(
        d.get("png_base64").is_none(),
        "the image isn't repeated as text"
    );

    // Mistakes come back as tool errors the model can read.
    let r = call(&c, "split", json!({"at": 1, "trakcs": [1]})).await;
    assert_eq!(r.is_error, Some(true));
    assert!(text(&r).contains("unknown field"), "{}", text(&r));
    let r = call(&c, "open", json!({"path": "/etc/hostname"})).await;
    assert!(text(&r).contains("outside the folders"), "{}", text(&r));
    let r = call(&c, "new", json!({})).await;
    assert!(text(&r).contains("unsaved_changes"), "{}", text(&r));

    // Undo history is shared with resources.
    ok(&c, "undo", json!({})).await;
    let res = c
        .read_resource(ReadResourceRequestParams::new("kerf://history"))
        .await
        .unwrap();
    let hist: Value = match &res.contents[0] {
        rmcp::model::ResourceContents::TextResourceContents { text, .. } => {
            serde_json::from_str(text).unwrap()
        }
        _ => panic!("text expected"),
    };
    assert_eq!(hist["undo"], json!(["Split at 0:03.000"]));

    // Dry run, then export with progress notifications.
    let dry = details(
        &ok(
            &c,
            "export",
            json!({"path": "out/take.flac", "preset": "flac", "dry_run": true}),
        )
        .await,
    );
    assert_eq!(dry["written"], false);
    assert!(!dir.path().join("out").exists());
    let mut params = CallToolRequestParams::new("export").with_arguments(
        json!({"path": "out/take.flac", "preset": "flac"})
            .as_object()
            .unwrap()
            .clone(),
    );
    params.meta = Some(RequestMetaObject::with_progress_token(ProgressToken(
        NumberOrString::Number(7),
    )));
    let r = c.call_tool(params).await.unwrap();
    assert_ne!(r.is_error, Some(true), "{}", text(&r));
    assert!(dir.path().join("out/take.flac").is_file());
    // Notifications may trail the response slightly.
    for _ in 0..50 {
        if progress.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(progress.load(Ordering::SeqCst) > 0, "progress was reported");

    let r = call(
        &c,
        "export",
        json!({"path": "out/take.flac", "preset": "flac"}),
    )
    .await;
    assert!(
        text(&r).contains("exists"),
        "never overwrites without force"
    );

    c.cancel().await.unwrap();
}

#[tokio::test]
async fn live_album_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let c = start(dir.path(), Client::default()).await;
    let prompts = c.list_all_prompts().await.unwrap();
    assert_eq!(prompts[0].name, "prepare_live_album");
    let p = c
        .get_prompt(
            GetPromptRequestParams::new("prepare_live_album").with_arguments(
                json!({"recording": "show.wav", "songs": "12"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    let msg = serde_json::to_value(&p.messages[0]).unwrap().to_string();
    assert!(msg.contains("songs = 12"), "{msg}");
    // Without a recording the assistant asks for one.
    let p = c
        .get_prompt(GetPromptRequestParams::new("prepare_live_album"))
        .await
        .unwrap();
    let msg = serde_json::to_value(&p.messages[0]).unwrap().to_string();
    assert!(msg.contains("Ask me which recording"), "{msg}");
    c.cancel().await.unwrap();
}

mod in_app {
    use super::*;
    use kerf_mcp::host::{Connection, Host};
    use kerf_mcp::{KerfServer, LocalSession, Options, Sandbox};
    use std::path::PathBuf;

    async fn start_host(dir: &Path) -> (Host, PathBuf) {
        let file = dir.join("run").join("assistant.json");
        let sandbox = Sandbox::new([dir.to_path_buf()]).unwrap();
        let exec = Arc::new(LocalSession::new(None));
        let f = file.clone();
        let host = tokio::task::spawn_blocking(move || {
            Host::start(
                f,
                move || {
                    KerfServer::with_executor(
                        sandbox.clone(),
                        exec.clone(),
                        Options {
                            hidden_tools: vec!["open", "new"],
                            intro: Some("You're working in the Kerf app.".into()),
                        },
                    )
                },
                || {},
            )
        })
        .await
        .unwrap()
        .unwrap();
        (host, file)
    }

    async fn bridge(file: &Path) -> RunningService<RoleClient, Client> {
        let transport = TokioChildProcess::new(
            tokio::process::Command::new(env!("CARGO_BIN_EXE_kerf-mcp")).configure(|c| {
                c.arg("--app").arg("--connection").arg(file);
            }),
        )
        .unwrap();
        Client::default().serve(transport).await.unwrap()
    }

    #[tokio::test]
    async fn bridge_reaches_the_host() {
        let dir = tempfile::tempdir().unwrap();
        tone(&dir.path().join("a.wav"), 2.0);
        let (host, file) = start_host(dir.path()).await;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "only the user can read the token");
        }

        let c = bridge(&file).await;
        let info = c.peer_info().unwrap();
        assert!(info
            .instructions
            .as_deref()
            .unwrap()
            .starts_with("You're working in the Kerf app."));
        let tools = c.list_all_tools().await.unwrap();
        assert!(!tools.iter().any(|t| t.name == "open"), "hidden in the app");
        ok(&c, "import", json!({"path": "a.wav"})).await;
        let r = ok(&c, "split", json!({"at": 1})).await;
        assert_eq!(text(&r), "Split at 0:01.000");
        for _ in 0..50 {
            if !host.clients().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(host.clients().len(), 1);

        // Disconnect from the app's side: the client's calls now fail.
        host.disconnect_all();
        for _ in 0..100 {
            if host.clients().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(host.clients().is_empty());
        let params = CallToolRequestParams::new("info");
        assert!(c.call_tool(params).await.is_err());

        // Stopping the host removes its connection file.
        tokio::task::spawn_blocking(move || drop(host))
            .await
            .unwrap();
        assert!(!file.exists());
    }

    #[tokio::test]
    async fn wrong_token_or_no_app_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (host, file) = start_host(dir.path()).await;
        let forged = dir.path().join("forged.json");
        std::fs::write(
            &forged,
            serde_json::to_string(&Connection {
                port: host.port(),
                token: "0".repeat(32),
                pid: 1,
            })
            .unwrap(),
        )
        .unwrap();
        let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_kerf-mcp"))
            .arg("--app")
            .arg("--connection")
            .arg(&forged)
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .unwrap();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("refused"));

        let missing = dir.path().join("nothing.json");
        let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_kerf-mcp"))
            .args(["--app", "--connection"])
            .arg(&missing)
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&out.stderr).contains("Preferences"));
        let _ = file;
        tokio::task::spawn_blocking(move || drop(host))
            .await
            .unwrap();
    }
}
