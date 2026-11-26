use anyhow::{Context, Result, ensure};
use half::f16;
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, params};
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub enum SourceStatus {
    Idle,
    Queued,
    Running,
    Failed(String),
    Cancelled,
}

impl SourceStatus {
    fn from_db(status: String) -> Self {
        match status.as_str() {
            "queued" => Self::Queued,
            "running" => Self::Running,
            "cancelled" => Self::Cancelled,
            "idle" => Self::Idle,
            _ => Self::Failed(status.strip_prefix("failed:").unwrap_or(&status).into()),
        }
    }

    fn to_db(&self) -> String {
        match self {
            Self::Idle => "idle".into(),
            Self::Queued => "queued".into(),
            Self::Running => "running".into(),
            Self::Cancelled => "cancelled".into(),
            Self::Failed(message) => format!("failed:{message}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub enum FileStatus {
    Ok,
    Failed,
}

/// How PDFs in a source are extracted (chosen per folder).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize)]
pub enum PdfMode {
    /// Plain text, ~7 ms/page. Tables become flat text.
    #[default]
    Text,
    /// pymupdf4llm markdown, ~125 ms/page. Keeps tables and headings.
    Markdown,
}

impl PdfMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Markdown => "markdown",
        }
    }
    fn from_db(value: &str) -> Self {
        if value == "markdown" {
            Self::Markdown
        } else {
            Self::Text
        }
    }
}

/// Notes take part in normal questions; a code repository is searched only when asked
/// for explicitly (`@repo` in the panel, or `code_search` from an agent).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize)]
pub enum SourceKind {
    #[default]
    Notes,
    Code,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Notes => "notes",
            Self::Code => "code",
        }
    }
    fn from_db(value: &str) -> Self {
        if value == "code" { Self::Code } else { Self::Notes }
    }
}

/// Which chunks a search sees.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scope {
    /// Enabled note sources (the panel's default).
    Notes,
    /// One source, enabled or not (a code repository).
    Source(i64),
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Source {
    pub kind: SourceKind,
    pub enabled: bool,
    pub captions: Option<bool>,
    pub is_file: bool,
    pub size: i64,
    pub id: i64,
    pub path: PathBuf,
    pub ignore_globs: Vec<String>,
    pub pdf_mode: PdfMode,
    pub last_ingested_at: Option<i64>,
    pub status: SourceStatus,
    pub file_count: i64,
    pub failed_count: i64,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct FileRow {
    pub id: i64,
    pub source_id: i64,
    pub path: PathBuf,
    pub mtime: i64,
    pub size: i64,
    pub hash: String,
    pub status: FileStatus,
    pub error: Option<String>,
    pub ingested_at: i64,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct NewChunk {
    pub header: String,
    pub text: String,
    pub page: Option<u32>,
    /// First and last source line (code chunks).
    pub line: Option<u32>,
    pub end_line: Option<u32>,
    pub vec: Vec<f32>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ChunkRow {
    pub id: i64,
    pub file_id: i64,
    pub path: PathBuf,
    pub header: String,
    pub text: String,
    pub page: Option<u32>,
    pub line: Option<u32>,
    pub end_line: Option<u32>,
    pub file_ingested_at: i64,
}

#[derive(serde::Serialize)]
pub struct Profile {
    pub id: i64,
    pub name: String,
    pub sources: Vec<i64>,
}

pub struct Store {
    conn: Mutex<Connection>,
    path: PathBuf,
    dim: usize,
    has_source_scope: bool,
    /// Older read-only indexes (eval against a copy) may predate code sources.
    has_kind: bool,
    has_lines: bool,
}

/// SQL pieces that restrict a query to a `Scope` on a given index.
#[derive(Clone, Debug)]
pub(crate) struct ScopeSql {
    /// Condition on `sources s`.
    pub filter: String,
    /// Columns for chunk line numbers (`NULL,NULL` on old indexes).
    pub lines: &'static str,
}

const SOURCES: &str = "SELECT s.id, s.path, s.ignore_globs, s.last_ingested_at, s.status,
    count(f.id), count(CASE WHEN f.status='failed' THEN 1 END), s.pdf_mode, s.enabled, s.is_file, coalesce(sum(f.size), 0), s.captions, s.kind
    FROM sources s LEFT JOIN files f ON f.source_id=s.id";

fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?) WHERE name=?)",
        [table, column],
        |row| row.get(0),
    )?)
}

fn source_column_exists(conn: &Connection, column: &str) -> Result<bool> {
    column_exists(conn, "sources", column)
}

fn model_matches(conn: &Connection, embed_model: &str, dim: usize) -> Result<bool> {
    let mut meta = conn.prepare_cached("SELECT value FROM meta WHERE key=?")?;
    let model: Option<String> = meta.query_row(["embed_model"], |r| r.get(0)).optional()?;
    let saved_dim: Option<String> = meta.query_row(["dim"], |r| r.get(0)).optional()?;
    Ok(model.as_deref() == Some(embed_model) && saved_dim == Some(dim.to_string()))
}

pub(crate) fn timestamp(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(delta) => delta.as_nanos().min(i64::MAX as u128) as i64,
        Err(delta) => -(delta.duration().as_nanos().min(i64::MAX as u128) as i64),
    }
}

fn db_path(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("path is not UTF-8: {}", path.display()))
}

fn source_row(row: &Row<'_>) -> rusqlite::Result<Source> {
    let globs: String = row.get(2)?;
    let ignore_globs = serde_json::from_str(&globs).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(err))
    })?;
    Ok(Source {
        kind: SourceKind::from_db(&row.get::<_, String>(12)?),
        enabled: row.get(8)?,
        is_file: row.get(9)?,
        size: row.get(10)?,
        captions: row.get(11)?,
        id: row.get(0)?,
        path: PathBuf::from(row.get::<_, String>(1)?),
        ignore_globs,
        last_ingested_at: row.get(3)?,
        status: SourceStatus::from_db(row.get(4)?),
        file_count: row.get(5)?,
        failed_count: row.get(6)?,
        pdf_mode: PdfMode::from_db(&row.get::<_, String>(7)?),
    })
}

fn file_row(row: &Row<'_>) -> rusqlite::Result<FileRow> {
    Ok(FileRow {
        id: row.get(0)?,
        source_id: row.get(1)?,
        path: PathBuf::from(row.get::<_, String>(2)?),
        mtime: row.get(3)?,
        size: row.get(4)?,
        hash: row.get(5)?,
        status: if row.get::<_, String>(6)? == "ok" {
            FileStatus::Ok
        } else {
            FileStatus::Failed
        },
        error: row.get(7)?,
        ingested_at: row.get(8)?,
    })
}

impl Store {
    pub fn open(path: &Path, embed_model: &str, dim: usize) -> Result<Self> {
        ensure!(dim > 0, "embedding dimension must be positive");
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.execute_batch(include_str!("schema.sql"))?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for (column, definition) in [
            ("pdf_mode", "TEXT NOT NULL DEFAULT 'text'"),
            ("enabled", "INTEGER NOT NULL DEFAULT 1"),
            ("is_file", "INTEGER NOT NULL DEFAULT 0"),
            ("captions", "INTEGER"),
            ("kind", "TEXT NOT NULL DEFAULT 'notes'"),
        ] {
            if !source_column_exists(&tx, column)? {
                tx.execute_batch(&format!(
                    "ALTER TABLE sources ADD COLUMN {column} {definition}"
                ))?;
            }
        }
        for column in ["line", "end_line"] {
            if !column_exists(&tx, "chunks", column)? {
                tx.execute_batch(&format!("ALTER TABLE chunks ADD COLUMN {column} INTEGER"))?;
            }
        }
        if !model_matches(&tx, embed_model, dim)? {
            tx.execute_batch(
                "DELETE FROM files;
                UPDATE sources SET last_ingested_at=NULL, status='idle';",
            )?;
            tx.execute(
                "INSERT OR REPLACE INTO meta VALUES ('embed_model', ?), ('dim', ?)",
                params![embed_model, dim.to_string()],
            )?;
        }
        tx.execute(
            "UPDATE sources SET status='cancelled' WHERE status IN ('queued', 'running')",
            [],
        )?;
        tx.commit()?;
        Ok(Self {
            conn: Mutex::new(conn),
            path: path.into(),
            dim,
            has_source_scope: true,
            has_kind: true,
            has_lines: true,
        })
    }

    pub(crate) fn open_existing(path: &Path, embed_model: &str, dim: usize) -> Result<Self> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("opening index {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        ensure!(
            model_matches(&conn, embed_model, dim)?,
            "index uses a different embedding model or dimension"
        );
        let has_source_scope = source_column_exists(&conn, "enabled")?;
        let has_kind = source_column_exists(&conn, "kind")?;
        let has_lines = column_exists(&conn, "chunks", "line")?;
        Ok(Self {
            conn: Mutex::new(conn),
            path: path.into(),
            dim,
            has_source_scope,
            has_kind,
            has_lines,
        })
    }

    pub(crate) fn source_volume(&self, id: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT identity FROM source_volumes WHERE source_id=?",
                [id],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub(crate) fn remember_source_volume(&self, id: i64, identity: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO source_volumes(source_id, identity) VALUES (?,?) ON CONFLICT DO NOTHING",
            params![id, identity],
        )?;
        Ok(())
    }

    pub fn add_source(&self, path: &Path, ignore_globs: &[String]) -> Result<i64> {
        self.add_source_of_kind(path, ignore_globs, SourceKind::Notes)
    }

    /// Adds a code repository (searched only on request, never in note answers).
    /// Fails if the folder is already a note source.
    pub fn add_code_source(&self, path: &Path, ignore_globs: &[String]) -> Result<i64> {
        self.add_source_of_kind(path, ignore_globs, SourceKind::Code)
    }

    fn add_source_of_kind(
        &self,
        path: &Path,
        ignore_globs: &[String],
        kind: SourceKind,
    ) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_owned());
        let path = db_path(&canonical)?;
        conn.execute(
            "INSERT INTO sources(path, ignore_globs, is_file, kind) VALUES (?, ?, ?, ?)
            ON CONFLICT(path) DO NOTHING",
            params![
                path,
                serde_json::to_string(ignore_globs)?,
                Path::new(path).is_file(),
                kind.as_str()
            ],
        )?;
        let (id, saved): (i64, String) = conn.query_row(
            "SELECT id, kind FROM sources WHERE path=?",
            [path],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        ensure!(
            SourceKind::from_db(&saved) == kind,
            "{path} is already added as a {} source",
            if kind == SourceKind::Code { "notes" } else { "code" }
        );
        Ok(id)
    }

    /// Sets the PDF extraction mode. When it changes, the source's PDF file rows (and
    /// their chunks) are dropped so the next ingest re-parses exactly those files.
    /// Returns whether the mode changed.
    pub fn set_pdf_mode(&self, id: i64, mode: PdfMode) -> Result<bool> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let changed = tx.execute(
            "UPDATE sources SET pdf_mode=?1 WHERE id=?2 AND pdf_mode!=?1",
            params![mode.as_str(), id],
        )? != 0;
        if changed {
            tx.execute(
                "DELETE FROM files WHERE source_id=? AND lower(path) LIKE '%.pdf'",
                [id],
            )?;
        }
        tx.commit()?;
        Ok(changed)
    }

    pub fn remove_source(&self, id: i64) -> Result<()> {
        self.conn
            .lock()
            .unwrap()
            .execute("DELETE FROM sources WHERE id=?", [id])?;
        Ok(())
    }

    pub fn set_source_captions(&self, id: i64, enabled: bool) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE sources SET captions=? WHERE id=?",
            params![enabled, id],
        )?;
        Ok(())
    }

    pub fn set_folder_excluded(&self, id: i64, relative: &Path, excluded: bool) -> Result<()> {
        let glob = crate::scan::literal_glob(db_path(relative)?);
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let saved: String =
            tx.query_row("SELECT ignore_globs FROM sources WHERE id=?", [id], |row| {
                row.get(0)
            })?;
        let mut globs: Vec<String> = serde_json::from_str(&saved)?;
        globs.retain(|value| value != &glob);
        if excluded {
            globs.push(glob);
        }
        tx.execute(
            "UPDATE sources SET ignore_globs=? WHERE id=?",
            params![serde_json::to_string(&globs)?, id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn set_source_enabled(&self, id: i64, enabled: bool) -> Result<()> {
        let changed = self.conn.lock().unwrap().execute(
            "UPDATE sources SET enabled=? WHERE id=?",
            params![enabled, id],
        )?;
        ensure!(changed != 0, "source {id} not found");
        Ok(())
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT value FROM settings WHERE key=?", [key], |row| {
                row.get(0)
            })
            .optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO settings VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn profiles(&self) -> Result<Vec<Profile>> {
        let conn = self.conn.lock().unwrap();
        let mut list = conn.prepare("SELECT id,name FROM profiles ORDER BY name")?;
        let mut members = conn.prepare(
            "SELECT source_id FROM profile_sources WHERE profile_id=? ORDER BY source_id",
        )?;
        let mut profiles = Vec::new();
        for row in list.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get(1)?)))? {
            let (id, name) = row?;
            let sources = members
                .query_map([id], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            profiles.push(Profile { id, name, sources });
        }
        Ok(profiles)
    }

    pub fn save_profile(&self, id: Option<i64>, name: &str, sources: &[i64]) -> Result<()> {
        let name = name.trim();
        ensure!(
            !name.is_empty() && name.chars().count() <= 60,
            "enter a profile name of 1–60 characters"
        );
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let id = if let Some(id) = id {
            ensure!(
                tx.execute("UPDATE profiles SET name=? WHERE id=?", params![name, id])? != 0,
                "profile not found"
            );
            tx.execute("DELETE FROM profile_sources WHERE profile_id=?", [id])?;
            id
        } else {
            tx.execute("INSERT INTO profiles(name) VALUES (?)", [name])?;
            tx.last_insert_rowid()
        };
        for source in sources {
            tx.execute(
                "INSERT OR IGNORE INTO profile_sources VALUES (?,?)",
                params![id, source],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn delete_profile(&self, id: i64) -> Result<()> {
        self.conn
            .lock()
            .unwrap()
            .execute("DELETE FROM profiles WHERE id=?", [id])?;
        Ok(())
    }

    pub fn apply_profile(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM profiles WHERE id=?)",
            [id],
            |row| row.get(0),
        )?;
        ensure!(exists, "profile not found");
        conn.execute(
            "UPDATE sources SET enabled=EXISTS(SELECT 1 FROM profile_sources WHERE profile_id=? AND source_id=sources.id)",
            [id],
        )?;
        Ok(())
    }

    pub fn clear_index(&self) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute_batch(
            "DELETE FROM files; UPDATE sources SET last_ingested_at=NULL, status='idle';",
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn list_sources(&self) -> Result<Vec<Source>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(&format!("{SOURCES} GROUP BY s.id ORDER BY s.id"))?;
        Ok(stmt.query_map([], source_row)?.collect::<Result<_, _>>()?)
    }

    pub fn get_source(&self, id: i64) -> Result<Source> {
        Ok(self.conn.lock().unwrap().query_row(
            &format!("{SOURCES} WHERE s.id=? GROUP BY s.id"),
            [id],
            source_row,
        )?)
    }

    pub fn set_source_status(&self, id: i64, status: &SourceStatus, finished: bool) -> Result<()> {
        let changed = self.conn.lock().unwrap().execute(
            "UPDATE sources SET status=?, last_ingested_at=coalesce(?, last_ingested_at) WHERE id=?",
            params![status.to_db(), finished.then(|| timestamp(SystemTime::now()) / 1_000_000_000), id],
        )?;
        ensure!(changed != 0, "source {id} not found");
        Ok(())
    }

    pub fn files_for_source(&self, id: i64) -> Result<Vec<FileRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached("SELECT * FROM files WHERE source_id=? ORDER BY id")?;
        Ok(stmt.query_map([id], file_row)?.collect::<Result<_, _>>()?)
    }

    pub fn failed_files(&self, source_id: i64, limit: usize) -> Result<Vec<FileRow>> {
        let limit = i64::try_from(limit)?;
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT * FROM files WHERE source_id=? AND status='failed' ORDER BY path LIMIT ?",
        )?;
        Ok(stmt
            .query_map(params![source_id, limit], file_row)?
            .collect::<Result<_, _>>()?)
    }

    pub fn owned_by_other(&self, source_id: i64, path: &Path) -> Result<bool> {
        Ok(self.conn.lock().unwrap().query_row(
            "SELECT EXISTS(SELECT 1 FROM files WHERE path=? AND source_id!=?)",
            params![db_path(path)?, source_id],
            |r| r.get(0),
        )?)
    }

    pub fn replace_file(
        &self,
        source_id: i64,
        path: &Path,
        mtime: i64,
        size: i64,
        hash: &str,
        chunks: &[NewChunk],
    ) -> Result<()> {
        self.write_file(source_id, path, mtime, size, hash, Ok(chunks))
    }

    pub fn mark_failed(
        &self,
        source_id: i64,
        path: &Path,
        mtime: i64,
        size: i64,
        hash: &str,
        error: &str,
    ) -> Result<()> {
        self.write_file(source_id, path, mtime, size, hash, Err(error))
    }

    fn write_file(
        &self,
        source_id: i64,
        path: &Path,
        mtime: i64,
        size: i64,
        hash: &str,
        result: Result<&[NewChunk], &str>,
    ) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let status = if result.is_ok() { "ok" } else { "failed" };
        let id: i64 = tx
            .query_row(
                "INSERT INTO files(source_id,path,mtime,size,hash,status,error,ingested_at)
             VALUES (?,?,?,?,?,?,?,?) ON CONFLICT(path) DO UPDATE SET mtime=excluded.mtime,
             size=excluded.size, hash=excluded.hash, status=excluded.status,
             error=excluded.error, ingested_at=excluded.ingested_at
             WHERE source_id=excluded.source_id RETURNING id",
                params![
                    source_id,
                    db_path(path)?,
                    mtime,
                    size,
                    hash,
                    status,
                    result.err(),
                    timestamp(SystemTime::now())
                ],
                |r| r.get(0),
            )
            .context("writing file index")?;
        tx.execute("DELETE FROM chunks WHERE file_id=?", [id])?;
        if let Ok(chunks) = result {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO chunks(file_id,header,text,page,vec,line,end_line) VALUES (?,?,?,?,?,?,?)",
            )?;
            for chunk in chunks {
                ensure!(
                    chunk.vec.len() == self.dim,
                    "invalid embedding dimension for {}",
                    path.display()
                );
                let blob: Vec<_> = chunk
                    .vec
                    .iter()
                    .flat_map(|&value| f16::from_f32(value).to_le_bytes())
                    .collect();
                stmt.execute(params![
                    id,
                    chunk.header,
                    chunk.text,
                    chunk.page,
                    blob,
                    chunk.line,
                    chunk.end_line
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn touch_file(&self, id: i64, mtime: i64, size: i64) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE files SET mtime=?, size=?, ingested_at=? WHERE id=?",
            params![mtime, size, timestamp(SystemTime::now()), id],
        )?;
        Ok(())
    }

    pub fn delete_files(&self, ids: &[i64]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached("DELETE FROM files WHERE id=?")?;
            for id in ids {
                stmt.execute([id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn all_vectors(&self) -> Result<(Vec<i64>, Vec<f16>)> {
        all_vectors(&self.conn.lock().unwrap(), self.dim, None)
    }

    pub fn fts_search(&self, query: &str, k: usize) -> Result<Vec<i64>> {
        fts_search(&self.conn.lock().unwrap(), query, k, &self.scope_sql(Scope::Notes))
    }

    pub fn chunks_by_ids(&self, ids: &[i64]) -> Result<Vec<ChunkRow>> {
        chunks_by_ids(&self.conn.lock().unwrap(), ids, &self.scope_sql(Scope::Notes))
    }

    /// SQL restricting queries to `scope` on this index.
    pub(crate) fn scope_sql(&self, scope: Scope) -> ScopeSql {
        let filter = match scope {
            Scope::Source(id) => format!("s.id={id}"),
            Scope::Notes if !self.has_source_scope => "1=1".into(),
            Scope::Notes if !self.has_kind => "s.enabled=1".into(),
            Scope::Notes => "s.enabled=1 AND s.kind='notes'".into(),
        };
        ScopeSql {
            filter,
            lines: if self.has_lines { "c.line,c.end_line" } else { "NULL,NULL" },
        }
    }

    /// Keep FTS, citation rows and published vectors on the same WAL read transaction.
    pub(crate) fn snapshot(&self, scope: Scope) -> Result<(Connection, Vec<i64>, Vec<f16>)> {
        let conn = Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch("BEGIN")?;
        let (ids, vectors) = all_vectors(&conn, self.dim, Some(&self.scope_sql(scope).filter))?;
        Ok((conn, ids, vectors))
    }
}

fn all_vectors(conn: &Connection, dim: usize, filter: Option<&str>) -> Result<(Vec<i64>, Vec<f16>)> {
    let sql = match filter {
        Some(filter) => format!(
            "SELECT c.id,c.vec FROM chunks c JOIN files f ON c.file_id=f.id
             JOIN sources s ON f.source_id=s.id WHERE {filter} ORDER BY c.id"
        ),
        None => "SELECT id,vec FROM chunks ORDER BY id".into(),
    };
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;
    let mut ids = Vec::new();
    let mut vectors = Vec::new();
    while let Some(row) = rows.next()? {
        let blob = row.get_ref(1)?.as_blob()?;
        ensure!(blob.len() == dim * 2, "invalid embedding blob length");
        ids.push(row.get(0)?);
        vectors.extend(
            blob.chunks_exact(2)
                .map(|bytes| f16::from_le_bytes([bytes[0], bytes[1]])),
        );
    }
    Ok((ids, vectors))
}

pub(crate) fn fts_search(
    conn: &Connection,
    query: &str,
    k: usize,
    scope: &ScopeSql,
) -> Result<Vec<i64>> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let k = i64::try_from(k)?;
    let sql = format!(
        "SELECT chunks_fts.rowid FROM chunks_fts JOIN chunks c ON c.id=chunks_fts.rowid
         JOIN files f ON f.id=c.file_id JOIN sources s ON s.id=f.source_id
         WHERE chunks_fts MATCH ? AND {} ORDER BY bm25(chunks_fts), chunks_fts.rowid LIMIT ?",
        scope.filter
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    Ok(stmt
        .query_map(params![query, k], |row| row.get(0))?
        .collect::<Result<_, _>>()?)
}

pub(crate) fn chunks_by_ids(
    conn: &Connection,
    ids: &[i64],
    scope: &ScopeSql,
) -> Result<Vec<ChunkRow>> {
    let sql = format!(
        "SELECT c.id,c.file_id,f.path,c.header,c.text,c.page,f.ingested_at,{} FROM chunks c
         JOIN files f ON f.id=c.file_id JOIN sources s ON s.id=f.source_id WHERE c.id=? AND {}",
        scope.lines, scope.filter
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let mut chunks = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(chunk) = stmt
            .query_row([id], |r| {
                Ok(ChunkRow {
                    id: r.get(0)?,
                    file_id: r.get(1)?,
                    path: PathBuf::from(r.get::<_, String>(2)?),
                    header: r.get(3)?,
                    text: r.get(4)?,
                    page: r.get(5)?,
                    file_ingested_at: r.get(6)?,
                    line: r.get(7)?,
                    end_line: r.get(8)?,
                })
            })
            .optional()?
        {
            chunks.push(chunk);
        }
    }
    Ok(chunks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(store: &Store, source: i64, text: &str) -> Result<()> {
        let chunks = [NewChunk {
            header: "Plan › Kosten".into(),
            text: text.into(),
            page: Some(3),
            line: None,
            end_line: None,
            vec: vec![1.0, 0.0, -0.25, 0.0],
        }];
        store.replace_file(source, Path::new("/one/test.pdf"), 1, 2, "hash", &chunks)
    }

    #[test]
    fn replace_file_roundtrip_and_fts_cascades() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = Store::open(&root.path().join("index.sqlite"), "test", 4)?;
        let source = store.add_source(Path::new("/one"), &["ignore".into()])?;
        let other = store.add_source(Path::new("/"), &[])?;
        put(&store, source, "Gebührenänderung 2026")?;
        let (ids, vectors) = store.all_vectors()?;
        assert_eq!(ids.len(), 1);
        assert_eq!(vectors, [1.0, 0.0, -0.25, 0.0].map(f16::from_f32));
        let chunks = store.chunks_by_ids(&ids)?;
        assert_eq!(chunks[0].page, Some(3));
        assert_eq!(chunks[0].header, "Plan › Kosten");
        assert_eq!(chunks[0].text, "Gebührenänderung 2026");
        assert_eq!(store.fts_search("\"Gebuhrenanderung\"", 10)?, ids);
        assert!(store.owned_by_other(other, &chunks[0].path)?);
        assert!(!store.owned_by_other(source, &chunks[0].path)?);
        assert_eq!(store.get_source(source)?.ignore_globs, ["ignore"]);
        put(&store, source, "Kaltmiete")?;
        assert!(store.fts_search("\"Gebuhrenanderung\"", 10)?.is_empty());
        assert_eq!(store.fts_search("\"Kaltmiete\"", 10)?.len(), 1);
        store.remove_source(source)?;
        assert!(store.all_vectors()?.0.is_empty());
        assert!(store.files_for_source(source)?.is_empty());
        assert!(store.fts_search("\"Kaltmiete\"", 10)?.is_empty());
        Ok(())
    }

    #[test]
    fn pdf_mode_defaults_to_text_and_switch_requeues_only_pdfs() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = Store::open(&root.path().join("index.sqlite"), "test", 4)?;
        let source = store.add_source(Path::new("/one"), &[])?;
        assert_eq!(store.get_source(source)?.pdf_mode, PdfMode::Text);
        put(&store, source, "pdf text")?;
        let note = [NewChunk {
            header: "n".into(),
            text: "note".into(),
            page: None,
            line: None,
            end_line: None,
            vec: vec![0.0, 1.0, 0.0, 0.0],
        }];
        store.replace_file(source, Path::new("/one/n.MD"), 1, 2, "h", &note)?;
        store.replace_file(source, Path::new("/one/Upper.PDF"), 1, 2, "h", &note)?;

        assert!(!store.set_pdf_mode(source, PdfMode::Text)?); // unchanged: nothing re-queued
        assert_eq!(store.files_for_source(source)?.len(), 3);

        assert!(store.set_pdf_mode(source, PdfMode::Markdown)?);
        assert_eq!(store.get_source(source)?.pdf_mode, PdfMode::Markdown);
        let left: Vec<_> = store
            .files_for_source(source)?
            .into_iter()
            .map(|f| f.path)
            .collect();
        assert_eq!(left, [PathBuf::from("/one/n.MD")]);
        assert!(store.fts_search("\"pdf\"", 10)?.is_empty());
        Ok(())
    }

    #[test]
    fn pdf_mode_column_added_to_existing_database() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("index.sqlite");
        {
            let conn = Connection::open(&path)?;
            conn.execute_batch(
                "CREATE TABLE sources (id INTEGER PRIMARY KEY AUTOINCREMENT, path TEXT NOT NULL UNIQUE,
                 ignore_globs TEXT NOT NULL, last_ingested_at INTEGER, status TEXT NOT NULL DEFAULT 'idle');
                 INSERT INTO sources(path, ignore_globs) VALUES ('/old', '[]');",
            )?;
        }
        let store = Store::open(&path, "test", 4)?;
        let source = &store.list_sources()?[0];
        assert_eq!(source.pdf_mode, PdfMode::Text);
        assert!(source.enabled);
        assert!(!source.is_file);
        Ok(())
    }

    #[test]
    fn meta_mismatch_clears() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("index.sqlite");
        let source = {
            let store = Store::open(&path, "test", 4)?;
            let source = store.add_source(Path::new("/one"), &[])?;
            put(&store, source, "test")?;
            source
        };
        let store = Store::open(&path, "test", 8)?;
        assert!(store.files_for_source(source)?.is_empty());
        assert_eq!(store.get_source(source)?.file_count, 0);
        assert!(store.all_vectors()?.0.is_empty());
        Ok(())
    }

    #[test]
    fn opening_existing_index_preserves_running_status() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("index.sqlite");
        let writer = Store::open(&path, "test", 4)?;
        let id = writer.add_source(Path::new("/one"), &[])?;
        writer.set_source_status(id, &SourceStatus::Running, false)?;
        let reader = Store::open_existing(&path, "test", 4)?;
        assert_eq!(reader.get_source(id)?.status, SourceStatus::Running);
        assert!(Store::open_existing(&path, "different", 4).is_err());
        assert_eq!(writer.get_source(id)?.status, SourceStatus::Running);
        Ok(())
    }
}
