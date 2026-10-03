use crate::{Database, DbError, DbResult, FileQuery};
use fm_types::*;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row};
use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BinaryHeap};
use std::path::Path;
use std::sync::Mutex;

const MIGRATIONS: &[(i64, &str, &str)] = &[(1, "init", include_str!("../migrations/sqlite/0001_init.sql"))];

pub struct SqliteDb {
    conn: Mutex<Connection>,
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

const SELECT_FILE: &str = "SELECT f.id, f.path, f.name, f.size, f.mtime_ns, f.mode, f.mime, f.hash, c.name, f.summary, f.attrs_json, \
     f.analyzed_at, f.analyzed_mtime_ns, f.indexed_at, \
     (SELECT group_concat(t.name, char(31)) FROM file_tags ft JOIN tags t ON t.id = ft.tag_id WHERE ft.file_id = f.id) \
     FROM files f LEFT JOIN categories c ON c.id = f.category_id";

fn row_to_file(r: &Row) -> rusqlite::Result<FileRecord> {
    let attrs_json: Option<String> = r.get(10)?;
    let tags: Option<String> = r.get(14)?;
    Ok(FileRecord {
        id: r.get(0)?,
        path: r.get(1)?,
        name: r.get(2)?,
        size: r.get::<_, i64>(3)? as u64,
        mtime_ns: r.get(4)?,
        mode: r.get::<_, i64>(5)? as u32,
        mime: r.get(6)?,
        hash: r.get(7)?,
        category: r.get(8)?,
        summary: r.get(9)?,
        attrs: attrs_json.and_then(|s| serde_json::from_str::<BTreeMap<String, String>>(&s).ok()).unwrap_or_default(),
        analyzed_at: r.get(11)?,
        analyzed_mtime_ns: r.get(12)?,
        indexed_at: r.get(13)?,
        tags: tags.map(|t| t.split('\u{1f}').map(str::to_string).collect()).unwrap_or_default(),
    })
}

fn parent_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) => "/",
        Some(i) => &path[..i],
        None => "",
    }
}

/// Byte-range bounds for "everything under `dir/`": `dir/` ≤ path < `dir0` ('0' == '/' + 1).
fn subtree_bounds(dir: &str) -> (String, String) {
    let d = dir.trim_end_matches('/');
    (format!("{d}/"), format!("{d}0"))
}

fn normalize(v: &[f32]) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n == 0.0 {
        v.to_vec()
    } else {
        v.iter().map(|x| x / n).collect()
    }
}
fn encode(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}
fn decode(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

#[derive(PartialEq)]
struct Score(f32, i64);
impl Eq for Score {}
impl PartialOrd for Score {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Score {
    fn cmp(&self, o: &Self) -> Ordering {
        self.0.total_cmp(&o.0).then(self.1.cmp(&o.1))
    }
}

impl SqliteDb {
    pub fn open(path: &Path) -> DbResult<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> DbResult<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> DbResult<Self> {
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Self::migrate(&mut conn)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn migrate(conn: &mut Connection) -> DbResult<()> {
        conn.execute_batch("CREATE TABLE IF NOT EXISTS migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL, applied_at INTEGER NOT NULL)")?;
        let current: i64 = conn.query_row("SELECT COALESCE(MAX(version), 0) FROM migrations", [], |r| r.get(0))?;
        for (v, name, sql) in MIGRATIONS.iter().filter(|(v, _, _)| *v > current) {
            let tx = conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.execute("INSERT INTO migrations(version, name, applied_at) VALUES (?1, ?2, ?3)", params![v, name, now()])?;
            tx.commit()?;
            tracing::info!("applied migration {v} ({name})");
        }
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl Database for SqliteDb {
    fn schema_version(&self) -> DbResult<i64> {
        Ok(self.lock().query_row("SELECT COALESCE(MAX(version), 0) FROM migrations", [], |r| r.get(0))?)
    }

    fn upsert_files(&self, run_id: i64, files: &[NewFile]) -> DbResult<usize> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let ts = now();
        {
            let mut st = tx.prepare_cached(
                "INSERT INTO files(path, name, dir, size, mtime_ns, mode, mime, indexed_at, seen_run) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
                 ON CONFLICT(path) DO UPDATE SET \
                   mime = CASE WHEN size != excluded.size OR mtime_ns != excluded.mtime_ns THEN excluded.mime ELSE mime END, \
                   size = excluded.size, mtime_ns = excluded.mtime_ns, mode = excluded.mode, \
                   indexed_at = excluded.indexed_at, seen_run = excluded.seen_run",
            )?;
            for f in files {
                st.execute(params![f.path, f.name, parent_of(&f.path), f.size as i64, f.mtime_ns, f.mode as i64, f.mime, ts, run_id])?;
            }
        }
        tx.commit()?;
        Ok(files.len())
    }

    fn prune_unseen(&self, root: &str, run_id: i64) -> DbResult<usize> {
        let (lo, hi) = subtree_bounds(root);
        Ok(self.lock().execute("DELETE FROM files WHERE path >= ?1 AND path < ?2 AND seen_run != ?3", params![lo, hi, run_id])?)
    }

    fn get_file(&self, path: &str) -> DbResult<Option<FileRecord>> {
        Ok(self.lock().query_row(&format!("{SELECT_FILE} WHERE f.path = ?1"), params![path], row_to_file).optional()?)
    }

    fn list_files(&self, q: &FileQuery) -> DbResult<Vec<FileRecord>> {
        let mut sql = String::from(SELECT_FILE);
        let mut cond: Vec<String> = vec![];
        let mut args: Vec<rusqlite::types::Value> = vec![];
        if let Some(d) = &q.under {
            let (lo, hi) = subtree_bounds(d);
            cond.push("f.path >= ? AND f.path < ?".into());
            args.push(lo.into());
            args.push(hi.into());
        }
        if let Some(n) = &q.name_contains {
            cond.push("f.name LIKE ? ESCAPE '\\'".into());
            args.push(format!("%{}%", n.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")).into());
        }
        if let Some(c) = &q.category {
            cond.push("c.name = ? COLLATE NOCASE".into());
            args.push(c.clone().into());
        }
        if let Some(t) = &q.tag {
            cond.push("EXISTS (SELECT 1 FROM file_tags ft JOIN tags t ON t.id = ft.tag_id WHERE ft.file_id = f.id AND t.name = ?)".into());
            args.push(t.to_lowercase().into());
        }
        if let Some(m) = &q.mime_prefix {
            cond.push("f.mime LIKE ?".into());
            args.push(format!("{m}%").into());
        }
        if q.needs_analysis {
            cond.push("(f.analyzed_at IS NULL OR f.analyzed_mtime_ns IS NOT f.mtime_ns)".into());
        }
        // regular files only (S_IFMT == S_IFREG)
        cond.push("(f.mode & 61440) = 32768".into());
        if !cond.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&cond.join(" AND "));
        }
        sql.push_str(" ORDER BY f.path");
        if let Some(l) = q.limit {
            sql.push_str(&format!(" LIMIT {l}"));
        }
        let conn = self.lock();
        let mut st = conn.prepare(&sql)?;
        let rows = st.query_map(params_from_iter(args.iter()), row_to_file)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn remove_path(&self, path: &str) -> DbResult<usize> {
        let (lo, hi) = subtree_bounds(path);
        Ok(self.lock().execute("DELETE FROM files WHERE path = ?1 OR (path >= ?2 AND path < ?3)", params![path, lo, hi])?)
    }

    fn rename_file(&self, from: &str, to: &str) -> DbResult<()> {
        let name = to.rsplit('/').next().unwrap_or(to);
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM files WHERE path = ?1", params![to])?; // stale row at the destination
        tx.execute("UPDATE files SET path = ?2, name = ?3, dir = ?4 WHERE path = ?1", params![from, to, name, parent_of(to)])?;
        tx.commit()?;
        Ok(())
    }

    fn count_files(&self) -> DbResult<u64> {
        Ok(self.lock().query_row("SELECT COUNT(*) FROM files", [], |r| r.get::<_, i64>(0))? as u64)
    }

    fn save_analysis(&self, file_id: i64, mtime_ns: i64, hash: Option<&str>, res: &AnalysisResult) -> DbResult<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let ts = now();
        let category = res.category.trim();
        let cat_id: Option<i64> = if category.is_empty() {
            None
        } else {
            tx.execute("INSERT OR IGNORE INTO categories(name, auto, created_at) VALUES (?1, 1, ?2)", params![category, ts])?;
            Some(tx.query_row("SELECT id FROM categories WHERE name = ?1 COLLATE NOCASE", params![category], |r| r.get(0))?)
        };
        // merge new attributes into previously extracted ones
        let old: Option<String> = tx.query_row("SELECT attrs_json FROM files WHERE id = ?1", params![file_id], |r| r.get(0))?;
        let mut attrs: BTreeMap<String, String> = old.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        attrs.extend(res.attributes.clone());
        let n = tx.execute(
            "UPDATE files SET category_id = ?2, summary = ?3, attrs_json = ?4, analyzed_at = ?5, analyzed_mtime_ns = ?6, \
             hash = COALESCE(?7, hash), mime = COALESCE(?8, mime) WHERE id = ?1",
            params![file_id, cat_id, res.summary, serde_json::to_string(&attrs)?, ts, mtime_ns, hash, res.mime],
        )?;
        if n == 0 {
            return Err(DbError::Other(format!("no file with id {file_id}")));
        }
        tx.execute("DELETE FROM file_tags WHERE file_id = ?1", params![file_id])?;
        for t in res.tags.iter().map(|t| t.trim().to_lowercase()).filter(|t| !t.is_empty()) {
            tx.execute("INSERT OR IGNORE INTO tags(name) VALUES (?1)", params![t])?;
            tx.execute("INSERT OR IGNORE INTO file_tags(file_id, tag_id) SELECT ?1, id FROM tags WHERE name = ?2", params![file_id, t])?;
        }
        if let Some(e) = &res.embedding {
            let v = normalize(e);
            tx.execute(
                "INSERT INTO embeddings(file_id, model, dim, vector, content_hash, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT(file_id) DO UPDATE SET model = excluded.model, dim = excluded.dim, vector = excluded.vector, \
                 content_hash = excluded.content_hash, created_at = excluded.created_at",
                params![file_id, res.embedding_model.clone().unwrap_or_default(), v.len() as i64, encode(&v), hash, ts],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn touch_analysis(&self, file_id: i64, mtime_ns: i64, hash: &str) -> DbResult<()> {
        self.lock().execute("UPDATE files SET analyzed_mtime_ns = ?2, hash = ?3 WHERE id = ?1", params![file_id, mtime_ns, hash])?;
        Ok(())
    }

    fn list_categories(&self) -> DbResult<Vec<(String, u64)>> {
        let conn = self.lock();
        let mut st = conn.prepare(
            "SELECT c.name, COUNT(f.id) FROM categories c LEFT JOIN files f ON f.category_id = c.id GROUP BY c.id ORDER BY 2 DESC, 1",
        )?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn search_similar(&self, query: &[f32], model: Option<&str>, limit: usize) -> DbResult<Vec<(FileRecord, f32)>> {
        if limit == 0 || query.is_empty() {
            return Ok(vec![]);
        }
        let q = normalize(query);
        let conn = self.lock();
        let mut heap: BinaryHeap<Reverse<Score>> = BinaryHeap::new();
        {
            let mut st = conn.prepare("SELECT file_id, vector, dim FROM embeddings WHERE (?1 IS NULL OR model = ?1)")?;
            let mut rows = st.query(params![model])?;
            while let Some(r) = rows.next()? {
                let (id, blob, dim): (i64, Vec<u8>, i64) = (r.get(0)?, r.get(1)?, r.get(2)?);
                if dim as usize != q.len() {
                    continue;
                }
                let v = decode(&blob);
                let s: f32 = v.iter().zip(&q).map(|(a, b)| a * b).sum();
                heap.push(Reverse(Score(s, id)));
                if heap.len() > limit {
                    heap.pop();
                }
            }
        }
        let mut top: Vec<Score> = heap.into_iter().map(|Reverse(s)| s).collect();
        top.sort_by(|a, b| b.cmp(a));
        let mut out = Vec::with_capacity(top.len());
        for Score(s, id) in top {
            if let Some(f) = conn.query_row(&format!("{SELECT_FILE} WHERE f.id = ?1"), params![id], row_to_file).optional()? {
                out.push((f, s));
            }
        }
        Ok(out)
    }

    fn save_rule(&self, rule: &SavedRule) -> DbResult<()> {
        self.lock().execute(
            "INSERT INTO sort_rules(name, description, prompt, spec_json, source, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(name) DO UPDATE SET description = excluded.description, prompt = excluded.prompt, \
             spec_json = excluded.spec_json, source = excluded.source",
            params![rule.name, rule.description, rule.prompt, serde_json::to_string(&rule.spec)?, rule.source, rule.created_at],
        )?;
        Ok(())
    }

    fn get_rule(&self, name: &str) -> DbResult<Option<SavedRule>> {
        let row = self
            .lock()
            .query_row(
                "SELECT name, description, prompt, spec_json, source, created_at FROM sort_rules WHERE name = ?1",
                params![name],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, i64>(5)?)),
            )
            .optional()?;
        row.map(|(name, description, prompt, spec, source, created_at)| {
            Ok(SavedRule { name, description, prompt, source, spec: serde_json::from_str(&spec)?, created_at })
        })
        .transpose()
    }

    fn list_rules(&self) -> DbResult<Vec<SavedRule>> {
        let names: Vec<String> = {
            let conn = self.lock();
            let mut st = conn.prepare("SELECT name FROM sort_rules ORDER BY name")?;
            let v = st.query_map([], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
            v
        };
        let mut out = vec![];
        for n in names {
            if let Some(r) = self.get_rule(&n)? {
                out.push(r);
            }
        }
        Ok(out)
    }

    fn append_sort_op(&self, op: &SortOpRecord) -> DbResult<i64> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO sort_history(batch_id, seq, rule_name, prompt, root, src, dst, status, error, applied_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![op.batch_id, op.seq, op.rule_name, op.prompt, op.root, op.src, op.dst, op.status.as_str(), op.error, op.applied_at],
        )?;
        Ok(conn.last_insert_rowid())
    }

    fn batch_ops(&self, batch_id: &str) -> DbResult<Vec<SortOpRecord>> {
        let conn = self.lock();
        let mut st = conn.prepare(
            "SELECT id, batch_id, seq, rule_name, prompt, root, src, dst, status, error, applied_at, undone_at \
             FROM sort_history WHERE batch_id = ?1 ORDER BY seq",
        )?;
        let rows = st
            .query_map(params![batch_id], |r| {
                Ok(SortOpRecord {
                    id: r.get(0)?,
                    batch_id: r.get(1)?,
                    seq: r.get::<_, i64>(2)? as u32,
                    rule_name: r.get(3)?,
                    prompt: r.get(4)?,
                    root: r.get(5)?,
                    src: r.get(6)?,
                    dst: r.get(7)?,
                    status: OpStatus::parse(&r.get::<_, String>(8)?),
                    error: r.get(9)?,
                    applied_at: r.get(10)?,
                    undone_at: r.get(11)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn list_batches(&self, limit: usize) -> DbResult<Vec<BatchSummary>> {
        let conn = self.lock();
        let mut st = conn.prepare(
            "SELECT batch_id, MAX(rule_name), MAX(prompt), MAX(root), MIN(applied_at), COUNT(*), \
             SUM(status = 'applied'), SUM(status = 'undone'), SUM(status = 'failed') \
             FROM sort_history GROUP BY batch_id ORDER BY MAX(id) DESC LIMIT ?1",
        )?;
        let rows = st
            .query_map(params![limit as i64], |r| {
                Ok(BatchSummary {
                    batch_id: r.get(0)?,
                    rule_name: r.get(1)?,
                    prompt: r.get(2)?,
                    root: r.get(3)?,
                    created_at: r.get(4)?,
                    total: r.get::<_, i64>(5)? as u32,
                    applied: r.get::<_, i64>(6)? as u32,
                    undone: r.get::<_, i64>(7)? as u32,
                    failed: r.get::<_, i64>(8)? as u32,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn last_undoable_batch(&self) -> DbResult<Option<String>> {
        Ok(self
            .lock()
            .query_row("SELECT batch_id FROM sort_history WHERE status = 'applied' ORDER BY id DESC LIMIT 1", [], |r| r.get(0))
            .optional()?)
    }

    fn set_op_status(&self, id: i64, status: OpStatus, error: Option<&str>) -> DbResult<()> {
        let undone_at = (status == OpStatus::Undone).then(now);
        self.lock().execute("UPDATE sort_history SET status = ?2, error = ?3, undone_at = ?4 WHERE id = ?1", params![id, status.as_str(), error, undone_at])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nf(path: &str, size: u64, mtime: i64) -> NewFile {
        NewFile {
            path: path.into(),
            name: path.rsplit('/').next().unwrap().into(),
            size,
            mtime_ns: mtime,
            mode: 0o100644,
            mime: Some("text/plain".into()),
        }
    }

    fn analysis(cat: &str, tags: &[&str], emb: Option<Vec<f32>>) -> AnalysisResult {
        AnalysisResult {
            category: cat.into(),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            summary: "s".into(),
            embedding: emb,
            embedding_model: Some("m".into()),
            ..Default::default()
        }
    }

    #[test]
    fn migrations_are_idempotent_and_recorded() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("x.db");
        assert_eq!(SqliteDb::open(&p).unwrap().schema_version().unwrap(), 1);
        let db = SqliteDb::open(&p).unwrap(); // second open must not re-apply
        assert_eq!(db.schema_version().unwrap(), 1);
        let n: i64 = db.lock().query_row("SELECT COUNT(*) FROM migrations", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn upsert_keeps_analysis_and_detects_staleness() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.upsert_files(1, &[nf("/r/a.txt", 5, 100)]).unwrap();
        let f = db.get_file("/r/a.txt").unwrap().unwrap();
        assert!(f.needs_analysis());
        db.save_analysis(f.id, 100, Some("h1"), &analysis("Docs", &["a", "b"], None)).unwrap();
        db.upsert_files(2, &[nf("/r/a.txt", 5, 100)]).unwrap();
        let f = db.get_file("/r/a.txt").unwrap().unwrap();
        assert!(!f.needs_analysis());
        assert_eq!(f.category.as_deref(), Some("Docs"));
        assert_eq!(f.tags.len(), 2);
        db.upsert_files(3, &[nf("/r/a.txt", 9, 200)]).unwrap(); // modified
        let f = db.get_file("/r/a.txt").unwrap().unwrap();
        assert!(f.needs_analysis());
        assert_eq!(f.category.as_deref(), Some("Docs")); // old analysis kept until replaced
        assert_eq!(db.list_files(&FileQuery { needs_analysis: true, ..Default::default() }).unwrap().len(), 1);
    }

    #[test]
    fn prune_removes_only_unseen_under_root() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.upsert_files(1, &[nf("/r/a", 1, 1), nf("/r/b", 1, 1), nf("/rr/c", 1, 1)]).unwrap();
        db.upsert_files(2, &[nf("/r/a", 1, 1)]).unwrap();
        assert_eq!(db.prune_unseen("/r", 2).unwrap(), 1); // /r/b only; /rr/c is a sibling, not a child
        assert!(db.get_file("/r/b").unwrap().is_none());
        assert!(db.get_file("/rr/c").unwrap().is_some());
    }

    #[test]
    fn query_filters() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.upsert_files(1, &[nf("/r/a.txt", 1, 1), nf("/r/sub/b.txt", 1, 1), nf("/x/c.txt", 1, 1)]).unwrap();
        let id = db.get_file("/r/sub/b.txt").unwrap().unwrap().id;
        db.save_analysis(id, 1, None, &analysis("Finance", &["invoice"], None)).unwrap();
        let under = db.list_files(&FileQuery { under: Some("/r".into()), ..Default::default() }).unwrap();
        assert_eq!(under.len(), 2);
        let by_cat = db.list_files(&FileQuery { category: Some("finance".into()), ..Default::default() }).unwrap();
        assert_eq!(by_cat.len(), 1);
        let by_tag = db.list_files(&FileQuery { tag: Some("invoice".into()), ..Default::default() }).unwrap();
        assert_eq!(by_tag[0].path, "/r/sub/b.txt");
        let by_name = db.list_files(&FileQuery { name_contains: Some("c.t".into()), ..Default::default() }).unwrap();
        assert_eq!(by_name.len(), 1);
        assert_eq!(db.list_categories().unwrap(), vec![("Finance".to_string(), 1)]);
    }

    #[test]
    fn vector_search_ranks_by_cosine() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.upsert_files(1, &[nf("/a", 1, 1), nf("/b", 1, 1), nf("/c", 1, 1)]).unwrap();
        for (p, v) in [("/a", vec![1.0, 0.0, 0.0]), ("/b", vec![0.9, 0.1, 0.0]), ("/c", vec![0.0, 0.0, 1.0])] {
            let id = db.get_file(p).unwrap().unwrap().id;
            db.save_analysis(id, 1, None, &analysis("X", &[], Some(v))).unwrap();
        }
        let r = db.search_similar(&[1.0, 0.05, 0.0], Some("m"), 2).unwrap();
        assert_eq!(r.iter().map(|(f, _)| f.path.as_str()).collect::<Vec<_>>(), ["/a", "/b"]);
        assert!(r[0].1 >= r[1].1);
        assert!(db.search_similar(&[1.0, 0.0], Some("m"), 5).unwrap().is_empty()); // dim mismatch ignored
        assert!(db.search_similar(&[1.0, 0.0, 0.0], Some("other-model"), 5).unwrap().is_empty());
    }

    #[test]
    fn rename_and_remove() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.upsert_files(1, &[nf("/r/a.txt", 1, 1), nf("/r/d/x", 1, 1), nf("/r/d/y", 1, 1)]).unwrap();
        db.rename_file("/r/a.txt", "/r/new/a.txt").unwrap();
        let f = db.get_file("/r/new/a.txt").unwrap().unwrap();
        assert_eq!((f.name.as_str(), db.get_file("/r/a.txt").unwrap().is_none()), ("a.txt", true));
        assert_eq!(db.remove_path("/r/d").unwrap(), 2);
        assert_eq!(db.count_files().unwrap(), 1);
    }

    #[test]
    fn rules_and_history_roundtrip() {
        let db = SqliteDb::open_in_memory().unwrap();
        let spec = RuleSet { name: "r".into(), rules: vec![Rule { dest: "{year}".into(), ..Default::default() }], ..Default::default() };
        db.save_rule(&SavedRule { name: "r".into(), description: "d".into(), prompt: Some("p".into()), source: "ai".into(), spec: spec.clone(), created_at: 1 }).unwrap();
        assert_eq!(db.get_rule("r").unwrap().unwrap().spec, spec);
        assert_eq!(db.list_rules().unwrap().len(), 1);

        for (b, seq) in [("b1", 0u32), ("b1", 1), ("b2", 0)] {
            db.append_sort_op(&SortOpRecord {
                id: 0, batch_id: b.into(), seq, rule_name: Some("r".into()), prompt: None, root: "/r".into(),
                src: format!("/r/{seq}"), dst: format!("/r/d/{seq}"), status: OpStatus::Applied, error: None, applied_at: 1, undone_at: None,
            })
            .unwrap();
        }
        assert_eq!(db.last_undoable_batch().unwrap().as_deref(), Some("b2"));
        let ops = db.batch_ops("b2").unwrap();
        db.set_op_status(ops[0].id, OpStatus::Undone, None).unwrap();
        assert_eq!(db.last_undoable_batch().unwrap().as_deref(), Some("b1"));
        let batches = db.list_batches(10).unwrap();
        assert_eq!(batches[0].batch_id, "b2");
        assert_eq!((batches[0].undone, batches[1].total), (1, 2));
    }
}
