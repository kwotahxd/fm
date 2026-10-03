use crate::analyze::AnalyzeOpts;
use crate::index::IndexOpts;
use crate::{now_nanos, App, AppResult};
use fm_core::mime;
use fm_types::NewFile;
use fm_watcher::FsEvent;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

#[derive(Debug, Clone, Default)]
pub struct WatchBatchReport {
    pub changed: usize,
    pub removed: usize,
    pub analyzed: usize,
    /// the kernel queue overflowed and the roots were re-indexed from scratch
    pub rescanned: bool,
}

impl App {
    fn upsert_path(&self, path: &Path) -> bool {
        let Ok(md) = std::fs::symlink_metadata(path) else { return false };
        if !md.is_file() {
            return false;
        }
        let (Some(name), Some(p)) = (path.file_name().and_then(|n| n.to_str()), path.to_str()) else { return false };
        let nf = NewFile {
            path: p.to_string(),
            name: name.to_string(),
            size: md.len(),
            mtime_ns: md.mtime() * 1_000_000_000 + md.mtime_nsec(),
            mode: md.mode(),
            mime: mime::from_ext(name).map(str::to_string),
        };
        self.db.upsert_files(now_nanos(), &[nf]).is_ok()
    }

    /// Index the roots, then keep the index current from inotify events until `stop` is set.
    /// Only the files that changed are re-indexed — and, if `[watch].auto_analyze` is on and the AI is
    /// reachable, only those are re-analysed.
    pub fn watch(&self, roots: &[PathBuf], stop: &AtomicBool, on_batch: &mut dyn FnMut(&WatchBatchReport)) -> AppResult<()> {
        let roots: Vec<PathBuf> = roots.iter().map(std::fs::canonicalize).collect::<Result<_, _>>()?;
        for r in &roots {
            self.index(r, &IndexOpts::default())?;
        }
        let (handle, rx) = fm_watcher::start(&roots, self.filter.clone(), Duration::from_millis(self.cfg.watch.debounce_ms))?;
        while !stop.load(Relaxed) {
            let batch = match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(b) => b,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            };
            let mut rep = WatchBatchReport::default();
            let mut changed = vec![];
            for ev in batch {
                match ev {
                    FsEvent::Overflow => {
                        for r in &roots {
                            self.index(r, &IndexOpts::default())?;
                        }
                        self.cache.clear();
                        rep.rescanned = true;
                    }
                    FsEvent::Changed(p) => {
                        if let Some(parent) = p.parent() {
                            self.cache.invalidate(parent);
                        }
                        if self.upsert_path(&p) {
                            rep.changed += 1;
                            changed.push(p);
                        } else if std::fs::symlink_metadata(&p).is_err() {
                            // vanished between the event and now
                            rep.removed += self.db.remove_path(&p.to_string_lossy())?;
                        }
                    }
                    FsEvent::Removed(p) => {
                        if let Some(parent) = p.parent() {
                            self.cache.invalidate(parent);
                        }
                        rep.removed += self.db.remove_path(&p.to_string_lossy())?;
                    }
                }
            }
            if self.cfg.watch.auto_analyze && !changed.is_empty() && self.ai_available() {
                let mut recs = vec![];
                for p in &changed {
                    if let Some(r) = self.db.get_file(&p.to_string_lossy())? {
                        if r.needs_analysis() {
                            recs.push(r);
                        }
                    }
                }
                match self.analyze_records(recs, &AnalyzeOpts::default(), &|_, _| {}) {
                    Ok(a) => rep.analyzed = a.analyzed,
                    Err(e) => tracing::warn!("auto-analysis skipped: {e}"),
                }
            }
            on_batch(&rep);
        }
        handle.stop();
        Ok(())
    }
}
