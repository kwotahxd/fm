//! fm-sorter: turns (files + rule set) into a move plan, executes it, and undoes it.
//! Depends only on `fm-types`: it never touches the DB, the AI or the walker. The caller
//! persists history through the `on_event` callback, so a crash mid-run still leaves a journal.

pub mod builtin;
pub mod exec;
pub mod plan;
pub mod template;

pub use exec::{execute, undo, ExecReport, OpOutcome, UndoOutcome, UndoReport};
pub use plan::{plan, requirements, Plan, PlannedOp, Requirements, Skipped, SortOptions};
pub use template::Template;

#[derive(Debug, thiserror::Error)]
pub enum SortError {
    #[error("invalid rule set: {0}")]
    InvalidRule(String),
    #[error("conflict: {dst} is already taken (strategy = fail)")]
    Conflict { dst: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}
