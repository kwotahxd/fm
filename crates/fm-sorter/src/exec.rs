use crate::plan::{Plan, PlannedOp};
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub enum OpOutcome {
    Applied,
    Failed(String),
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExecReport {
    pub applied: usize,
    pub failed: usize,
}

fn cpath(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))
}

/// Move `src` → `dst` without ever overwriting: `renameat2(RENAME_NOREPLACE)`, with a checked
/// fallback for filesystems lacking it and copy+delete across devices.
pub fn move_no_replace(src: &Path, dst: &Path) -> io::Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    let (cs, cd) = (cpath(src)?, cpath(dst)?);
    const RENAME_NOREPLACE: libc::c_uint = 1;
    let rc = unsafe { libc::syscall(libc::SYS_renameat2, libc::AT_FDCWD, cs.as_ptr(), libc::AT_FDCWD, cd.as_ptr(), RENAME_NOREPLACE) };
    if rc == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EEXIST) => Err(io::Error::new(io::ErrorKind::AlreadyExists, "destination exists")),
        Some(libc::EXDEV) => copy_then_remove(src, dst),
        // flag unsupported by this kernel/filesystem: plain rename after an explicit existence check
        Some(libc::EINVAL) | Some(libc::ENOSYS) | Some(libc::ENOTSUP) => {
            if dst.symlink_metadata().is_ok() {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "destination exists"));
            }
            match fs::rename(src, dst) {
                Err(e) if e.raw_os_error() == Some(libc::EXDEV) => copy_then_remove(src, dst),
                r => r,
            }
        }
        _ => Err(err),
    }
}

fn copy_then_remove(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    if !meta.file_type().is_file() {
        return Err(io::Error::other("cross-device move is only supported for regular files"));
    }
    let mut from = fs::File::open(src)?;
    let mut to = fs::OpenOptions::new().write(true).create_new(true).open(dst)?; // O_EXCL: never overwrite
    let copied = io::copy(&mut from, &mut to).and_then(|_| to.sync_all()).and_then(|_| fs::set_permissions(dst, meta.permissions()));
    if let Err(e) = copied {
        let _ = fs::remove_file(dst);
        return Err(e);
    }
    // keep mtime: sorting must not change it (dates, staleness checks depend on it)
    let times = [
        libc::timespec { tv_sec: meta.atime(), tv_nsec: meta.atime_nsec() },
        libc::timespec { tv_sec: meta.mtime(), tv_nsec: meta.mtime_nsec() },
    ];
    let cd = cpath(dst)?;
    unsafe { libc::utimensat(libc::AT_FDCWD, cd.as_ptr(), times.as_ptr(), 0) };
    if let Err(e) = fs::remove_file(src) {
        let _ = fs::remove_file(dst);
        return Err(e);
    }
    Ok(())
}

/// Execute a plan. `on_event(seq, op, outcome)` fires after every operation — persist the journal there.
pub fn execute(plan: &Plan, on_event: &mut dyn FnMut(usize, &PlannedOp, &OpOutcome)) -> ExecReport {
    let mut rep = ExecReport::default();
    for (seq, op) in plan.ops.iter().enumerate() {
        let outcome = match move_no_replace(&op.src, &op.dst) {
            Ok(()) => {
                rep.applied += 1;
                OpOutcome::Applied
            }
            Err(e) => {
                rep.failed += 1;
                tracing::warn!("move {} → {} failed: {}", op.src.display(), op.dst.display(), e);
                OpOutcome::Failed(e.to_string())
            }
        };
        on_event(seq, op, &outcome);
    }
    rep
}

#[derive(Debug, Clone)]
pub struct UndoItem {
    pub id: i64,
    pub src: PathBuf,
    pub dst: PathBuf,
}

#[derive(Debug, Clone, PartialEq)]
pub enum UndoOutcome {
    Restored,
    /// the moved file is no longer at its destination (moved/deleted since)
    Missing,
    /// the original location is occupied again, or the move failed
    Blocked(String),
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct UndoReport {
    pub restored: usize,
    pub missing: usize,
    pub blocked: usize,
}

fn remove_empty_parents(mut dir: PathBuf, stop: &Path) {
    while dir != stop && dir.starts_with(stop) {
        if fs::remove_dir(&dir).is_err() {
            break; // not empty (or not ours): stop
        }
        match dir.parent() {
            Some(p) => dir = p.to_path_buf(),
            None => break,
        }
    }
}

/// Reverse a batch: last operation first, never overwriting anything.
pub fn undo(items: &[UndoItem], root: &Path, on_event: &mut dyn FnMut(&UndoItem, &UndoOutcome)) -> UndoReport {
    let mut rep = UndoReport::default();
    for it in items.iter().rev() {
        let outcome = if it.dst.symlink_metadata().is_err() {
            rep.missing += 1;
            UndoOutcome::Missing
        } else {
            match move_no_replace(&it.dst, &it.src) {
                Ok(()) => {
                    rep.restored += 1;
                    if let Some(p) = it.dst.parent() {
                        remove_empty_parents(p.to_path_buf(), root);
                    }
                    UndoOutcome::Restored
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    rep.blocked += 1;
                    UndoOutcome::Blocked("original location is occupied".into())
                }
                Err(e) => {
                    rep.blocked += 1;
                    UndoOutcome::Blocked(e.to_string())
                }
            }
        };
        on_event(it, &outcome);
    }
    rep
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{plan, SortOptions};
    use fm_types::{Conflict, FileRecord};

    fn rec(dir: &Path, name: &str, mime: &str, body: &str) -> FileRecord {
        let p = dir.join(name);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, body).unwrap();
        FileRecord { path: p.to_string_lossy().into(), name: p.file_name().unwrap().to_string_lossy().into(), mime: Some(mime.into()), ..Default::default() }
    }

    fn items(p: &Plan) -> Vec<UndoItem> {
        p.ops.iter().enumerate().map(|(i, o)| UndoItem { id: i as i64, src: o.src.clone(), dst: o.dst.clone() }).collect()
    }

    #[test]
    fn execute_then_undo_restores_everything_and_cleans_dirs() {
        let t = tempfile::tempdir().unwrap();
        let files = vec![rec(t.path(), "a.jpg", "image/jpeg", "A"), rec(t.path(), "sub/b.jpg", "image/jpeg", "B"), rec(t.path(), "c.rs", "text/x-rust", "C")];
        let p = plan(&files, &crate::builtin::get("by-type").unwrap(), &SortOptions { root: t.path().into(), conflict: Conflict::Rename }).unwrap();
        let mut events = vec![];
        let rep = execute(&p, &mut |seq, op, out| events.push((seq, op.dst.clone(), out.clone())));
        assert_eq!(rep, ExecReport { applied: 3, failed: 0 });
        assert_eq!(events.len(), 3);
        assert_eq!(fs::read_to_string(t.path().join("Images/a.jpg")).unwrap(), "A");
        assert!(!t.path().join("a.jpg").exists());

        let ur = undo(&items(&p), t.path(), &mut |_, o| assert_eq!(*o, UndoOutcome::Restored));
        assert_eq!(ur.restored, 3);
        for f in &files {
            assert!(Path::new(&f.path).exists(), "{}", f.path);
        }
        assert!(!t.path().join("Images").exists() && !t.path().join("Code").exists(), "empty dest dirs are removed");
        assert!(t.path().join("sub").exists(), "pre-existing dirs stay");
    }

    #[test]
    fn execute_never_overwrites_a_file_that_appeared_after_planning() {
        let t = tempfile::tempdir().unwrap();
        let f = rec(t.path(), "a.jpg", "image/jpeg", "NEW");
        let p = plan(&[f], &crate::builtin::get("by-type").unwrap(), &SortOptions { root: t.path().into(), conflict: Conflict::Rename }).unwrap();
        fs::create_dir_all(t.path().join("Images")).unwrap();
        fs::write(t.path().join("Images/a.jpg"), "PRECIOUS").unwrap(); // race
        let mut outs = vec![];
        let rep = execute(&p, &mut |_, _, o| outs.push(o.clone()));
        assert_eq!(rep.failed, 1);
        assert!(matches!(outs[0], OpOutcome::Failed(_)));
        assert_eq!(fs::read_to_string(t.path().join("Images/a.jpg")).unwrap(), "PRECIOUS");
        assert!(t.path().join("a.jpg").exists());
    }

    #[test]
    fn missing_source_is_a_reported_failure() {
        let t = tempfile::tempdir().unwrap();
        let f = rec(t.path(), "a.jpg", "image/jpeg", "x");
        let p = plan(&[f], &crate::builtin::get("by-type").unwrap(), &SortOptions { root: t.path().into(), conflict: Conflict::Rename }).unwrap();
        fs::remove_file(t.path().join("a.jpg")).unwrap();
        assert_eq!(execute(&p, &mut |_, _, _| {}).failed, 1);
    }

    #[test]
    fn undo_is_blocked_when_original_spot_is_taken_and_reports_missing() {
        let t = tempfile::tempdir().unwrap();
        let files = vec![rec(t.path(), "a.jpg", "image/jpeg", "A"), rec(t.path(), "b.jpg", "image/jpeg", "B")];
        let p = plan(&files, &crate::builtin::get("by-type").unwrap(), &SortOptions { root: t.path().into(), conflict: Conflict::Rename }).unwrap();
        execute(&p, &mut |_, _, _| {});
        fs::write(t.path().join("a.jpg"), "someone else's file").unwrap(); // original spot re-occupied
        fs::remove_file(t.path().join("Images/b.jpg")).unwrap(); // user deleted the other one
        let mut outs = vec![];
        let ur = undo(&items(&p), t.path(), &mut |_, o| outs.push(o.clone()));
        assert_eq!((ur.restored, ur.blocked, ur.missing), (0, 1, 1));
        assert_eq!(fs::read_to_string(t.path().join("a.jpg")).unwrap(), "someone else's file");
        assert_eq!(fs::read_to_string(t.path().join("Images/a.jpg")).unwrap(), "A", "nothing was lost");
    }

    #[test]
    fn move_across_dirs_keeps_mtime_when_copying() {
        let t = tempfile::tempdir().unwrap();
        let a = t.path().join("a");
        fs::write(&a, "data").unwrap();
        let before = fs::metadata(&a).unwrap().mtime();
        copy_then_remove(&a, &t.path().join("b")).unwrap();
        assert!(!a.exists());
        assert_eq!(fs::metadata(t.path().join("b")).unwrap().mtime(), before);
        assert!(copy_then_remove(&t.path().join("b"), &t.path().join("b")).is_err(), "never clobbers");
    }
}
