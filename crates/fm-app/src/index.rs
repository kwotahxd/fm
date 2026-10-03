use crate::{now_nanos, App, AppError, AppResult};
use fm_core::listing::FileKind;
use fm_core::{mime, walk, WalkOpts};
use fm_db::DbError;
use fm_types::NewFile;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::sync_channel;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct IndexOpts {
    /// delete DB rows for files that no longer exist under the root
    pub prune: bool,
    pub threads: Option<usize>,
}

impl Default for IndexOpts {
    fn default() -> Self {
        Self { prune: true, threads: None }
    }
}

#[derive(Debug, Clone, Default)]
pub struct IndexReport {
    pub dirs: u64,
    pub files: u64,
    /// names that are not valid UTF-8 are not indexed (documented limitation)
    pub non_utf8: u64,
    pub errors: u64,
    pub pruned: usize,
    pub elapsed: Duration,
}

impl App {
    /// Walk `root` in parallel (Core) and stream batches, one per directory, to a single DB writer thread.
    /// Metadata only: file *content* is never read here.
    pub fn index(&self, root: &Path, opts: &IndexOpts) -> AppResult<IndexReport> {
        let started = Instant::now();
        let root = std::fs::canonicalize(root)?;
        if !root.is_dir() {
            return Err(AppError::Invalid(format!("{} is not a directory", root.display())));
        }
        let run_id = now_nanos();
        let db = self.db.clone();
        let (tx, rx) = sync_channel::<Vec<fm_types::NewFile>>(64);
        let writer = std::thread::spawn(move || -> Result<usize, DbError> {
            let (mut buf, mut n) = (Vec::new(), 0usize);
            for batch in rx {
                buf.extend(batch);
                if buf.len() >= 2000 {
                    n += db.upsert_files(run_id, &buf)?;
                    buf.clear();
                }
            }
            if !buf.is_empty() {
                n += db.upsert_files(run_id, &buf)?;
            }
            Ok(n)
        });

        let (files, non_utf8) = (AtomicU64::new(0), AtomicU64::new(0));
        let wopts = WalkOpts { threads: opts.threads.unwrap_or_else(|| self.cfg.walk_threads()), read: self.walk_read_opts() };
        let stats = walk(&[root.clone()], &wopts, |l| {
            let mut batch = Vec::with_capacity(l.len());
            for e in l.iter() {
                let (FileKind::File, Some(m)) = (e.kind, e.meta) else { continue };
                let (Some(name), Some(path)) = (e.name.to_str(), l.path.join(e.name).to_str().map(str::to_string)) else {
                    non_utf8.fetch_add(1, Relaxed);
                    continue;
                };
                batch.push(NewFile {
                    path,
                    name: name.to_string(),
                    size: m.size,
                    mtime_ns: m.mtime_ns,
                    mode: m.mode,
                    mime: mime::from_ext(name).map(str::to_string),
                });
            }
            files.fetch_add(batch.len() as u64, Relaxed);
            if !batch.is_empty() {
                let _ = tx.send(batch); // a dead writer surfaces through join() below
            }
        });
        drop(tx);
        writer.join().map_err(|_| AppError::Invalid("index writer thread panicked".into()))??;

        let mut pruned = 0;
        if opts.prune {
            if stats.errors == 0 {
                pruned = self.db.prune_unseen(&root.to_string_lossy(), run_id)?;
            } else {
                // an unreadable directory would look like "everything in it was deleted"
                tracing::warn!("{} directories could not be read: skipping the prune step", stats.errors);
            }
        }
        let rep = IndexReport { dirs: stats.dirs, files: files.load(Relaxed), non_utf8: non_utf8.load(Relaxed), errors: stats.errors, pruned, elapsed: started.elapsed() };
        tracing::info!("indexed {} files in {} dirs ({} pruned) in {:?}", rep.files, rep.dirs, rep.pruned, rep.elapsed);
        Ok(rep)
    }
}
