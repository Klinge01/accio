use crate::{panel, state::AppState};
use accio_core::{
    code::repo_name,
    llm::AnswerProvider,
    prompt::{Turn, build_code_messages, build_messages, code_location, merge_followup, retrieval_query},
    store::Scope,
};
use anyhow::Result;
use serde_json::{Value, json};
use std::{path::Path, time::Instant};
use tauri::{AppHandle, Emitter, Manager, State, WebviewWindow};
use tauri_plugin_opener::OpenerExt;
use tokio_util::sync::CancellationToken;

#[tauri::command]
pub fn prefetch(query: String, history: Vec<Turn>, state: State<'_, AppState>) {
    let preferences = state.preferences.lock().unwrap();
    if preferences.prefetch && !query.trim().is_empty() {
        let history = recent(&history, preferences.memory);
        state
            .engine
            .prefetch(retrieval_query(query.trim(), history));
    }
}

#[tauri::command]
pub async fn ask(
    query: String,
    history: Vec<Turn>,
    request_id: Option<u64>,
    carry: Option<Vec<i64>>,
    state: State<'_, AppState>,
    window: WebviewWindow,
) -> Result<(), String> {
    let query = query.trim();
    let preferences = state.preferences.lock().unwrap().clone();
    let history = recent(&history, preferences.memory);
    if query.is_empty() {
        return Err("enter a question".into());
    }
    let request_id = request_id.unwrap_or(0);
    let token = CancellationToken::new();
    {
        let mut active = state.answer_cancel.lock().unwrap();
        if let Some((_, previous)) = active.replace((request_id, token.clone())) {
            previous.cancel();
        }
    }
    let emit = |event: &str, mut payload: Value| {
        let active = state.answer_cancel.lock().unwrap();
        if !token.is_cancelled() && active.as_ref().is_some_and(|(id, _)| *id == request_id) {
            payload["request_id"] = json!(request_id);
            let _ = window.emit(event, payload);
        }
    };
    let result: Result<()> = async {
        let engine = state.engine.clone();
        // A code repository in scope: answer from its code, refreshing it if due.
        let repo = match engine.scope() {
            Scope::Source(id) => {
                let source = engine.store().get_source(id)?;
                let code = state.code.clone();
                let checked = source.clone();
                tokio::task::spawn_blocking(move || code.refresh_if_due(&checked)).await?;
                Some(source)
            }
            Scope::Notes => None,
        };
        let search = retrieval_query(query, history);
        let started = Instant::now();
        let carry = if preferences.memory == 0 {
            Vec::new()
        } else {
            carry.unwrap_or_default()
        };
        let mut context = tokio::select! {
            _ = token.cancelled() => return Ok(()),
            result = tokio::task::spawn_blocking(move || -> Result<_> {
                let fresh = engine.retrieve(&search)?.hits.clone();
                Ok(merge_followup(fresh, engine.hits_by_ids(&carry)?))
            }) => result??,
        };
        context.truncate(preferences.excerpts);
        let hits: Vec<_> = context.iter().enumerate().map(|(index, hit)| json!({
            "n": index + 1, "chunk_id": hit.chunk_id, "path": hit.path, "header": hit.header,
            "page": hit.page, "stale": hit.stale, "line": hit.line,
            "location": repo.as_ref().map(|source| code_location(hit, &source.path)),
        })).collect();
        let provider = state.provider.lock().unwrap().clone();
        emit(
            "answer://meta",
            json!({
                "provider": provider.label(),
                "hits": hits, "retrieval_ms": started.elapsed().as_secs_f32() * 1000.0,
            }),
        );
        let messages = match &repo {
            Some(source) => {
                build_code_messages(&repo_name(source), &source.path, query, &context, history)
            }
            None => build_messages(query, &context, history),
        };
        if token.is_cancelled() {
            return Ok(());
        }
        *state.last_prompt.lock().unwrap() = messages.clone();
        let stats = AnswerProvider::stream(&provider, messages, token.clone(), |text| {
            emit("answer://token", json!({ "text": text }));
        })
        .await?;
        let generation_seconds = (stats.total_ms - stats.ttft_ms) / 1000.0;
        let rate = if generation_seconds > 0.0 {
            stats.completion_tokens as f32 / generation_seconds
        } else {
            0.0
        };
        emit(
            "answer://done",
            json!({
                "ttft_ms": stats.ttft_ms, "total_ms": stats.total_ms,
                "tokens": stats.completion_tokens, "tok_per_s": rate,
            }),
        );
        Ok(())
    }
    .await;
    if let Err(error) = result {
        emit("answer://error", json!({ "message": format!("{error:#}") }));
    }
    let mut active = state.answer_cancel.lock().unwrap();
    if !token.is_cancelled() {
        active.take();
    }
    Ok(())
}

#[tauri::command]
pub fn cancel_answer(request_id: Option<u64>, state: State<'_, AppState>) {
    state.cancel(request_id);
}

/// Switches the panel to one code repository (`id`) or back to notes (`None`).
#[tauri::command]
pub async fn use_code_scope(app: AppHandle, id: Option<i64>) -> Result<(), String> {
    tokio::task::spawn_blocking(move || -> Result<()> {
        let state = app.state::<AppState>();
        let _scope = state.scope_change.lock().unwrap();
        let scope = match id {
            Some(id) => {
                let source = state.engine.store().get_source(id)?;
                anyhow::ensure!(
                    source.kind == accio_core::store::SourceKind::Code,
                    "not a code repository"
                );
                state.code.refresh_if_due(&source);
                Scope::Source(id)
            }
            None => Scope::Notes,
        };
        if state.engine.scope() != scope {
            state.engine.set_scope(scope)?;
            let cancelled = state.cancel(None);
            let _ = app.emit("scope://changed", cancelled);
        }
        crate::tray::refresh(&app);
        Ok(())
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| format!("{error:#}"))
}

/// Opens `path` at `line` in the configured code editor (`ACCIO_CODE_EDITOR`): its
/// command-line tool when installed, else its URL scheme.
fn open_in_editor(editor: &str, path: &str, line: u32, window: &WebviewWindow) -> Result<()> {
    let at = format!("{path}:{line}");
    let (cli, args, url): (&str, Vec<String>, String) = match editor {
        "idea" => (
            "idea",
            vec!["--line".into(), line.to_string(), path.into()],
            format!("idea://open?file={}&line={line}", url::form_urlencoded::byte_serialize(path.as_bytes()).collect::<String>()),
        ),
        "cursor" => ("cursor", vec!["-g".into(), at.clone()], format!("cursor://file{at}")),
        "code" | "vscode" => ("code", vec!["-g".into(), at.clone()], format!("vscode://file{at}")),
        _ => ("zed", vec![at.clone()], format!("zed://file{at}")),
    };
    if std::process::Command::new(cli).args(&args).spawn().is_ok() {
        return Ok(());
    }
    window.opener().open_url(url, None::<&str>)?;
    Ok(())
}

#[tauri::command]
pub fn open_citation(
    path: String,
    page: Option<u32>,
    line: Option<u32>,
    state: State<'_, AppState>,
    window: WebviewWindow,
) -> Result<(), String> {
    let _ = page;
    let file = Path::new(&path);
    if !file.is_absolute() {
        return Err("citation path must be absolute".into());
    }
    let in_code = matches!(state.engine.scope(), Scope::Source(_));
    if in_code || line.is_some() {
        return open_in_editor(&state.engine.config().code_editor, &path, line.unwrap_or(1), &window)
            .map_err(|error| format!("{error:#}"));
    }
    if file
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
    {
        let mut url = url::Url::parse("obsidian://open").map_err(|error| error.to_string())?;
        url.query_pairs_mut().append_pair("path", &path);
        window
            .opener()
            .open_url(url.as_str().replace('+', "%20"), None::<&str>)
            .map_err(|error| error.to_string())
    } else {
        window
            .opener()
            .open_path(path, None::<&str>)
            .map_err(|error| error.to_string())
    }
}

#[tauri::command]
pub fn hide_panel(window: WebviewWindow) -> Result<(), String> {
    panel::hide(&window).map_err(|error| error.to_string())
}

fn recent(history: &[Turn], memory: usize) -> &[Turn] {
    &history[history.len().saturating_sub(memory * 2)..]
}
