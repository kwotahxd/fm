//! One-directory reader: `open(O_DIRECTORY)` → `getdents64` loop → one `fstatat(AT_SYMLINK_NOFOLLOW)`
//! per entry. Names live in a single arena (no per-entry allocation).

use crate::filter::PathFilter;
use std::cell::RefCell;
use std::ffi::{CString, OsStr};
use std::io;
use std::mem::MaybeUninit;
use std::os::raw::c_char;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    Unknown,
    Fifo,
    CharDev,
    Dir,
    BlockDev,
    File,
    Symlink,
    Socket,
}

impl FileKind {
    pub fn from_dtype(t: u8) -> Self {
        match t {
            libc::DT_FIFO => Self::Fifo,
            libc::DT_CHR => Self::CharDev,
            libc::DT_DIR => Self::Dir,
            libc::DT_BLK => Self::BlockDev,
            libc::DT_REG => Self::File,
            libc::DT_LNK => Self::Symlink,
            libc::DT_SOCK => Self::Socket,
            _ => Self::Unknown,
        }
    }
    pub fn from_mode(m: u32) -> Self {
        match m & libc::S_IFMT {
            libc::S_IFIFO => Self::Fifo,
            libc::S_IFCHR => Self::CharDev,
            libc::S_IFDIR => Self::Dir,
            libc::S_IFBLK => Self::BlockDev,
            libc::S_IFREG => Self::File,
            libc::S_IFLNK => Self::Symlink,
            libc::S_IFSOCK => Self::Socket,
            _ => Self::Unknown,
        }
    }
    /// The first character of `ls -l`'s mode column.
    pub fn type_char(self) -> u8 {
        match self {
            Self::File => b'-',
            Self::Dir => b'd',
            Self::Symlink => b'l',
            Self::CharDev => b'c',
            Self::BlockDev => b'b',
            Self::Fifo => b'p',
            Self::Socket => b's',
            Self::Unknown => b'?',
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Meta {
    pub dev: u64,
    pub size: u64,
    pub blocks: u64,
    pub mode: u32,
    pub nlink: u64,
    pub uid: u32,
    pub gid: u32,
    pub mtime_ns: i64,
}

impl Meta {
    fn from_stat(st: &libc::stat) -> Meta {
        Meta {
            dev: st.st_dev as u64,
            size: st.st_size as u64,
            blocks: st.st_blocks as u64,
            mode: st.st_mode as u32,
            nlink: st.st_nlink as u64,
            uid: st.st_uid,
            gid: st.st_gid,
            mtime_ns: st.st_mtime as i64 * 1_000_000_000 + st.st_mtime_nsec as i64,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct RawEntry {
    name_off: u32,
    name_len: u32,
    ino: u64,
    kind: FileKind,
    meta: Meta,
}

#[derive(Clone, Debug)]
pub struct ReadOpts {
    /// call fstatat for every entry (needed for sizes/mtimes); otherwise only unknown d_type entries
    pub read_meta: bool,
    /// getdents64 buffer size in bytes
    pub buf_size: usize,
    /// threads used to stat one very large directory
    pub stat_threads: usize,
    /// only parallelise stat above this many entries
    pub par_threshold: usize,
    pub filter: Option<Arc<PathFilter>>,
}

impl Default for ReadOpts {
    fn default() -> Self {
        Self { read_meta: true, buf_size: 256 * 1024, stat_threads: 1, par_threshold: 20_000, filter: None }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    None,
    Name,
    Size,
    Mtime,
}

pub struct EntryRef<'a> {
    pub name: &'a OsStr,
    pub ino: u64,
    pub kind: FileKind,
    /// `None` when metadata was not requested or the entry vanished before fstatat
    pub meta: Option<&'a Meta>,
}

pub struct DirListing {
    pub path: PathBuf,
    pub dir_mtime_ns: i64,
    pub has_meta: bool,
    pub loaded_at: Instant,
    arena: Vec<u8>,
    entries: Vec<RawEntry>,
}

impl DirListing {
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    fn name_bytes(&self, i: usize) -> &[u8] {
        let e = &self.entries[i];
        &self.arena[e.name_off as usize..(e.name_off + e.name_len) as usize]
    }
    pub fn get(&self, i: usize) -> EntryRef<'_> {
        let e = &self.entries[i];
        EntryRef {
            name: OsStr::from_bytes(self.name_bytes(i)),
            ino: e.ino,
            kind: e.kind,
            meta: (self.has_meta && e.meta.mode != 0).then_some(&e.meta),
        }
    }
    pub fn iter(&self) -> impl Iterator<Item = EntryRef<'_>> + '_ {
        (0..self.entries.len()).map(move |i| self.get(i))
    }
    /// Indices in display order. The listing itself stays immutable (it is shared through the cache).
    pub fn order(&self, key: SortKey, reverse: bool) -> Vec<usize> {
        let mut idx: Vec<usize> = (0..self.entries.len()).collect();
        match key {
            SortKey::None => {}
            SortKey::Name => idx.sort_unstable_by(|&a, &b| self.name_bytes(a).cmp(self.name_bytes(b))),
            SortKey::Size => idx.sort_unstable_by(|&a, &b| {
                self.entries[b].meta.size.cmp(&self.entries[a].meta.size).then_with(|| self.name_bytes(a).cmp(self.name_bytes(b)))
            }),
            SortKey::Mtime => idx.sort_unstable_by(|&a, &b| {
                self.entries[b].meta.mtime_ns.cmp(&self.entries[a].meta.mtime_ns).then_with(|| self.name_bytes(a).cmp(self.name_bytes(b)))
            }),
        }
        if reverse {
            idx.reverse();
        }
        idx
    }
}

struct DirFd(libc::c_int);
impl Drop for DirFd {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

fn cstr(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

fn open_dir(path: &Path) -> io::Result<DirFd> {
    let c = cstr(path)?;
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(DirFd(fd))
    }
}

fn getdents64(fd: libc::c_int, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        let n = unsafe { libc::syscall(libc::SYS_getdents64, fd, buf.as_mut_ptr(), buf.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        return Ok(n as usize);
    }
}

/// mtime of a directory (one `stat`), used by the cache for validation.
pub fn dir_mtime_ns(path: &Path) -> io::Result<i64> {
    let c = cstr(path)?;
    let mut st = MaybeUninit::<libc::stat>::zeroed();
    if unsafe { libc::stat(c.as_ptr(), st.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let st = unsafe { st.assume_init() };
    Ok(st.st_mtime as i64 * 1_000_000_000 + st.st_mtime_nsec as i64)
}

thread_local! {
    static BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

fn stat_chunk(fd: libc::c_int, arena: &[u8], chunk: &mut [RawEntry]) {
    for e in chunk {
        let mut st = MaybeUninit::<libc::stat>::zeroed();
        // the arena stores every name NUL-terminated, so the pointer can go straight to the kernel
        let rc = unsafe {
            libc::fstatat(fd, arena.as_ptr().add(e.name_off as usize) as *const c_char, st.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW)
        };
        if rc == 0 {
            let st = unsafe { st.assume_init() };
            e.meta = Meta::from_stat(&st);
            e.kind = FileKind::from_mode(e.meta.mode);
        }
    }
}

pub fn read_dir(path: &Path, opts: &ReadOpts) -> io::Result<DirListing> {
    let dir = open_dir(path)?;
    let mut st = MaybeUninit::<libc::stat>::zeroed();
    if unsafe { libc::fstat(dir.0, st.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let st = unsafe { st.assume_init() };
    let dir_mtime_ns = st.st_mtime as i64 * 1_000_000_000 + st.st_mtime_nsec as i64;

    let mut arena: Vec<u8> = Vec::with_capacity(4096);
    let mut entries: Vec<RawEntry> = Vec::with_capacity(64);
    let buf_size = opts.buf_size.max(1024);
    let filter = opts.filter.as_deref();

    BUF.with(|cell| -> io::Result<()> {
        let mut b = cell.borrow_mut();
        if b.len() < buf_size {
            b.resize(buf_size, 0);
        }
        let buf = &mut b[..buf_size];
        loop {
            let n = getdents64(dir.0, buf)?;
            if n == 0 {
                return Ok(());
            }
            let mut off = 0usize;
            while off < n {
                // struct linux_dirent64 { u64 d_ino; i64 d_off; u16 d_reclen; u8 d_type; char d_name[]; }
                let rec = &buf[off..];
                let ino = u64::from_ne_bytes(rec[0..8].try_into().unwrap());
                let reclen = u16::from_ne_bytes(rec[16..18].try_into().unwrap()) as usize;
                let dtype = rec[18];
                let raw = &rec[19..reclen];
                let nul = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
                let name = &raw[..nul];
                off += reclen;
                if name == b"." || name == b".." {
                    continue;
                }
                if let Some(f) = filter {
                    if f.skip_name(name) {
                        continue;
                    }
                }
                let name_off = arena.len() as u32;
                arena.extend_from_slice(name);
                arena.push(0);
                entries.push(RawEntry { name_off, name_len: name.len() as u32, ino, kind: FileKind::from_dtype(dtype), meta: Meta::default() });
            }
        }
    })?;

    if opts.read_meta {
        let n = entries.len();
        let threads = opts.stat_threads.max(1);
        if threads > 1 && n >= opts.par_threshold {
            let chunk = n.div_ceil(threads);
            let (fd, ar) = (dir.0, &arena[..]);
            std::thread::scope(|s| {
                for c in entries.chunks_mut(chunk) {
                    s.spawn(move || stat_chunk(fd, ar, c));
                }
            });
        } else {
            stat_chunk(dir.0, &arena, &mut entries);
        }
    } else {
        // filesystems without d_type: resolve just those entries
        for e in entries.iter_mut().filter(|e| e.kind == FileKind::Unknown) {
            stat_chunk(dir.0, &arena, std::slice::from_mut(e));
        }
    }

    Ok(DirListing { path: path.to_path_buf(), dir_mtime_ns, has_meta: opts.read_meta, loaded_at: Instant::now(), arena, entries })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn make(dir: &Path, n: usize) {
        for i in 0..n {
            std::fs::write(dir.join(format!("f{i:05}.txt")), vec![b'x'; i % 97]).unwrap();
        }
        std::fs::create_dir(dir.join("sub")).unwrap();
        std::os::unix::fs::symlink("f00000.txt", dir.join("link")).unwrap();
    }

    #[test]
    fn matches_std_read_dir() {
        let t = tempfile::tempdir().unwrap();
        make(t.path(), 300);
        let l = read_dir(t.path(), &ReadOpts::default()).unwrap();
        assert_eq!(l.len(), 302);
        let mut ours: Vec<(String, u64, bool)> =
            l.iter().map(|e| (e.name.to_string_lossy().into_owned(), e.meta.unwrap().size, e.kind == FileKind::Dir)).collect();
        ours.sort();
        let mut theirs: Vec<(String, u64, bool)> = std::fs::read_dir(t.path())
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                let m = std::fs::symlink_metadata(e.path()).unwrap();
                (e.file_name().to_string_lossy().into_owned(), m.size(), m.is_dir())
            })
            .collect();
        theirs.sort();
        assert_eq!(ours, theirs);
    }

    #[test]
    fn small_buffer_forces_multiple_getdents_calls() {
        let t = tempfile::tempdir().unwrap();
        make(t.path(), 2000);
        let opts = ReadOpts { buf_size: 1024, ..Default::default() };
        let l = read_dir(t.path(), &opts).unwrap();
        assert_eq!(l.len(), 2002);
    }

    #[test]
    fn symlink_is_not_followed_and_kinds_are_right() {
        let t = tempfile::tempdir().unwrap();
        make(t.path(), 3);
        let l = read_dir(t.path(), &ReadOpts::default()).unwrap();
        let kinds: std::collections::HashMap<String, FileKind> =
            l.iter().map(|e| (e.name.to_string_lossy().into_owned(), e.kind)).collect();
        assert_eq!(kinds["link"], FileKind::Symlink);
        assert_eq!(kinds["sub"], FileKind::Dir);
        assert_eq!(kinds["f00001.txt"], FileKind::File);
    }

    #[test]
    fn no_meta_mode_still_reports_kinds() {
        let t = tempfile::tempdir().unwrap();
        make(t.path(), 3);
        let l = read_dir(t.path(), &ReadOpts { read_meta: false, ..Default::default() }).unwrap();
        assert!(l.iter().all(|e| e.meta.is_none()));
        assert!(l.iter().any(|e| e.kind == FileKind::Dir));
    }

    #[test]
    fn parallel_stat_gives_same_result() {
        let t = tempfile::tempdir().unwrap();
        make(t.path(), 500);
        let a = read_dir(t.path(), &ReadOpts::default()).unwrap();
        let b = read_dir(t.path(), &ReadOpts { stat_threads: 4, par_threshold: 10, ..Default::default() }).unwrap();
        let sum = |l: &DirListing| l.iter().map(|e| e.meta.unwrap().size).sum::<u64>();
        assert_eq!(sum(&a), sum(&b));
        assert_eq!(a.len(), b.len());
    }

    #[test]
    fn ordering() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("b"), "1").unwrap();
        std::fs::write(t.path().join("a"), "123").unwrap();
        std::fs::write(t.path().join("c"), "").unwrap();
        let l = read_dir(t.path(), &ReadOpts::default()).unwrap();
        let names = |o: Vec<usize>| o.into_iter().map(|i| l.get(i).name.to_string_lossy().into_owned()).collect::<Vec<_>>();
        assert_eq!(names(l.order(SortKey::Name, false)), ["a", "b", "c"]);
        assert_eq!(names(l.order(SortKey::Size, false)), ["a", "b", "c"]);
        assert_eq!(names(l.order(SortKey::Name, true)), ["c", "b", "a"]);
    }

    #[test]
    fn errors_are_reported() {
        assert!(read_dir(Path::new("/definitely/not/here"), &ReadOpts::default()).is_err());
    }

    #[test]
    fn filter_skips_before_stat() {
        let t = tempfile::tempdir().unwrap();
        make(t.path(), 5);
        let f = PathFilter::new(&["sub".into()], &["*.txt".into()], &[], false);
        let l = read_dir(t.path(), &ReadOpts { filter: Some(Arc::new(f)), ..Default::default() }).unwrap();
        let names: Vec<_> = l.iter().map(|e| e.name.to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["link"]);
    }
}
