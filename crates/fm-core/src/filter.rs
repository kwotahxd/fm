use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Blacklist applied *before* any stat call: excluded names cost nothing but the getdents parse.
#[derive(Clone, Debug, Default)]
pub struct PathFilter {
    names: HashSet<Vec<u8>>,
    globs: Vec<Vec<u8>>,
    prefixes: Vec<PathBuf>,
    pub skip_hidden: bool,
}

impl PathFilter {
    pub fn new(names: &[String], globs: &[String], prefixes: &[PathBuf], skip_hidden: bool) -> Self {
        Self {
            names: names.iter().map(|s| s.as_bytes().to_vec()).collect(),
            globs: globs.iter().map(|s| s.as_bytes().to_vec()).collect(),
            prefixes: prefixes.to_vec(),
            skip_hidden,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty() && self.globs.is_empty() && self.prefixes.is_empty() && !self.skip_hidden
    }

    /// Name-only rules (cheap, evaluated per directory entry).
    pub fn skip_name(&self, name: &[u8]) -> bool {
        if self.skip_hidden && name.first() == Some(&b'.') {
            return true;
        }
        if self.names.contains(name) {
            return true;
        }
        self.globs.iter().any(|g| glob_match(g, name))
    }

    /// Full-path rules (evaluated once per directory before descending).
    pub fn skip_path(&self, path: &Path) -> bool {
        self.prefixes.iter().any(|p| path.starts_with(p))
    }
}

/// Minimal glob: `*` (any run) and `?` (any byte). Iterative, no recursion / no backtracking blowup.
pub fn glob_match(pat: &[u8], s: &[u8]) -> bool {
    let (mut p, mut i) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while i < s.len() {
        if p < pat.len() && (pat[p] == b'?' || pat[p] == s[i]) {
            p += 1;
            i += 1;
        } else if p < pat.len() && pat[p] == b'*' {
            star = p;
            mark = i;
            p += 1;
        } else if star != usize::MAX {
            p = star + 1;
            mark += 1;
            i = mark;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == b'*' {
        p += 1;
    }
    p == pat.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob_match(b"*.tmp", b"a.tmp"));
        assert!(glob_match(b"*~", b"file~"));
        assert!(glob_match(b"a?c", b"abc"));
        assert!(glob_match(b"*", b""));
        assert!(!glob_match(b"*.tmp", b"a.tmpx"));
        assert!(!glob_match(b"a?c", b"ac"));
        assert!(glob_match(b"a*b*c", b"aXXbYYc"));
    }

    #[test]
    fn filter_rules() {
        let f = PathFilter::new(&[".git".into()], &["*.swp".into()], &[PathBuf::from("/proc")], true);
        assert!(f.skip_name(b".git"));
        assert!(f.skip_name(b".hidden"));
        assert!(f.skip_name(b"x.swp"));
        assert!(!f.skip_name(b"x.txt"));
        assert!(f.skip_path(Path::new("/proc/1/fd")));
        assert!(!f.skip_path(Path::new("/procession")));
    }
}
