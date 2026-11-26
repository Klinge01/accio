use crate::{
    config::Config,
    models::EmbedModel,
    lang::{self, Lang},
    search::{content_terms, dedupe, fts_query, keyword_match, prefix_query, rrf},
    store::{Scope, ScopeSql, Store, chunks_by_ids, fts_search, timestamp},
    vindex::VecIndex,
};
use anyhow::{Result, ensure};
use rusqlite::Connection;
use serde::Serialize;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::Instant,
};

#[derive(Clone, Debug, Serialize)]
pub struct Hit {
    pub chunk_id: i64,
    pub path: PathBuf,
    pub header: String,
    pub text: String,
    pub page: Option<u32>,
    /// First and last source line (code chunks).
    pub line: Option<u32>,
    pub end_line: Option<u32>,
    pub score: f32,
    pub stale: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct StageTimings {
    pub translate_ms: f32,
    pub embed_ms: f32,
    pub dense_ms: f32,
    pub fts_ms: f32,
    pub total_ms: f32,
}

#[derive(Clone, Debug)]
pub struct Retrieval {
    pub query: String,
    /// The question in the other language (German <-> English), when it was searched too.
    pub translation: Option<String>,
    pub hits: Vec<Hit>,
    pub timings: StageTimings,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Dense,
    Fts,
    Hybrid,
}

struct View {
    scope: Scope,
    sql: ScopeSql,
    conn: Mutex<Connection>,
    index: VecIndex,
}

#[derive(Clone)]
struct Cached {
    retrieval: Arc<Retrieval>,
    ingest_times: Vec<i64>,
}

struct State {
    /// What the panel searches: enabled notes, or one code repository.
    scope: Scope,
    view: Arc<View>,
    /// Snapshots of code repositories searched by agents, built on first use.
    code_views: HashMap<i64, Arc<View>>,
    search: (f32, usize),
    cached: Option<Cached>,
    generation: u64,
    pending: Option<String>,
    prefetch_running: bool,
}

pub struct Engine {
    cfg: Config,
    store: Store,
    embedder: Arc<dyn EmbedModel>,
    state: Mutex<State>,
    publishing: Mutex<()>,
}

fn ms(start: Instant) -> f32 {
    start.elapsed().as_secs_f32() * 1000.0
}

fn stale(path: &Path, indexed_at: i64) -> bool {
    path.metadata()
        .and_then(|metadata| metadata.modified())
        .is_ok_and(|modified| timestamp(modified) > indexed_at)
}

impl Engine {
    pub fn new(cfg: Config, embedder: Arc<dyn EmbedModel>) -> Result<Arc<Self>> {
        let store = Store::open(&cfg.db_path(), embedder.model_id(), embedder.dim())?;
        Self::with_store(cfg, store, embedder)
    }

    pub fn open_existing(cfg: Config, embedder: Arc<dyn EmbedModel>) -> Result<Arc<Self>> {
        let store = Store::open_existing(&cfg.db_path(), embedder.model_id(), embedder.dim())?;
        Self::with_store(cfg, store, embedder)
    }

    fn with_store(cfg: Config, store: Store, embedder: Arc<dyn EmbedModel>) -> Result<Arc<Self>> {
        let view = Self::view(&store, embedder.dim(), Scope::Notes)?;
        let search = (cfg.min_similarity, cfg.max_context_chunks);
        Ok(Arc::new(Self {
            cfg,
            store,
            embedder,
            state: Mutex::new(State {
                scope: Scope::Notes,
                view,
                code_views: HashMap::new(),
                search,
                cached: None,
                generation: 0,
                pending: None,
                prefetch_running: false,
            }),
            publishing: Mutex::new(()),
        }))
    }

    fn view(store: &Store, dim: usize, scope: Scope) -> Result<Arc<View>> {
        let (conn, ids, vectors) = store.snapshot(scope)?;
        Ok(Arc::new(View {
            scope,
            sql: store.scope_sql(scope),
            conn: Mutex::new(conn),
            index: VecIndex::build(ids, vectors, dim),
        }))
    }

    pub fn store(&self) -> &Store {
        &self.store
    }
    pub fn embedder(&self) -> &Arc<dyn EmbedModel> {
        &self.embedder
    }
    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn reload_index(&self) -> Result<()> {
        let _publishing = self.publishing.lock().unwrap();
        let mut scope = self.state.lock().unwrap().scope;
        if let Scope::Source(id) = scope
            && self.store.get_source(id).is_err()
        {
            scope = Scope::Notes; // the repository the panel was on was removed
        }
        let view = Self::view(&self.store, self.embedder.dim(), scope)?;
        let mut state = self.state.lock().unwrap();
        state.scope = scope;
        state.view = view;
        state.code_views.clear();
        state.cached = None;
        Ok(())
    }

    /// What the panel searches.
    pub fn scope(&self) -> Scope {
        self.state.lock().unwrap().scope
    }

    /// Switches the panel between notes and one code repository.
    pub fn set_scope(&self, scope: Scope) -> Result<()> {
        let _publishing = self.publishing.lock().unwrap();
        if self.scope() == scope {
            return Ok(());
        }
        let view = Self::view(&self.store, self.embedder.dim(), scope)?;
        let mut state = self.state.lock().unwrap();
        state.scope = scope;
        state.view = view;
        state.generation += 1;
        state.cached = None;
        Ok(())
    }

    /// After a code repository changed: drop its snapshot (rebuilt on the next search)
    /// and refresh the panel's view if the panel is on that repository. The notes view
    /// never contains code, so it stays as it is.
    pub fn reload_source(&self, source_id: i64) -> Result<()> {
        let scope = Scope::Source(source_id);
        let removed = self.state.lock().unwrap().code_views.remove(&source_id);
        drop(removed);
        if self.scope() == scope {
            let _publishing = self.publishing.lock().unwrap();
            let view = Self::view(&self.store, self.embedder.dim(), scope)?;
            let mut state = self.state.lock().unwrap();
            if state.scope == scope {
                state.view = view;
                state.cached = None;
            }
        }
        Ok(())
    }

    fn code_view(&self, source_id: i64) -> Result<Arc<View>> {
        let scope = Scope::Source(source_id);
        {
            let state = self.state.lock().unwrap();
            if state.scope == scope {
                return Ok(state.view.clone());
            }
            if let Some(view) = state.code_views.get(&source_id) {
                return Ok(view.clone());
            }
        }
        let _publishing = self.publishing.lock().unwrap();
        if let Some(view) = self.state.lock().unwrap().code_views.get(&source_id) {
            return Ok(view.clone());
        }
        let view = Self::view(&self.store, self.embedder.dim(), scope)?;
        self.state
            .lock()
            .unwrap()
            .code_views
            .insert(source_id, view.clone());
        Ok(view)
    }

    /// Hybrid search in one code repository, without the similarity gate: an agent
    /// gets the best `limit` matches with their scores and judges them itself.
    pub fn search_source(&self, source_id: i64, query: &str, limit: usize) -> Result<Retrieval> {
        let view = self.code_view(source_id)?;
        Ok(self.run(query, Mode::Hybrid, false, &view, (0.0, limit))?.0)
    }

    pub fn set_search_options(&self, similarity: f32, excerpts: usize) {
        let mut state = self.state.lock().unwrap();
        state.search = (similarity, excerpts);
        state.generation += 1;
        state.cached = None;
    }

    pub fn retrieve(&self, query: &str) -> Result<Arc<Retrieval>> {
        let (view, options, cached) = {
            let state = self.state.lock().unwrap();
            (
                state.view.clone(),
                state.search,
                state
                    .cached
                    .as_ref()
                    .filter(|cached| cached.retrieval.query == query)
                    .cloned(),
            )
        };
        if let Some(cache) = cached {
            let stale_flags: Vec<_> = cache
                .retrieval
                .hits
                .iter()
                .zip(&cache.ingest_times)
                .map(|(hit, &time)| stale(&hit.path, time))
                .collect();
            if cache
                .retrieval
                .hits
                .iter()
                .zip(&stale_flags)
                .all(|(hit, flag)| hit.stale == *flag)
            {
                return Ok(cache.retrieval.clone());
            }
            let mut refreshed = (*cache.retrieval).clone();
            for (hit, flag) in refreshed.hits.iter_mut().zip(stale_flags) {
                hit.stale = flag;
            }
            return Ok(Arc::new(refreshed));
        }
        let (result, _) = self.run(query, Mode::Hybrid, true, &view, options)?;
        Ok(Arc::new(result))
    }

    pub fn retrieve_with(
        &self,
        query: &str,
        mode: Mode,
        apply_threshold: bool,
    ) -> Result<Retrieval> {
        let (view, options) = {
            let state = self.state.lock().unwrap();
            (state.view.clone(), state.search)
        };
        Ok(self.run(query, mode, apply_threshold, &view, options)?.0)
    }

    pub fn prefetch(self: &Arc<Self>, query: String) {
        let mut state = self.state.lock().unwrap();
        state.generation += 1;
        state.pending = (!query.trim().is_empty()).then_some(query);
        if state.prefetch_running || state.pending.is_none() {
            return;
        }
        state.prefetch_running = true;
        let engine = self.clone();
        thread::spawn(move || {
            loop {
                let (query, generation, view, options) = {
                    let mut state = engine.state.lock().unwrap();
                    let Some(query) = state.pending.take() else {
                        state.prefetch_running = false;
                        return;
                    };
                    if state
                        .cached
                        .as_ref()
                        .is_some_and(|cache| cache.retrieval.query == query)
                    {
                        continue;
                    }
                    (query, state.generation, state.view.clone(), state.search)
                };
                let result = engine.run(&query, Mode::Hybrid, true, &view, options);
                let mut state = engine.state.lock().unwrap();
                if state.generation == generation
                    && Arc::ptr_eq(&state.view, &view)
                    && let Ok((retrieval, ingest_times)) = result
                {
                    state.cached = Some(Cached {
                        retrieval: Arc::new(retrieval),
                        ingest_times,
                    });
                }
            }
        });
    }

    /// Chunks by id from the current index snapshot, in the given order. Ids that no
    /// longer exist (file re-ingested or source removed) are skipped. Used to carry the
    /// previous turn's notes into a follow-up question.
    pub fn hits_by_ids(&self, ids: &[i64]) -> Result<Vec<Hit>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let view = self.state.lock().unwrap().view.clone();
        let chunks = chunks_by_ids(&view.conn.lock().unwrap(), ids, &view.sql)?;
        Ok(chunks
            .into_iter()
            .map(|chunk| Hit {
                stale: stale(&chunk.path, chunk.file_ingested_at),
                chunk_id: chunk.id,
                path: chunk.path,
                header: chunk.header,
                text: chunk.text,
                page: chunk.page,
                line: chunk.line,
                end_line: chunk.end_line,
                score: 0.0,
            })
            .collect())
    }

    /// The question translated between German and English, or None (disabled,
    /// unavailable, or no different wording). Errors only cost the cross-lingual part.
    fn translation(&self, query: &str) -> Option<String> {
        if !self.cfg.translate || query.trim().is_empty() {
            return None;
        }
        let to = match lang::detect(query) {
            Lang::De => Lang::En,
            Lang::En => Lang::De,
        };
        match self.embedder.translate(query, to) {
            Ok(Some(text)) if text.trim().to_lowercase() != query.trim().to_lowercase() => {
                Some(text)
            }
            Ok(_) => None,
            Err(error) => {
                eprintln!("translation skipped: {error:#}");
                None
            }
        }
    }

    fn run(
        &self,
        query: &str,
        mode: Mode,
        apply_threshold: bool,
        view: &View,
        (min_similarity, max_context_chunks): (f32, usize),
    ) -> Result<(Retrieval, Vec<i64>)> {
        let started = Instant::now();
        // Code is searched on request; the best matches are always shown.
        let apply_threshold = apply_threshold && view.scope == Scope::Notes;
        let mut timings = StageTimings::default();
        let mut translation = None;
        // One ranked list per (wording of the question, search kind).
        let mut dense_lists: Vec<Vec<(i64, f32)>> = Vec::new();
        let mut sparse_lists: Vec<Vec<i64>> = Vec::new();
        let mut terms: Vec<String> = Vec::new();
        if !view.index.is_empty() && !query.trim().is_empty() {
            let start = Instant::now();
            translation = self.translation(query);
            timings.translate_ms = ms(start);
            let mut queries = vec![query];
            queries.extend(translation.as_deref());
            if mode != Mode::Fts {
                let start = Instant::now();
                let embeddings = self.embedder.embed_queries(&queries)?;
                timings.embed_ms = ms(start);
                ensure!(
                    embeddings.len() == queries.len()
                        && embeddings.iter().all(|e| e.len() == self.embedder.dim()),
                    "invalid query embedding dimension"
                );

                let start = Instant::now();
                dense_lists = embeddings
                    .iter()
                    .map(|embedding| view.index.search(embedding, 50))
                    .collect();
                timings.dense_ms = ms(start);
            }
            if mode != Mode::Dense {
                let start = Instant::now();
                let conn = view.conn.lock().unwrap();
                for wording in &queries {
                    if let Some(fts) = fts_query(wording) {
                        sparse_lists.push(fts_search(&conn, &fts, 50, &view.sql)?);
                    }
                }
                timings.fts_ms = ms(start);
            }
            for term in queries.iter().flat_map(|wording| content_terms(wording)) {
                if !terms.contains(&term) {
                    terms.push(term);
                }
            }
        }

        // No relevant notes: the best semantic match is below the similarity floor.
        // Notes containing the question's key terms still count: the embedding can
        // underrate an exact term ("planet transit" for a German "Planeten-Transit").
        // Keyword-only mode has no similarity score and is never gated.
        let best = dense_lists
            .iter()
            .filter_map(|list| list.first())
            .map(|&(_, score)| score)
            .fold(0.0, f32::max);
        if apply_threshold && mode != Mode::Fts && best < min_similarity {
            dense_lists.clear();
            sparse_lists.clear();
            if mode == Mode::Hybrid
                && let Some(fts) = prefix_query(&terms)
            {
                let conn = view.conn.lock().unwrap();
                let candidates = fts_search(&conn, &fts, 50, &view.sql)?;
                let mut chunks = chunks_by_ids(&conn, &candidates, &view.sql)?;
                chunks.retain(|chunk| {
                    keyword_match(&format!("{} {}", chunk.header, chunk.text), &terms)
                });
                sparse_lists.push(chunks.into_iter().map(|chunk| chunk.id).collect());
            }
        }

        // Copies of a file (the same PDF in several folders) give identical chunks.
        // Count each text once so copies do not crowd out other notes.
        let mut lists: Vec<Vec<i64>> = dense_lists
            .iter()
            .map(|list| list.iter().map(|&(id, _)| id).collect())
            .collect();
        let dense_count = lists.len();
        lists.extend(sparse_lists);
        let mut candidates: Vec<i64> = lists.iter().flatten().copied().collect();
        candidates.sort_unstable();
        candidates.dedup();
        let keys: HashMap<i64, blake3::Hash> = chunks_by_ids(
            &view.conn.lock().unwrap(),
            &candidates,
            &view.sql,
        )?
        .into_iter()
        .map(|chunk| (chunk.id, blake3::hash(chunk.text.as_bytes())))
        .collect();
        let lists = dedupe(lists, |id| keys.get(&id).copied());
        let kept: HashMap<blake3::Hash, i64> = lists
            .iter()
            .flatten()
            .filter_map(|&id| Some((*keys.get(&id)?, id)))
            .collect();
        let mut scores: HashMap<i64, f32> = HashMap::new();
        for &(id, score) in dense_lists.iter().flatten() {
            let id = keys.get(&id).and_then(|key| kept.get(key)).copied().unwrap_or(id);
            let best = scores.entry(id).or_insert(score);
            *best = best.max(score);
        }
        let mut ids = match mode {
            Mode::Dense => {
                let mut ranked: Vec<_> = scores.iter().map(|(&id, &score)| (id, score)).collect();
                ranked.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
                ranked.into_iter().map(|(id, _)| id).collect()
            }
            Mode::Fts => rrf(&lists[dense_count..], 60.0),
            Mode::Hybrid => rrf(&lists, 60.0),
        };
        ids.truncate(max_context_chunks);
        let chunks = chunks_by_ids(&view.conn.lock().unwrap(), &ids, &view.sql)?;
        let mut ranked: Vec<_> = chunks
            .into_iter()
            .map(|chunk| {
                let score = scores.get(&chunk.id).copied().unwrap_or_default();
                (chunk, score)
            })
            .collect();
        ranked.truncate(max_context_chunks);
        let (hits, ingest_times) = ranked
            .into_iter()
            .map(|(chunk, score)| {
                let is_stale = stale(&chunk.path, chunk.file_ingested_at);
                (
                    Hit {
                        chunk_id: chunk.id,
                        path: chunk.path,
                        header: chunk.header,
                        text: chunk.text,
                        page: chunk.page,
                        line: chunk.line,
                        end_line: chunk.end_line,
                        score,
                        stale: is_stale,
                    },
                    chunk.file_ingested_at,
                )
            })
            .unzip();
        timings.total_ms = ms(started);
        Ok((
            Retrieval {
                query: query.into(),
                translation,
                hits,
                timings,
            },
            ingest_times,
        ))
    }
}
