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
    after_help = "Example client configuration:\n  \
                  {\"command\": \"kerf-mcp\", \"args\": [\"--allow\", \"/home/me/Music\"]}"
)]
struct Args {
    /// Folder the server may read and write (repeatable). Defaults to the
    /// current folder. Relative paths in requests resolve against the first.
    #[arg(long, value_name = "DIR")]
    allow: Vec<PathBuf>,
    /// Decode audio into memory instead of the shared disk cache.
    #[arg(long)]
    no_cache: bool,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let args = Args::parse();
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
