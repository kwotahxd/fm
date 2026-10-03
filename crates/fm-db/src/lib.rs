//! fm-db: persistence behind the [`Database`] trait. SQLite is the shipped backend;
//! PostgreSQL would be a second `impl Database` selected in [`open`] (not implemented, see docs).

mod sqlite;

use fm_types::*;
use std::sync::Arc;

pub use sqlite::SqliteDb;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}
pub type DbResult<T> = Result<T, DbError>;

#[derive(Debug, Clone, Default)]
pub struct FileQuery {
    /// only files under this directory (recursive)
    pub under: Option<String>,
    /// substring match on the file name
    pub name_contains: Option<String>,
    pub category: Option<String>,
    pub tag: Option<String>,
    pub mime_prefix: Option<String>,
    /// only rows whose analysis is missing or stale
    pub needs_analysis: bool,
    pub limit: Option<usize>,
}

pub trait Database: Send + Sync {
    fn schema_version(&self) -> DbResult<i64>;

    // ── index ──
    /// Upsert a batch in one transaction. `run_id` marks the rows as seen (see `prune_unseen`).
    /// Analysis data of existing rows is preserved. Returns the number of rows written.
    fn upsert_files(&self, run_id: i64, files: &[NewFile]) -> DbResult<usize>;
    /// Delete rows under `root` not touched by `run_id` (files deleted since the last index).
    fn prune_unseen(&self, root: &str, run_id: i64) -> DbResult<usize>;
    fn get_file(&self, path: &str) -> DbResult<Option<FileRecord>>;
    fn list_files(&self, q: &FileQuery) -> DbResult<Vec<FileRecord>>;
    /// Remove a path (and everything below it if it was a directory).
    fn remove_path(&self, path: &str) -> DbResult<usize>;
    /// Keep the index in sync after a file was moved on disk.
    fn rename_file(&self, from: &str, to: &str) -> DbResult<()>;
    fn count_files(&self) -> DbResult<u64>;

    // ── analysis ──
    fn save_analysis(&self, file_id: i64, mtime_ns: i64, hash: Option<&str>, res: &AnalysisResult) -> DbResult<()>;
    /// File turned out to be byte-identical (same hash) despite a new mtime: just refresh the stamp.
    fn touch_analysis(&self, file_id: i64, mtime_ns: i64, hash: &str) -> DbResult<()>;
    fn list_categories(&self) -> DbResult<Vec<(String, u64)>>;

    // ── vector search ──
    /// Cosine similarity, top-k. (Brute force over stored vectors on SQLite; a PostgreSQL backend
    /// would delegate to pgvector.)
    fn search_similar(&self, query: &[f32], model: Option<&str>, limit: usize) -> DbResult<Vec<(FileRecord, f32)>>;

    // ── rules ──
    fn save_rule(&self, rule: &SavedRule) -> DbResult<()>;
    fn get_rule(&self, name: &str) -> DbResult<Option<SavedRule>>;
    fn list_rules(&self) -> DbResult<Vec<SavedRule>>;

    // ── sort history ──
    fn append_sort_op(&self, op: &SortOpRecord) -> DbResult<i64>;
    fn batch_ops(&self, batch_id: &str) -> DbResult<Vec<SortOpRecord>>;
    fn list_batches(&self, limit: usize) -> DbResult<Vec<BatchSummary>>;
    /// Newest batch that still has applied ops.
    fn last_undoable_batch(&self) -> DbResult<Option<String>>;
    fn set_op_status(&self, id: i64, status: OpStatus, error: Option<&str>) -> DbResult<()>;
}

pub fn open(cfg: &fm_config::Config) -> DbResult<Arc<dyn Database>> {
    match cfg.database.backend.as_str() {
        "sqlite" => Ok(Arc::new(SqliteDb::open(&cfg.db_path())?)),
        "postgres" => Err(DbError::Other(
            "the PostgreSQL backend is an extension point only: implement `fm_db::Database` and add it to fm_db::open".into(),
        )),
        other => Err(DbError::Other(format!("unknown database backend '{other}'"))),
    }
}
