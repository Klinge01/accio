//! Ask questions against the real index from the command line, through the same
//! pipeline the app uses (retrieve → similarity gate → prompt → Groq stream).
//! Prints the best dense cosine per question to help calibrate ACCIO_MIN_SIMILARITY.
//!
//! cargo run -p accio-core --release --example ask -- "question" ["another question" ...]
//! Add --no-llm to print retrieval only.
use anyhow::Result;
use accio_core::{
    config::Config,
    engine::{Engine, Mode},
    llm::Groq,
    models::SidecarEmbedder,
    prompt::build_messages,
};
use std::{io::Write, sync::Arc, time::Instant};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<()> {
    let mut questions: Vec<String> = std::env::args().skip(1).collect();
    let no_llm = questions.iter().any(|q| q == "--no-llm");
    questions.retain(|q| q != "--no-llm");
    let config = Config::load()?;
    let groq = Groq::new(config.groq_api_key.clone(), config.groq_model.clone());
    let threshold = config.min_similarity;
    let embedder = SidecarEmbedder::start(config.embed_cmd.clone())?;
    let engine = Engine::open_existing(config, Arc::new(embedder))?;
    groq.warm().await;
    for question in questions {
        // Ungated view first, to show the best cosine even when the gate drops everything.
        let raw = engine.retrieve_with(&question, Mode::Dense, false)?;
        let best = raw.hits.first().map_or(0.0, |hit| hit.score);
        let started = Instant::now();
        let retrieval = engine.retrieve(&question)?;
        let retrieval_ms = started.elapsed().as_secs_f32() * 1000.0;
        println!("\n━━ {question}");
        println!(
            "best cosine {best:.3} (gate {threshold}) → {} notes · retrieval {retrieval_ms:.0} ms (translate {:.0} ms, embed {:.0} ms)",
            retrieval.hits.len(),
            retrieval.timings.translate_ms,
            retrieval.timings.embed_ms
        );
        if let Some(translation) = &retrieval.translation {
            println!("also searched: {translation}");
        }
        for (n, hit) in retrieval.hits.iter().enumerate() {
            println!("  [{}] {:.3}  {}", n + 1, hit.score, hit.header.chars().take(90).collect::<String>());
        }
        if no_llm {
            continue;
        }
        print!("  ▸ ");
        let stats = groq
            .stream(build_messages(&question, &retrieval.hits, &[]), CancellationToken::new(), |text| {
                print!("{}", text.replace('\n', "\n    "));
                let _ = std::io::stdout().flush();
            })
            .await?;
        println!(
            "\n  ttft {:.0} ms · total {:.0} ms · {} tokens",
            stats.ttft_ms, stats.total_ms, stats.completion_tokens
        );
    }
    Ok(())
}
