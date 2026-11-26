//! Ingest one folder into the app's real index from the command line, with timing.
//! Run only while the app is closed (it would not see the new index until restart).
//!
//! cargo run -p accio-core --release --example ingest_source -- <folder> [--force]
use anyhow::{Context, Result};
use accio_core::{
    config::Config,
    engine::Engine,
    ingest::{IngestEvent, IngestManager},
    models::SidecarEmbedder,
};
use std::{
    path::PathBuf,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let folder = PathBuf::from(args.next().context("usage: ingest_source <folder> [--force]")?);
    let force = args.next().as_deref() == Some("--force");
    let config = Config::load()?;
    let started = Instant::now();
    let embedder = SidecarEmbedder::start(config.embed_cmd.clone())?;
    let engine = Engine::new(config, Arc::new(embedder))?;
    println!("embedding helper ready in {:.1}s", started.elapsed().as_secs_f32());

    let id = engine.store().add_source(&folder, &[])?;
    let (send, events) = mpsc::channel();
    let ingest = IngestManager::new(engine.clone(), Arc::new(move |event| {
        let _ = send.send(event);
    }));
    let started = Instant::now();
    ingest.start(id, force);
    let mut last_print = Instant::now() - Duration::from_secs(60);
    loop {
        match events.recv()? {
            IngestEvent::Progress { done, total, current, .. } => {
                if last_print.elapsed() > Duration::from_secs(15) || done == total {
                    last_print = Instant::now();
                    let name = current.rsplit('/').next().unwrap_or_default();
                    println!("[{:>6.0}s] {done}/{total}  {name}", started.elapsed().as_secs_f32());
                }
            }
            IngestEvent::Finished { parsed, failed, deleted, .. } => {
                let (ids, _) = engine.store().all_vectors()?;
                println!(
                    "done in {:.0}s: parsed {parsed}, failed {failed}, deleted {deleted}, chunks in index {}",
                    started.elapsed().as_secs_f32(),
                    ids.len()
                );
                for file in engine.store().failed_files(id, 20)? {
                    println!("  failed: {} — {}", file.path.display(), file.error.unwrap_or_default());
                }
                return Ok(());
            }
            other => anyhow::bail!("ingestion ended: {}", serde_json::to_string(&other)?),
        }
    }
}
