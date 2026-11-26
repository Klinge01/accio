use crate::prompt::Turn;
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

const BASE_URL: &str = "https://api.groq.com/openai/v1";

#[derive(Debug, PartialEq, Eq)]
pub enum SseEvent {
    Delta(String),
    Usage { completion_tokens: u32 },
    Done,
    Error(String),
}

#[derive(Default)]
pub struct SseParser {
    partial: Vec<u8>,
}

impl SseParser {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.partial.extend_from_slice(bytes);
        let mut complete = 0;
        let mut events = Vec::new();
        for line in self.partial.split_inclusive(|&byte| byte == b'\n') {
            if !line.ends_with(b"\n") {
                break;
            }
            complete += line.len();
            let Some(data) = line.strip_prefix(b"data:") else {
                continue;
            };
            if data.trim_ascii() == b"[DONE]" {
                events.push(SseEvent::Done);
                continue;
            }
            let Ok(value) = serde_json::from_slice::<Value>(data) else {
                continue;
            };
            if let Some(message) = value.pointer("/error/message").and_then(Value::as_str) {
                events.push(SseEvent::Error(message.to_owned()));
            }
            if value["type"] == "message_stop" {
                events.push(SseEvent::Done);
            }
            if let Some(content) = value
                .pointer("/choices/0/delta/content")
                .or_else(|| {
                    (value["type"] == "content_block_delta"
                        && value["delta"]["type"] == "text_delta")
                        .then(|| &value["delta"]["text"])
                })
                .and_then(Value::as_str)
                && !content.is_empty()
            {
                events.push(SseEvent::Delta(content.to_owned()));
            }
            if let Some(tokens) = value
                .pointer("/x_groq/usage/completion_tokens")
                .or_else(|| value.pointer("/usage/completion_tokens"))
                .or_else(|| value.pointer("/usage/output_tokens"))
                .and_then(Value::as_u64)
                .and_then(|tokens| u32::try_from(tokens).ok())
            {
                events.push(SseEvent::Usage {
                    completion_tokens: tokens,
                });
            }
        }
        self.partial.drain(..complete);
        events
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    #[default]
    Groq,
    OpenAi,
    Anthropic,
    ClaudeCode,
    Codex,
    Mlx,
}

impl Provider {
    pub fn id(self) -> &'static str {
        match self {
            Self::Groq => "groq",
            Self::OpenAi => "open_ai",
            Self::Anthropic => "anthropic",
            Self::ClaudeCode => "claude_code",
            Self::Codex => "codex",
            Self::Mlx => "mlx",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Groq => "Groq",
            Self::OpenAi => "OpenAI",
            Self::Anthropic => "Anthropic",
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
            Self::Mlx => "Local MLX",
        }
    }

    pub fn requires_key(self) -> bool {
        matches!(self, Self::Groq | Self::OpenAi | Self::Anthropic)
    }

    fn api_url(self) -> &'static str {
        match self {
            Self::Groq => BASE_URL,
            Self::OpenAi => "https://api.openai.com/v1",
            Self::Anthropic => "https://api.anthropic.com/v1",
            Self::Mlx => "http://127.0.0.1:8080/v1",
            _ => "",
        }
    }
}

pub trait AnswerProvider {
    fn stream(
        &self,
        messages: Vec<Turn>,
        cancel: CancellationToken,
        on_token: impl FnMut(&str) + Send,
    ) -> impl std::future::Future<Output = Result<LlmStats>> + Send;
}

#[derive(Clone)]
pub struct Groq {
    client: Client,
    api_key: Option<String>,
    model: String,
    base_url: String,
    provider: Provider,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct LlmStats {
    pub ttft_ms: f32,
    pub total_ms: f32,
    pub completion_tokens: u32,
}

impl Groq {
    pub fn new(api_key: Option<String>, model: String) -> Self {
        Self::for_provider(Provider::Groq, api_key, model)
    }

    pub fn for_provider(provider: Provider, api_key: Option<String>, model: String) -> Self {
        let builder = if provider == Provider::Mlx {
            Client::builder().no_proxy()
        } else {
            Client::builder()
        };
        let client = builder
            .redirect(reqwest::redirect::Policy::none())
            .http2_adaptive_window(true)
            .pool_idle_timeout(Duration::from_secs(600))
            .tcp_keepalive(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(60))
            .build()
            .expect("building Groq HTTP client");
        Self {
            client,
            api_key,
            model,
            base_url: provider.api_url().into(),
            provider,
        }
    }

    pub fn label(&self) -> &'static str {
        self.provider.label()
    }

    pub fn with_model(&self, model: String) -> Self {
        Self {
            model,
            ..self.clone()
        }
    }

    fn authenticate(&self, builder: reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder> {
        match (&self.api_key, self.provider) {
            (Some(key), Provider::Anthropic) => Ok(builder
                .header("x-api-key", key)
                .header("anthropic-version", "2023-06-01")),
            (Some(key), _) => Ok(builder.bearer_auth(key)),
            (None, Provider::Groq) => bail!("GROQ_API_KEY missing — add it to .env"),
            (None, provider) if provider.requires_key() => bail!("Add an API key in Settings"),
            _ => Ok(builder),
        }
    }

    pub async fn models(&self) -> Result<Vec<String>> {
        match self.provider {
            Provider::ClaudeCode => {
                return Ok(["haiku", "sonnet", "opus"].map(str::to_owned).into());
            }
            Provider::Codex => {
                return tokio::time::timeout(Duration::from_secs(10), codex_models())
                    .await
                    .context("Codex model list timed out")?;
            }
            _ => {}
        }
        let mut url = format!("{}/models", self.base_url);
        let mut models = Vec::new();
        loop {
            let response = self
                .authenticate(self.client.get(&url))?
                .timeout(Duration::from_secs(10))
                .send()
                .await?
                .error_for_status()?
                .json::<Value>()
                .await?;
            let ids = response["data"]
                .as_array()
                .context("missing model list")?
                .iter()
                .filter_map(|model| model["id"].as_str());
            models.extend(ids.map(str::to_owned));
            if self.provider != Provider::Anthropic || response["has_more"] != true {
                break;
            }
            let after = response["last_id"]
                .as_str()
                .context("missing model list cursor")?;
            let mut next = reqwest::Url::parse(&format!("{}/models", self.base_url))?;
            next.query_pairs_mut().append_pair("after_id", after);
            url = next.into();
        }
        models.sort_unstable();
        models.dedup();
        Ok(models)
    }

    pub async fn warm(&self) {
        if self.provider.requires_key() && self.api_key.is_some() {
            let _ = self.models().await;
        }
    }

    pub async fn stream(
        &self,
        messages: Vec<Turn>,
        cancel: CancellationToken,
        mut on_token: impl FnMut(&str),
    ) -> Result<LlmStats> {
        if matches!(self.provider, Provider::ClaudeCode | Provider::Codex) {
            return self.stream_cli(messages, cancel, on_token).await;
        }
        let label = self.provider.label();
        let started = Instant::now();
        let (endpoint, mut request) = if self.provider == Provider::Anthropic {
            let system = messages
                .iter()
                .find(|turn| turn.role == "system")
                .map(|turn| turn.content.as_str())
                .unwrap_or_default();
            let turns: Vec<_> = messages
                .iter()
                .filter(|turn| turn.role != "system")
                .collect();
            (
                "messages",
                json!({"model": self.model, "system": system, "messages": turns, "stream": true, "max_tokens": 2048}),
            )
        } else {
            (
                "chat/completions",
                json!({"model": self.model, "messages": messages, "stream": true}),
            )
        };
        if self.provider == Provider::Mlx {
            request["max_tokens"] = json!(2048);
        } else if self.provider != Provider::Anthropic {
            request["max_completion_tokens"] = json!(2048);
        }
        if self.provider == Provider::OpenAi {
            request["stream_options"] = json!({"include_usage": true});
        }
        if self.provider == Provider::Groq && self.model.starts_with("openai/gpt-oss-") {
            request["reasoning_effort"] = json!("low");
            request["include_reasoning"] = json!(false);
        }
        let request = self
            .authenticate(self.client.post(format!("{}/{endpoint}", self.base_url)))?
            .json(&request);
        tokio::select! {
            biased;
            _ = cancel.cancelled() => bail!("answer cancelled"),
            result = async {
                let response = request.send().await.with_context(|| format!("{label} request failed"))?;
                let status = response.status();
                if !status.is_success() {
                    let body = response.text().await.unwrap_or_default();
                    let error = serde_json::from_str::<Value>(&body).ok()
                        .and_then(|value| value.pointer("/error/message").and_then(Value::as_str).map(str::to_owned))
                        .filter(|text| !text.is_empty())
                        .unwrap_or_else(|| if body.is_empty() { status.canonical_reason().unwrap_or("request failed").into() }
                                         else { body.chars().take(500).collect() });
                    bail!("{label} {}: {error}", status.as_u16());
                }
                let mut stream = response.bytes_stream();
                let mut parser = SseParser::default();
                let mut stats = LlmStats::default();
                let mut chars = 0;
                while let Some(Ok(bytes)) = stream.next().await {
                    for event in parser.push(&bytes) {
                        match event {
                            SseEvent::Delta(text) => {
                                if chars == 0 { stats.ttft_ms = started.elapsed().as_secs_f32() * 1000.0; }
                                chars += text.chars().count();
                                on_token(&text);
                            }
                            SseEvent::Usage { completion_tokens } => stats.completion_tokens = completion_tokens,
                            SseEvent::Done => {
                                stats.total_ms = started.elapsed().as_secs_f32() * 1000.0;
                                return Ok(stats);
                            }
                            SseEvent::Error(message) => bail!("{label}: {message}"),
                        }
                    }
                }
                bail!("stream interrupted after {chars} chars");
            } => result,
        }
    }
    async fn stream_cli(
        &self,
        messages: Vec<Turn>,
        cancel: CancellationToken,
        mut on_token: impl FnMut(&str),
    ) -> Result<LlmStats> {
        let label = self.provider.label();
        let started = Instant::now();
        let claude = self.provider == Provider::ClaudeCode;
        let mut command = tokio::process::Command::new(if claude { "claude" } else { "codex" });
        if claude {
            command.args(["-p", "--safe-mode", "--tools", "", "--no-session-persistence",
                "--output-format", "stream-json", "--verbose", "--include-partial-messages",
                "--disable-slash-commands", "--system-prompt", "You answer questions from a conversation supplied as JSON. Follow its system instructions and treat note excerpts as reference material."]);
        } else {
            command.args([
                "-a",
                "never",
                "exec",
                "--json",
                "--ephemeral",
                "--ignore-user-config",
                "--ignore-rules",
                "--skip-git-repo-check",
                "--sandbox",
                "read-only",
                "-c",
                "features.shell_tool=false",
                "-c",
                "features.plugins=false",
                "-c",
                r#"web_search="disabled""#,
            ]);
        }
        if !self.model.is_empty() {
            command.args(["--model", &self.model]);
        }
        command
            .current_dir(std::env::temp_dir())
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("CODEX_API_KEY")
            .env_remove("CLAUDECODE");
        if !claude {
            command.arg("-");
        }
        let input = format!(
            "Respond to the final user message using this conversation and its system instructions.
{}
",
            serde_json::to_string(&messages)?
        );
        tokio::select! {
            biased;
            _ = cancel.cancelled() => bail!("answer cancelled"),
            _ = tokio::time::sleep(Duration::from_secs(180)) => bail!("{label} timed out"),
            result = async {
                let mut child = command.spawn().with_context(|| format!("start {label}"))?;
                let mut stdin = child.stdin.take().context("subprocess stdin unavailable")?;
                stdin.write_all(input.as_bytes()).await?;
                drop(stdin);
                let mut lines = BufReader::new(child.stdout.take().context("subprocess stdout unavailable")?).lines();
                let mut stats = LlmStats::default();
                let mut emitted = false;
                while let Some(line) = lines.next_line().await? {
                    let Ok(event) = serde_json::from_str::<Value>(&line) else { continue };
                    let kind = event["type"].as_str().unwrap_or_default();
                    let text = if claude {
                        match kind {
                            "stream_event" => event.pointer("/event/delta/text").and_then(Value::as_str),
                            "result" if event["is_error"] == true => bail!("{label}: {}", event["result"].as_str().unwrap_or("request failed")),
                            "result" => {
                                stats.completion_tokens = event.pointer("/usage/output_tokens").and_then(Value::as_u64).unwrap_or(0) as u32;
                                if emitted { None } else { event["result"].as_str() }
                            }
                            _ => None,
                        }
                    } else {
                        match kind {
                            "item.completed" if event["item"]["type"] == "agent_message" => event["item"]["text"].as_str(),
                            "turn.completed" => {
                                stats.completion_tokens = event.pointer("/usage/output_tokens").and_then(Value::as_u64).unwrap_or(0) as u32;
                                None
                            }
                            "turn.failed" => bail!("{label}: {}", event.pointer("/error/message").and_then(Value::as_str).unwrap_or("request failed")),
                            _ => None,
                        }
                    };
                    if let Some(text) = text.filter(|text| !text.is_empty()) {
                        if !emitted {
                            stats.ttft_ms = started.elapsed().as_secs_f32() * 1000.0;
                            emitted = true;
                        }
                        on_token(text);
                    }
                }
                let status = child.wait().await?;
                anyhow::ensure!(status.success() && emitted, "{label} returned no answer ({status}). Check your CLI login and model.");
                stats.total_ms = started.elapsed().as_secs_f32() * 1000.0;
                Ok(stats)
            } => result,
        }
    }
}

impl AnswerProvider for Groq {
    async fn stream(
        &self,
        messages: Vec<Turn>,
        cancel: CancellationToken,
        on_token: impl FnMut(&str) + Send,
    ) -> Result<LlmStats> {
        Groq::stream(self, messages, cancel, on_token).await
    }
}

async fn codex_models() -> Result<Vec<String>> {
    let mut child = tokio::process::Command::new("codex")
        .args([
            "app-server",
            "--listen",
            "stdio://",
            "-c",
            "features.plugins=false",
            "-c",
            "mcp_servers={}",
        ])
        .current_dir(std::env::temp_dir())
        .env_remove("OPENAI_API_KEY")
        .env_remove("CODEX_API_KEY")
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("start Codex to list models")?;
    let mut input = child.stdin.take().context("Codex stdin unavailable")?;
    let mut output =
        BufReader::new(child.stdout.take().context("Codex stdout unavailable")?).lines();
    let initialize = json!({"id": 1, "method": "initialize", "params": {
        "clientInfo": {"name": "accio", "title": "Accio", "version": env!("CARGO_PKG_VERSION")}
    }});
    input
        .write_all(format!("{initialize}\n").as_bytes())
        .await?;
    while let Some(line) = output.next_line().await? {
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(error) = event.pointer("/error/message").and_then(Value::as_str) {
            bail!("Codex: {error}");
        }
        match event["id"].as_u64() {
            Some(1) => {
                let initialized = json!({"method": "initialized"});
                let list =
                    json!({"id": 2, "method": "model/list", "params": {"includeHidden": false}});
                input
                    .write_all(format!("{initialized}\n{list}\n").as_bytes())
                    .await?;
            }
            Some(2) => {
                return Ok(event["result"]["data"]
                    .as_array()
                    .context("missing Codex model list")?
                    .iter()
                    .filter_map(|model| model["model"].as_str())
                    .map(str::to_owned)
                    .collect());
            }
            _ => {}
        }
    }
    bail!("Codex stopped before returning a model list")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[test]
    fn sse_split_across_pushes() {
        let mut parser = SseParser::default();
        let mut events =
            parser.push(b"data: {\"choices\":[{\"delta\":{\"content\":\"Hal\"}}]}\n\nda");
        events.extend(
            parser.push(
                b"ta: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\r\n\r\ndata: [DONE]\n\n",
            ),
        );
        assert_eq!(
            events,
            [
                SseEvent::Delta("Hal".into()),
                SseEvent::Delta("lo".into()),
                SseEvent::Done
            ]
        );
    }

    #[test]
    fn sse_usage_and_error_events() {
        let mut parser = SseParser::default();
        assert_eq!(parser.push(b"data: {\"x_groq\":{\"usage\":{\"completion_tokens\":30}}}\n\ndata: {\"error\":{\"message\":\"problem\"}}\n\n"),
            [SseEvent::Usage { completion_tokens: 30 }, SseEvent::Error("problem".into())]);
    }

    #[test]
    fn anthropic_sse_keeps_text_and_usage() {
        let mut parser = SseParser::default();
        let events = parser.push(concat!(
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"private\"}}\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":12}}\n",
            "data: {\"type\":\"message_stop\"}\n",
        ).as_bytes());
        assert_eq!(
            events,
            [
                SseEvent::Delta("hello".into()),
                SseEvent::Usage {
                    completion_tokens: 12
                },
                SseEvent::Done,
            ]
        );
    }

    async fn canned(status: &str, content_type: &str, body: &str) -> Result<Groq> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let mut groq = Groq::new(Some("test".into()), "openai/gpt-oss-20b".into());
        groq.base_url = format!("http://{}", listener.local_addr()?);
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        tokio::spawn(async move {
            let (mut client, _) = listener.accept().await.unwrap();
            let mut request = vec![];
            let mut buf = [0; 1024];
            loop {
                let n = client.read(&mut buf).await.unwrap();
                if n == 0 {
                    return;
                }
                request.extend_from_slice(&buf[..n]);
                let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..end]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if request.len() >= end + 4 + length {
                    break;
                }
            }
            client.write_all(response.as_bytes()).await.unwrap();
        });
        Ok(groq)
    }

    #[tokio::test]
    async fn missing_key_error_message() {
        let groq = Groq::new(None, "openai/gpt-oss-20b".into());
        let error = groq
            .stream(vec![], CancellationToken::new(), |_| {})
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "GROQ_API_KEY missing — add it to .env");
    }

    #[tokio::test]
    async fn http_error_message() -> Result<()> {
        for (status, message) in [
            ("429 Too Many Requests", "Rate limited"),
            ("401 Unauthorized", "Invalid key"),
        ] {
            let groq = canned(
                status,
                "application/json",
                &json!({"error": {"message": message}}).to_string(),
            )
            .await?;
            let error = groq
                .stream(vec![], CancellationToken::new(), |_| {})
                .await
                .unwrap_err();
            assert_eq!(
                error.to_string(),
                format!("Groq {}: {message}", &status[..3])
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn stream_interrupted() -> Result<()> {
        let groq = canned(
            "200 OK",
            "text/event-stream",
            "data: {\"choices\":[{\"delta\":{\"content\":\"Héy\"}}]}\n\n",
        )
        .await?;
        let mut received = String::new();
        let error = groq
            .stream(vec![], CancellationToken::new(), |part| {
                received.push_str(part)
            })
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "stream interrupted after 3 chars");
        assert_eq!(received, "Héy");
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires GROQ_API_KEY and internet"]
    async fn live_groq_ttft() -> Result<()> {
        let config = crate::config::Config::load()?;
        let groq = Groq::new(config.groq_api_key, config.groq_model);
        let key = groq
            .api_key
            .as_ref()
            .context("GROQ_API_KEY missing — add it to .env")?;
        let models: Value = groq
            .client
            .get(format!("{}/models", groq.base_url))
            .bearer_auth(key)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let ids: Vec<_> = models["data"]
            .as_array()
            .context("missing models list")?
            .iter()
            .filter_map(|model| model["id"].as_str())
            .collect();
        assert!(ids.contains(&groq.model.as_str()), "available IDs: {ids:?}");
        let mut answer = String::new();
        let stats = groq
            .stream(
                vec![Turn::new("user", "Say hi")],
                CancellationToken::new(),
                |part| answer.push_str(part),
            )
            .await?;
        assert!(!answer.is_empty());
        println!("{stats:?}");
        Ok(())
    }
}
