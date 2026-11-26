use anyhow::{Context, Result, anyhow, ensure};
use std::{
    collections::HashMap,
    env,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone)]
pub struct Config {
    pub home: PathBuf,
    pub groq_api_key: Option<String>,
    pub groq_model: String,
    pub min_similarity: f32,
    pub max_context_chunks: usize,
    pub ingest_cmd: Vec<String>,
    pub embed_cmd: Vec<String>,
    pub file_timeout: Duration,
    /// Also search each question translated between German and English.
    pub translate: bool,
    /// Opens code citations at their line: "zed" (default), "idea", "cursor" or "code".
    pub code_editor: String,
}

const REPO: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");

/// Cosine similarity below which the best match counts as "no relevant notes".
/// Measured on a real index: questions about the notes 0.40-0.66, general questions
/// at most 0.27.
pub const DEFAULT_MIN_SIMILARITY: f32 = 0.30;

/// The source checkout this binary was built from; only exists on the build machine.
const DEV_INGEST: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../ingest");

/// The Python helper project copied into a packaged app
/// (`Accio.app/Contents/Resources/ingest`, see `scripts/package-macos.sh`).
pub fn bundled_ingest_dir() -> Option<PathBuf> {
    let exe = env::current_exe().ok()?;
    let dir = exe.parent()?.parent()?.join("Resources/ingest");
    dir.join("pyproject.toml").is_file().then_some(dir)
}

/// `override_cmd` split on whitespace, else `uv run --project <ingest> accio-ingest <subcommand>`
/// with the helper project from `ACCIO_INGEST_DIR`, the app bundle, or the source checkout.
fn command(override_cmd: Option<String>, subcommand: &str) -> Vec<String> {
    if let Some(args) = override_cmd
        .map(|value| value.split_whitespace().map(str::to_owned).collect::<Vec<_>>())
        .filter(|args| !args.is_empty())
    {
        return args;
    }
    let bundled = env::var_os("ACCIO_INGEST_DIR")
        .is_none()
        .then(bundled_ingest_dir)
        .flatten();
    let project = env::var_os("ACCIO_INGEST_DIR")
        .map(PathBuf::from)
        .or_else(|| bundled.clone())
        .unwrap_or_else(|| PathBuf::from(DEV_INGEST));
    let mut command = vec!["uv".to_owned(), "run".to_owned()];
    if bundled.is_some() {
        // The bundle is read-only: use its lock file as is.
        command.push("--frozen".to_owned());
    }
    command.extend([
        "--project".to_owned(),
        project.to_string_lossy().into_owned(),
        "accio-ingest".to_owned(),
        subcommand.to_owned(),
    ]);
    command
}

/// Where uv keeps the helpers' Python environment for a packaged app: outside the
/// read-only bundle, and kept across app updates.
pub fn bundled_python_env() -> Option<PathBuf> {
    bundled_ingest_dir()?;
    let home = env::var_os("ACCIO_HOME").map(PathBuf::from).or_else(|| {
        dirs::home_dir().map(|path| path.join("Library/Application Support/accio"))
    })?;
    Some(home.join("python"))
}

/// A helper that could not start. A missing uv gets install instructions: a packaged
/// app needs it on the user's Mac.
pub(crate) fn spawn_error(what: &str, program: &str, error: std::io::Error) -> anyhow::Error {
    if error.kind() == std::io::ErrorKind::NotFound && program == "uv" {
        anyhow!(
            "{what}: uv is not installed. Accio runs its local models with uv; install it \
             (`brew install uv`, or see https://docs.astral.sh/uv/) and restart Accio"
        )
    } else {
        anyhow::Error::new(error).context(format!("{what} ({program})"))
    }
}

fn env_file(path: &Path) -> Result<HashMap<String, String>> {
    if !path.try_exists()? {
        return Ok(HashMap::new());
    }
    dotenvy::from_path_iter(path)?
        .collect::<Result<_, _>>()
        .with_context(|| format!("reading {}", path.display()))
}

impl Config {
    pub fn load() -> Result<Self> {
        let repo = env_file(&Path::new(REPO).join(".env"))?;
        let home = env::var_os("ACCIO_HOME")
            .map(PathBuf::from)
            .or_else(|| repo.get("ACCIO_HOME").map(PathBuf::from))
            .or_else(|| {
                dirs::home_dir().map(|path| path.join("Library/Application Support/accio"))
            })
            .ok_or_else(|| anyhow!("cannot determine ACCIO_HOME"))?;
        let local = env_file(&home.join(".env"))?;
        let values = |key: &str| {
            local
                .get(key)
                .cloned()
                .or_else(|| env::var(key).ok())
                .or_else(|| repo.get(key).cloned())
        };
        let mut config = Self::from_values(home, values)?;
        if let Some(caption) = values("ACCIO_CAPTION") {
            // Pass the dotenv override to the child without mutating global process state.
            config
                .ingest_cmd
                .splice(0..0, ["env".into(), format!("ACCIO_CAPTION={caption}")]);
        }
        Ok(config)
    }

    fn from_values(home: PathBuf, get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let min_similarity = get("ACCIO_MIN_SIMILARITY")
            .map(|value| value.parse::<f32>())
            .transpose()
            .context("invalid ACCIO_MIN_SIMILARITY")?
            .unwrap_or(DEFAULT_MIN_SIMILARITY);
        ensure!(
            (0.0..=1.0).contains(&min_similarity),
            "ACCIO_MIN_SIMILARITY must be between 0 and 1"
        );
        let embed_cmd = command(get("ACCIO_EMBED_CMD"), "embed-server");
        let ingest_cmd = command(get("ACCIO_INGEST_CMD"), "serve");
        Ok(Self {
            home,
            groq_api_key: get("GROQ_API_KEY").filter(|key| !key.trim().is_empty()),
            groq_model: get("GROQ_MODEL")
                .filter(|model| !model.trim().is_empty())
                .unwrap_or_else(|| "openai/gpt-oss-20b".into()),
            min_similarity,
            max_context_chunks: 8,
            ingest_cmd,
            embed_cmd,
            file_timeout: Duration::from_secs(300),
            translate: get("ACCIO_TRANSLATE").is_none_or(|value| value.trim() != "0"),
            code_editor: get("ACCIO_CODE_EDITOR")
                .map(|value| value.trim().to_lowercase())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "zed".into()),
        })
    }

    pub fn db_path(&self) -> PathBuf {
        self.home.join("index.sqlite")
    }
    pub fn models_dir(&self) -> PathBuf {
        self.home.join("models")
    }
    /// The local search socket used by `accio-mcp`.
    pub fn socket_path(&self) -> PathBuf {
        self.home.join(crate::socket::SOCKET_NAME)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let cfg = Config::from_values(temp.path().into(), |_| None)?;
        assert_eq!(cfg.db_path(), temp.path().join("index.sqlite"));
        assert_eq!(cfg.groq_model, "openai/gpt-oss-20b");
        assert_eq!(cfg.min_similarity, DEFAULT_MIN_SIMILARITY);
        assert_eq!(cfg.max_context_chunks, 8);
        assert_eq!(cfg.embed_cmd.last().unwrap(), "embed-server");
        assert_eq!(cfg.file_timeout.as_secs(), 300);
        assert_eq!(cfg.ingest_cmd[0], "uv");
        assert!(cfg.translate);
        assert_eq!(cfg.code_editor, "zed");
        let off = Config::from_values(temp.path().into(), |key| {
            (key == "ACCIO_TRANSLATE").then(|| "0".to_owned())
        })?;
        assert!(!off.translate);
        Ok(())
    }
}
