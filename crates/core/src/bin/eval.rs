use anyhow::{Context, Result, ensure};
use accio_core::{
    config::Config,
    engine::{Engine, Mode, Retrieval},
    models::SidecarEmbedder,
};
use serde::Deserialize;
use std::{
    env,
    fs::File,
    io::{BufRead, BufReader},
    sync::Arc,
};

#[derive(Deserialize)]
struct Question {
    q: String,
    expect: Option<String>,
}

fn recall_at_k(results: &[(Vec<String>, Option<String>)], k: usize) -> f32 {
    let mut hit = 0;
    let mut expected = 0;
    for (paths, reference) in results {
        if let Some(reference) = reference {
            expected += 1;
            hit += usize::from(paths.iter().take(k).any(|path| path.contains(reference)));
        }
    }
    hit as f32 / expected.max(1) as f32
}

fn percentile(mut values: Vec<f32>, percentile: f32) -> f32 {
    values.sort_by(f32::total_cmp);
    let rank = (values.len() as f32 * percentile).ceil() as usize;
    values[rank.saturating_sub(1)]
}

fn main() -> Result<()> {
    let mut config = Config::load()?;
    let mut args = env::args().skip(1);
    let mut path = None;
    while let Some(arg) = args.next() {
        if arg == "--min-sim" {
            config.min_similarity = args.next().context("--min-sim requires a value")?.parse()?;
        } else {
            ensure!(
                !arg.starts_with('-') && path.is_none(),
                "usage: eval [questions.jsonl] [--min-sim 0.3]"
            );
            path = Some(arg);
        }
    }
    ensure!(
        (0.0..=1.0).contains(&config.min_similarity),
        "--min-sim must be between 0 and 1"
    );
    let path = path.unwrap_or_else(|| "eval/questions.jsonl".into());
    let file = File::open(&path).with_context(|| format!("opening {path}"))?;
    let mut questions: Vec<Question> = Vec::new();
    for (i, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        if !line.trim().is_empty() {
            questions
                .push(serde_json::from_str(&line).with_context(|| format!("{path}:{}", i + 1))?);
        }
    }
    ensure!(!questions.is_empty(), "no questions in {path}");
    let tau = config.min_similarity;
    let embedder = SidecarEmbedder::start(config.embed_cmd.clone())?;
    let engine = Engine::open_existing(config, Arc::new(embedder))?;
    println!(
        "{:<17} {:>8}   {:>13} {:>13} {:>13} {:>13}",
        "mode", "recall@8", "embed", "dense", "fts", "total"
    );
    println!("Timings in milliseconds (p50/p95)");
    let mut null_false_positives = 0;
    let mut dropped = 0;
    for (label, mode) in [
        ("dense", Mode::Dense),
        ("fts", Mode::Fts),
        ("hybrid", Mode::Hybrid),
    ] {
        let runs: Vec<Retrieval> = questions
            .iter()
            .map(|q| engine.retrieve_with(&q.q, mode, false))
            .collect::<Result<_>>()?;
        let paths = runs
            .iter()
            .zip(&questions)
            .map(|(run, q)| {
                (
                    run.hits
                        .iter()
                        .map(|hit| hit.path.display().to_string())
                        .collect(),
                    q.expect.clone(),
                )
            })
            .collect::<Vec<_>>();
        print!("{label:<17} {:>7.1}%", 100.0 * recall_at_k(&paths, 8));
        let stages: [fn(&Retrieval) -> f32; 4] = [
            |r| r.timings.embed_ms,
            |r| r.timings.dense_ms,
            |r| r.timings.fts_ms,
            |r| r.timings.total_ms,
        ];
        for stage in stages {
            let values: Vec<_> = runs.iter().map(stage).collect();
            print!(
                "     {:>5.1}/{:<5.1}",
                percentile(values.clone(), 0.5),
                percentile(values, 0.95)
            );
        }
        println!();
        if mode == Mode::Hybrid {
            for (run, q) in runs.iter().zip(&questions) {
                // Mirrors the engine's gate: best dense cosine vs the similarity floor.
                let best = run.hits.iter().map(|hit| hit.score).fold(0.0, f32::max);
                let kept = best >= tau;
                null_false_positives += usize::from(q.expect.is_none() && kept);
                dropped += usize::from(q.expect.is_some() && !run.hits.is_empty() && !kept);
            }
        }
    }
    let null_count = questions.iter().filter(|q| q.expect.is_none()).count();
    println!(
        "Null-question false-positive rate at τ={tau}: {}",
        if null_count == 0 {
            "n/a".into()
        } else {
            format!(
                "{:.1}% ({null_false_positives}/{null_count})",
                100.0 * null_false_positives as f32 / null_count as f32
            )
        }
    );
    println!("In-notes questions dropped by τ: {dropped}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recall_at_5_counts() {
        let paths = vec![
            (vec!["/folder/miete.md".into()], Some("miete.md".into())),
            (
                (0..6).map(|i| format!("{i}.md")).collect(),
                Some("5.md".into()),
            ),
            (vec![], None),
        ];
        assert_eq!(recall_at_k(&paths, 5), 0.5);
        assert_eq!(recall_at_k(&paths, 6), 1.0);
    }
}
