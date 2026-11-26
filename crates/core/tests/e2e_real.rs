use anyhow::{Result, ensure};
use accio_core::{
    config::Config,
    engine::Engine,
    ingest::{IngestEvent, IngestManager},
    models::SidecarEmbedder,
};
use std::{
    fs,
    process::Command,
    sync::{Arc, mpsc},
    time::Duration,
};

#[test]
#[ignore = "requires uv, Python 3.12, and model downloads"]
fn real_worker_mixed_folder() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("sources");
    fs::create_dir(&root)?;
    fs::write(
        root.join("rezept.md"),
        "# Kuchen\nMehl, Zucker, und Eier verrühren.",
    )?;
    let ingest_project = concat!(env!("CARGO_MANIFEST_DIR"), "/../../ingest");
    let generate = r#"
import sys
from pathlib import Path
import pymupdf
from docx import Document
root = Path(sys.argv[1])
doc = Document()
doc.add_heading('Vertrag', level=1)
doc.add_paragraph('Die Kündigungsfrist beträgt drei Monate.')
doc.save(root / 'vertrag.docx')
with pymupdf.open() as pdf:
    pdf.new_page().insert_text((60, 100), 'Anlagendokumentation: Wechselrichter und Solarmodule')
    pdf.save(root / 'anlage.pdf')
"#;
    let status = Command::new("uv")
        .args(["run", "--project", ingest_project, "python", "-c", generate])
        .arg(&root)
        .status()?;
    ensure!(
        status.success(),
        "generating office and PDF fixtures failed"
    );
    let config = Config {
        home: temp.path().join("data"),
        groq_api_key: None,
        groq_model: "openai/gpt-oss-20b".into(),
        min_similarity: accio_core::config::DEFAULT_MIN_SIMILARITY,
        max_context_chunks: 8,
        file_timeout: Duration::from_secs(300),
        ingest_cmd: [
            "env",
            "ACCIO_CAPTION=0",
            "uv",
            "run",
            "--project",
            ingest_project,
            "accio-ingest",
            "serve",
        ]
        .map(str::to_owned)
        .to_vec(),
        embed_cmd: ["uv", "run", "--project", ingest_project, "accio-ingest", "embed-server"]
            .map(str::to_owned)
            .to_vec(),
        translate: true,
        code_editor: "zed".into(),
    };
    let embedder = SidecarEmbedder::start(config.embed_cmd.clone())?;
    let engine = Engine::new(config, Arc::new(embedder))?;
    let (send, receive) = mpsc::channel();
    let manager = IngestManager::new(
        engine.clone(),
        Arc::new(move |event| {
            let _ = send.send(event);
        }),
    );
    let source = engine.store().add_source(&root, &[])?;
    manager.start(source, false);
    loop {
        match receive.recv_timeout(Duration::from_secs(310))? {
            IngestEvent::Progress { .. } => {}
            IngestEvent::Finished {
                parsed: 3,
                failed: 0,
                ..
            } => break,
            event => anyhow::bail!(
                "unexpected ingestion result: {event:?}; {:?}",
                engine.store().failed_files(source, 20)?
            ),
        }
    }
    let result = engine.retrieve("Welche Kündigungsfrist steht im Vertrag?")?;
    println!("{:?}", result.timings);
    assert!(
        result
            .hits
            .first()
            .is_some_and(|hit| hit.path.ends_with("vertrag.docx"))
    );
    Ok(())
}
