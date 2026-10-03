use crate::listing::{read_dir, DirListing, FileKind, ReadOpts};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::SeqCst};
use std::sync::{Condvar, Mutex};

#[derive(Clone, Debug)]
pub struct WalkOpts {
    pub threads: usize,
    pub read: ReadOpts,
}

impl Default for WalkOpts {
    fn default() -> Self {
        Self { threads: 1, read: ReadOpts::default() }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WalkStats {
    pub dirs: u64,
    pub entries: u64,
    pub errors: u64,
}

struct Shared {
    q: Mutex<VecDeque<PathBuf>>,
    cv: Condvar,
    /// directories queued or in flight; 0 means the walk is finished
    pending: AtomicUsize,
    dirs: AtomicU64,
    entries: AtomicU64,
    errors: AtomicU64,
}

/// Parallel breadth-first walk. `on_dir` is called once per directory, from worker threads,
/// with the full listing (batching per directory). Symlinks are never followed.
/// Unreadable directories are counted in `errors` and skipped.
pub fn walk<F>(roots: &[PathBuf], opts: &WalkOpts, on_dir: F) -> WalkStats
where
    F: Fn(&DirListing) + Sync,
{
    let sh = Shared {
        q: Mutex::new(roots.iter().cloned().collect()),
        cv: Condvar::new(),
        pending: AtomicUsize::new(roots.len()),
        dirs: AtomicU64::new(0),
        entries: AtomicU64::new(0),
        errors: AtomicU64::new(0),
    };
    if roots.is_empty() {
        return WalkStats::default();
    }
    let threads = opts.threads.max(1);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| worker(&sh, opts, &on_dir));
        }
    });
    WalkStats { dirs: sh.dirs.load(SeqCst), entries: sh.entries.load(SeqCst), errors: sh.errors.load(SeqCst) }
}

fn worker<F: Fn(&DirListing)>(sh: &Shared, opts: &WalkOpts, on_dir: &F) {
    loop {
        let dir = {
            let mut q = sh.q.lock().unwrap();
            loop {
                if let Some(d) = q.pop_front() {
                    break Some(d);
                }
                if sh.pending.load(SeqCst) == 0 {
                    break None;
                }
                q = sh.cv.wait(q).unwrap();
            }
        };
        let Some(dir) = dir else { return };

        match read_dir(&dir, &opts.read) {
            Ok(listing) => {
                sh.dirs.fetch_add(1, SeqCst);
                sh.entries.fetch_add(listing.len() as u64, SeqCst);
                let filter = opts.read.filter.as_deref();
                let subs: Vec<PathBuf> = listing
                    .iter()
                    .filter(|e| e.kind == FileKind::Dir)
                    .map(|e| dir.join(e.name))
                    .filter(|p| !filter.is_some_and(|f| f.skip_path(p)))
                    .collect();
                on_dir(&listing);
                if !subs.is_empty() {
                    sh.pending.fetch_add(subs.len(), SeqCst);
                    sh.q.lock().unwrap().extend(subs);
                    sh.cv.notify_all();
                }
            }
            Err(e) => {
                sh.errors.fetch_add(1, SeqCst);
                tracing::debug!("cannot read {}: {}", dir.display(), e);
            }
        }
        if sh.pending.fetch_sub(1, SeqCst) == 1 {
            // last directory done: take the lock so no waiter can miss the wakeup between its check and wait
            drop(sh.q.lock().unwrap());
            sh.cv.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::PathFilter;
    use std::sync::{Arc, Mutex};

    fn tree(root: &std::path::Path) -> usize {
        let mut files = 0;
        for a in 0..5 {
            for b in 0..5 {
                let d = root.join(format!("d{a}/e{b}"));
                std::fs::create_dir_all(&d).unwrap();
                for f in 0..10 {
                    std::fs::write(d.join(format!("f{f}")), "x").unwrap();
                    files += 1;
                }
            }
        }
        files
    }

    #[test]
    fn walks_everything_single_and_multi_threaded() {
        let t = tempfile::tempdir().unwrap();
        let files = tree(t.path());
        for threads in [1, 4, 8] {
            let seen = Mutex::new(0usize);
            let st = walk(&[t.path().to_path_buf()], &WalkOpts { threads, ..Default::default() }, |l| {
                *seen.lock().unwrap() += l.iter().filter(|e| e.kind == FileKind::File).count();
            });
            assert_eq!(*seen.lock().unwrap(), files, "threads={threads}");
            assert_eq!(st.dirs, 1 + 5 + 25);
            assert_eq!(st.errors, 0);
        }
    }

    #[test]
    fn blacklist_prunes_subtrees() {
        let t = tempfile::tempdir().unwrap();
        tree(t.path());
        let f = PathFilter::new(&["d1".into(), "d2".into()], &[], &[t.path().join("d3/e0")], false);
        let opts = WalkOpts { threads: 4, read: ReadOpts { filter: Some(Arc::new(f)), ..Default::default() } };
        let st = walk(&[t.path().to_path_buf()], &opts, |_| {});
        // root + d0,d3,d4 + their e* except d3/e0 → 1 + 3 + 14
        assert_eq!(st.dirs, 1 + 3 + 14);
    }

    #[test]
    fn unreadable_or_missing_root_is_counted_not_fatal() {
        let st = walk(&[PathBuf::from("/nope/nope")], &WalkOpts::default(), |_| {});
        assert_eq!(st.errors, 1);
        assert_eq!(st.dirs, 0);
    }

    #[test]
    fn symlink_loops_do_not_hang() {
        let t = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(t.path(), t.path().join("loop")).unwrap();
        let st = walk(&[t.path().to_path_buf()], &WalkOpts { threads: 2, ..Default::default() }, |_| {});
        assert_eq!(st.dirs, 1);
    }
}
