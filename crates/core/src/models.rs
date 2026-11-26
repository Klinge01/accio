use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use crate::lang::Lang;
use serde::Deserialize;
use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{Mutex, mpsc},
    thread,
    time::Duration,
};

pub trait EmbedModel: Send + Sync {
    fn model_id(&self) -> &str;
    fn dim(&self) -> usize;
    fn process_id(&self) -> Option<u32> {
        None
    }
    fn embed_query(&self, query: &str) -> Result<Vec<f32>>;
    fn embed_docs(&self, docs: &[(&str, &str)]) -> Result<Vec<Vec<f32>>>;
    /// Several queries in one model call.
    fn embed_queries(&self, queries: &[&str]) -> Result<Vec<Vec<f32>>> {
        queries.iter().map(|query| self.embed_query(query)).collect()
    }
    /// `text` translated into `to`; `None` when translation is unavailable.
    fn translate(&self, _text: &str, _to: Lang) -> Result<Option<String>> {
        Ok(None)
    }
}

pub(crate) fn normalize(vector: &mut [f32]) {
    let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        vector.iter_mut().for_each(|value| *value /= norm);
    }
}

/// Matryoshka truncation of EmbeddingGemma's 768-dim output.
pub const DIM: usize = 512;
const MODEL_ID: &str = "EmbeddingGemma300M-mlx-bf16";
/// Per request once the helper is up.
const TIMEOUT: Duration = Duration::from_secs(120);
/// First request after start: includes loading the model and, on a fresh install,
/// downloading it (~600 MB).
const STARTUP_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// EmbeddingGemma on the Apple GPU (MLX), served by the resident helper process
/// `accio-ingest embed-server`. Protocol: one JSON line per request
/// `{"id", "texts"}` and per response `{"id", "n", "dim", "f32le"}` (base64
/// little-endian f32, row-major) or `{"id", "error"}`. A helper that dies or
/// stops answering is killed and respawned once per request.
pub struct SidecarEmbedder {
    cmd: Vec<String>,
    sidecar: Mutex<(Option<Sidecar>, u64)>,
}

struct Sidecar {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<String>,
}

#[derive(Deserialize)]
struct Response {
    id: u64,
    #[serde(default)]
    n: usize,
    #[serde(default)]
    dim: usize,
    #[serde(default)]
    f32le: String,
    #[serde(default)]
    text: Option<String>,
    error: Option<String>,
}

impl Sidecar {
    fn spawn(cmd: &[String]) -> Result<Self> {
        let (program, args) = cmd.split_first().context("empty embedding command")?;
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| crate::config::spawn_error("starting embedding helper", program, error))?;
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let (send, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in stdout.lines().map_while(Result::ok) {
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        Ok(Self { child, stdin, lines })
    }

    /// Outer error: transport failure (helper dead or silent). Inner: model error.
    fn call(
        &mut self,
        id: u64,
        mut request: serde_json::Value,
        timeout: Duration,
    ) -> Result<Result<Response, String>> {
        request["id"] = id.into();
        writeln!(self.stdin, "{request}").context("embedding helper closed its input")?;
        self.stdin.flush()?;
        loop {
            let line = self.lines.recv_timeout(timeout).map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => anyhow!("embedding helper timed out"),
                mpsc::RecvTimeoutError::Disconnected => anyhow!("embedding helper exited"),
            })?;
            let mut response: Response = serde_json::from_str(&line)
                .with_context(|| format!("invalid embedding helper output: {line}"))?;
            if response.id != id {
                continue; // answer to a request that timed out earlier
            }
            return Ok(match response.error.take() {
                Some(error) => Err(error),
                None => Ok(response),
            });
        }
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn decode(response: &Response, expected: usize) -> Result<Vec<Vec<f32>>> {
    ensure!(response.n == expected, "embedding helper returned {} of {expected} vectors", response.n);
    ensure!(response.dim >= DIM, "embedding helper returned {} dimensions", response.dim);
    let bytes = BASE64.decode(&response.f32le).context("decoding embeddings")?;
    ensure!(bytes.len() == response.n * response.dim * 4, "embedding payload has the wrong size");
    let values: Vec<f32> = bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    Ok(values
        .chunks_exact(response.dim)
        .map(|row| {
            let mut vector = row[..DIM].to_vec();
            normalize(&mut vector);
            vector
        })
        .collect())
}

impl SidecarEmbedder {
    /// Starts the helper and waits until the model answers.
    pub fn start(cmd: Vec<String>) -> Result<Self> {
        let embedder = Self { cmd, sidecar: Mutex::new((None, 0)) };
        embedder
            .embed_with_timeout(vec!["warm up".into()], STARTUP_TIMEOUT)
            .context("starting the embedding model")?;
        Ok(embedder)
    }

    fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        self.embed_with_timeout(texts, TIMEOUT)
    }

    fn embed_with_timeout(&self, texts: Vec<String>, timeout: Duration) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let response = self.request(serde_json::json!({ "texts": texts }), timeout)?;
        decode(&response, texts.len())
    }

    /// One request to the helper, respawning it once if it died or went silent.
    fn request(&self, request: serde_json::Value, timeout: Duration) -> Result<Response> {
        let mut guard = self.sidecar.lock().unwrap();
        let (sidecar, next_id) = &mut *guard;
        let mut last_error = None;
        for _attempt in 0..2 {
            if sidecar.is_none() {
                *sidecar = Some(Sidecar::spawn(&self.cmd)?);
            }
            *next_id += 1;
            match sidecar.as_mut().unwrap().call(*next_id, request.clone(), timeout) {
                Ok(Ok(response)) => return Ok(response),
                Ok(Err(model_error)) => bail!("embedding failed: {model_error}"),
                Err(transport) => {
                    *sidecar = None; // kill; respawn on the next attempt
                    last_error = Some(transport);
                }
            }
        }
        Err(last_error.unwrap())
    }
}

impl EmbedModel for SidecarEmbedder {
    fn model_id(&self) -> &str {
        MODEL_ID
    }
    fn dim(&self) -> usize {
        DIM
    }

    fn process_id(&self) -> Option<u32> {
        self.sidecar.try_lock().ok()?.0.as_ref().map(|sidecar| sidecar.child.id())
    }
    fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        self.embed_queries(&[query])?
            .pop()
            .context("embedding helper returned no result")
    }

    fn embed_queries(&self, queries: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.embed(
            queries
                .iter()
                .map(|query| format!("task: search result | query: {query}"))
                .collect(),
        )
    }

    fn translate(&self, text: &str, to: Lang) -> Result<Option<String>> {
        let request = serde_json::json!({ "translate": text, "to": to.code() });
        Ok(self.request(request, TIMEOUT)?.text.filter(|text| !text.trim().is_empty()))
    }

    fn embed_docs(&self, docs: &[(&str, &str)]) -> Result<Vec<Vec<f32>>> {
        self.embed(
            docs.iter()
                .map(|(header, text)| format!("title: {header} | text: {text}"))
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake(mode: &str) -> Vec<String> {
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake_embed_server.py");
        vec!["python3".into(), script.into(), mode.into()]
    }

    #[test]
    fn sidecar_roundtrip_truncates_and_normalizes() -> Result<()> {
        let embedder = SidecarEmbedder::start(fake("ok"))?;
        let docs = embedder.embed_docs(&[("a", "x"), ("b", "yy")])?;
        assert_eq!(docs.len(), 2);
        for vector in &docs {
            assert_eq!(vector.len(), DIM);
            assert!((vector.iter().map(|x| x * x).sum::<f32>() - 1.0).abs() < 1e-5);
        }
        assert_ne!(docs[0], docs[1]);
        assert_eq!(embedder.embed_query("q")?.len(), DIM);
        assert_eq!(embedder.embed_queries(&["q", "r"])?.len(), 2);
        assert_eq!(embedder.translate("hallo", Lang::En)?.as_deref(), Some("en:hallo"));
        Ok(())
    }

    #[test]
    fn sidecar_respawns_after_crash() -> Result<()> {
        // "crash-once": the helper exits on its second request, then a fresh one works.
        let embedder = SidecarEmbedder::start(fake("crash-second"))?;
        assert_eq!(embedder.embed_query("after crash")?.len(), DIM);
        Ok(())
    }

    #[test]
    fn sidecar_model_error_is_reported() {
        let embedder = SidecarEmbedder { cmd: fake("error"), sidecar: Mutex::new((None, 0)) };
        let error = embedder.embed_query("q").unwrap_err().to_string();
        assert_eq!(error, "embedding failed: boom");
    }

    #[test]
    #[ignore = "needs the real MLX helper and cached model"]
    fn real_sidecar_semantic_order() -> Result<()> {
        let embedder = SidecarEmbedder::start(crate::config::Config::load()?.embed_cmd)?;
        for text in [
            "Wie hoch ist die Miete?",
            "Was ist der Unterschied zwischen Wetter und Klima?",
            "Photosynthese",
            "Welche Planeten gibt es im Sonnensystem und wie groß sind sie?",
            "Gartenplanung",
        ] {
            let start = std::time::Instant::now();
            embedder.embed_query(text)?;
            println!("query embedding {:5.1} ms  {text}", start.elapsed().as_secs_f32() * 1000.0);
        }
        let query = embedder.embed_query("Wie hoch ist die Miete?")?;
        let docs = embedder.embed_docs(&[
            ("Wohnung › Kosten", "Die Kaltmiete beträgt 950 Euro im Monat"),
            ("Rezept › Kuchen", "Mehl und Zucker verrühren"),
        ])?;
        let dot = |doc: &[f32]| query.iter().zip(doc).map(|(q, d)| q * d).sum::<f32>();
        println!("relevant {:.3} irrelevant {:.3}", dot(&docs[0]), dot(&docs[1]));
        assert!(dot(&docs[0]) > dot(&docs[1]));
        Ok(())
    }
}
