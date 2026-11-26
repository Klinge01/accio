//! Local search service for coding agents: a Unix socket (user-only permissions) that
//! takes one JSON request per line and answers with one JSON line.
//!
//! Requests (`path`, `kind`, `context` and `limit` are optional):
//!   {"op": "code_search", "repo": "<path inside a repository>", "query": "...", "limit": 8,
//!    "path": "app/frontend/src", "kind": "code", "context": 20}
//!   {"op": "usages", "repo": "<path inside a repository>", "query": "<exact text>",
//!    "path": "...", "kind": "test"}
//!   {"op": "ping"}
//! Responses:
//!   {"ok": true, "text": "<compact result for an agent>", "result": {...}}
//!   {"ok": false, "error": "..."}

use crate::code::{CodeService, DEFAULT_LIMIT, SearchOptions};
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::Duration,
};

pub const SOCKET_NAME: &str = "accio.sock";

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Request {
    CodeSearch {
        repo: PathBuf,
        query: String,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        kind: Option<String>,
        #[serde(default)]
        context: Option<usize>,
    },
    Usages {
        repo: PathBuf,
        query: String,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        kind: Option<String>,
    },
    Ping,
}

fn respond(service: &CodeService, line: &str) -> Value {
    let result = serde_json::from_str::<Request>(line)
        .context("invalid request")
        .and_then(|request| match request {
            Request::Ping => Ok(json!({ "ok": true, "text": "pong" })),
            Request::CodeSearch {
                repo,
                query,
                limit,
                path,
                kind,
                context,
            } => {
                let options = SearchOptions {
                    limit: limit.unwrap_or(DEFAULT_LIMIT),
                    path,
                    kind,
                    context: context.unwrap_or(0),
                };
                let search = service.search(&repo, &query, &options)?;
                Ok(json!({ "ok": true, "text": search.to_text(), "result": search }))
            }
            Request::Usages {
                repo,
                query,
                path,
                kind,
            } => {
                let options = SearchOptions {
                    path,
                    kind,
                    ..SearchOptions::default()
                };
                let usages = service.usages(&repo, &query, &options)?;
                Ok(json!({ "ok": true, "text": usages.to_text(), "result": usages }))
            }
        });
    result.unwrap_or_else(|error| json!({ "ok": false, "error": format!("{error:#}") }))
}

fn handle(service: &CodeService, stream: UnixStream) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(600)))?;
    let mut writer = stream.try_clone()?;
    for line in BufReader::new(stream).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = respond(service, &line);
        writeln!(writer, "{response}")?;
        writer.flush()?;
    }
    Ok(())
}

/// Listens on `path` in a background thread. A stale socket file from an earlier run
/// is replaced.
pub fn serve(path: &Path, service: Arc<CodeService>) -> Result<()> {
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            anyhow::bail!("another Accio is already serving {}", path.display());
        }
        std::fs::remove_file(path)
            .with_context(|| format!("removing stale socket {}", path.display()))?;
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("listening on {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    thread::Builder::new()
        .name("accio-socket".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let service = service.clone();
                thread::spawn(move || {
                    if let Err(error) = handle(&service, stream) {
                        eprintln!("search socket: {error:#}");
                    }
                });
            }
        })?;
    Ok(())
}
