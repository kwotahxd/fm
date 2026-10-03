use crate::listing::{dir_mtime_ns, read_dir, DirListing, ReadOpts};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, Copy, Default)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub entries: usize,
}

struct Inner {
    map: HashMap<PathBuf, Arc<DirListing>>,
    order: VecDeque<PathBuf>,
}

/// Directory-listing cache. An entry is valid while (a) the directory's mtime is unchanged and
/// (b) it is younger than `ttl`. (a) catches adds/removes/renames; (b) bounds staleness of sizes/mtimes
/// of existing files, which do not bump the directory mtime. The watcher calls `invalidate` for
/// instant coherence.
pub struct MetaCache {
    inner: Mutex<Inner>,
    cap: usize,
    ttl: Duration,
    opts: ReadOpts,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl MetaCache {
    pub fn new(cap: usize, ttl: Duration, opts: ReadOpts) -> Self {
        Self {
            inner: Mutex::new(Inner { map: HashMap::new(), order: VecDeque::new() }),
            cap: cap.max(1),
            ttl,
            opts,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    pub fn get(&self, path: &Path) -> io::Result<Arc<DirListing>> {
        let cached = self.inner.lock().unwrap().map.get(path).cloned();
        if let Some(l) = cached {
            if l.loaded_at.elapsed() < self.ttl && dir_mtime_ns(path)? == l.dir_mtime_ns {
                self.hits.fetch_add(1, Relaxed);
                return Ok(l);
            }
        }
        self.misses.fetch_add(1, Relaxed);
        // the (possibly slow) read happens outside the lock
        let l = Arc::new(read_dir(path, &self.opts)?);
        let mut g = self.inner.lock().unwrap();
        if g.map.insert(path.to_path_buf(), l.clone()).is_none() {
            g.order.push_back(path.to_path_buf());
        }
        while g.map.len() > self.cap {
            match g.order.pop_front() {
                Some(old) => {
                    g.map.remove(&old);
                }
                None => break,
            }
        }
        Ok(l)
    }

    pub fn invalidate(&self, path: &Path) {
        let mut g = self.inner.lock().unwrap();
        if g.map.remove(path).is_some() {
            g.order.retain(|p| p != path);
        }
    }

    pub fn clear(&self) {
        let mut g = self.inner.lock().unwrap();
        g.map.clear();
        g.order.clear();
    }

    pub fn stats(&self) -> CacheStats {
        CacheStats { hits: self.hits.load(Relaxed), misses: self.misses.load(Relaxed), entries: self.inner.lock().unwrap().map.len() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_then_invalidated_by_new_file() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("a"), "1").unwrap();
        let c = MetaCache::new(8, Duration::from_secs(60), ReadOpts::default());
        assert_eq!(c.get(t.path()).unwrap().len(), 1);
        assert_eq!(c.get(t.path()).unwrap().len(), 1);
        assert_eq!(c.stats().hits, 1);
        // make sure the directory mtime actually moves on coarse-timestamp filesystems
        std::thread::sleep(Duration::from_millis(30));
        std::fs::write(t.path().join("b"), "2").unwrap();
        assert_eq!(c.get(t.path()).unwrap().len(), 2);
        assert_eq!(c.stats().misses, 2);
    }

    #[test]
    fn ttl_expiry_and_explicit_invalidate() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("a"), "1").unwrap();
        let c = MetaCache::new(8, Duration::from_millis(20), ReadOpts::default());
        c.get(t.path()).unwrap();
        std::thread::sleep(Duration::from_millis(40));
        c.get(t.path()).unwrap();
        assert_eq!(c.stats().misses, 2);
        let c = MetaCache::new(8, Duration::from_secs(60), ReadOpts::default());
        c.get(t.path()).unwrap();
        c.invalidate(t.path());
        c.get(t.path()).unwrap();
        assert_eq!(c.stats().misses, 2);
    }

    #[test]
    fn capacity_is_enforced() {
        let t = tempfile::tempdir().unwrap();
        let c = MetaCache::new(2, Duration::from_secs(60), ReadOpts::default());
        for i in 0..5 {
            let d = t.path().join(format!("d{i}"));
            std::fs::create_dir(&d).unwrap();
            c.get(&d).unwrap();
        }
        assert_eq!(c.stats().entries, 2);
    }
}
