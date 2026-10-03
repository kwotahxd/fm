use crate::template::Template;
use crate::SortError;
use fm_types::{Conflict, FileRecord, Filter, Rule, RuleSet};
use std::collections::{BTreeSet, HashSet};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone)]
pub struct SortOptions {
    /// destination directories are created under this root
    pub root: PathBuf,
    pub conflict: Conflict,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlannedOp {
    pub file_id: i64,
    pub src: PathBuf,
    pub dst: PathBuf,
    pub rule: String,
    /// e.g. "renamed to avoid a name conflict"
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Skipped {
    pub src: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub root: PathBuf,
    pub ops: Vec<PlannedOp>,
    pub skipped: Vec<Skipped>,
}

/// Which AI-derived data a rule set depends on (so the caller can analyse first).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Requirements {
    pub category: bool,
    pub tags: bool,
    pub attrs: BTreeSet<String>,
}

impl Requirements {
    pub fn any(&self) -> bool {
        self.category || self.tags || !self.attrs.is_empty()
    }
}

struct Compiled<'a> {
    rule: &'a Rule,
    dest: Template,
    rename: Option<Template>,
}

fn compile(set: &RuleSet) -> Result<Vec<Compiled<'_>>, SortError> {
    if set.rules.is_empty() {
        return Err(SortError::InvalidRule("rule set has no rules".into()));
    }
    set.rules
        .iter()
        .map(|r| {
            if r.dest.starts_with('/') || r.dest.contains('\0') || r.dest.split('/').any(|c| c == "..") {
                return Err(SortError::InvalidRule(format!("dest '{}' must be a relative path without '..'", r.dest)));
            }
            let rename = r.rename.as_deref().map(Template::parse).transpose()?;
            Ok(Compiled { rule: r, dest: Template::parse(&r.dest)?, rename })
        })
        .collect()
}

pub fn requirements(set: &RuleSet) -> Result<Requirements, SortError> {
    let mut req = Requirements::default();
    for c in compile(set)? {
        let vars = c.dest.vars().chain(c.rename.iter().flat_map(|t| t.vars()));
        for v in vars {
            match v {
                "category" => req.category = true,
                "tag" => req.tags = true,
                a if a.starts_with("attr.") => {
                    req.attrs.insert(a[5..].to_string());
                }
                _ => {}
            }
        }
        let f = &c.rule.filter;
        req.category |= !f.category_in.is_empty();
        req.tags |= !f.tag_any.is_empty();
        req.attrs.extend(f.attr_equals.keys().cloned());
    }
    req.attrs.extend(set.attributes.iter().map(|a| a.name.clone()));
    Ok(req)
}

fn matches(f: &Filter, r: &FileRecord) -> bool {
    if !f.mime_prefix.is_empty() && !r.mime.as_deref().is_some_and(|m| f.mime_prefix.iter().any(|p| m.starts_with(p.as_str()))) {
        return false;
    }
    if !f.ext.is_empty() {
        let Some(e) = r.ext() else { return false };
        if !f.ext.iter().any(|x| x.trim_start_matches('.').eq_ignore_ascii_case(&e)) {
            return false;
        }
    }
    if f.min_size.is_some_and(|m| r.size < m) || f.max_size.is_some_and(|m| r.size > m) {
        return false;
    }
    if !f.category_in.is_empty() && !r.category.as_deref().is_some_and(|c| f.category_in.iter().any(|x| x.eq_ignore_ascii_case(c))) {
        return false;
    }
    if !f.tag_any.is_empty() && !r.tags.iter().any(|t| f.tag_any.iter().any(|x| x.eq_ignore_ascii_case(t))) {
        return false;
    }
    f.attr_equals.iter().all(|(k, v)| r.attrs.get(k).is_some_and(|a| a.eq_ignore_ascii_case(v)))
}

fn missing_ai(f: &FileRecord, req: &Requirements) -> Option<String> {
    if req.category && f.category.is_none() {
        return Some("category".into());
    }
    if req.tags && f.tags.is_empty() {
        return Some("tags".into());
    }
    req.attrs.iter().find(|a| !f.attrs.contains_key(*a)).map(|a| format!("attribute '{a}'"))
}

fn dir_components(rendered: &str) -> PathBuf {
    rendered.split('/').filter(|c| !c.is_empty() && *c != ".").filter(|c| *c != "..").collect()
}

fn clean_filename(s: &str) -> String {
    crate::template::sanitize(s)
}

fn taken(p: &Path, claimed: &HashSet<PathBuf>) -> bool {
    claimed.contains(p) || p.symlink_metadata().is_ok()
}

fn unique(dst: &Path, claimed: &HashSet<PathBuf>) -> PathBuf {
    let name = dst.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name.as_str(), ""),
    };
    let dir = dst.parent().unwrap_or(Path::new(""));
    (1u32..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|c| !taken(c, claimed))
        .expect("unbounded iterator")
}

/// Pure planning: nothing is moved. This *is* the dry-run: print the returned plan.
pub fn plan(files: &[FileRecord], set: &RuleSet, opts: &SortOptions) -> Result<Plan, SortError> {
    let compiled = compile(set)?;
    let req = requirements(set)?;
    let mut sorted: Vec<&FileRecord> = files.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path)); // deterministic conflict resolution
    let mut claimed: HashSet<PathBuf> = HashSet::new();
    let mut out = Plan { root: opts.root.clone(), ..Default::default() };

    for f in sorted {
        let skip = |out: &mut Plan, reason: String| out.skipped.push(Skipped { src: f.path.clone(), reason });
        let Some(c) = compiled.iter().find(|c| matches(&c.rule.filter, f)) else {
            let reason = match missing_ai(f, &req) {
                Some(m) => format!("not analysed: no {m} yet"),
                None => "no rule matched".into(),
            };
            skip(&mut out, reason);
            continue;
        };
        let dir = match c.dest.render(f) {
            Ok(d) => d,
            Err(v) => {
                skip(&mut out, format!("not analysed: no value for {{{v}}}"));
                continue;
            }
        };
        let fname = match &c.rename {
            Some(t) => match t.render(f) {
                Ok(n) => clean_filename(&n),
                Err(v) => {
                    skip(&mut out, format!("not analysed: no value for {{{v}}}"));
                    continue;
                }
            },
            None => f.name.clone(),
        };
        let src = PathBuf::from(&f.path);
        let mut dst = opts.root.join(dir_components(&dir)).join(&fname);
        if dst.components().any(|c| c == Component::ParentDir) {
            skip(&mut out, "destination escapes the root".into());
            continue;
        }
        if dst == src {
            skip(&mut out, "already in place".into());
            continue;
        }
        let mut note = None;
        if taken(&dst, &claimed) {
            match opts.conflict {
                Conflict::Rename => {
                    dst = unique(&dst, &claimed);
                    note = Some("renamed to avoid a name conflict".to_string());
                }
                Conflict::Skip => {
                    skip(&mut out, format!("destination exists: {}", dst.display()));
                    continue;
                }
                Conflict::Fail => return Err(SortError::Conflict { dst: dst.display().to_string() }),
            }
        }
        claimed.insert(dst.clone());
        let rule = if c.rule.name.is_empty() { set.name.clone() } else { c.rule.name.clone() };
        out.ops.push(PlannedOp { file_id: f.id, src, dst, rule, note });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fm_types::Rule;
    use std::fs;

    fn rec(dir: &Path, name: &str, mime: &str) -> FileRecord {
        let p = dir.join(name);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, name).unwrap();
        FileRecord { id: 1, path: p.to_string_lossy().into(), name: p.file_name().unwrap().to_string_lossy().into(), mime: Some(mime.into()), mtime_ns: 1_700_000_000 * 1_000_000_000, ..Default::default() }
    }
    fn opts(root: &Path, c: Conflict) -> SortOptions {
        SortOptions { root: root.to_path_buf(), conflict: c }
    }
    fn set(rules: Vec<Rule>) -> RuleSet {
        RuleSet { name: "t".into(), rules, ..Default::default() }
    }
    fn rule(dest: &str, f: Filter) -> Rule {
        Rule { dest: dest.into(), filter: f, ..Default::default() }
    }

    #[test]
    fn by_type_and_by_date() {
        let t = tempfile::tempdir().unwrap();
        let files = vec![rec(t.path(), "a.jpg", "image/jpeg"), rec(t.path(), "b.rs", "text/x-rust"), rec(t.path(), "c.pdf", "application/pdf")];
        let p = plan(&files, &crate::builtin::get("by-type").unwrap(), &opts(t.path(), Conflict::Rename)).unwrap();
        let dsts: Vec<_> = p.ops.iter().map(|o| o.dst.strip_prefix(t.path()).unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(dsts, ["Images/a.jpg", "Code/b.rs", "Documents/c.pdf"]);
        let p = plan(&files[..1], &crate::builtin::get("by-date").unwrap(), &opts(t.path(), Conflict::Rename)).unwrap();
        assert!(p.ops[0].dst.ends_with("2023/11/a.jpg"));
    }

    #[test]
    fn first_matching_rule_wins_and_unmatched_stay() {
        let t = tempfile::tempdir().unwrap();
        let files = vec![rec(t.path(), "a.rs", "text/x-rust"), rec(t.path(), "b.so", "application/octet-stream"), rec(t.path(), "c.txt", "text/plain")];
        let s = set(vec![
            rule("Sources", Filter { ext: vec!["rs".into(), ".py".into()], ..Default::default() }),
            rule("Binaries", Filter { ext: vec!["so".into()], ..Default::default() }),
        ]);
        let p = plan(&files, &s, &opts(t.path(), Conflict::Rename)).unwrap();
        assert_eq!(p.ops.len(), 2);
        assert_eq!(p.skipped.len(), 1);
        assert_eq!(p.skipped[0].reason, "no rule matched");
    }

    #[test]
    fn in_plan_collisions_are_renamed_deterministically() {
        let t = tempfile::tempdir().unwrap();
        let files = vec![rec(t.path(), "x/a.jpg", "image/jpeg"), rec(t.path(), "y/a.jpg", "image/jpeg"), rec(t.path(), "z/a.jpg", "image/jpeg")];
        let p = plan(&files, &crate::builtin::get("by-type").unwrap(), &opts(t.path(), Conflict::Rename)).unwrap();
        let names: Vec<_> = p.ops.iter().map(|o| o.dst.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["a.jpg", "a (1).jpg", "a (2).jpg"]);
        assert!(p.ops[0].note.is_none() && p.ops[1].note.is_some());
    }

    #[test]
    fn existing_destination_strategies() {
        let t = tempfile::tempdir().unwrap();
        let f = rec(t.path(), "in/a.jpg", "image/jpeg");
        fs::create_dir_all(t.path().join("Images")).unwrap();
        fs::write(t.path().join("Images/a.jpg"), "old").unwrap();
        let s = crate::builtin::get("by-type").unwrap();
        let p = plan(std::slice::from_ref(&f), &s, &opts(t.path(), Conflict::Rename)).unwrap();
        assert!(p.ops[0].dst.ends_with("Images/a (1).jpg"));
        let p = plan(std::slice::from_ref(&f), &s, &opts(t.path(), Conflict::Skip)).unwrap();
        assert!(p.ops.is_empty() && p.skipped[0].reason.starts_with("destination exists"));
        assert!(matches!(plan(std::slice::from_ref(&f), &s, &opts(t.path(), Conflict::Fail)), Err(SortError::Conflict { .. })));
    }

    #[test]
    fn already_in_place_is_skipped() {
        let t = tempfile::tempdir().unwrap();
        let f = rec(t.path(), "Images/a.jpg", "image/jpeg");
        let p = plan(&[f], &crate::builtin::get("by-type").unwrap(), &opts(t.path(), Conflict::Rename)).unwrap();
        assert_eq!(p.skipped[0].reason, "already in place");
    }

    #[test]
    fn ai_rules_report_missing_analysis() {
        let t = tempfile::tempdir().unwrap();
        let mut a = rec(t.path(), "a.txt", "text/plain");
        let b = rec(t.path(), "b.txt", "text/plain");
        a.category = Some("Finance".into());
        let s = crate::builtin::get("by-category").unwrap();
        assert_eq!(requirements(&s).unwrap(), Requirements { category: true, ..Default::default() });
        let p = plan(&[a, b], &s, &opts(t.path(), Conflict::Rename)).unwrap();
        assert_eq!(p.ops.len(), 1);
        assert!(p.ops[0].dst.ends_with("Finance/a.txt"));
        assert!(p.skipped[0].reason.starts_with("not analysed"));
    }

    #[test]
    fn attributes_in_filters_and_templates() {
        let t = tempfile::tempdir().unwrap();
        let mut a = rec(t.path(), "a.jpg", "image/jpeg");
        a.attrs.insert("event".into(), "Wedding".into());
        let s = RuleSet {
            name: "photos".into(),
            attributes: vec![fm_types::AttrSpec { name: "event".into(), description: String::new() }],
            rules: vec![rule("Photos/{year}/{attr.event}", Filter { mime_prefix: vec!["image/".into()], ..Default::default() })],
            ..Default::default()
        };
        assert!(requirements(&s).unwrap().attrs.contains("event"));
        let p = plan(&[a], &s, &opts(t.path(), Conflict::Rename)).unwrap();
        assert!(p.ops[0].dst.ends_with("Photos/2023/Wedding/a.jpg"));
    }

    #[test]
    fn rename_template() {
        let t = tempfile::tempdir().unwrap();
        let f = rec(t.path(), "IMG.JPG", "image/jpeg");
        let s = set(vec![Rule { dest: "p".into(), rename: Some("{year}-{month}-{day}_{stem}.{ext}".into()), ..Default::default() }]);
        let p = plan(&[f], &s, &opts(t.path(), Conflict::Rename)).unwrap();
        assert!(p.ops[0].dst.ends_with("p/2023-11-14_IMG.jpg"));
    }

    #[test]
    fn invalid_rules_are_rejected() {
        let t = tempfile::tempdir().unwrap();
        let o = opts(t.path(), Conflict::Rename);
        for bad in ["../x", "/abs", "a/../b", "{nope}"] {
            assert!(plan(&[], &set(vec![rule(bad, Filter::default())]), &o).is_err(), "{bad}");
        }
        assert!(plan(&[], &set(vec![]), &o).is_err());
    }

    #[test]
    fn hostile_attribute_values_cannot_escape() {
        let t = tempfile::tempdir().unwrap();
        let mut a = rec(t.path(), "a.txt", "text/plain");
        a.category = Some("../../etc".into());
        let p = plan(&[a], &crate::builtin::get("by-category").unwrap(), &opts(t.path(), Conflict::Rename)).unwrap();
        assert!(p.ops[0].dst.starts_with(t.path()));
        assert_eq!(p.ops[0].dst.strip_prefix(t.path()).unwrap().components().count(), 2);
    }
}
