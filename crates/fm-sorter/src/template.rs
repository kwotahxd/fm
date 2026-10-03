use crate::SortError;
use fm_types::FileRecord;

#[derive(Debug, Clone, PartialEq)]
enum Seg {
    Lit(String),
    Var(String),
}

/// `Photos/{year}/{attr.event}` — parsed once, rendered per file.
#[derive(Debug, Clone, PartialEq)]
pub struct Template {
    segs: Vec<Seg>,
}

const KNOWN: &[&str] = &["name", "stem", "ext", "year", "month", "day", "mime", "mime_major", "kind", "category", "tag", "size_kb"];

impl Template {
    pub fn parse(src: &str) -> Result<Self, SortError> {
        let mut segs = vec![];
        let mut lit = String::new();
        let mut it = src.chars().peekable();
        while let Some(c) = it.next() {
            match c {
                '{' => {
                    let mut var = String::new();
                    loop {
                        match it.next() {
                            Some('}') => break,
                            Some(ch) => var.push(ch),
                            None => return Err(SortError::InvalidRule(format!("unclosed '{{' in template '{src}'"))),
                        }
                    }
                    let known = KNOWN.contains(&var.as_str()) || var.strip_prefix("attr.").is_some_and(|a| !a.is_empty());
                    if !known {
                        return Err(SortError::InvalidRule(format!("unknown placeholder '{{{var}}}' (known: {}, attr.<name>)", KNOWN.join(", "))));
                    }
                    if !lit.is_empty() {
                        segs.push(Seg::Lit(std::mem::take(&mut lit)));
                    }
                    segs.push(Seg::Var(var));
                }
                '}' => return Err(SortError::InvalidRule(format!("stray '}}' in template '{src}'"))),
                c => lit.push(c),
            }
        }
        if !lit.is_empty() {
            segs.push(Seg::Lit(lit));
        }
        Ok(Self { segs })
    }

    /// Names of all placeholders used.
    pub fn vars(&self) -> impl Iterator<Item = &str> {
        self.segs.iter().filter_map(|s| if let Seg::Var(v) = s { Some(v.as_str()) } else { None })
    }

    /// Render for one file. `Err(var)` = the value of that placeholder is unknown for this file
    /// (typically: not analysed yet).
    pub fn render(&self, f: &FileRecord) -> Result<String, String> {
        let mut out = String::new();
        for s in &self.segs {
            match s {
                Seg::Lit(l) => out.push_str(l),
                Seg::Var(v) => out.push_str(&sanitize(&value_of(f, v).ok_or_else(|| v.clone())?)),
            }
        }
        Ok(out)
    }
}

/// A single path component: no separators, no control chars, never empty, never `.` / `..`.
/// (Leading dots are kept so dotfile names survive; without a `/` they cannot traverse anything.)
pub fn sanitize(s: &str) -> String {
    let cleaned: String = s.chars().map(|c| if c == '/' || c == '\\' || c == '\0' || c.is_control() { '_' } else { c }).collect();
    let t: String = cleaned.trim().chars().take(120).collect();
    if t.is_empty() || t == "." || t == ".." {
        "_".into()
    } else {
        t
    }
}

/// Coarse content kind derived from MIME + extension.
pub fn kind_of(mime: Option<&str>, ext: Option<&str>) -> &'static str {
    let ext = ext.unwrap_or("");
    let m = mime.unwrap_or("");
    if m.starts_with("image/") {
        "Images"
    } else if m.starts_with("video/") {
        "Video"
    } else if m.starts_with("audio/") {
        "Audio"
    } else if matches!(ext, "zip" | "tar" | "gz" | "tgz" | "xz" | "bz2" | "zst" | "7z" | "rar" | "iso") {
        "Archives"
    } else if matches!(ext, "rs" | "py" | "c" | "h" | "cpp" | "cc" | "hpp" | "go" | "java" | "js" | "ts" | "tsx" | "jsx" | "sh" | "nix" | "sql" | "rb" | "php" | "lua") {
        "Code"
    } else if m == "application/x-elf" || matches!(ext, "so" | "o" | "a" | "exe" | "dll" | "bin") {
        "Binaries"
    } else if m == "application/pdf" || m.starts_with("text/") || m.contains("officedocument") || m.contains("msword") || m.contains("opendocument") || m == "application/epub+zip" {
        "Documents"
    } else {
        "Other"
    }
}

/// Civil date (UTC) from unix seconds — Howard Hinnant's algorithm, no libc / no time crate.
pub fn civil_from_secs(secs: i64) -> (i64, u32, u32) {
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn value_of(f: &FileRecord, var: &str) -> Option<String> {
    let (y, m, d) = civil_from_secs(f.mtime_ns.div_euclid(1_000_000_000));
    Some(match var {
        "name" => f.name.clone(),
        "stem" => f.stem(),
        "ext" => f.ext().unwrap_or_else(|| "no-ext".into()),
        "year" => format!("{y:04}"),
        "month" => format!("{m:02}"),
        "day" => format!("{d:02}"),
        "mime" => f.mime.clone()?,
        "mime_major" => f.mime.as_ref()?.split('/').next()?.to_string(),
        "kind" => kind_of(f.mime.as_deref(), f.ext().as_deref()).to_string(),
        "size_kb" => (f.size / 1024).to_string(),
        "category" => f.category.clone().filter(|c| !c.is_empty())?,
        "tag" => f.tags.first()?.clone(),
        a => f.attrs.get(a.strip_prefix("attr.")?).filter(|v| !v.trim().is_empty())?.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec() -> FileRecord {
        FileRecord {
            name: "IMG_1.JPG".into(),
            mtime_ns: 1_700_000_000 * 1_000_000_000, // 2023-11-14 22:13:20 UTC
            mime: Some("image/jpeg".into()),
            category: Some("Travel".into()),
            tags: vec!["beach".into()],
            attrs: [("event".to_string(), "Summer/Trip".to_string())].into(),
            ..Default::default()
        }
    }

    #[test]
    fn renders_and_sanitizes_values() {
        let t = Template::parse("{kind}/{year}/{month}/{category}/{attr.event}/{tag}").unwrap();
        assert_eq!(t.render(&rec()).unwrap(), "Images/2023/11/Travel/Summer_Trip/beach");
        assert_eq!(Template::parse("{stem}.{ext}").unwrap().render(&rec()).unwrap(), "IMG_1.jpg");
    }

    #[test]
    fn missing_value_is_reported() {
        let mut r = rec();
        r.category = None;
        assert_eq!(Template::parse("{category}").unwrap().render(&r), Err("category".into()));
        assert_eq!(Template::parse("{attr.nope}").unwrap().render(&rec()), Err("attr.nope".into()));
    }

    #[test]
    fn parse_errors() {
        assert!(Template::parse("{oops}").is_err());
        assert!(Template::parse("{year").is_err());
        assert!(Template::parse("a}b").is_err());
        assert!(Template::parse("{attr.}").is_err());
    }

    #[test]
    fn sanitize_blocks_traversal() {
        let s = sanitize("../../etc");
        assert_eq!(s, ".._.._etc");
        assert!(!s.contains('/'));
        assert_eq!(sanitize(".."), "_");
        assert_eq!(sanitize("  .hidden "), ".hidden");
        assert_eq!(sanitize(""), "_");
    }

    #[test]
    fn dates() {
        assert_eq!(civil_from_secs(0), (1970, 1, 1));
        assert_eq!(civil_from_secs(1_700_000_000), (2023, 11, 14));
        assert_eq!(civil_from_secs(951_782_400), (2000, 2, 29));
        assert_eq!(civil_from_secs(-1), (1969, 12, 31));
    }

    #[test]
    fn kinds() {
        assert_eq!(kind_of(Some("image/png"), Some("png")), "Images");
        assert_eq!(kind_of(Some("text/x-rust"), Some("rs")), "Code");
        assert_eq!(kind_of(Some("application/x-elf"), None), "Binaries");
        assert_eq!(kind_of(Some("application/pdf"), Some("pdf")), "Documents");
        assert_eq!(kind_of(None, None), "Other");
    }
}
