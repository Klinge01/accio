mod commands;
mod diagnostics;
mod drop_target;
mod panel;
mod settings;
mod state;
mod tray;

use accio_core::{
    code::CodeService, config::Config, engine::Engine, ingest::IngestManager,
    models::SidecarEmbedder,
};
use state::AppState;
use std::{
    sync::{Arc, Mutex},
    time::Instant,
};
use tauri::{ActivationPolicy, Manager, WindowEvent};

/// Apps launched from Finder/launchd get PATH=/usr/bin:/bin:/usr/sbin:/sbin, so `uv`
/// (ingestion worker) and `soffice` (.ppt conversion) would not resolve. Append the
/// usual user tool dirs. Must run before any thread is spawned.
fn extend_path() {
    let current = std::env::var_os("PATH").unwrap_or_default();
    let mut dirs: Vec<_> = std::env::split_paths(&current).collect();
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let extra = [
        Some("/opt/homebrew/bin".into()),
        Some("/usr/local/bin".into()),
        home.as_ref().map(|h| h.join(".local/bin")),
        home.as_ref().map(|h| h.join(".cargo/bin")),
        Some("/Applications/LibreOffice.app/Contents/MacOS".into()),
    ];
    for dir in extra.into_iter().flatten() {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    if let Ok(path) = std::env::join_paths(dirs) {
        // SAFETY: called first thing in main, before any other thread exists.
        unsafe { std::env::set_var("PATH", path) };
    }
    // A packaged app's helper project sits in the read-only bundle; uv keeps its
    // environment in Accio's data folder instead.
    if std::env::var_os("UV_PROJECT_ENVIRONMENT").is_none()
        && let Some(env) = accio_core::config::bundled_python_env()
    {
        // SAFETY: as above, no other thread exists yet.
        unsafe { std::env::set_var("UV_PROJECT_ENVIRONMENT", env) };
    }
}

fn main() {
    extend_path();
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--accio-hash-worker")
    {
        if let Err(error) = accio_core::worker::serve_hash_worker() {
            eprintln!("hash worker: {error:#}");
            std::process::exit(1);
        }
        return;
    }
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            commands::ask,
            commands::prefetch,
            commands::cancel_answer,
            commands::open_citation,
            commands::use_code_scope,
            commands::hide_panel,
            settings::open_settings,
            settings::panel_options,
            settings::settings_snapshot,
            settings::change_preference,
            settings::provider_models,
            settings::test_api_key,
            settings::change_source,
            settings::source_folders,
            settings::failed_files,
            settings::pick_source,
            settings::pick_code_repository,
            settings::save_profile,
            settings::delete_profile,
            settings::use_profile,
            settings::last_prompt,
            settings::reveal_index,
            settings::reveal_logs,
            settings::embedding_status,
            settings::clear_index,
            settings::launch_at_login,
        ])
        .setup(|app| {
            app.set_activation_policy(ActivationPolicy::Accessory);
            let cfg = Config::load()?;
            diagnostics::log(&cfg.home, "Accio started");
            let embedder = SidecarEmbedder::start(cfg.embed_cmd.clone())?;
            let engine = Engine::new(cfg, Arc::new(embedder))?;
            let preferences = settings::Preferences::load(engine.store(), engine.config())?;
            let provider = preferences
                .answer_provider(engine.config())
                .unwrap_or_else(|error| {
                    diagnostics::log(
                        &engine.config().home,
                        &format!("Cannot load provider key: {error:#}"),
                    );
                    preferences.provider_without_key()
                });
            if let Some(id) = preferences.default_profile {
                engine.store().apply_profile(id)?;
                engine.reload_index()?;
            }
            engine.set_search_options(preferences.min_similarity, preferences.excerpts);
            let shortcut = preferences.shortcut.parse()?;
            if let Some(window) = app.get_webview_window("panel") {
                window.set_size(tauri::LogicalSize::new(
                    preferences.panel_width,
                    preferences.panel_height,
                ))?;
            }
            let handle = app.handle().clone();
            let ingest = IngestManager::with_hash_worker(
                engine.clone(),
                Arc::new(move |event| tray::on_ingest(&handle, event)),
                Some(std::env::current_exe()?),
            );
            let code = CodeService::new(engine.clone(), ingest.clone());
            let socket = engine.config().socket_path();
            if let Err(error) = accio_core::socket::serve(&socket, code.clone()) {
                diagnostics::log(
                    &engine.config().home,
                    &format!("Code search for agents unavailable: {error:#}"),
                );
            }
            app.manage(AppState {
                engine,
                ingest,
                code,
                provider: Mutex::new(provider),
                preferences: Mutex::new(preferences),
                last_prompt: Mutex::new(Vec::new()),
                scope_change: Mutex::new(()),
                answer_cancel: Mutex::new(None),
                hidden_at: Mutex::new(Some(Instant::now())),
                tray: Mutex::new(Default::default()),
            });
            tray::install(app.handle())?;
            if let Err(error) = settings::register_shortcut(app.handle(), shortcut) {
                tray::show_error(
                    app.handle(),
                    format!("Registering the panel shortcut: {error}"),
                );
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() == "settings"
                && let WindowEvent::CloseRequested { api, .. } = event
            {
                api.prevent_close();
                let _ = window.hide();
            }
            if window.label() == "drop-hud"
                && let WindowEvent::DragDrop(event) = event
            {
                match event {
                    tauri::DragDropEvent::Enter { .. } => {
                        drop_target::mark_hud(window.app_handle());
                    }
                    tauri::DragDropEvent::Leave => {
                        drop_target::hide_hud_after(window.app_handle(), std::time::Duration::ZERO)
                    }
                    tauri::DragDropEvent::Drop { paths, .. } => {
                        drop_target::hide_hud_after(window.app_handle(), std::time::Duration::ZERO);
                        drop_target::drop_paths(
                            window.app_handle(),
                            paths.clone(),
                            drop_target::Origin::Tray,
                        );
                    }
                    _ => {}
                }
            }
            if window.label() == "panel"
                && let Some(panel) = window.get_webview_window("panel")
            {
                match event {
                    WindowEvent::Focused(false) => {
                        let _ = panel::hide(&panel);
                    }
                    WindowEvent::DragDrop(event) => {
                        use tauri::{DragDropEvent, Emitter};
                        match event {
                            DragDropEvent::Enter { .. } => {
                                let _ = panel.emit("drop://hover", true);
                            }
                            DragDropEvent::Leave => {
                                let _ = panel.emit("drop://hover", false);
                            }
                            DragDropEvent::Drop { paths, .. } => {
                                let _ = panel.emit("drop://hover", false);
                                drop_target::drop_paths(
                                    window.app_handle(),
                                    paths.clone(),
                                    drop_target::Origin::Panel,
                                );
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("starting accio");
}
