//! `kerf-mcp`: Kerf's editing engine as an MCP server over stdio.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use kerf_mcp::{KerfServer, Sandbox};
use rmcp::ServiceExt;

#[derive(Parser)]
#[command(
    name = "kerf-mcp",
    version,
    about = "MCP server for the Kerf audio editor (stdio).",
    after_help = "Example client configurations:\n  \
                  {\"command\": \"kerf-mcp\", \"args\": [\"--allow\", \"/home/me/Music\"]}\n  \
                  {\"command\": \"kerf-mcp\", \"args\": [\"--app\"]}   (edit what's open in the Kerf app)"
)]
struct Args {
    /// Folder the server may read and write (repeatable). Defaults to the
    /// current folder. Relative paths in requests resolve against the first.
    #[arg(long, value_name = "DIR")]
    allow: Vec<PathBuf>,
    /// Decode audio into memory instead of the shared disk cache.
    #[arg(long)]
    no_cache: bool,
    /// Work on the project open in the running Kerf app (turn on
    /// Preferences → Assistant there) instead of a session of its own.
    #[arg(long, conflicts_with_all = ["allow", "no_cache"])]
    app: bool,
    /// Connection file the app writes (default: Kerf's runtime folder).
    #[arg(long, value_name = "FILE", requires = "app")]
    connection: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let args = Args::parse();
    if args.app {
        let file = args
            .connection
            .unwrap_or_else(kerf_mcp::host::default_connection_file);
        return match kerf_mcp::host::bridge(&file).await {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("kerf-mcp: {e}");
                std::process::ExitCode::FAILURE
            }
        };
    }
    let roots = if args.allow.is_empty() {
        vec![std::env::current_dir().unwrap_or_else(|_| ".".into())]
    } else {
        args.allow
    };
    let sandbox = match Sandbox::new(roots) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("kerf-mcp: an allowed folder can't be used: {e}");
            return std::process::ExitCode::from(2);
        }
    };
    let cache = if args.no_cache {
        None
    } else {
        kerf_core::AudioCache::open_default().ok().map(Arc::new)
    };
    eprintln!(
        "kerf-mcp {} serving on stdio; allowed folders: {}",
        env!("CARGO_PKG_VERSION"),
        sandbox
            .roots()
            .iter()
            .map(|r| r.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let server = match KerfServer::new(sandbox, cache)
        .serve(rmcp::transport::stdio())
        .await
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!("kerf-mcp: could not start: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    match server.waiting().await {
        Ok(_) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kerf-mcp: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
