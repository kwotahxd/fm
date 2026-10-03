//! fm-watcher: recursive inotify watcher with debouncing. Emits *batches* of coalesced events so the
//! consumer (re-indexer) works per burst, not per syscall. Raw `inotify_*` via libc, no extra crates.

use fm_core::listing::FileKind;
use fm_core::{walk, PathFilter, ReadOpts, WalkOpts};
use std::collections::HashMap;
use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum FsEvent {
    /// created, written-and-closed, or moved in
    Changed(PathBuf),
    /// deleted or moved out (may be a directory: consumers should drop the whole subtree)
    Removed(PathBuf),
    /// the kernel queue overflowed: events were lost, do a full rescan
    Overflow,
}

pub struct WatchHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    watched: Arc<AtomicUsize>,
}

impl WatchHandle {
    pub fn watched_dirs(&self) -> usize {
        self.watched.load(Relaxed)
    }
    pub fn stop(mut self) {
        self.shutdown();
    }
    fn shutdown(&mut self) {
        self.stop.store(true, Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
impl Drop for WatchHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct Inner {
    fd: libc::c_int,
    wds: HashMap<i32, PathBuf>,
    filter: Arc<PathFilter>,
    watched: Arc<AtomicUsize>,
    limit_warned: bool,
}

const MASK: u32 = libc::IN_CREATE | libc::IN_DELETE | libc::IN_CLOSE_WRITE | libc::IN_MOVED_FROM | libc::IN_MOVED_TO | libc::IN_ONLYDIR | libc::IN_DONT_FOLLOW;

impl Inner {
    fn add_watch(&mut self, dir: &Path) {
        let Ok(c) = CString::new(dir.as_os_str().as_bytes()) else { return };
        let wd = unsafe { libc::inotify_add_watch(self.fd, c.as_ptr(), MASK) };
        if wd < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::ENOSPC) {
                if !self.limit_warned {
                    tracing::warn!("inotify watch limit reached (raise fs.inotify.max_user_watches); coverage is partial");
                    self.limit_warned = true;
                }
            } else {
                tracing::debug!("cannot watch {}: {}", dir.display(), e);
            }
            return;
        }
        self.wds.insert(wd, dir.to_path_buf());
        self.watched.store(self.wds.len(), Relaxed);
    }

    /// Watch `root` and every directory below it; returns the files found there (for race compensation:
    /// files created before the watch existed would otherwise be missed).
    fn add_tree(&mut self, root: &Path) -> Vec<PathBuf> {
        let dirs = Mutex::new(vec![]);
        let files = Mutex::new(vec![]);
        let opts = WalkOpts { threads: 1, read: ReadOpts { read_meta: false, filter: Some(self.filter.clone()), ..Default::default() } };
        walk(&[root.to_path_buf()], &opts, |l| {
            dirs.lock().unwrap().push(l.path.clone());
            files.lock().unwrap().extend(l.iter().filter(|e| e.kind != FileKind::Dir).map(|e| l.path.join(e.name)));
        });
        for d in dirs.into_inner().unwrap() {
            self.add_watch(&d);
        }
        files.into_inner().unwrap()
    }

    fn handle(&mut self, buf: &[u8], pending: &mut HashMap<PathBuf, bool>, overflow: &mut bool) {
        let mut off = 0;
        while off + 16 <= buf.len() {
            let wd = i32::from_ne_bytes(buf[off..off + 4].try_into().unwrap());
            let mask = u32::from_ne_bytes(buf[off + 4..off + 8].try_into().unwrap());
            let len = u32::from_ne_bytes(buf[off + 12..off + 16].try_into().unwrap()) as usize;
            let raw = &buf[off + 16..(off + 16 + len).min(buf.len())];
            off += 16 + len;
            if mask & libc::IN_Q_OVERFLOW != 0 {
                *overflow = true;
                continue;
            }
            if mask & libc::IN_IGNORED != 0 {
                self.wds.remove(&wd);
                self.watched.store(self.wds.len(), Relaxed);
                continue;
            }
            let Some(dir) = self.wds.get(&wd).cloned() else { continue };
            let name = &raw[..raw.iter().position(|&c| c == 0).unwrap_or(raw.len())];
            if name.is_empty() || self.filter.skip_name(name) {
                continue;
            }
            let path = dir.join(std::ffi::OsStr::from_bytes(name));
            let is_dir = mask & libc::IN_ISDIR != 0;
            let gone = mask & (libc::IN_DELETE | libc::IN_MOVED_FROM) != 0;
            let arrived = mask & (libc::IN_CREATE | libc::IN_MOVED_TO | libc::IN_CLOSE_WRITE) != 0;
            if gone {
                pending.insert(path, true);
            } else if arrived {
                if is_dir {
                    if !self.filter.skip_path(&path) {
                        for f in self.add_tree(&path) {
                            pending.insert(f, false);
                        }
                    }
                } else {
                    pending.insert(path, false);
                }
            }
        }
    }
}

fn run(mut inner: Inner, stop: Arc<AtomicBool>, tx: Sender<Vec<FsEvent>>, debounce: Duration) {
    let mut buf = vec![0u8; 64 * 1024];
    let mut pending: HashMap<PathBuf, bool> = HashMap::new();
    let mut overflow = false;
    let mut last_event = Instant::now();
    while !stop.load(Relaxed) {
        let mut pfd = libc::pollfd { fd: inner.fd, events: libc::POLLIN, revents: 0 };
        let timeout = if pending.is_empty() && !overflow { 200 } else { debounce.as_millis().clamp(10, 200) as i32 };
        let n = unsafe { libc::poll(&mut pfd, 1, timeout) };
        if n > 0 {
            loop {
                let r = unsafe { libc::read(inner.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                if r <= 0 {
                    break; // EAGAIN (non-blocking): drained
                }
                inner.handle(&buf[..r as usize], &mut pending, &mut overflow);
                last_event = Instant::now();
            }
        }
        if (!pending.is_empty() || overflow) && last_event.elapsed() >= debounce {
            let mut batch: Vec<FsEvent> =
                pending.drain().map(|(p, removed)| if removed { FsEvent::Removed(p) } else { FsEvent::Changed(p) }).collect();
            if std::mem::take(&mut overflow) {
                batch.push(FsEvent::Overflow);
            }
            batch.sort();
            if tx.send(batch).is_err() {
                break; // consumer gone
            }
        }
    }
    unsafe { libc::close(inner.fd) };
}

/// Start watching `roots` recursively. Batches arrive on the returned channel once the filesystem has been
/// quiet for `debounce`. Dropping/stopping the handle stops the thread.
pub fn start(roots: &[PathBuf], filter: Arc<PathFilter>, debounce: Duration) -> io::Result<(WatchHandle, Receiver<Vec<FsEvent>>)> {
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let watched = Arc::new(AtomicUsize::new(0));
    let mut inner = Inner { fd, wds: HashMap::new(), filter, watched: watched.clone(), limit_warned: false };
    for r in roots {
        inner.add_tree(r);
    }
    tracing::info!("watching {} directories under {} root(s)", inner.wds.len(), roots.len());
    let (tx, rx) = channel();
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    let thread = std::thread::Builder::new().name("fm-watcher".into()).spawn(move || run(inner, s2, tx, debounce))?;
    Ok((WatchHandle { stop, thread: Some(thread), watched }, rx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn collect_until(rx: &Receiver<Vec<FsEvent>>, want: impl Fn(&[FsEvent]) -> bool) -> Vec<FsEvent> {
        let mut all = vec![];
        let end = Instant::now() + Duration::from_secs(5);
        while Instant::now() < end {
            if let Ok(b) = rx.recv_timeout(Duration::from_millis(200)) {
                all.extend(b);
                if want(&all) {
                    break;
                }
            }
        }
        all
    }

    fn start_on(dir: &Path) -> (WatchHandle, Receiver<Vec<FsEvent>>) {
        start(&[dir.to_path_buf()], Arc::new(PathFilter::default()), Duration::from_millis(80)).unwrap()
    }

    #[test]
    fn reports_create_modify_delete() {
        let t = tempfile::tempdir().unwrap();
        let (_h, rx) = start_on(t.path());
        let f = t.path().join("a.txt");
        fs::write(&f, "1").unwrap();
        let ev = collect_until(&rx, |e| e.contains(&FsEvent::Changed(f.clone())));
        assert!(ev.contains(&FsEvent::Changed(f.clone())), "{ev:?}");
        fs::remove_file(&f).unwrap();
        let ev = collect_until(&rx, |e| e.contains(&FsEvent::Removed(f.clone())));
        assert!(ev.contains(&FsEvent::Removed(f.clone())), "{ev:?}");
    }

    #[test]
    fn new_subdirectories_are_watched_recursively() {
        let t = tempfile::tempdir().unwrap();
        let (h, rx) = start_on(t.path());
        let before = h.watched_dirs();
        let d = t.path().join("x/y");
        fs::create_dir_all(&d).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let f = d.join("deep.txt");
        fs::write(&f, "1").unwrap();
        let ev = collect_until(&rx, |e| e.contains(&FsEvent::Changed(f.clone())));
        assert!(ev.contains(&FsEvent::Changed(f.clone())), "{ev:?}");
        assert!(h.watched_dirs() >= before + 2);
    }

    #[test]
    fn bursts_are_coalesced_into_one_event_per_path() {
        let t = tempfile::tempdir().unwrap();
        let (_h, rx) = start_on(t.path());
        let f = t.path().join("burst");
        for i in 0..20 {
            fs::write(&f, i.to_string()).unwrap();
        }
        let ev = collect_until(&rx, |e| e.contains(&FsEvent::Changed(f.clone())));
        assert_eq!(ev.iter().filter(|e| **e == FsEvent::Changed(f.clone())).count(), 1, "{ev:?}");
    }

    #[test]
    fn blacklisted_names_are_ignored() {
        let t = tempfile::tempdir().unwrap();
        let filter = Arc::new(PathFilter::new(&[], &["*.tmp".into()], &[], false));
        let (_h, rx) = start(&[t.path().to_path_buf()], filter, Duration::from_millis(80)).unwrap();
        fs::write(t.path().join("skip.tmp"), "1").unwrap();
        let keep = t.path().join("keep.txt");
        fs::write(&keep, "1").unwrap();
        let ev = collect_until(&rx, |e| e.contains(&FsEvent::Changed(keep.clone())));
        assert!(ev.contains(&FsEvent::Changed(keep)));
        assert!(!ev.iter().any(|e| matches!(e, FsEvent::Changed(p) if p.extension().is_some_and(|x| x == "tmp"))));
    }

    #[test]
    fn stop_joins_the_thread() {
        let t = tempfile::tempdir().unwrap();
        let (h, _rx) = start_on(t.path());
        h.stop(); // must return promptly
    }
}
