use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Debug, Deserialize, Serialize)]
pub struct ParsedChunk {
    pub header: String,
    pub text: String,
    pub page: Option<u32>,
    #[serde(default)]
    pub line: Option<u32>,
    #[serde(default)]
    pub end_line: Option<u32>,
}

#[derive(Deserialize, Serialize)]
struct Request {
    id: u64,
    path: PathBuf,
    /// "text" | "markdown"; only sent for parse requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pdf_mode: Option<String>,
    /// Code repositories: the repository root, for chunk headers relative to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root: Option<PathBuf>,
}

#[derive(Deserialize, Serialize)]
struct Response {
    id: u64,
    chunks: Vec<ParsedChunk>,
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hash: Option<String>,
}

pub fn serve_hash_worker() -> Result<()> {
    let mut output = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines() {
        let request: Request = serde_json::from_str(&line?)?;
        let (hash, error) = match crate::scan::hash_file(&request.path) {
            Ok(hash) => (Some(hash), None),
            Err(error) => (None, Some(format!("{error:#}"))),
        };
        serde_json::to_writer(
            &mut output,
            &Response {
                id: request.id,
                chunks: vec![],
                error,
                hash,
            },
        )?;
        writeln!(output)?;
        output.flush()?;
    }
    Ok(())
}

pub struct Worker {
    child: Child,
    thread: Option<JoinHandle<()>>,
    requests: Option<mpsc::Sender<String>>,
    responses: mpsc::Receiver<Response>,
    next_id: u64,
}

impl Worker {
    pub fn spawn(cmd: &[String]) -> Result<Self> {
        let (executable, args) = cmd.split_first().context("empty ingestion command")?;
        let mut command = Command::new(executable);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command
            .spawn()
            .map_err(|error| crate::config::spawn_error("starting ingestion worker", executable, error))?;
        let mut input = BufWriter::new(child.stdin.take().unwrap());
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let (requests, incoming) = mpsc::channel::<String>();
        let (outgoing, responses) = mpsc::channel();
        let thread = thread::spawn(move || {
            for request in incoming {
                if writeln!(input, "{request}")
                    .and_then(|_| input.flush())
                    .is_err()
                {
                    break;
                }
                let mut line = String::new();
                if !matches!(output.read_line(&mut line), Ok(n) if n > 0) {
                    break;
                }
                let Ok(response) = serde_json::from_str(&line) else {
                    break;
                };
                if outgoing.send(response).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            thread: Some(thread),
            requests: Some(requests),
            responses,
            next_id: 0,
        })
    }

    pub fn parse(
        &mut self,
        path: &Path,
        timeout: Duration,
    ) -> Result<Result<Vec<ParsedChunk>, String>> {
        self.parse_until(path, None, None, timeout, || false)
    }

    pub(crate) fn parse_until(
        &mut self,
        path: &Path,
        pdf_mode: Option<&str>,
        root: Option<&Path>,
        timeout: Duration,
        cancelled: impl Fn() -> bool,
    ) -> Result<Result<Vec<ParsedChunk>, String>> {
        let response = self.request(path, pdf_mode, root, timeout, cancelled)?;
        Ok(match response.error {
            Some(error) => Err(error),
            None => Ok(response.chunks),
        })
    }

    pub(crate) fn hash_until(
        &mut self,
        path: &Path,
        timeout: Duration,
        cancelled: impl Fn() -> bool,
    ) -> Result<Result<String, String>> {
        let response = self.request(path, None, None, timeout, cancelled)?;
        Ok(match response.error {
            Some(error) => Err(error),
            None => Ok(response
                .hash
                .context("hash worker did not return a digest")?),
        })
    }

    fn request(
        &mut self,
        path: &Path,
        pdf_mode: Option<&str>,
        root: Option<&Path>,
        timeout: Duration,
        cancelled: impl Fn() -> bool,
    ) -> Result<Response> {
        self.next_id += 1;
        let request = serde_json::to_string(&Request {
            id: self.next_id,
            path: path.to_owned(),
            pdf_mode: pdf_mode.map(str::to_owned),
            root: root.map(Path::to_owned),
        })?;
        self.requests
            .as_ref()
            .unwrap()
            .send(request)
            .map_err(|_| anyhow!("worker crashed"))?;
        let started = Instant::now();
        loop {
            ensure!(!cancelled(), "cancelled");
            let remaining = timeout.saturating_sub(started.elapsed());
            ensure!(!remaining.is_zero(), "timeout after {}s", timeout.as_secs());
            match self
                .responses
                .recv_timeout(remaining.min(Duration::from_millis(50)))
            {
                Ok(response) => {
                    ensure!(response.id == self.next_id, "worker crashed");
                    return Ok(response);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => bail!("worker crashed"),
            }
        }
    }

    pub fn kill(self) {
        drop(self);
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.requests.take();
        #[cfg(unix)]
        {
            // The child starts its own process group so uv and converters die together.
            unsafe {
                libc::kill(-(self.child.id() as i32), libc::SIGKILL);
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
