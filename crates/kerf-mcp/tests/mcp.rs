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
    assert!(c
        .get_prompt(GetPromptRequestParams::new("prepare_live_album"))
        .await
        .is_err());
    c.cancel().await.unwrap();
}
