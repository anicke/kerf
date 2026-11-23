//! Kerf as an MCP server: any MCP client (Claude Desktop, Claude Code, IDEs)
//! can open, inspect, edit and export audio with Kerf's engine.
//!
//! The tools are the operations of [`kerf_core::ops`], one per request type,
//! running against one live session with full undo. An assistant can't hear,
//! so results report levels and positions in text, and the `waveform` tool
//! returns a picture. Files are only read or written inside the folders the
//! server was started with.

pub mod editor;
pub mod host;
pub mod sandbox;
pub mod tools;

use std::sync::{Arc, Mutex};

use kerf_core::ops::{self, Data, OpError, Request, Response, Session};
use kerf_core::AudioCache;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, GetPromptRequestParams,
    GetPromptResponse, GetPromptResult, Implementation, ListPromptsResult, ListResourcesResult,
    ListToolsResult, PaginatedRequestParams, ProgressNotificationParam, Prompt, PromptArgument,
    PromptMessage, ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, Role, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::{json, Value};

pub use sandbox::Sandbox;

const INSTRUCTIONS: &str = "\
Kerf is a non-destructive audio editor. You work on one open project at a time: `open` a .kerf \
project or an audio file, edit it, then `save` and/or `export`. Every edit is one undoable step \
(`undo`, `redo`, `history`); nothing touches the source audio.

Times are seconds (83.5), \"m:ss\" (\"1:23.5\"), \"h:mm:ss\" or exact samples (\"4008000smp\"). \
Results report positions as seconds, frames and m:ss.

You can't hear the audio, so look and measure: `info` for the structure, `analyze` for levels, \
`waveform` for a picture of any range (zoom in by choosing start/end), `find_silences`, and \
`detect_songs` for live recordings. Check edits the same way.

Exports never overwrite files unless `force` is set; run `export` with `dry_run` first and show \
the user what will be written. For a live concert, use the prompt `prepare_live_album`.";

/// Where operations run: a session of the server's own, or an editor's
/// open project (see the app's assistant host).
pub trait Executor: Send + Sync + 'static {
    /// Run one operation (on a blocking thread). `progress` gets 0..=1 and
    /// returns `false` to cancel.
    fn execute(
        &self,
        request: Request,
        progress: &mut dyn FnMut(f64) -> bool,
    ) -> Result<Response, OpError>;
}

/// A session of the server's own (headless use).
pub struct LocalSession(Mutex<Session>);

impl LocalSession {
    pub fn new(cache: Option<Arc<AudioCache>>) -> Self {
        Self(Mutex::new(Session::new(cache)))
    }
}

impl Executor for LocalSession {
    fn execute(
        &self,
        request: Request,
        progress: &mut dyn FnMut(f64) -> bool,
    ) -> Result<Response, OpError> {
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        s.execute_with_progress(request, progress)
    }
}

/// How a server presents itself.
#[derive(Clone, Default)]
pub struct Options {
    /// Operations not offered as tools (e.g. `open` inside the app).
    pub hidden_tools: Vec<&'static str>,
    /// Replaces the opening paragraph of the instructions.
    pub intro: Option<String>,
}

struct Inner {
    executor: Arc<dyn Executor>,
    sandbox: Sandbox,
    tools: Vec<Tool>,
    instructions: String,
}

/// The MCP server. Cheap to clone; clones share the session.
#[derive(Clone)]
pub struct KerfServer {
    inner: Arc<Inner>,
}

impl KerfServer {
    /// A headless server with a session of its own.
    pub fn new(sandbox: Sandbox, cache: Option<Arc<AudioCache>>) -> Self {
        Self::with_executor(
            sandbox,
            Arc::new(LocalSession::new(cache)),
            Options::default(),
        )
    }

    pub fn with_executor(sandbox: Sandbox, executor: Arc<dyn Executor>, options: Options) -> Self {
        let tools = tools::tools()
            .into_iter()
            .filter(|t| !options.hidden_tools.contains(&t.name.as_ref()))
            .collect();
        let instructions = match &options.intro {
            Some(intro) => {
                let rest = INSTRUCTIONS.split_once("\n\n").map_or("", |x| x.1);
                format!("{intro}\n\n{rest}")
            }
            None => INSTRUCTIONS.to_string(),
        };
        Self {
            inner: Arc::new(Inner {
                executor,
                sandbox,
                tools,
                instructions,
            }),
        }
    }

    /// Run a request against the session on a blocking thread, reporting
    /// progress and honouring cancellation.
    async fn run(
        &self,
        request: Request,
        ctx: Option<&RequestContext<RoleServer>>,
    ) -> Result<Result<Response, OpError>, ErrorData> {
        let token = ctx.and_then(|c| c.meta.get_progress_token());
        let ct = ctx.map(|c| c.ct.clone());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<f64>();
        let forward = match (token, ctx) {
            (Some(token), Some(ctx)) => {
                let peer = ctx.peer.clone();
                Some(tokio::spawn(async move {
                    while let Some(p) = rx.recv().await {
                        let param =
                            ProgressNotificationParam::new(token.clone(), p).with_total(1.0);
                        let _ = peer.notify_progress(param).await;
                    }
                }))
            }
            _ => None,
        };
        let inner = self.inner.clone();
        let result = tokio::task::spawn_blocking(move || {
            let mut last = -1.0;
            let mut progress = |p: f64| {
                if p - last >= 0.01 {
                    last = p;
                    let _ = tx.send(p);
                }
                !ct.as_ref().is_some_and(|c| c.is_cancelled())
            };
            inner.executor.execute(request, &mut progress)
        })
        .await
        .map_err(|e| ErrorData::internal_error(format!("operation failed: {e}"), None))?;
        if let Some(f) = forward {
            let _ = f.await;
        }
        Ok(result)
    }
}

/// Response data as JSON, without the summary (shown separately).
fn data_json(r: &Response) -> Value {
    let mut v = serde_json::to_value(r).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.remove("summary");
    }
    v
}

/// Turn an operation's outcome into tool content: the summary, the details
/// as JSON, and for waveforms the image itself.
pub fn tool_result(result: Result<Response, OpError>) -> CallToolResult {
    match result {
        Err(e) => {
            let code = serde_json::to_value(e.code).unwrap_or(Value::Null);
            CallToolResult::error(vec![ContentBlock::text(format!(
                "Error ({}): {}",
                code.as_str().unwrap_or("error"),
                e.message
            ))])
        }
        Ok(r) => {
            let mut content = Vec::new();
            let mut details = data_json(&r);
            if let Data::Waveform(w) = &r.data {
                content.push(ContentBlock::image(w.png_base64.clone(), "image/png"));
                if let Some(o) = details.as_object_mut() {
                    o.remove("png_base64");
                }
            }
            let text = if details.get("type") == Some(&json!("none")) {
                r.summary.clone()
            } else {
                format!("{}\n{}", r.summary, details)
            };
            content.insert(0, ContentBlock::text(text));
            CallToolResult::success(content)
        }
    }
}

const RESOURCES: [(&str, &str, &str); 3] = [
    (
        "kerf://project",
        "project",
        "The open project: format, length, tracks, clips, markers and album details.",
    ),
    (
        "kerf://markers",
        "markers",
        "Markers (song starts) with their positions and names.",
    ),
    (
        "kerf://history",
        "history",
        "Undoable edit steps, oldest first.",
    ),
];

impl ServerHandler for KerfServer {
    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_prompts()
                .build(),
        );
        info.server_info = Implementation::new("kerf", env!("CARGO_PKG_VERSION"));
        info.instructions = Some(self.inner.instructions.clone());
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(self.inner.tools.clone()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let name = request.name.to_string();
        if !self.inner.tools.iter().any(|t| t.name == name) {
            return Err(ErrorData::invalid_params(
                format!("unknown tool {name}"),
                None,
            ));
        }
        let mut args = request.arguments.unwrap_or_default();
        args.insert("op".into(), json!(name));
        let op: Request = match serde_json::from_value(Value::Object(args)) {
            Ok(r) => r,
            Err(e) => {
                return Ok(tool_result(Err(OpError::new(
                    ops::ErrorCode::InvalidRequest,
                    format!("invalid arguments for {name}: {e}"),
                )))
                .into())
            }
        };
        let op = match self.inner.sandbox.check(op) {
            Ok(r) => r,
            Err(msg) => {
                return Ok(
                    tool_result(Err(OpError::new(ops::ErrorCode::InvalidArgument, msg))).into(),
                )
            }
        };
        Ok(tool_result(self.run(op, Some(&ctx)).await?).into())
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult::with_all_items(
            RESOURCES
                .iter()
                .map(|(uri, name, desc)| {
                    Resource::new(*uri, *name)
                        .with_description(*desc)
                        .with_mime_type("application/json")
                })
                .collect(),
        ))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let uri = request.uri.as_str();
        let (req, key) = match uri {
            "kerf://project" => (Request::Info, None),
            "kerf://markers" => (Request::Info, Some("markers")),
            "kerf://history" => (Request::History, None),
            _ => {
                return Err(ErrorData::resource_not_found(
                    format!("no resource {uri}"),
                    None,
                ))
            }
        };
        let r = self
            .run(req, None)
            .await?
            .map_err(|e| ErrorData::internal_error(e.message, None))?;
        let mut v = data_json(&r);
        if let Some(k) = key {
            v = v.get(k).cloned().unwrap_or(Value::Null);
        }
        let text = serde_json::to_string_pretty(&v).unwrap_or_default();
        let contents = ResourceContents::TextResourceContents {
            uri: uri.to_string(),
            mime_type: Some("application/json".into()),
            text,
            meta: None,
        };
        Ok(ReadResourceResult::new(vec![contents]).into())
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        Ok(ListPromptsResult::with_all_items(vec![live_album_prompt()]))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, ErrorData> {
        if request.name != "prepare_live_album" {
            return Err(ErrorData::invalid_params(
                format!("no prompt {}", request.name),
                None,
            ));
        }
        let args = request.arguments.unwrap_or_default();
        let arg = |k: &str| {
            args.get(k)
                .map(|v| {
                    v.as_str()
                        .map(String::from)
                        .unwrap_or_else(|| v.to_string())
                })
                .filter(|s| !s.trim().is_empty())
        };
        let recording = arg("recording")
            .ok_or_else(|| ErrorData::invalid_params("the recording argument is required", None))?;
        let text = live_album_text(
            &recording,
            arg("songs").as_deref(),
            arg("setlist").as_deref(),
            arg("target_lufs").as_deref(),
            arg("output").as_deref(),
            arg("format").as_deref(),
        );
        let mut result = GetPromptResult::new(vec![PromptMessage::new_text(Role::User, text)]);
        result.description = Some("Turn a live recording into a finished album".into());
        Ok(result.into())
    }
}

fn live_album_prompt() -> Prompt {
    let arg = |name: &str, desc: &str, required: bool| {
        PromptArgument::new(name)
            .with_description(desc)
            .with_required(required)
    };
    Prompt::new(
        "prepare_live_album",
        Some("Turn a live concert recording into a finished album: find the songs, name them, level, tag and export."),
        Some(vec![
            arg("recording", "The concert recording (audio file or .kerf project).", true),
            arg("songs", "How many songs were played, if known.", false),
            arg("setlist", "Song titles in order, one per line.", false),
            arg("target_lufs", "Album loudness, e.g. -16 (dynamic live music) or -14 (streaming).", false),
            arg("output", "Folder for the finished files.", false),
            arg("format", "cd, flac or wav (default flac).", false),
        ]),
    )
}

fn live_album_text(
    recording: &str,
    songs: Option<&str>,
    setlist: Option<&str>,
    target: Option<&str>,
    output: Option<&str>,
    format: Option<&str>,
) -> String {
    let mut t = format!(
        "Help me turn the live recording \"{recording}\" into a finished album using the Kerf tools. \
         Work step by step and show me what you find before anything is written.\n\n\
         1. `open` the recording and check `info` and `analyze` (length, levels, clipping).\n"
    );
    t += &match songs {
        Some(n) => format!("2. `detect_songs` with songs = {n}"),
        None => "2. `detect_songs` (I'm not sure how many songs there are)".into(),
    };
    match setlist {
        Some(s) => {
            let titles: Vec<String> = s
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(|l| serde_json::to_string(l).unwrap_or_default())
                .collect();
            t += &format!(" and setlist = [{}]", titles.join(", "));
        }
        None => t += "; we'll name the songs together",
    }
    t += ", without `apply` first. List the song starts with their confidence. Look at weak \
           or doubtful boundaries with `waveform` (a minute or so around each), and tell me \
           which ones you're unsure about. When we agree, run it again with `apply` (or adjust \
           markers with `add_marker`, `update_marker`, `remove_marker`).\n";
    t += "3. Ask me for the album details (artist, album title, year, genre, cover image) if I \
          haven't given them, and set them with `set_album`.\n";
    t += &format!(
        "4. `album_summary` to check every track (flags such as short tracks or clipping), then \
         `album_loudness` to {} LUFS with a -1 dBFS ceiling, so the songs keep their relative levels.\n",
        target.unwrap_or("-16")
    );
    let fmt = format.unwrap_or("flac");
    let out = output.map_or("a folder next to the recording".to_string(), |o| {
        format!("\"{o}\"")
    });
    t += &format!(
        "5. `export` kind = tracks, preset = {fmt}, path = {out} with `dry_run` first; show me the \
         file list and any warnings, then export for real once I confirm. Never use `force` \
         without asking me.\n6. `save` the project next to the recording so I can review it in \
         the Kerf app."
    );
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_text_mentions_choices() {
        let t = live_album_text(
            "show.wav",
            Some("14"),
            Some("1. Intro\n\nSong \"B\"\n"),
            None,
            None,
            Some("cd"),
        );
        assert!(t.contains("songs = 14"));
        assert!(t.contains(r#"setlist = ["1. Intro", "Song \"B\""]"#), "{t}");
        assert!(t.contains("preset = cd"));
        assert!(t.contains("-16 LUFS"));
    }

    #[test]
    fn errors_and_waveforms_as_content() {
        let r = tool_result(Err(OpError::new(ops::ErrorCode::Exists, "x exists")));
        assert_eq!(r.is_error, Some(true));
        assert_eq!(
            r.content[0].as_text().unwrap().text,
            "Error (exists): x exists"
        );
    }
}
