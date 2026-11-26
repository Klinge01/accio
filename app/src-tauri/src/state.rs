use accio_core::{
    code::CodeService, engine::Engine, ingest::IngestManager, llm::Groq, prompt::Turn,
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub struct TrayState {
    pub progress: HashMap<i64, (usize, usize)>,
    pub last_progress_update: Option<Instant>,
    pub progress_update_pending: bool,
    pub drop_generation: u64,
}

pub struct AppState {
    pub engine: Arc<Engine>,
    pub ingest: Arc<IngestManager>,
    pub code: Arc<CodeService>,
    pub provider: Mutex<Groq>,
    pub preferences: Mutex<crate::settings::Preferences>,
    pub last_prompt: Mutex<Vec<Turn>>,
    pub scope_change: Mutex<()>,
    pub answer_cancel: Mutex<Option<(u64, CancellationToken)>>,
    pub hidden_at: Mutex<Option<Instant>>,
    pub tray: Mutex<TrayState>,
}

impl AppState {
    pub fn cancel(&self, request_id: Option<u64>) -> Option<u64> {
        let mut active = self.answer_cancel.lock().unwrap();
        if active
            .as_ref()
            .is_some_and(|(id, _)| request_id.is_none_or(|request| request == *id))
        {
            if let Some((id, token)) = active.take() {
                token.cancel();
                return Some(id);
            }
        }
        None
    }
}
