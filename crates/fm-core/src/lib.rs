//! fm-core: raw-syscall directory reading (getdents64 + fstatat), parallel walking,
//! a metadata cache, lazy content helpers (hash / MIME sniffing) and a small worker pool.
//! No dependency on any other fm-* crate, and no shelling out to ls/find/stat.

pub mod cache;
pub mod content;
pub mod filter;
pub mod listing;
pub mod mime;
pub mod pool;
pub mod walk;

pub use cache::{CacheStats, MetaCache};
pub use filter::PathFilter;
pub use listing::{read_dir, DirListing, EntryRef, FileKind, Meta, ReadOpts, SortKey};
pub use pool::spawn_pool;
pub use walk::{walk, WalkOpts, WalkStats};
