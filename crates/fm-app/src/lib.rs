//! fm-app: the application layer. It wires Core + Database + AI client + Sorter + Watcher together and is the
//! *only* place that knows about more than one module. The CLI (and a future GUI) call this crate, nothing else.
//!
//! Graceful degradation is decided here: every AI-dependent call checks availability first, and everything
//! else (indexing, listing, built-in sort rules, undo, watching) keeps working when the engine or Ollama is down.

mod analyze;
mod index;
mod sorting;
mod watching;

pub use analyze::{AnalyzeOpts, AnalyzeReport};
pub use index::{IndexOpts, IndexReport};
pub use sorting::{ApplyReport, RuleSource, SortPlan, SortRequest, UndoSummary};
pub use watching::WatchBatchReport;

use fm_ai::{AiClient, AiError};
use fm_config::Config;
use fm_core::{DirListing, MetaCache, PathFilter, ReadOpts};
use fm_db::{Database, DbError, FileQuery};
use fm_types::*;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("database: {0}")]
    Db(#[from] DbError),
    #[error("{0}")]
    Ai(AiError),
    /// AI features are off, the engine is not running, or Ollama is down. Everything non-AI still works.
    #[error("AI is unavailable: {0}")]
    AiUnavailable(String),
    #[error("sorter: {0}")]
    Sort(#[from] fm_sorter::SortError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Invalid(String),
}

impl From<AiError> for AppError {
    fn from(e: AiError) -> Self {
        if e.is_unavailable() {
            AppError::AiUnavailable(e.to_string())
        } else {
            AppError::Ai(e)
        }
    }
}

pub type AppResult<T> = Result<T, AppError>;

pub struct App {
    pub cfg: Config,
    pub db: Arc<dyn Database>,
    pub cache: Arc<MetaCache>,
    pub filter: Arc<PathFilter>,
    ai: Arc<AiClient>,
    ai_probe: Mutex<Option<(Instant, bool)>>,
}

pub(crate) fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}
pub(crate) fn now_nanos() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as i64).unwrap_or(0)
}

impl App {
    pub fn new(cfg: Config) -> AppResult<Self> {
        let db = fm_db::open(&cfg)?;
        Ok(Self::with_db(cfg, db))
    }

    /// Any `Database` implementation can be plugged in here.
    pub fn with_db(cfg: Config, db: Arc<dyn Database>) -> Self {
        let prefixes: Vec<PathBuf> = cfg.index.blacklist_paths.iter().map(|p| fm_config::expand(p)).collect();
        let filter = Arc::new(PathFilter::new(&cfg.index.blacklist_names, &cfg.index.blacklist_globs, &prefixes, cfg.index.skip_hidden));
        let read = ReadOpts {
            read_meta: true,
            buf_size: cfg.core.read_buffer_kb.max(1) * 1024,
            stat_threads: cfg.walk_threads(),
            filter: Some(filter.clone()),
            ..Default::default()
        };
        let cache = Arc::new(MetaCache::new(cfg.core.cache_entries, Duration::from_millis(cfg.core.cache_ttl_ms), read));
        let ai = Arc::new(AiClient::new(cfg.socket_path(), Duration::from_secs(cfg.ai.request_timeout_s)));
        Self { cfg, db, cache, filter, ai, ai_probe: Mutex::new(None) }
    }

    pub(crate) fn walk_read_opts(&self) -> ReadOpts {
        ReadOpts {
            read_meta: true,
            buf_size: self.cfg.core.read_buffer_kb.max(1) * 1024,
            stat_threads: 1, // the walker already parallelises across directories
            filter: Some(self.filter.clone()),
            ..Default::default()
        }
    }

    /// Directory listing through the metadata cache (for navigation; never touches the AI).
    pub fn listing(&self, path: &Path) -> std::io::Result<Arc<DirListing>> {
        self.cache.get(path)
    }

    /// Is the AI engine reachable right now? Cached for a few seconds so hot paths don't ping per file.
    pub fn ai_available(&self) -> bool {
        if !self.cfg.ai.enabled {
            return false;
        }
        let mut g = self.ai_probe.lock().unwrap();
        if let Some((t, ok)) = *g {
            if t.elapsed() < Duration::from_secs(if ok { 10 } else { 3 }) {
                return ok;
            }
        }
        let ok = match self.ai.ping() {
            Ok(()) => true,
            Err(e) => {
                tracing::debug!("AI engine not reachable: {e}");
                false
            }
        };
        *g = Some((Instant::now(), ok));
        ok
    }

    pub(crate) fn require_ai(&self) -> AppResult<()> {
        if !self.cfg.ai.enabled {
            return Err(AppError::AiUnavailable("disabled in the config ([ai].enabled = false)".into()));
        }
        if !self.ai_available() {
            return Err(AppError::AiUnavailable(format!("engine not reachable at {} (start it: `python -m aiengine`)", self.ai.socket().display())));
        }
        Ok(())
    }

    pub fn ai_status(&self) -> AppResult<ServiceStatus> {
        if !self.cfg.ai.enabled {
            return Err(AppError::AiUnavailable("disabled in the config".into()));
        }
        Ok(self.ai.status()?)
    }

    pub fn find(&self, q: &FileQuery) -> AppResult<Vec<FileRecord>> {
        Ok(self.db.list_files(q)?)
    }

    /// "files like …": embed the query with the local model, then cosine search in the index.
    pub fn search_semantic(&self, query: &str, limit: usize, under: Option<&Path>) -> AppResult<Vec<(FileRecord, f32)>> {
        self.require_ai()?;
        let emb = self.ai.embed(&[query.to_string()])?;
        let v = emb.vectors.first().ok_or_else(|| AppError::Invalid("engine returned no vector".into()))?;
        let want = if under.is_some() { limit.saturating_mul(10).max(50) } else { limit };
        let mut hits = self.db.search_similar(v, Some(&emb.model), want)?;
        if let Some(u) = under {
            let prefix = format!("{}/", u.to_string_lossy().trim_end_matches('/'));
            hits.retain(|(f, _)| f.path.starts_with(&prefix));
        }
        hits.truncate(limit);
        Ok(hits)
    }

    /// Cached at the engine boundary so `fm` can print a helpful hint once instead of failing per file.
    pub fn ai_client(&self) -> Arc<AiClient> {
        self.ai.clone()
    }
}
