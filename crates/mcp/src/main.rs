//! `accio-mcp`: a stdio MCP server with one tool, `code_search`, for Claude Code,
//! Codex and other coding agents. It holds no model and no index: it forwards each
//! search to the running Accio app over a local socket, so any number of agent
//! sessions share Accio's one embedding helper.
//!
//! The repository searched is the one containing the server's working directory
//! (agents start MCP servers in the project directory), or `ACCIO_REPO`.

use serde_json::{Value, json};
use std::{
    env,
    io::{self, BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    time::Duration,
};

const PROTOCOL_VERSION: &str = "2025-06-18";

const TOOL_DESCRIPTION: &str = "Semantic code search and exhaustive usages for this repository, ~0.1 s, English or German. \
Load and use this BEFORE your first grep, find, rg or git ls-files in this repository.\n\
mode \"search\" (default): concept and where/which/how questions (\"where are lead sources defined\", \
\"which payment providers are implemented\", \"how does an order reach the database\"). Returns the most \
relevant classes, methods, templates, SQL and config, ranked, with file:line and the best lines of code. \
Tests are not in this index.\n\
mode \"usages\": every line that mentions an exact identifier, data-test id, i18n key or string, complete and \
unranked, from the working tree including tests, convention guards and uncommitted files. Use it before a rename \
or refactor and for \"who uses X\" or \"list all X\".\n\
Narrow either mode with path (folder prefix such as app/frontend/src) and kind (code, sql, config, test, doc). \
In search mode, context=N returns up to N current lines of each file around the hit, so you need not open it.";

const INSTRUCTIONS: &str = "Accio indexes this repository. code_search finds code by meaning (English or German) in about 0.1 s, \
and its usages mode lists every reference to an identifier, tests included. Use it before grep or find \
when you don't know the exact file, and before refactors to see everything that mentions a symbol.";

fn socket_path() -> Option<PathBuf> {
    if let Some(path) = env::var_os("ACCIO_SOCKET") {
        return Some(path.into());
    }
    let home = env::var_os("ACCIO_HOME").map(PathBuf::from).or_else(|| {
        env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Application Support/accio"))
    })?;
    Some(home.join("accio.sock"))
}

fn repo() -> PathBuf {
    env::var_os("ACCIO_REPO")
        .map(PathBuf::from)
        .or_else(|| env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// One request to the Accio app. `Err` holds a message meant for the agent.
fn send(mut request: Value) -> Result<String, String> {
    let not_running = "Accio is not running, so code_search is unavailable. Start /Applications/Accio.app \
        and try again, or use grep meanwhile.";
    let path = socket_path().ok_or(not_running)?;
    let stream = UnixStream::connect(&path).map_err(|_| not_running.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .map_err(|error| error.to_string())?;
    let mut writer = stream.try_clone().map_err(|error| error.to_string())?;
    request["repo"] = json!(repo());
    writeln!(writer, "{request}").map_err(|error| format!("sending to Accio: {error}"))?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|error| format!("waiting for Accio: {error}"))?;
    let response: Value =
        serde_json::from_str(&line).map_err(|error| format!("invalid answer from Accio: {error}"))?;
    if response["ok"] == true {
        Ok(response["text"].as_str().unwrap_or_default().to_owned())
    } else {
        Err(response["error"]
            .as_str()
            .unwrap_or("Accio could not search")
            .to_owned())
    }
}

fn tools() -> Value {
    json!({ "tools": [{
        "name": "code_search",
        "title": "Semantic code search and usages (Accio)",
        "description": TOOL_DESCRIPTION,
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "search: what you are looking for, in words or identifiers, e.g. \"who creates lead sources\" or \"SmsGateway implementations\". usages: the exact text, e.g. \"DataGrid\" or \"save-button\" (case-sensitive; identifiers match as whole words)"
                },
                "mode": {
                    "type": "string", "enum": ["search", "usages"], "default": "search",
                    "description": "search: ranked matches by meaning. usages: every line mentioning the exact query text, tests included"
                },
                "path": {
                    "type": "string",
                    "description": "Only files under this folder or file, relative to the repository root, e.g. app/frontend/src"
                },
                "kind": {
                    "type": "string", "enum": ["code", "sql", "config", "test", "doc"],
                    "description": "Only this kind of file. test works in usages mode only"
                },
                "context": {
                    "type": "integer", "minimum": 0, "maximum": 80, "default": 0,
                    "description": "search mode: lines of the current file to show per hit (e.g. 15) instead of a 4-line snippet"
                },
                "limit": {
                    "type": "integer", "minimum": 1, "maximum": 20, "default": 8,
                    "description": "search mode: number of matches"
                }
            },
            "required": ["query"]
        },
        "annotations": { "readOnlyHint": true, "openWorldHint": false }
    }]})
}

fn call_tool(params: &Value) -> Value {
    let text_result = |text: String, is_error: bool| {
        json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
    };
    if params["name"] != "code_search" {
        return text_result(format!("unknown tool {}", params["name"]), true);
    }
    let arguments = &params["arguments"];
    let Some(query) = arguments["query"].as_str().filter(|query| !query.trim().is_empty()) else {
        return text_result("query is required".into(), true);
    };
    let mut request = match arguments["mode"].as_str().unwrap_or("search") {
        "search" => json!({
            "op": "code_search",
            "query": query,
            "limit": arguments["limit"].as_u64().unwrap_or(8).clamp(1, 20),
            "context": arguments["context"].as_u64().unwrap_or(0).min(80),
        }),
        "usages" => json!({ "op": "usages", "query": query }),
        other => {
            return text_result(
                format!("unknown mode \"{other}\"; use search or usages"),
                true,
            );
        }
    };
    for key in ["path", "kind"] {
        if let Some(value) = arguments[key]
            .as_str()
            .filter(|value| !value.trim().is_empty())
        {
            request[key] = json!(value);
        }
    }
    match send(request) {
        Ok(text) => text_result(text, false),
        Err(message) => text_result(message, true),
    }
}

/// The JSON-RPC response for one message, or None for notifications.
fn handle(message: &Value) -> Option<Value> {
    let id = message.get("id")?.clone();
    let method = message["method"].as_str().unwrap_or_default();
    let params = &message["params"];
    let result = match method {
        "initialize" => json!({
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or(PROTOCOL_VERSION),
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "accio", "version": env!("CARGO_PKG_VERSION") },
            "instructions": INSTRUCTIONS,
        }),
        "ping" => json!({}),
        "tools/list" => tools(),
        "tools/call" => call_tool(params),
        _ => {
            return Some(json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": -32601, "message": format!("method not found: {method}") }
            }));
        }
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn main() -> io::Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(message) => handle(&message),
            Err(error) => Some(json!({
                "jsonrpc": "2.0", "id": null,
                "error": { "code": -32700, "message": format!("parse error: {error}") }
            })),
        };
        if let Some(response) = response {
            writeln!(stdout, "{response}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn initialize_list_and_unknown_method() {
        let init = handle(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-03-26"}}))
        .unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(init["result"]["serverInfo"]["name"], "accio");
        assert!(handle(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).is_none());
        let list = handle(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})).unwrap();
        assert_eq!(list["result"]["tools"][0]["name"], "code_search");
        let unknown = handle(&json!({"jsonrpc": "2.0", "id": 3, "method": "nope"})).unwrap();
        assert_eq!(unknown["error"]["code"], -32601);
    }

    /// All environment-dependent cases in one test: tests run in parallel threads and
    /// share the process environment.
    #[test]
    fn tool_call_forwards_to_the_app_socket() {
        let dir = std::env::temp_dir().join(format!("accio-mcp-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("s.sock");
        let _ = std::fs::remove_file(&socket);
        // SAFETY: the only test touching the environment.
        unsafe {
            env::set_var("ACCIO_SOCKET", &socket);
            env::set_var("ACCIO_REPO", "/code/shop");
        }
        let call = |query: &str| {
            handle(&json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call",
                "params": {"name": "code_search", "arguments": {"query": query, "limit": 3}}}))
            .unwrap()["result"]
                .clone()
        };
        let down = call("order status");
        assert_eq!(down["isError"], true);
        assert!(down["content"][0]["text"].as_str().unwrap().contains("not running"));

        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap()).read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["op"], "code_search");
            assert_eq!(request["repo"], "/code/shop");
            assert_eq!(request["limit"], 3);
            assert_eq!(request["context"], 0);
            let mut stream = stream;
            writeln!(stream, "{}", json!({"ok": true, "text": "1. src/A.java:3"})).unwrap();
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["op"], "usages");
            assert_eq!(request["query"], "OrderStatus");
            assert_eq!(request["kind"], "test");
            assert_eq!(request["repo"], "/code/shop");
            assert!(request.get("path").is_none());
            let mut stream = stream;
            writeln!(stream, "{}", json!({"ok": true, "text": "2 lines"})).unwrap();
        });
        let found = call("order status");
        assert_eq!(found["isError"], false);
        assert_eq!(found["content"][0]["text"], "1. src/A.java:3");
        let usages = handle(&json!({"jsonrpc": "2.0", "id": 10, "method": "tools/call",
            "params": {"name": "code_search", "arguments": {"query": "OrderStatus", "mode": "usages", "kind": "test", "path": " "}}}))
        .unwrap()["result"]
            .clone();
        server.join().unwrap();
        assert_eq!(usages["content"][0]["text"], "2 lines");
        assert_eq!(call(" ")["isError"], true);
        let bad_mode = handle(&json!({"jsonrpc": "2.0", "id": 11, "method": "tools/call",
            "params": {"name": "code_search", "arguments": {"query": "x", "mode": "grep"}}}))
        .unwrap()["result"]
            .clone();
        assert_eq!(bad_mode["isError"], true);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
