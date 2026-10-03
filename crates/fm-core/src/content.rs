//! Lazy content access: nothing here runs during a directory walk.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

/// blake3 of the file, streamed. `None` when the file is larger than `max_bytes`.
pub fn hash_file(path: &Path, max_bytes: Option<u64>) -> io::Result<Option<String>> {
    let mut f = File::open(path)?;
    if let Some(max) = max_bytes {
        if f.metadata()?.len() > max {
            return Ok(None);
        }
    }
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 128 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(Some(h.finalize().to_hex().to_string()))
}

pub fn read_head(path: &Path, n: usize) -> io::Result<Vec<u8>> {
    let mut f = File::open(path)?;
    let mut buf = vec![0u8; n];
    let mut got = 0;
    while got < n {
        let r = f.read(&mut buf[got..])?;
        if r == 0 {
            break;
        }
        got += r;
    }
    buf.truncate(got);
    Ok(buf)
}

/// Best-effort MIME: extension first, magic bytes when the extension says nothing or is generic.
pub fn detect_mime(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_string_lossy();
    let by_ext = crate::mime::from_ext(&name);
    match by_ext {
        Some(m) if m != "application/octet-stream" => Some(m.to_string()),
        _ => read_head(path, 8192).ok().and_then(|h| crate::mime::sniff(&h)).map(str::to_string).or(by_ext.map(str::to_string)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_and_limits() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("a");
        std::fs::write(&p, "hello").unwrap();
        let h = hash_file(&p, None).unwrap().unwrap();
        assert_eq!(h, blake3::hash(b"hello").to_hex().to_string());
        assert_eq!(hash_file(&p, Some(2)).unwrap(), None);
    }

    #[test]
    fn detect_by_magic_when_no_extension() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("noext");
        std::fs::write(&p, b"%PDF-1.4\n").unwrap();
        assert_eq!(detect_mime(&p).as_deref(), Some("application/pdf"));
    }
}
