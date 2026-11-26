use crate::{diagnostics, panel, state::AppState, tray};
use accio_core::{
    config::Config,
    llm::{AnswerProvider, Groq, Provider},
    prompt::Turn,
    store::{PdfMode, Store},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
use tauri::{AppHandle, Emitter, Manager, State, WebviewWindow};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};
use tauri_plugin_opener::OpenerExt;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub provider: Provider,
    pub models: HashMap<String, String>,
    pub min_similarity: f32,
    pub excerpts: usize,
    pub memory: usize,
    pub prefetch: bool,
    pub shortcut: String,
    pub panel_width: u32,
    pub panel_height: u32,
    pub reset_minutes: u32,
    pub default_profile: Option<i64>,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            provider: Provider::Groq,
            models: HashMap::new(),
            min_similarity: accio_core::config::DEFAULT_MIN_SIMILARITY,
            excerpts: 8,
            memory: 6,
            prefetch: true,
            shortcut: "Alt+Space".into(),
            panel_width: 860,
            panel_height: 620,
            reset_minutes: 10,
            default_profile: None,
        }
    }
}

impl Preferences {
    pub fn load(store: &Store, config: &Config) -> Result<Self> {
        let mut preferences: Self = store
            .setting("preferences")?
            .map(|saved| serde_json::from_str(&saved))
            .transpose()?
            .unwrap_or_else(|| Self {
                min_similarity: config.min_similarity,
                excerpts: config.max_context_chunks,
                ..Self::default()
            });
        preferences
            .models
            .entry(Provider::Groq.id().into())
            .or_insert_with(|| config.groq_model.clone());
        if preferences.default_profile.is_some_and(|id| {
            store
                .profiles()
                .is_ok_and(|profiles| !profiles.iter().any(|p| p.id == id))
        }) {
            preferences.default_profile = None;
        }
        Ok(preferences)
    }

    fn save(&self, store: &Store) -> Result<()> {
        store.set_setting("preferences", &serde_json::to_string(self)?)
    }

    fn model(&self, provider: Provider) -> String {
        self.models.get(provider.id()).cloned().unwrap_or_default()
    }

    pub fn provider_without_key(&self) -> Groq {
        Groq::for_provider(self.provider, None, self.model(self.provider))
    }

    pub fn answer_provider(&self, config: &Config) -> Result<Groq> {
        let key = keychain_key(self.provider)?.or_else(|| {
            (self.provider == Provider::Groq)
                .then(|| config.groq_api_key.clone())
                .flatten()
        });
        Ok(Groq::for_provider(
            self.provider,
            key,
            self.model(self.provider),
        ))
    }
}

#[cfg(target_os = "macos")]
fn keychain_key(provider: Provider) -> Result<Option<String>> {
    if !provider.requires_key() {
        return Ok(None);
    }
    match security_framework::passwords::get_generic_password(
        "io.github.klinge01.accio",
        provider.id(),
    ) {
        Ok(key) => Ok(Some(String::from_utf8(key)?)),
        Err(error) if error.code() == -25300 => Ok(None),
        Err(error) => Err(error).context("reading API key from Keychain"),
    }
}

#[cfg(not(target_os = "macos"))]
fn keychain_key(_provider: Provider) -> Result<Option<String>> {
    Ok(None)
}

#[cfg(target_os = "macos")]
fn save_key(provider: Provider, key: &str) -> Result<()> {
    security_framework::passwords::set_generic_password(
        "io.github.klinge01.accio",
        provider.id(),
        key.as_bytes(),
    )
    .context("writing API key to Keychain")
}

#[cfg(not(target_os = "macos"))]
fn save_key(_provider: Provider, _key: &str) -> Result<()> {
    anyhow::bail!("Keychain requires macOS")
}

pub fn show(app: &AppHandle) -> Result<()> {
    if let Some(window) = app.get_webview_window("settings") {
        window.show()?;
        window.set_focus()?;
    }
    Ok(())
}

pub fn register_shortcut(app: &AppHandle, shortcut: Shortcut) -> Result<()> {
    app.global_shortcut()
        .on_shortcut(shortcut, |app, _, event| {
            if event.state == ShortcutState::Pressed {
                let app = app.clone();
                let _ = app.clone().run_on_main_thread(move || {
                    if let Err(error) = panel::toggle(&app) {
                        eprintln!("toggling chat panel: {error}");
                    }
                });
            }
        })?;
    Ok(())
}

fn change_scope(app: &AppHandle, update: impl FnOnce(&Store) -> Result<()>) -> Result<()> {
    let state = app.state::<AppState>();
    let _scope = state.scope_change.lock().unwrap();
    update(state.engine.store())?;
    state.engine.reload_index()?;
    let cancelled = state.cancel(None);
    let _ = app.emit("scope://changed", cancelled);
    tray::refresh(app);
    Ok(())
}

pub fn set_enabled(app: &AppHandle, id: i64, enabled: bool) -> Result<()> {
    change_scope(app, |store| store.set_source_enabled(id, enabled))
}

pub fn apply_profile(app: &AppHandle, id: i64) -> Result<()> {
    change_scope(app, |store| store.apply_profile(id))
}

#[tauri::command]
pub fn open_settings(app: AppHandle) -> Result<(), String> {
    show(&app).map_err(message)
}

pub fn message(error: anyhow::Error) -> String {
    format!("{error:#}")
}

#[tauri::command]
pub fn panel_options(state: State<'_, AppState>) -> Result<Value, String> {
    let prefs = state.preferences.lock().unwrap();
    let all = state.engine.store().list_sources().map_err(message)?;
    let (repos, sources): (Vec<_>, Vec<_>) = all
        .into_iter()
        .partition(|source| source.kind == accio_core::store::SourceKind::Code);
    let scope = match state.engine.scope() {
        accio_core::store::Scope::Source(id) => Some(id),
        accio_core::store::Scope::Notes => None,
    };
    Ok(json!({
        "active": sources.iter().filter(|source| source.enabled).count(),
        "total": sources.len(), "reset_minutes": prefs.reset_minutes,
        "profiles": state.engine.store().profiles().map_err(message)?.iter().map(|p| json!({"id": p.id, "name": p.name})).collect::<Vec<_>>(),
        "repos": repos.iter().map(|source| json!({"id": source.id, "name": accio_core::code::repo_name(source)})).collect::<Vec<_>>(),
        "code_scope": scope,
    }))
}

#[tauri::command]
pub fn settings_snapshot(state: State<'_, AppState>) -> Result<Value, String> {
    let prefs = state.preferences.lock().unwrap();
    let store = state.engine.store();
    Ok(json!({
        "preferences": &*prefs, "sources": store.list_sources().map_err(message)?,
        "profiles": store.profiles().map_err(message)?,
        "index_path": state.engine.config().db_path(), "index_size": index_size(&state.engine.config().db_path()),
        "embedder": state.engine.embedder().model_id(),
        "launch_status": login_status().unwrap_or(-1),
    }))
}

fn index_size(path: &Path) -> u64 {
    let size = |path: &Path| path.metadata().map_or(0, |metadata| metadata.len());
    size(path) + size(&path.with_extension("sqlite-wal"))
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PreferenceChange {
    Provider {
        provider: Provider,
    },
    Model {
        provider: Provider,
        model: String,
    },
    Search {
        similarity: f32,
        excerpts: usize,
        memory: usize,
        prefetch: bool,
    },
    Panel {
        width: u32,
        height: u32,
        reset_minutes: u32,
    },
    Shortcut {
        value: String,
    },
    DefaultProfile {
        id: Option<i64>,
    },
}

#[tauri::command]
pub fn change_preference(
    app: AppHandle,
    state: State<'_, AppState>,
    change: PreferenceChange,
) -> Result<(), String> {
    let result = || -> Result<()> {
        let engine = &state.engine;
        let mut prefs = state.preferences.lock().unwrap();
        let mut updated = prefs.clone();
        let mut new_provider = None;
        match change {
            PreferenceChange::Provider { provider } => {
                updated.provider = provider;
                new_provider = Some(updated.answer_provider(engine.config())?);
            }
            PreferenceChange::Model { provider, model } => {
                ensure!(
                    model.len() <= 250 && !model.starts_with('-'),
                    "invalid model ID"
                );
                updated
                    .models
                    .insert(provider.id().into(), model.trim().into());
                if provider == updated.provider {
                    new_provider = Some(
                        state
                            .provider
                            .lock()
                            .unwrap()
                            .with_model(updated.model(provider)),
                    );
                }
            }
            PreferenceChange::Search {
                similarity,
                excerpts,
                memory,
                prefetch,
            } => {
                ensure!(
                    (0.0..=1.0).contains(&similarity)
                        && (1..=10).contains(&excerpts)
                        && memory <= 6,
                    "invalid search settings"
                );
                updated.min_similarity = similarity;
                updated.excerpts = excerpts;
                updated.memory = memory;
                updated.prefetch = prefetch;
            }
            PreferenceChange::Panel {
                width,
                height,
                reset_minutes,
            } => {
                ensure!(
                    (480..=1600).contains(&width)
                        && (320..=1000).contains(&height)
                        && reset_minutes <= 120,
                    "invalid panel settings"
                );
                if let Some(panel) = app.get_webview_window("panel") {
                    panel.set_size(tauri::LogicalSize::new(width, height))?;
                }
                updated.panel_width = width;
                updated.panel_height = height;
                updated.reset_minutes = reset_minutes;
            }
            PreferenceChange::Shortcut { value } => {
                let shortcut: Shortcut = value
                    .parse()
                    .context("invalid shortcut, for example Alt+Space")?;
                ensure!(
                    shortcut
                        .mods
                        .contains(tauri_plugin_global_shortcut::Modifiers::ALT)
                        || shortcut
                            .mods
                            .contains(tauri_plugin_global_shortcut::Modifiers::SUPER)
                        || shortcut
                            .mods
                            .contains(tauri_plugin_global_shortcut::Modifiers::CONTROL),
                    "choose a shortcut with Option, Command, or Control"
                );
                let old: Shortcut = prefs.shortcut.parse()?;
                let registered = app.global_shortcut().is_registered(old);
                if shortcut != old || !registered {
                    register_shortcut(&app, shortcut)?;
                    if registered && let Err(error) = app.global_shortcut().unregister(old) {
                        let _ = app.global_shortcut().unregister(shortcut);
                        return Err(error.into());
                    }
                    updated.shortcut = value;
                }
            }
            PreferenceChange::DefaultProfile { id } => {
                ensure!(
                    id.is_none_or(|id| engine
                        .store()
                        .profiles()
                        .is_ok_and(|profiles| profiles.iter().any(|p| p.id == id))),
                    "profile not found"
                );
                updated.default_profile = id;
            }
        }
        updated.save(engine.store())?;
        if let Some(provider) = new_provider {
            *state.provider.lock().unwrap() = provider;
        }
        engine.set_search_options(updated.min_similarity, updated.excerpts);
        if !updated.prefetch {
            engine.prefetch(String::new());
        }
        *prefs = updated;
        drop(prefs);
        tray::refresh(&app);
        Ok(())
    };
    result().map_err(message)
}

#[tauri::command]
pub async fn provider_models(
    state: State<'_, AppState>,
    provider: Provider,
) -> Result<Vec<String>, String> {
    let answer = {
        let mut prefs = state.preferences.lock().unwrap().clone();
        prefs.provider = provider;
        prefs
            .answer_provider(state.engine.config())
            .map_err(message)?
    };
    answer.models().await.map_err(message)
}

#[tauri::command]
pub async fn test_api_key(
    state: State<'_, AppState>,
    provider: Provider,
    key: String,
    model: String,
) -> Result<Value, String> {
    let result = async {
        ensure!(
            provider.requires_key() && !key.trim().is_empty(),
            "enter an API key"
        );
        let key = key.trim();
        let answer = Groq::for_provider(provider, Some(key.to_owned()), model.trim().into());
        let models = answer.models().await?;
        let selected = if model.trim().is_empty() {
            models
                .iter()
                .find(|id| match provider {
                    Provider::Groq => id.starts_with("openai/gpt-oss"),
                    Provider::OpenAi => id.starts_with("gpt-") && id.ends_with("-mini"),
                    Provider::Anthropic => id.contains("haiku"),
                    _ => false,
                })
                .or_else(|| models.first())
                .context("no models available")?
                .clone()
        } else {
            model.trim().to_owned()
        };
        let answer = answer.with_model(selected.clone());
        let stats = AnswerProvider::stream(
            &answer,
            vec![Turn::new("user", "Reply with OK.")],
            CancellationToken::new(),
            |_| {},
        )
        .await?;
        ensure!(stats.ttft_ms > 0.0, "provider returned no text");
        let mut prefs = state.preferences.lock().unwrap();
        save_key(provider, key)?;
        let mut updated = prefs.clone();
        updated
            .models
            .insert(provider.id().into(), selected.clone());
        updated.save(state.engine.store())?;
        if updated.provider == provider {
            *state.provider.lock().unwrap() = answer;
        }
        *prefs = updated;
        Ok(json!({"ttft_ms": stats.ttft_ms, "models": models, "model": selected}))
    }
    .await;
    result.map_err(message)
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceChange {
    Enabled { enabled: bool },
    Reingest { force: bool },
    Cancel,
    Remove,
    Pdf { markdown: bool },
    Captions { enabled: bool },
    Exclude { path: PathBuf, excluded: bool },
}

#[tauri::command]
pub async fn change_source(app: AppHandle, id: i64, change: SourceChange) -> Result<(), String> {
    tokio::task::spawn_blocking(move || -> Result<()> {
        let state = app.state::<AppState>();
        let store = state.engine.store();
        match change {
            SourceChange::Enabled { enabled } => return set_enabled(&app, id, enabled),
            SourceChange::Reingest { force } => state.ingest.start(id, force),
            SourceChange::Cancel => state.ingest.cancel(id),
            SourceChange::Remove => {
                state.ingest.remove_source(id)?;
                let cancelled = state.cancel(None);
                let _ = app.emit("scope://changed", cancelled);
            }
            SourceChange::Pdf { markdown } => {
                ensure!(!state.ingest.is_active(id), "wait for ingestion to finish");
                if store.set_pdf_mode(
                    id,
                    if markdown {
                        PdfMode::Markdown
                    } else {
                        PdfMode::Text
                    },
                )? {
                    state.engine.reload_index()?;
                    state.ingest.start(id, false);
                }
            }
            SourceChange::Captions { enabled } => {
                ensure!(!state.ingest.is_active(id), "wait for ingestion to finish");
                store.set_source_captions(id, enabled)?;
                state.ingest.start(id, true);
            }
            SourceChange::Exclude { path, excluded } => {
                ensure!(!state.ingest.is_active(id), "wait for ingestion to finish");
                let (root, folder) = source_folder(store, id, &path)?;
                ensure!(root != folder, "use the source toggle for the root folder");
                store.set_folder_excluded(id, folder.strip_prefix(root)?, excluded)?;
                state.ingest.start(id, false);
            }
        }
        tray::refresh(&app);
        Ok(())
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(message)
}

fn source_folder(store: &Store, id: i64, path: &Path) -> Result<(PathBuf, PathBuf)> {
    let root = store.get_source(id)?.path;
    let folder = if path.as_os_str().is_empty() {
        root.clone()
    } else {
        path.canonicalize()?
    };
    ensure!(
        folder.starts_with(&root) && folder.is_dir(),
        "folder is outside this source"
    );
    Ok((root, folder))
}

#[tauri::command]
pub fn source_folders(state: State<'_, AppState>, id: i64, path: PathBuf) -> Result<Value, String> {
    let result = || -> Result<_> {
        let store = state.engine.store();
        let source = store.get_source(id)?;
        let (root, folder) = source_folder(store, id, &path)?;
        let mut entries = Vec::new();
        for entry in folder.read_dir()? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() || entry.file_name().to_string_lossy().starts_with('.')
            {
                continue;
            }
            let path = entry.path();
            let relative = path.strip_prefix(&root)?.to_string_lossy();
            let excluded = source
                .ignore_globs
                .contains(&accio_core::scan::literal_glob(&relative));
            entries.push((
                entry.file_name().to_string_lossy().into_owned(),
                path,
                excluded,
            ));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(json!(entries))
    };
    result().map_err(message)
}

#[tauri::command]
pub fn failed_files(state: State<'_, AppState>, id: i64) -> Result<Value, String> {
    Ok(json!(
        state
            .engine
            .store()
            .failed_files(id, 1000)
            .map_err(message)?
    ))
}

#[tauri::command]
pub fn pick_source(app: AppHandle, files: bool) {
    tray::pick_source(&app, files);
}

#[tauri::command]
pub fn pick_code_repository(app: AppHandle) {
    tray::pick_code_repository(&app);
}

#[tauri::command]
pub fn save_profile(
    app: AppHandle,
    state: State<'_, AppState>,
    id: Option<i64>,
    name: String,
    sources: Vec<i64>,
) -> Result<(), String> {
    state
        .engine
        .store()
        .save_profile(id, &name, &sources)
        .map_err(message)?;
    tray::refresh(&app);
    Ok(())
}

#[tauri::command]
pub fn delete_profile(app: AppHandle, state: State<'_, AppState>, id: i64) -> Result<(), String> {
    state.engine.store().delete_profile(id).map_err(message)?;
    let mut prefs = state.preferences.lock().unwrap();
    if prefs.default_profile == Some(id) {
        prefs.default_profile = None;
        prefs.save(state.engine.store()).map_err(message)?;
    }
    tray::refresh(&app);
    Ok(())
}

#[tauri::command]
pub async fn use_profile(app: AppHandle, id: i64) -> Result<(), String> {
    tokio::task::spawn_blocking(move || apply_profile(&app, id))
        .await
        .map_err(|error| error.to_string())?
        .map_err(message)
}

#[tauri::command]
pub fn last_prompt(state: State<'_, AppState>) -> Vec<Turn> {
    state.last_prompt.lock().unwrap().clone()
}

#[tauri::command]
pub fn reveal_index(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    app.opener()
        .reveal_item_in_dir(state.engine.config().db_path())
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn reveal_logs(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    app.opener()
        .reveal_item_in_dir(diagnostics::log_path(&state.engine.config().home))
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn embedding_status(state: State<'_, AppState>) -> Result<Value, String> {
    let pid = state.engine.embedder().process_id();
    let memory = tokio::task::spawn_blocking(move || pid.and_then(diagnostics::helper_memory))
        .await
        .map_err(|error| error.to_string())?;
    Ok(json!({"running": memory.is_some(), "memory": memory}))
}

#[tauri::command]
pub async fn clear_index(app: AppHandle) -> Result<(), String> {
    tokio::task::spawn_blocking(move || -> Result<()> {
        let state = app.state::<AppState>();
        state.ingest.clear_index()?;
        let cancelled = state.cancel(None);
        let _ = app.emit("scope://changed", cancelled);
        tray::refresh(&app);
        Ok(())
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(message)
}

#[cfg(target_os = "macos")]
#[link(name = "ServiceManagement", kind = "framework")]
unsafe extern "C" {}

#[cfg(target_os = "macos")]
fn login_service() -> Result<objc2::rc::Retained<objc2::runtime::AnyObject>> {
    let class = objc2::runtime::AnyClass::get(c"SMAppService")
        .context("launch at login requires macOS 13")?;
    let service = unsafe { objc2::msg_send![class, mainAppService] };
    Ok(service)
}

fn login_status() -> Result<isize> {
    #[cfg(target_os = "macos")]
    {
        let service = login_service()?;
        let status: isize = unsafe { objc2::msg_send![&*service, status] };
        Ok(status)
    }
    #[cfg(not(target_os = "macos"))]
    anyhow::bail!("launch at login requires macOS")
}

#[tauri::command]
pub fn launch_at_login(enabled: bool, window: WebviewWindow) -> Result<isize, String> {
    let _ = window;
    #[cfg(target_os = "macos")]
    {
        let service = login_service().map_err(message)?;
        let result: Result<(), objc2::rc::Retained<objc2_foundation::NSError>> = unsafe {
            if enabled {
                objc2::msg_send![&*service, registerAndReturnError: _]
            } else {
                objc2::msg_send![&*service, unregisterAndReturnError: _]
            }
        };
        result.map_err(|error| error.localizedDescription().to_string())?;
        login_status().map_err(message)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = enabled;
        Err("launch at login requires macOS".into())
    }
}
