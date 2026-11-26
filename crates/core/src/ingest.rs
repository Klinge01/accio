use crate::{
    engine::Engine,
    scan::{ScanEntry, diff, hash_file, scan, source_volume},
    store::{NewChunk, SourceKind, SourceStatus},
    worker::{ParsedChunk, Worker},
};
use anyhow::{Context, Result, anyhow, ensure};
use serde::Serialize;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum IngestEvent {
    Progress {
        source_id: i64,
        done: usize,
        total: usize,
        current: String,
    },
    Finished {
        source_id: i64,
        parsed: usize,
        failed: usize,
        deleted: usize,
    },
    Failed {
        source_id: i64,
        error: String,
    },
    Cancelled {
        source_id: i64,
    },
}

/// Chunks per embedding call. Measured on an M4: 16 beats 32 for code-sized chunks.
const EMBED_BATCH: usize = 16;
/// Chunks collected across parsed files before embedding them together.
const EMBED_GROUP: usize = 64;

/// A parsed file waiting for embedding: (position, file, content hash, chunks or error).
type ParsedFile = (usize, ScanEntry, String, Result<Vec<ParsedChunk>, String>);
type EmbeddedFile = (usize, ScanEntry, String, Result<Vec<NewChunk>, String>);

struct Job {
    source_id: i64,
    force: bool,
    cancelled: AtomicBool,
}

pub struct IngestManager {
    engine: Arc<Engine>,
    active: Mutex<HashMap<i64, Arc<Job>>>,
    operation: Mutex<()>,
    queue: mpsc::Sender<Arc<Job>>,
    hash_worker_cmd: Option<Vec<String>>,
    stopping: AtomicBool,
    on_event: Arc<dyn Fn(IngestEvent) + Send + Sync>,
}

impl IngestManager {
    pub fn new(engine: Arc<Engine>, on_event: Arc<dyn Fn(IngestEvent) + Send + Sync>) -> Arc<Self> {
        Self::with_hash_worker(engine, on_event, None)
    }

    pub fn with_hash_worker(
        engine: Arc<Engine>,
        on_event: Arc<dyn Fn(IngestEvent) + Send + Sync>,
        helper: Option<PathBuf>,
    ) -> Arc<Self> {
        let (queue, incoming) = mpsc::channel();
        let manager = Arc::new(Self {
            engine,
            active: Mutex::new(HashMap::new()),
            operation: Mutex::new(()),
            queue,
            hash_worker_cmd: helper.map(|path| {
                vec![
                    path.to_string_lossy().into_owned(),
                    "--accio-hash-worker".into(),
                ]
            }),
            stopping: AtomicBool::new(false),
            on_event,
        });
        let reference = Arc::downgrade(&manager);
        thread::spawn(move || {
            for job in incoming {
                let Some(manager) = reference.upgrade() else {
                    break;
                };
                manager.process(&job);
            }
        });
        manager
    }

    pub fn start(&self, source_id: i64, force: bool) {
        let mut active = self.active.lock().unwrap();
        if self.stopping.load(Ordering::Relaxed) || active.contains_key(&source_id) {
            return;
        }
        let result = self
            .engine
            .store()
            .set_source_status(source_id, &SourceStatus::Queued, false);
        let job = Arc::new(Job {
            source_id,
            force,
            cancelled: AtomicBool::new(false),
        });
        match result {
            Ok(()) => {
                active.insert(source_id, job.clone());
                // The receiver lives as long as this manager and its Sender.
                let _ = self.queue.send(job);
            }
            Err(error) => {
                drop(active);
                (self.on_event)(IngestEvent::Failed {
                    source_id,
                    error: format!("{error:#}"),
                });
            }
        }
    }

    pub fn cancel(&self, source_id: i64) {
        let mut active = self.active.lock().unwrap();
        let Some(job) = active.get(&source_id) else {
            return;
        };
        job.cancelled.store(true, Ordering::Relaxed);
        if self
            .engine
            .store()
            .get_source(source_id)
            .is_ok_and(|source| source.status == SourceStatus::Queued)
            && self
                .engine
                .store()
                .set_source_status(source_id, &SourceStatus::Cancelled, false)
                .is_ok()
        {
            active.remove(&source_id);
            drop(active);
            (self.on_event)(IngestEvent::Cancelled { source_id });
        }
    }

    pub fn is_active(&self, source_id: i64) -> bool {
        self.active.lock().unwrap().contains_key(&source_id)
    }

    pub fn shutdown(&self) {
        let ids: Vec<_> = {
            let active = self.active.lock().unwrap();
            self.stopping.store(true, Ordering::Relaxed);
            active.keys().copied().collect()
        };
        for id in ids {
            self.cancel(id);
        }
        // The operation guard stays held until parsing and worker cleanup have returned.
        let _operation = self.operation.lock().unwrap();
    }

    pub fn remove_source(&self, source_id: i64) -> Result<()> {
        self.cancel(source_id);
        let _operation = self.operation.lock().unwrap();
        let active = self.active.lock().unwrap();
        ensure!(
            !active.contains_key(&source_id),
            "ingestion started again while removing this source"
        );
        self.engine.store().remove_source(source_id)?;
        drop(active);
        self.engine.reload_index()
    }

    pub fn clear_index(&self) -> Result<()> {
        let _operation = self
            .operation
            .try_lock()
            .map_err(|_| anyhow::anyhow!("wait for ingestion to finish"))?;
        let _active = self.active.lock().unwrap();
        ensure!(_active.is_empty(), "wait for ingestion to finish");
        self.engine.store().clear_index()?;
        self.engine.reload_index()
    }

    fn process(&self, job: &Arc<Job>) {
        let source_id = job.source_id;
        let operation = self.operation.lock().unwrap();
        let start = {
            let active = self.active.lock().unwrap();
            if !active
                .get(&source_id)
                .is_some_and(|current| Arc::ptr_eq(current, job))
            {
                return;
            }
            self.engine
                .store()
                .set_source_status(source_id, &SourceStatus::Running, false)
        };
        let result = start.and_then(|()| self.run(job));
        if result.is_err() {
            // Preserve any files already committed if a job fails late.
            let _ = self.engine.reload_index();
        }
        let store = self.engine.store();
        let mut active = self.active.lock().unwrap();
        let (event, status, finished) = match result {
            Ok(_) if job.cancelled.load(Ordering::Relaxed) => (
                IngestEvent::Cancelled { source_id },
                SourceStatus::Cancelled,
                false,
            ),
            Ok((parsed, failed, deleted)) => (
                IngestEvent::Finished {
                    source_id,
                    parsed,
                    failed,
                    deleted,
                },
                SourceStatus::Idle,
                true,
            ),
            Err(error) => {
                let error = format!("{error:#}");
                (
                    IngestEvent::Failed {
                        source_id,
                        error: error.clone(),
                    },
                    SourceStatus::Failed(error),
                    false,
                )
            }
        };
        let event = match store.set_source_status(source_id, &status, finished) {
            Ok(()) => event,
            Err(error) => IngestEvent::Failed {
                source_id,
                error: format!("{error:#}"),
            },
        };
        active.remove(&source_id);
        drop(active);
        drop(operation);
        (self.on_event)(event);
    }

    fn run(&self, job: &Job) -> Result<(usize, usize, usize)> {
        let store = self.engine.store();
        let source = store.get_source(job.source_id)?;
        let config = self.engine.config();
        let cancelled = || job.cancelled.load(Ordering::Relaxed);
        let kind = if source.is_file { "file" } else { "folder" };
        let volume = source_volume(&source.path, source.is_file)?;
        let saved_volume = store.source_volume(source.id)?;
        ensure!(
            saved_volume.as_ref().is_none_or(|saved| saved == &volume),
            "{kind} not found: {} (original volume is unavailable)",
            source.path.display()
        );

        let mut candidates = Vec::new();
        for entry in scan(&source.path, &source.ignore_globs, source.kind)? {
            if cancelled() {
                return Ok((0, 0, 0));
            }
            if store.owned_by_other(source.id, &entry.path)? {
                ensure!(
                    !source.is_file,
                    "{} is already indexed by another source",
                    entry.path.display()
                );
            } else {
                candidates.push(entry);
            }
        }
        let known = store.files_for_source(source.id)?;
        let hash_errors = std::cell::RefCell::new(HashMap::new());
        let hash_worker = std::cell::RefCell::new(None);
        let plan = diff(&candidates, &known, job.force, &|path| {
            if cancelled() {
                return Ok(String::new());
            }
            let result = self.hash(
                path,
                config.file_timeout,
                cancelled,
                &mut hash_worker.borrow_mut(),
            );
            match result {
                Ok(hash) => Ok(hash),
                Err(error) => {
                    hash_errors
                        .borrow_mut()
                        .insert(path.to_owned(), format!("cannot hash file: {error:#}"));
                    Ok(String::new())
                }
            }
        })?;
        drop(hash_worker);
        if cancelled() {
            return Ok((0, 0, 0));
        }

        // Check the root again before committing deletions. It can disappear during the walk.
        ensure!(
            source_volume(&source.path, source.is_file)? == volume,
            "folder not found: {}",
            source.path.display()
        );
        store.remember_source_volume(source.id, &volume)?;
        store.delete_files(&plan.delete)?;
        for (id, entry) in plan.touch {
            store.touch_file(id, entry.mtime, entry.size)?;
        }

        let mut hash_errors = hash_errors.into_inner();
        let mut parsed = 0;
        let mut failed = 0;
        let total = plan.parse.len();
        (self.on_event)(IngestEvent::Progress {
            source_id: source.id,
            done: 0,
            total,
            current: String::new(),
        });
        let to_parse = plan.parse;
        let pdf_mode = source.pdf_mode.as_str();
        let mut ingest_cmd = config.ingest_cmd.clone();
        if let Some(captions) = source.captions {
            let assignment = format!("ACCIO_CAPTION={}", u8::from(captions));
            if ingest_cmd.first().is_some_and(|arg| arg == "env") {
                ingest_cmd.retain(|arg| !arg.starts_with("ACCIO_CAPTION="));
                ingest_cmd.insert(1, assignment);
            } else {
                ingest_cmd.splice(0..0, ["env".into(), assignment]);
            }
        }
        // Pipeline: the parser thread (Python worker: text extraction, OCR on CPU/ANE)
        // runs ahead while this thread embeds on the GPU and writes. Chunks of several
        // files are embedded together, so many small files still fill a model call.
        let code = source.kind == SourceKind::Code;
        let root = code.then(|| source.path.clone());
        thread::scope(|scope| -> Result<()> {
            // Code files parse in milliseconds: let the parser run far ahead.
            let (send, parsed_files) = mpsc::sync_channel(if code { 256 } else { 2 });
            let cancelled = &cancelled;
            let root = root.as_deref();
            let parser = scope.spawn(move || -> Result<()> {
                let mut worker: Option<Worker> = None;
                for (index, (entry, hash)) in to_parse.into_iter().enumerate() {
                    if cancelled() {
                        break;
                    }
                    let result = if let Some(error) = hash_errors.remove(&entry.path) {
                        Err(error)
                    } else {
                        if worker.is_none() {
                            worker = Some(Worker::spawn(&ingest_cmd)?);
                        }
                        match worker.as_mut().unwrap().parse_until(
                            &entry.path,
                            Some(pdf_mode),
                            root,
                            config.file_timeout,
                            cancelled,
                        ) {
                            Ok(result) => result,
                            Err(error) => {
                                drop(worker.take());
                                Err(error.to_string())
                            }
                        }
                    };
                    if send.send((index, entry, hash, result)).is_err() {
                        break; // consumer stopped (cancelled or failed)
                    }
                }
                Ok(())
            });
            let mut pending: Vec<ParsedFile> = Vec::new();
            let mut pending_chunks = 0;
            'files: loop {
                if cancelled() {
                    break;
                }
                // Wait for the first file; then take whatever else is already parsed.
                let next = if pending.is_empty() {
                    parsed_files.recv().ok()
                } else {
                    parsed_files.try_recv().ok()
                };
                match next {
                    Some(file) => {
                        pending_chunks += file.3.as_ref().map_or(0, Vec::len);
                        pending.push(file);
                        if pending_chunks < EMBED_GROUP {
                            continue;
                        }
                    }
                    None if pending.is_empty() => break, // parser finished
                    None => {}
                }
                pending_chunks = 0;
                for (index, entry, hash, chunks) in
                    self.embed_files(std::mem::take(&mut pending), cancelled)
                {
                    if cancelled() {
                        break 'files;
                    }
                    match chunks {
                        Ok(chunks) => {
                            store
                                .replace_file(
                                    source.id,
                                    &entry.path,
                                    entry.mtime,
                                    entry.size,
                                    &hash,
                                    &chunks,
                                )
                                .with_context(|| format!("indexing {}", entry.path.display()))?;
                            parsed += 1;
                        }
                        Err(error) => {
                            store.mark_failed(
                                source.id,
                                &entry.path,
                                entry.mtime,
                                entry.size,
                                &hash,
                                &error,
                            )?;
                            failed += 1;
                        }
                    }
                    (self.on_event)(IngestEvent::Progress {
                        source_id: source.id,
                        done: index + 1,
                        total,
                        current: entry.path.display().to_string(),
                    });
                }
            }
            drop(parsed_files); // unblock a parser that is still sending
            parser
                .join()
                .map_err(|_| anyhow!("parser thread panicked"))?
        })?;
        if !code {
            self.engine.reload_index()?;
        } else if parsed + failed + plan.delete.len() > 0 {
            // The notes view never holds code; only this repository's snapshot changes.
            self.engine.reload_source(source.id)?;
        }
        eprintln!(
            "source {}: parsed {parsed}, failed {failed}, deleted {}",
            source.id,
            plan.delete.len()
        );
        Ok((parsed, failed, plan.delete.len()))
    }

    /// Embeds the chunks of several parsed files together. Chunks are sorted by length so
    /// each model call pads little. A failed call fails only the files it contained.
    fn embed_files(
        &self,
        files: Vec<ParsedFile>,
        cancelled: &dyn Fn() -> bool,
    ) -> Vec<EmbeddedFile> {
        let chunk = |(file, index): (usize, usize)| -> &ParsedChunk {
            &files[file].3.as_ref().expect("only parsed files are embedded")[index]
        };
        let mut order: Vec<(usize, usize)> = files
            .iter()
            .enumerate()
            .flat_map(|(file, parsed)| {
                let count = parsed.3.as_ref().map_or(0, Vec::len);
                (0..count).map(move |index| (file, index))
            })
            .collect();
        order.sort_by_key(|&key| {
            let chunk = chunk(key);
            chunk.header.len() + chunk.text.len()
        });
        let mut vectors: Vec<Vec<Option<Vec<f32>>>> = files
            .iter()
            .map(|parsed| vec![None; parsed.3.as_ref().map_or(0, Vec::len)])
            .collect();
        let mut errors: Vec<Option<String>> = vec![None; files.len()];
        for group in order.chunks(EMBED_BATCH) {
            if cancelled() {
                break;
            }
            let docs: Vec<_> = group
                .iter()
                .map(|&key| {
                    let chunk = chunk(key);
                    (chunk.header.as_str(), chunk.text.as_str())
                })
                .collect();
            let error = match self.engine.embedder().embed_docs(&docs) {
                Ok(embeddings) if embeddings.len() == group.len() => {
                    for (&(file, index), vector) in group.iter().zip(embeddings) {
                        vectors[file][index] = Some(vector);
                    }
                    continue;
                }
                Ok(_) => "embedder returned an invalid number of embeddings".to_string(),
                Err(error) => format!("{error:#}"),
            };
            for &(file, _) in group {
                errors[file] = Some(error.clone());
            }
        }
        files
            .into_iter()
            .zip(vectors)
            .zip(errors)
            .map(|(((index, entry, hash, result), vectors), error)| {
                let result = result.and_then(|chunks| {
                    if let Some(error) = error {
                        return Err(error);
                    }
                    chunks
                        .into_iter()
                        .zip(vectors)
                        .map(|(chunk, vector)| {
                            Ok(NewChunk {
                                header: chunk.header,
                                text: chunk.text,
                                page: chunk.page,
                                line: chunk.line,
                                end_line: chunk.end_line,
                                vec: vector.ok_or_else(|| "cancelled".to_string())?,
                            })
                        })
                        .collect()
                });
                (index, entry, hash, result)
            })
            .collect()
    }

    fn hash(
        &self,
        path: &Path,
        timeout: Duration,
        cancelled: impl Fn() -> bool,
        worker: &mut Option<Worker>,
    ) -> Result<String> {
        if let Some(command) = &self.hash_worker_cmd {
            if worker.is_none() {
                *worker = Some(Worker::spawn(command)?);
            }
            return match worker
                .as_mut()
                .unwrap()
                .hash_until(path, timeout, cancelled)
            {
                Ok(result) => result.map_err(anyhow::Error::msg),
                Err(error) => {
                    drop(worker.take());
                    Err(error)
                }
            };
        }

        // Library callers without a helper can still abandon a blocked filesystem read.
        let (send, receive) = mpsc::channel();
        let path = path.to_owned();
        thread::Builder::new().spawn(move || {
            let _ = send.send(hash_file(&path));
        })?;
        let started = Instant::now();
        loop {
            ensure!(!cancelled(), "cancelled");
            let remaining = timeout.saturating_sub(started.elapsed());
            ensure!(!remaining.is_zero(), "timeout after {}s", timeout.as_secs());
            match receive.recv_timeout(remaining.min(Duration::from_millis(50))) {
                Ok(result) => return result,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(anyhow!("hash read failed"));
                }
            }
        }
    }
}
