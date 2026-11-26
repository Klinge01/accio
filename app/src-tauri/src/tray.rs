use crate::{diagnostics, panel, settings, state::AppState};
use accio_core::{
    ingest::IngestEvent,
    store::{PdfMode, Scope, Source, SourceKind, SourceStatus},
};
use anyhow::{Context, Result};
use std::{
    path::Path,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tauri::{
    AppHandle, Emitter, Manager,
    image::Image,
    menu::{CheckMenuItemBuilder, MenuBuilder, MenuItem, MenuItemBuilder, SubmenuBuilder},
    tray::TrayIconBuilder,
};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_opener::OpenerExt;

const TRAY_ID: &str = "accio";
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

fn relative_time(then: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    match now.saturating_sub(then).max(0) {
        0..=59 => "just now".into(),
        elapsed @ 60..=3599 => format!("{}m ago", elapsed / 60),
        elapsed @ 3600..=86399 => format!("{}h ago", elapsed / 3600),
        elapsed => format!("{}d ago", elapsed / 86400),
    }
}

fn status_line(source: &Source, progress: Option<(usize, usize)>) -> String {
    match &source.status {
        SourceStatus::Queued => "◌ queued".into(),
        SourceStatus::Running => progress.map_or_else(
            || "⟳ scanning…".into(),
            |(done, total)| format!("⟳ {done}/{total}"),
        ),
        SourceStatus::Failed(error) => format!("✗ {error}"),
        SourceStatus::Cancelled => "– cancelled".into(),
        SourceStatus::Idle => source.last_ingested_at.map_or_else(
            || "– never ingested".into(),
            |then| format!("✓ {} · {} files", relative_time(then), source.file_count),
        ),
    }
}

pub fn install(app: &AppHandle) -> Result<()> {
    // Template image (alpha only): speech bubble with a lightning bolt cut out.
    // Source: icons/src/tray-icon.svg, rendered at 36x36 for @2x menu bars.
    let icon = Image::from_bytes(include_bytes!("../icons/tray.png"))?;
    let icon = TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon)
        .icon_as_template(true)
        .tooltip("Accio")
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| {
            if let Err(error) = action(app, event.id().as_ref()) {
                show_error(app, error);
            }
        })
        .build(app)?;
    #[cfg(target_os = "macos")]
    crate::drop_target::install(app, &icon)?;
    rebuild(app)
}

pub fn rebuild(app: &AppHandle) -> Result<()> {
    let state = app.state::<AppState>();
    let store = state.engine.store();
    let progress = state.tray.lock().unwrap().progress.clone();
    let mut menu = MenuBuilder::new(app)
        .text(
            "ask",
            format!("Ask…  {}", state.preferences.lock().unwrap().shortcut),
        )
        .separator();
    let mut sources = SubmenuBuilder::new(app, "Sources");
    let mut files = SubmenuBuilder::new(app, "Files");
    let mut code = SubmenuBuilder::new(app, "Code");
    let mut has_code = false;
    let all = store.list_sources()?;
    let (repos, list): (Vec<_>, Vec<_>) = all
        .into_iter()
        .partition(|source| source.kind == SourceKind::Code);
    let scope = state.engine.scope();
    for source in repos {
        has_code = true;
        let prefix = format!("src:{}", source.id);
        let name = accio_core::code::repo_name(&source);
        let asking = scope == Scope::Source(source.id);
        let active = state.ingest.is_active(source.id);
        let mut submenu = SubmenuBuilder::new(app, if asking { format!("{name} (asking)") } else { name })
            .item(&MenuItem::with_id(
                app,
                format!("{prefix}:status"),
                status_line(&source, progress.get(&source.id).copied()),
                false,
                None::<&str>,
            )?)
            .item(&MenuItem::with_id(
                app,
                format!("{prefix}:path"),
                source.path.display().to_string(),
                false,
                None::<&str>,
            )?)
            .separator();
        submenu = if asking {
            submenu.text(format!("{prefix}:notes"), "Back to notes")
        } else {
            submenu.text(format!("{prefix}:ask"), "Ask about this repository")
        };
        submenu = if active {
            submenu.text(format!("{prefix}:cancel"), "Cancel indexing")
        } else {
            submenu
                .text(format!("{prefix}:reingest"), "Re-index changes")
                .text(format!("{prefix}:force"), "Rebuild index")
        };
        if source.failed_count > 0 {
            let mut failed =
                SubmenuBuilder::new(app, format!("Failed files ({})", source.failed_count));
            for file in store.failed_files(source.id, 20)? {
                let name = file
                    .path
                    .file_name()
                    .unwrap_or(file.path.as_os_str())
                    .to_string_lossy();
                let error = file.error.as_deref().unwrap_or_default();
                failed = failed.text(
                    format!("{prefix}:failed:{}", file.id),
                    format!("{name} — {error}"),
                );
            }
            submenu = submenu.item(&failed.build()?);
        }
        submenu = submenu
            .separator()
            .text(format!("{prefix}:remove"), "Remove");
        code = code.item(&submenu.build()?);
    }
    if list.iter().all(|source| source.is_file) {
        sources = sources.item(&MenuItem::with_id(
            app,
            "empty",
            "No folders added",
            false,
            None::<&str>,
        )?);
    }
    let mut has_files = false;
    for source in list {
        let prefix = format!("src:{}", source.id);
        let title = source.path.file_name().map_or_else(
            || source.path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        let status = MenuItem::with_id(
            app,
            format!("{prefix}:status"),
            status_line(&source, progress.get(&source.id).copied()),
            false,
            None::<&str>,
        )?;
        let location = MenuItem::with_id(
            app,
            format!("{prefix}:path"),
            source.path.display().to_string(),
            false,
            None::<&str>,
        )?;
        let title = if source.enabled {
            title
        } else {
            format!("{title} (off)")
        };
        let mut submenu = SubmenuBuilder::new(app, title)
            .items(&[&status, &location])
            .item(
                &CheckMenuItemBuilder::with_id(format!("{prefix}:enabled"), "Use in answers")
                    .checked(source.enabled)
                    .build(app)?,
            )
            .separator();
        let active = state.ingest.is_active(source.id);
        if active {
            submenu = submenu.text(format!("{prefix}:cancel"), "Cancel");
        } else {
            submenu = submenu
                .text(format!("{prefix}:reingest"), "Re-ingest")
                .text(format!("{prefix}:force"), "Force rebuild");
        }
        let pdf_label = |mode: PdfMode, label: &str| {
            let mark = if source.pdf_mode == mode {
                "✓ "
            } else {
                "    "
            };
            MenuItem::with_id(
                app,
                format!("{prefix}:pdf:{}", mode.as_str()),
                format!("{mark}{label}"),
                !active,
                None::<&str>,
            )
        };
        let pdf_menu = SubmenuBuilder::new(app, "PDF extraction")
            .item(&pdf_label(PdfMode::Text, "Plain text (fast)")?)
            .item(&pdf_label(
                PdfMode::Markdown,
                "Markdown (keeps tables, ~40× slower)",
            )?)
            .build()?;
        if !source.is_file
            || source
                .path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
        {
            submenu = submenu.item(&pdf_menu);
        }
        if source.failed_count > 0 {
            let mut failed =
                SubmenuBuilder::new(app, format!("Failed files ({})", source.failed_count));
            for file in store.failed_files(source.id, 20)? {
                let name = file
                    .path
                    .file_name()
                    .unwrap_or(file.path.as_os_str())
                    .to_string_lossy();
                let error = file.error.as_deref().unwrap_or_default();
                failed = failed.text(
                    format!("{prefix}:failed:{}", file.id),
                    format!("{name} — {error}"),
                );
            }
            submenu = submenu.item(&failed.build()?);
        }
        submenu = submenu
            .separator()
            .text(format!("{prefix}:remove"), "Remove");
        if source.is_file {
            has_files = true;
            files = files.item(&submenu.build()?);
        } else {
            sources = sources.item(&submenu.build()?);
        }
    }
    menu = menu.item(&sources.build()?);
    if has_files {
        menu = menu.item(&files.build()?);
    }
    if has_code {
        menu = menu.item(&code.build()?);
    }
    let profiles = store.profiles()?;
    if !profiles.is_empty() {
        let mut submenu = SubmenuBuilder::new(app, "Profiles");
        for profile in profiles {
            submenu = submenu.text(format!("profile:{}", profile.id), profile.name);
        }
        menu = menu.item(&submenu.build()?);
    }
    menu = menu
        .text("add_files", "Add files…")
        .text("add", "Add folder…")
        .text("add_code", "Add code repository…")
        .text("reingest_all", "Re-ingest all")
        .separator()
        .item(
            &MenuItemBuilder::with_id("settings", "Settings…")
                .accelerator("CmdOrCtrl+,")
                .build(app)?,
        )
        .text("quit", "Quit");
    let icon = app.tray_by_id(TRAY_ID).context("missing tray icon")?;
    icon.set_menu(Some(menu.build()?))?;
    let title = progress
        .values()
        .next()
        .map(|(done, total)| format!("⟳ {done}/{total}"));
    icon.set_title(title)?;
    Ok(())
}

pub fn refresh(app: &AppHandle) {
    let _ = app.emit("sources://changed", ());
    let callback_app = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Err(error) = rebuild(&callback_app) {
            eprintln!("updating tray: {error:#}");
        }
    });
}

pub fn on_ingest(app: &AppHandle, event: IngestEvent) {
    let _ = app.emit("ingest://event", &event);
    let state = app.state::<AppState>();
    let mut tray = state.tray.lock().unwrap();
    if let IngestEvent::Progress {
        source_id,
        done,
        total,
        ..
    } = &event
    {
        tray.progress.insert(*source_id, (*done, *total));
        if tray.progress_update_pending {
            return;
        }
        let delay = tray.last_progress_update.map_or(Duration::ZERO, |last| {
            PROGRESS_INTERVAL.saturating_sub(last.elapsed())
        });
        if !delay.is_zero() {
            tray.progress_update_pending = true;
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(delay).await;
                {
                    let state = app.state::<AppState>();
                    let mut tray = state.tray.lock().unwrap();
                    tray.progress_update_pending = false;
                    if tray.progress.is_empty() {
                        return;
                    }
                    tray.last_progress_update = Some(Instant::now());
                }
                refresh(&app);
            });
            return;
        }
        tray.last_progress_update = Some(Instant::now());
    } else {
        diagnostics::log(&state.engine.config().home, &format!("{event:?}"));
        let source_id = match &event {
            IngestEvent::Finished { source_id, .. }
            | IngestEvent::Failed { source_id, .. }
            | IngestEvent::Cancelled { source_id } => *source_id,
            IngestEvent::Progress { .. } => unreachable!(),
        };
        tray.progress.remove(&source_id);
    }
    drop(tray);
    refresh(app);
}

pub fn show_error(app: &AppHandle, error: impl std::fmt::Display) {
    let error = error.to_string();
    diagnostics::log(&app.state::<AppState>().engine.config().home, &error);
    app.dialog()
        .message(error)
        .title("Accio")
        .kind(MessageDialogKind::Error)
        .show(|_| {});
}

/// After "Add folder…": choose how this folder's PDFs are extracted, then ingest.
fn ask_pdf_mode_then_ingest(app: &AppHandle, source_id: i64) {
    let app = app.clone();
    app.clone()
        .dialog()
        .message(
            "How should PDFs in this folder be extracted?\n\n\
             Plain text: fast (~7 ms/page). Tables become flat text.\n\
             Markdown: keeps tables and headings, ~40× slower (~125 ms/page).\n\n\
             You can change this later under Sources.",
        )
        .title("PDF extraction")
        .buttons(MessageDialogButtons::OkCancelCustom(
            "Plain text".into(),
            "Markdown".into(),
        ))
        .show(move |plain| {
            let state = app.state::<AppState>();
            let mode = if plain {
                PdfMode::Text
            } else {
                PdfMode::Markdown
            };
            if let Err(error) = state.engine.store().set_pdf_mode(source_id, mode) {
                show_error(&app, error);
            }
            state.ingest.start(source_id, false);
            refresh(&app);
        });
}

fn action(app: &AppHandle, id: &str) -> Result<()> {
    let state = app.state::<AppState>();
    if let Some(id) = id.strip_prefix("profile:") {
        return settings::apply_profile(app, id.parse()?);
    }
    match id {
        "ask" => panel::toggle(app)?,
        "quit" => {
            let app = app.clone();
            let ingest = state.ingest.clone();
            std::thread::spawn(move || {
                ingest.shutdown();
                app.exit(0);
            });
        }
        "settings" => settings::show(app)?,
        "add" => pick_source(app, false),
        "add_files" => pick_source(app, true),
        "add_code" => pick_code_repository(app),
        "reingest_all" => {
            for source in state.engine.store().list_sources()? {
                state.ingest.start(source.id, false);
            }
            rebuild(app)?;
        }
        _ => {
            let parts: Vec<_> = id.split(':').collect();
            let ["src", source, action, rest @ ..] = parts.as_slice() else {
                return Ok(());
            };
            let source_id = source.parse::<i64>()?;
            match (*action, rest) {
                ("reingest", []) | ("force", []) => {
                    state.ingest.start(source_id, *action == "force");
                    rebuild(app)?;
                }
                ("cancel", []) => state.ingest.cancel(source_id),
                ("ask", []) | ("notes", []) => {
                    let app = app.clone();
                    let id = (*action == "ask").then_some(source_id);
                    tauri::async_runtime::spawn(async move {
                        match crate::commands::use_code_scope(app.clone(), id).await {
                            Ok(()) => {
                                let visible = app
                                    .get_webview_window("panel")
                                    .and_then(|panel| panel.is_visible().ok())
                                    .unwrap_or(false);
                                if id.is_some() && !visible {
                                    let handle = app.clone();
                                    let _ = app.run_on_main_thread(move || {
                                        let _ = panel::toggle(&handle);
                                    });
                                }
                            }
                            Err(error) => show_error(&app, error),
                        }
                    });
                }
                ("enabled", []) => settings::set_enabled(
                    app,
                    source_id,
                    !state.engine.store().get_source(source_id)?.enabled,
                )?,

                ("pdf", [mode]) => {
                    let mode = if *mode == "markdown" {
                        PdfMode::Markdown
                    } else {
                        PdfMode::Text
                    };
                    if !state.ingest.is_active(source_id)
                        && state.engine.store().set_pdf_mode(source_id, mode)?
                    {
                        // Only this source's PDFs were dropped; re-ingest parses just those.
                        state.ingest.start(source_id, false);
                    }
                    rebuild(app)?;
                }
                ("failed", [file_id]) => {
                    let file_id = file_id.parse::<i64>()?;
                    if let Some(file) = state
                        .engine
                        .store()
                        .failed_files(source_id, 20)?
                        .into_iter()
                        .find(|f| f.id == file_id)
                    {
                        app.opener().reveal_item_in_dir(&file.path)?;
                    }
                }
                ("remove", []) => {
                    let source = state.engine.store().get_source(source_id)?;
                    let app = app.clone();
                    app.clone()
                        .dialog()
                        .message(format!(
                            "Remove \"{}\" and its index?",
                            Path::new(&source.path).display()
                        ))
                        .title("Remove source")
                        .buttons(MessageDialogButtons::OkCancel)
                        .show(move |ok| {
                            if ok {
                                let app = app.clone();
                                std::thread::spawn(move || {
                                    let state = app.state::<AppState>();
                                    match state.ingest.remove_source(source_id) {
                                        Ok(()) => {
                                            let cancelled = state.cancel(None);
                                            let _ = app.emit("scope://changed", cancelled);
                                            refresh(&app);
                                        }
                                        Err(error) => show_error(&app, error),
                                    }
                                });
                            }
                        });
                }
                _ => {}
            }
        }
    }
    Ok(())
}

pub fn add_path(app: &AppHandle, path: &Path, prompt_pdf: bool) -> Result<()> {
    let path = path.canonicalize()?;
    accio_core::scan::validate_source(&path)?;
    let state = app.state::<AppState>();
    let store = state.engine.store();
    let existing = store
        .list_sources()?
        .into_iter()
        .find(|source| source.path == path);
    if path.is_file()
        && store.owned_by_other(existing.as_ref().map_or(0, |source| source.id), &path)?
    {
        anyhow::bail!("{} is already indexed by a folder", path.display());
    }
    let id = store.add_source(&path, &[])?;
    if prompt_pdf && existing.is_none() {
        ask_pdf_mode_then_ingest(app, id);
    } else {
        state.ingest.start(id, false);
        refresh(app);
    }
    Ok(())
}

/// "Add code repository…": the chosen folder's git repository root becomes a code
/// source, which agents (accio-mcp) and `@name` panel questions can search.
pub fn pick_code_repository(app: &AppHandle) {
    let app = app.clone();
    app.clone().dialog().file().pick_folder(move |folder| {
        let Some(folder) = folder else { return };
        let result = folder
            .into_path()
            .map_err(anyhow::Error::from)
            .and_then(|path| add_code_repository(&app, &path));
        if let Err(error) = result {
            show_error(&app, error);
        }
    });
}

pub fn add_code_repository(app: &AppHandle, path: &Path) -> Result<i64> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("running git")?;
    anyhow::ensure!(
        output.status.success(),
        "{} is not inside a git repository",
        path.display()
    );
    let root = std::path::PathBuf::from(String::from_utf8(output.stdout)?.trim()).canonicalize()?;
    let state = app.state::<AppState>();
    let id = state
        .engine
        .store()
        .add_code_source(&root, &accio_core::scan::default_code_ignores())?;
    state.ingest.start(id, false);
    refresh(app);
    Ok(id)
}

pub fn pick_source(app: &AppHandle, files: bool) {
    let picker = app.dialog().file();
    let app = app.clone();
    if files {
        picker
            .add_filter("Supported files", accio_core::scan::SUPPORTED_EXTS)
            .pick_files(move |paths| {
                let paths = paths
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|path| path.into_path().ok())
                    .collect();
                crate::drop_target::drop_paths(&app, paths, crate::drop_target::Origin::Picker);
            });
    } else {
        picker.pick_folder(move |folder| {
            if let Some(folder) = folder {
                let result = folder
                    .into_path()
                    .map_err(anyhow::Error::from)
                    .and_then(|path| add_path(&app, &path, true));
                if let Err(error) = result {
                    show_error(&app, error);
                }
            }
        });
    }
}
