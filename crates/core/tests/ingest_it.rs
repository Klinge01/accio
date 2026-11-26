#![cfg(feature = "testutil")]

use anyhow::{Context, Result};
use accio_core::{
    config::Config,
    engine::Engine,
    ingest::{IngestEvent, IngestManager},
    lang::Lang,
    models::EmbedModel,
    code::{CodeService, SearchOptions},
    store::{PdfMode, Scope},
    testutil::FakeEmbedder,
};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

struct Fixture {
    _home: tempfile::TempDir,
    root: PathBuf,
    engine: Arc<Engine>,
    ingest: Arc<IngestManager>,
    events: mpsc::Receiver<IngestEvent>,
}

impl Fixture {
    fn new() -> Result<Self> {
        Self::with_embedder(Arc::new(FakeEmbedder::new(512)))
    }

    fn with_embedder(embedder: Arc<dyn EmbedModel>) -> Result<Self> {
        let home = tempfile::tempdir()?;
        let root = home.path().join("notes");
        fs::create_dir(&root)?;
        let config = Config {
            home: home.path().join("state"),
            groq_api_key: None,
            groq_model: "test".into(),
            min_similarity: 0.2,
            max_context_chunks: 6,
            file_timeout: Duration::from_secs(2),
            ingest_cmd: vec![
                "python3".into(),
                concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake_worker.py").into(),
            ],
            embed_cmd: Vec::new(),
            translate: true,
            code_editor: "zed".into(),
        };
        let engine = Engine::new(config, embedder)?;
        let (sender, events) = mpsc::channel();
        let ingest = IngestManager::new(
            engine.clone(),
            Arc::new(move |event| {
                let _ = sender.send(event);
            }),
        );
        Ok(Self {
            _home: home,
            root,
            engine,
            ingest,
            events,
        })
    }

    fn add(&self, root: &Path) -> Result<i64> {
        self.engine.store().add_source(root, &[])
    }

    fn next(&self) -> Result<IngestEvent> {
        self.events
            .recv_timeout(Duration::from_secs(15))
            .context("waiting for ingestion")
    }

    fn finished(&self) -> Result<IngestEvent> {
        loop {
            let event = self.next()?;
            if !matches!(event, IngestEvent::Progress { .. }) {
                return Ok(event);
            }
        }
    }

    fn seed(&self) -> Result<i64> {
        fs::write(self.root.join("miete.md"), "Kaltmiete 950 Euro Nebenkosten")?;
        fs::write(self.root.join("kuchen.md"), "Mehl Zucker Ei")?;
        let id = self.add(&self.root)?;
        self.ingest.start(id, false);
        assert!(matches!(
            self.finished()?,
            IngestEvent::Finished {
                parsed: 2,
                failed: 0,
                ..
            }
        ));
        Ok(id)
    }
}

#[test]
fn ingest_then_retrieve() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.seed()?;
    let retrieved = fixture.engine.retrieve("Nebenkosten Euro")?;
    assert!(retrieved.hits[0].path.ends_with("miete.md"));
    assert!(
        fixture
            .engine
            .retrieve("quantum chromodynamics gluon")?
            .hits
            .is_empty()
    );
    Ok(())
}

#[test]
fn reingest_incremental_and_stale_cache() -> Result<()> {
    let fixture = Fixture::new()?;
    let id = fixture.seed()?;
    fixture.ingest.start(id, false);
    assert!(matches!(
        fixture.finished()?,
        IngestEvent::Finished {
            parsed: 0,
            deleted: 0,
            ..
        }
    ));

    let query = "Zucker";
    fixture.engine.prefetch(query.into());
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let a = fixture.engine.retrieve(query)?;
        let b = fixture.engine.retrieve(query)?;
        if Arc::ptr_eq(&a, &b) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "prefetch never populated the cache"
        );
        thread::sleep(Duration::from_millis(5));
    }
    fs::write(fixture.root.join("kuchen.md"), "Mehl, Zucker, Eier, Butter")?;
    assert!(fixture.engine.retrieve(query)?.hits[0].stale);
    fs::remove_file(fixture.root.join("miete.md"))?;
    fixture.ingest.start(id, false);
    assert!(matches!(
        fixture.finished()?,
        IngestEvent::Finished {
            parsed: 1,
            deleted: 1,
            ..
        }
    ));
    let hit = fixture.engine.retrieve(query)?;
    assert!(!hit.hits[0].stale);
    assert!(hit.hits[0].text.contains("Butter"));
    Ok(())
}

#[test]
fn missing_root_deletes_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    let id = fixture.seed()?;
    fs::rename(&fixture.root, fixture.root.with_extension("offline"))?;
    fixture.ingest.start(id, false);
    assert!(
        matches!(fixture.finished()?, IngestEvent::Failed { error, .. }
        if error.starts_with("folder not found: "))
    );
    assert_eq!(fixture.engine.store().all_vectors()?.0.len(), 2);
    Ok(())
}

#[test]
fn timeout_and_crash_continue_and_duplicate_start_is_noop() -> Result<()> {
    let fixture = Fixture::new()?;
    let id = fixture.add(&fixture.root)?;
    for name in ["slow", "crash", "bad", "z-ok"] {
        fs::write(fixture.root.join(format!("{name}.md")), "test")?;
    }
    fixture.ingest.start(id, false);
    fixture.ingest.start(id, false);
    assert!(matches!(
        fixture.finished()?,
        IngestEvent::Finished {
            parsed: 1,
            failed: 3,
            ..
        }
    ));
    assert!(
        fixture
            .events
            .recv_timeout(Duration::from_millis(100))
            .is_err()
    );
    let files = fixture.engine.store().failed_files(id, 20)?;
    let errors: Vec<_> = files
        .iter()
        .map(|file| file.error.as_deref().unwrap())
        .collect();
    assert_eq!(
        errors,
        ["ValueError: bad file", "worker crashed", "timeout after 2s"]
    );
    Ok(())
}

#[test]
fn query_during_ingest_uses_old_dense_and_fts_snapshot() -> Result<()> {
    let fixture = Fixture::new()?;
    let id = fixture.seed()?;
    fs::write(fixture.root.join("kuchen.md"), "Schokoglasur")?;
    fs::write(fixture.root.join("slow.md"), "This parse blocks")?;
    fixture.ingest.start(id, false);
    while !matches!(fixture.next()?, IngestEvent::Progress { done: 1, .. }) {}
    assert_eq!(
        fixture
            .engine
            .store()
            .fts_search("\"Schokoglasur\"", 10)?
            .len(),
        1
    );
    let started = Instant::now();
    let old = fixture.engine.retrieve("Zucker")?;
    assert!(old.hits[0].text.contains("Zucker"));
    assert!(started.elapsed() < Duration::from_millis(100));
    assert!(fixture.engine.retrieve("Schokoglasur")?.hits.is_empty());
    fixture.ingest.cancel(id);
    assert!(matches!(fixture.finished()?, IngestEvent::Cancelled { .. }));
    assert!(
        fixture.engine.retrieve("Schokoglasur")?.hits[0]
            .text
            .contains("Schokoglasur")
    );
    Ok(())
}

#[test]
fn overlapping_sources_and_fifo() -> Result<()> {
    let fixture = Fixture::new()?;
    let subdir = fixture.root.join("sub");
    fs::create_dir(&subdir)?;
    fs::write(subdir.join("note.md"), "Nebenkosten")?;
    let outer = fixture.add(&fixture.root)?;
    let inner = fixture.add(&subdir)?;
    fixture.ingest.start(outer, false);
    fixture.ingest.start(inner, false);
    assert!(
        matches!(fixture.finished()?, IngestEvent::Finished {source_id, parsed: 1, ..} if source_id == outer)
    );
    assert!(
        matches!(fixture.finished()?, IngestEvent::Finished {source_id, parsed: 0, ..} if source_id == inner)
    );
    assert_eq!(fixture.engine.store().all_vectors()?.0.len(), 1);
    Ok(())
}

#[test]
fn changed_backing_volume_keeps_the_index() -> Result<()> {
    let fixture = Fixture::new()?;
    let id = fixture.seed()?;
    // Simulate an earlier ingest on a volume that is currently unmounted.
    let conn = rusqlite::Connection::open(fixture.engine.config().db_path())?;
    conn.execute(
        "UPDATE source_volumes SET identity='unmounted' WHERE source_id=?",
        [id],
    )?;
    fs::remove_file(fixture.root.join("miete.md"))?;
    fs::remove_file(fixture.root.join("kuchen.md"))?;
    fixture.ingest.start(id, false);
    assert!(
        matches!(fixture.finished()?, IngestEvent::Failed { error, .. }
        if error.starts_with("folder not found: "))
    );
    assert_eq!(fixture.engine.store().all_vectors()?.0.len(), 2);
    Ok(())
}

#[test]
fn shutdown_cancels_and_waits_for_the_active_worker() -> Result<()> {
    let fixture = Fixture::new()?;
    let id = fixture.add(&fixture.root)?;
    fs::write(fixture.root.join("slow.md"), "test")?;
    fixture.ingest.start(id, false);
    assert!(matches!(
        fixture.next()?,
        IngestEvent::Progress { done: 0, .. }
    ));
    fixture.ingest.shutdown();
    assert!(matches!(fixture.finished()?, IngestEvent::Cancelled { .. }));
    assert!(!fixture.ingest.is_active(id));
    Ok(())
}

#[cfg(unix)]
#[test]
fn hash_worker_timeout_does_not_block_other_files() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new()?;
    let script = fixture._home.path().join("hash-worker");
    fs::write(
        &script,
        r#"#!/usr/bin/env python3
import json, sys, time
for line in sys.stdin:
    request = json.loads(line)
    if request["path"].endswith("slow-hash.md"):
        time.sleep(10)
    print(json.dumps({"id": request["id"], "chunks": [], "error": None, "hash": "test"}), flush=True)
"#,
    )?;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700))?;
    fs::write(fixture.root.join("slow-hash.md"), "placeholder")?;
    fs::write(fixture.root.join("z-ok.md"), "actual content")?;
    let source = fixture.add(&fixture.root)?;
    let (send, receive) = mpsc::channel();
    let ingest = IngestManager::with_hash_worker(
        fixture.engine.clone(),
        Arc::new(move |event| {
            let _ = send.send(event);
        }),
        Some(script),
    );
    ingest.start(source, false);
    loop {
        match receive.recv_timeout(Duration::from_secs(15))? {
            IngestEvent::Progress { .. } => {}
            IngestEvent::Finished {
                parsed: 1,
                failed: 1,
                ..
            } => break,
            event => anyhow::bail!("unexpected ingestion result: {event:?}"),
        }
    }
    let failures = fixture.engine.store().failed_files(source, 20)?;
    assert_eq!(
        failures[0].error.as_deref(),
        Some("cannot hash file: timeout after 2s")
    );
    assert!(
        fixture.engine.retrieve("actual content")?.hits[0]
            .path
            .ends_with("z-ok.md")
    );
    Ok(())
}

#[test]
fn remove_cancels_active_ingestion_and_removes_published_chunks() -> Result<()> {
    let fixture = Fixture::new()?;
    let id = fixture.seed()?;
    fs::write(fixture.root.join("slow.md"), "running")?;
    fixture.ingest.start(id, false);
    assert!(matches!(
        fixture.next()?,
        IngestEvent::Progress { done: 0, .. }
    ));
    fixture.ingest.remove_source(id)?;
    assert!(matches!(fixture.finished()?, IngestEvent::Cancelled { .. }));
    assert!(fixture.engine.store().list_sources()?.is_empty());
    assert!(fixture.engine.retrieve("Nebenkosten")?.hits.is_empty());
    Ok(())
}

#[test]
fn pdf_mode_is_sent_and_switch_reparses_only_pdfs() -> Result<()> {
    let fixture = Fixture::new()?;
    fs::write(fixture.root.join("report.pdf"), "Umsatz Tabelle")?;
    fs::write(fixture.root.join("note.md"), "Notiz")?;
    let id = fixture.add(&fixture.root)?;
    fixture.ingest.start(id, false);
    assert!(matches!(fixture.finished()?, IngestEvent::Finished { parsed: 2, .. }));
    let store = fixture.engine.store();
    assert_eq!(store.fts_search("\"pdfmodetext\"", 10)?.len(), 1);

    assert!(store.set_pdf_mode(id, PdfMode::Markdown)?);
    fixture.ingest.start(id, false);
    assert!(matches!(fixture.finished()?, IngestEvent::Finished { parsed: 1, deleted: 0, .. }));
    assert!(store.fts_search("\"pdfmodetext\"", 10)?.is_empty());
    assert_eq!(store.fts_search("\"pdfmodemarkdown\"", 10)?.len(), 1);
    assert_eq!(store.fts_search("\"Notiz\"", 10)?.len(), 1);
    Ok(())
}

#[test]
fn file_source_survives_move_and_disabled_ingestion_stays_out_of_retrieval() -> Result<()> {
    use accio_core::engine::Mode;

    let fixture = Fixture::new()?;
    let file = fixture.root.join("pricing.MD");
    fs::write(&file, "Kaltmiete 950 Euro")?;
    let id = fixture.add(&file)?;
    assert!(fixture.engine.store().get_source(id)?.is_file);
    fixture.ingest.start(id, false);
    assert!(matches!(
        fixture.finished()?,
        IngestEvent::Finished { parsed: 1, .. }
    ));
    let hits = fixture
        .engine
        .retrieve_with("Kaltmiete", Mode::Fts, false)?
        .hits;
    assert_eq!(hits.len(), 1);
    let first_id = hits[0].chunk_id;

    fixture.engine.prefetch("Kaltmiete".into());
    fixture.engine.store().set_source_enabled(id, false)?;
    fixture.engine.reload_index()?;
    assert!(fixture.engine.retrieve("Kaltmiete")?.hits.is_empty());
    assert!(fixture.engine.hits_by_ids(&[first_id])?.is_empty());
    for mode in [Mode::Dense, Mode::Fts] {
        assert!(
            fixture
                .engine
                .retrieve_with("Kaltmiete", mode, false)?
                .hits
                .is_empty()
        );
    }
    fs::write(&file, "Kaltmiete updated to 980 Euro")?;
    fixture.ingest.start(id, false);
    assert!(matches!(
        fixture.finished()?,
        IngestEvent::Finished { parsed: 1, .. }
    ));
    assert!(fixture.engine.retrieve("Kaltmiete")?.hits.is_empty());

    fs::rename(&file, file.with_extension("offline"))?;
    fixture.ingest.start(id, false);
    assert!(matches!(fixture.finished()?, IngestEvent::Failed { .. }));
    assert_eq!(fixture.engine.store().files_for_source(id)?.len(), 1);

    fixture.engine.store().save_profile(None, "Housing", &[id])?;
    let profile = &fixture.engine.store().profiles()?[0];
    fixture.engine.store().apply_profile(profile.id)?;
    fixture.engine.reload_index()?;
    let hits = fixture
        .engine
        .retrieve_with("Kaltmiete", Mode::Fts, false)?
        .hits;
    assert_eq!(hits.len(), 1);
    assert!(hits[0].text.contains("980 Euro"));
    Ok(())
}

impl Fixture {
    fn ingest_files(&self, files: &[(&str, &str)]) -> Result<()> {
        for (name, text) in files {
            let path = self.root.join(name);
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(path, text)?;
        }
        let id = self.add(&self.root)?;
        self.ingest.start(id, false);
        assert!(matches!(self.finished()?, IngestEvent::Finished { failed: 0, .. }));
        Ok(())
    }
}

#[test]
fn keyword_match_passes_the_similarity_gate() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.ingest_files(&[
        ("transit.md", "Der Planeten-Transit TRAPPIST: Helligkeit Abdunklung Umlaufzeit Radius Atmosphäre"),
        ("other.md", "Bus transit stages and route planning"),
    ])?;
    // No note is semantically close enough under this floor.
    fixture.engine.set_search_options(0.99, 6);
    let hits = fixture.engine.retrieve("what is a planet transit?")?.hits.clone();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].path.ends_with("transit.md"));
    assert!(fixture.engine.retrieve("how do I bake bread?")?.hits.is_empty());
    assert!(fixture.engine.retrieve("what is it?")?.hits.is_empty());
    Ok(())
}

#[test]
fn copies_of_a_file_count_once() -> Result<()> {
    let fixture = Fixture::new()?;
    let text = "Tidal locking: rotation and orbit period";
    fixture.ingest_files(&[("a/tidal.md", text), ("b/tidal.md", text), ("c/tidal.md", text)])?;
    let hits = fixture.engine.retrieve("tidal locking rotation orbit")?.hits.clone();
    assert_eq!(hits.iter().filter(|hit| hit.text.contains("rotation and orbit")).count(), 1);
    Ok(())
}

/// Fake embedder whose translation is a fixed dictionary, to test cross-lingual retrieval.
struct Translating(FakeEmbedder);

impl EmbedModel for Translating {
    fn model_id(&self) -> &str {
        self.0.model_id()
    }
    fn dim(&self) -> usize {
        self.0.dim()
    }
    fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        self.0.embed_query(query)
    }
    fn embed_docs(&self, docs: &[(&str, &str)]) -> Result<Vec<Vec<f32>>> {
        self.0.embed_docs(docs)
    }
    fn translate(&self, text: &str, to: Lang) -> Result<Option<String>> {
        Ok(match (text, to) {
            ("Wie hoch ist die Miete?", Lang::En) => Some("how high is the rent?".into()),
            ("what is tidal locking?", Lang::De) => Some("was ist die gebundene Rotation?".into()),
            _ => None,
        })
    }
}

#[test]
fn questions_also_search_in_the_other_language() -> Result<()> {
    let fixture = Fixture::with_embedder(Arc::new(Translating(FakeEmbedder::new(512))))?;
    fixture.ingest_files(&[
        ("rent.md", "Monthly rent: 950 euros"),
        ("rotation.md", "Gebundene Rotation beim Mond"),
    ])?;
    let german = fixture.engine.retrieve("Wie hoch ist die Miete?")?;
    assert!(german.hits[0].path.ends_with("rent.md"));
    assert_eq!(german.translation.clone().as_deref(), Some("how high is the rent?"));
    let english = fixture.engine.retrieve("what is tidal locking?")?;
    assert!(english.hits[0].path.ends_with("rotation.md"));

    // Without translation neither question reaches the other language.
    let plain = Fixture::new()?;
    plain.ingest_files(&[("rent.md", "Monthly rent: 950 euros")])?;
    assert!(plain.engine.retrieve("Wie hoch ist die Miete?")?.hits.is_empty());
    Ok(())
}

#[test]
fn code_repository_stays_out_of_notes_and_answers_code_search() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.seed()?;
    let repo = fixture.root.parent().unwrap().join("repo");
    fs::create_dir_all(repo.join("src/shop"))?;
    fs::create_dir_all(repo.join("src/test"))?;
    fs::write(
        repo.join("src/shop/OrderSource.java"),
        "package shop;\npublic enum OrderSource {\n  PARTNER_API,\n  WEBSITE\n}\n",
    )?;
    fs::write(repo.join("src/test/OrderSourceTest.java"), "class OrderSourceTest {}")?;
    fs::write(repo.join("notes.pdf"), "not code")?;
    let id = fixture
        .engine
        .store()
        .add_code_source(&repo, &accio_core::scan::default_code_ignores())?;
    // A code repository cannot also be added as notes, and the other way round.
    assert!(fixture.engine.store().add_source(&repo, &[]).is_err());
    assert!(fixture.engine.store().add_code_source(&fixture.root, &[]).is_err());
    fixture.ingest.start(id, false);
    assert!(matches!(
        fixture.finished()?,
        IngestEvent::Finished { parsed: 1, failed: 0, .. }
    ));

    // Note questions never see code, even with the same words.
    let notes = fixture.engine.retrieve("OrderSource PARTNER_API")?;
    assert!(notes.hits.iter().all(|hit| hit.path.extension().unwrap() == "md"));

    let service = CodeService::new(fixture.engine.clone(), fixture.ingest.clone());
    let search = service.search(
        &repo.join("src"),
        "OrderSource PARTNER_API",
        &SearchOptions::limit(8),
    )?;
    assert_eq!(search.repo, "repo");
    assert_eq!(search.hits.len(), 1);
    let hit = &search.hits[0];
    assert_eq!(hit.path, "src/shop/OrderSource.java");
    assert_eq!((hit.line, hit.end_line), (Some(1), Some(5)));
    assert_eq!(hit.kind, "code");
    assert!(search.to_text().contains("1. src/shop/OrderSource.java:1-5"));
    assert!(
        service
            .search(Path::new("/nowhere"), "x", &SearchOptions::limit(8))
            .is_err()
    );
    // Filters narrow the results; tests are not in the index.
    let elsewhere = SearchOptions {
        path: Some("docs".into()),
        ..SearchOptions::limit(8)
    };
    assert!(
        service
            .search(&repo, "OrderSource PARTNER_API", &elsewhere)?
            .hits
            .is_empty()
    );
    let only_tests = SearchOptions {
        kind: Some("test".into()),
        ..SearchOptions::limit(8)
    };
    assert!(service.search(&repo, "OrderSource", &only_tests).is_err());
    let context = SearchOptions {
        context: 10,
        ..SearchOptions::limit(8)
    };
    let with_context = service.search(&repo, "OrderSource PARTNER_API", &context)?;
    assert_eq!(with_context.hits[0].snippet.len(), 5);
    assert_eq!(with_context.hits[0].snippet[4], (Some(5), "}".into()));

    // The panel can switch to the repository and back.
    fixture.engine.set_scope(Scope::Source(id))?;
    let scoped = fixture.engine.retrieve("enum OrderSource")?;
    assert!(scoped.hits[0].path.ends_with("OrderSource.java"));
    assert_eq!(scoped.hits[0].line, Some(1));
    fixture.engine.set_scope(Scope::Notes)?;
    assert!(fixture.engine.retrieve("enum OrderSource")?.hits.iter().all(|hit| hit.line.is_none()));
    Ok(())
}

#[test]
fn search_socket_answers_ping_and_code_search() -> Result<()> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let fixture = Fixture::new()?;
    let repo = fixture.root.parent().unwrap().join("repo");
    fs::create_dir_all(&repo)?;
    fs::write(repo.join("Mailer.java"), "class SmtpMailer implements Mailer {}")?;
    let id = fixture.engine.store().add_code_source(&repo, &[])?;
    fixture.ingest.start(id, false);
    assert!(matches!(fixture.finished()?, IngestEvent::Finished { parsed: 1, .. }));

    let socket = fixture.root.parent().unwrap().join("s.sock");
    let service = CodeService::new(fixture.engine.clone(), fixture.ingest.clone());
    accio_core::socket::serve(&socket, service.clone())?;
    // A second server on the same socket refuses to start.
    assert!(accio_core::socket::serve(&socket, service).is_err());

    let stream = UnixStream::connect(&socket)?;
    let mut writer = stream.try_clone()?;
    let mut lines = BufReader::new(stream).lines();
    let mut ask = |request: serde_json::Value| -> Result<serde_json::Value> {
        writeln!(writer, "{request}")?;
        Ok(serde_json::from_str(&lines.next().context("no response")??)?)
    };
    assert_eq!(ask(serde_json::json!({"op": "ping"}))?["text"], "pong");
    let found = ask(serde_json::json!({
        "op": "code_search", "repo": repo, "query": "SmtpMailer", "limit": 3
    }))?;
    assert_eq!(found["ok"], true, "{found}");
    assert!(found["text"].as_str().unwrap().contains("Mailer.java:1"));
    assert_eq!(found["result"]["hits"][0]["path"], "Mailer.java");
    // Usages need a git checkout and list every mention, tests included.
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .output()
    };
    git(&["init", "-q"])?;
    fs::create_dir_all(repo.join("src/test"))?;
    fs::write(
        repo.join("src/test/MailerTest.java"),
        "class MailerTest { SmtpMailer mailer; }",
    )?;
    fs::write(repo.join("Other.java"), "class SmtpMailerFactory {}")?;
    let usages = ask(serde_json::json!({"op": "usages", "repo": repo, "query": "SmtpMailer"}))?;
    assert_eq!(usages["ok"], true, "{usages}");
    let files: Vec<_> = usages["result"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| {
            (
                file["path"].as_str().unwrap().to_owned(),
                file["kind"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    // Whole word: SmtpMailerFactory does not count. Untracked files do.
    assert_eq!(
        files,
        [
            ("Mailer.java".to_owned(), "code".to_owned()),
            ("src/test/MailerTest.java".to_owned(), "test".to_owned())
        ]
    );
    let missing = ask(serde_json::json!({"op": "code_search", "repo": "/nowhere", "query": "x"}))?;
    assert_eq!(missing["ok"], false);
    assert!(missing["error"].as_str().unwrap().contains("no code repository"));
    Ok(())
}
