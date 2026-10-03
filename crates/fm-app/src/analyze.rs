use crate::{App, AppResult};
use fm_core::content::hash_file;
use fm_db::FileQuery;
use fm_types::*;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;

#[derive(Debug, Clone, Default)]
pub struct AnalyzeOpts {
    /// extra attributes to extract (from a sort rule set)
    pub attributes: Vec<AttrSpec>,
    /// re-analyse even if the file is up to date
    pub force: bool,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct AnalyzeReport {
    pub total: usize,
    pub analyzed: usize,
    /// mtime changed but the content hash did not: no model call needed
    pub unchanged: usize,
    pub failed: usize,
    pub too_large: usize,
    /// the engine went away mid-run; remaining files were left for the next run
    pub aborted: bool,
    pub errors: Vec<String>,
}

struct Job {
    rec: FileRecord,
    attrs: Vec<AttrSpec>,
    categories: Arc<Vec<String>>,
}

enum Outcome {
    Analyzed { res: AnalysisResult, hash: String, mtime_ns: i64 },
    Unchanged { hash: String, mtime_ns: i64 },
    TooLarge,
    Unavailable(String),
    Failed(String),
    Aborted,
}

fn run_job(ai: &fm_ai::AiClient, job: &Job, max_bytes: u64, force: bool) -> Outcome {
    let rec = &job.rec;
    let path = Path::new(&rec.path);
    let md = match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_file() => m,
        Ok(_) => return Outcome::Failed("not a regular file".into()),
        Err(e) => return Outcome::Failed(e.to_string()),
    };
    if md.len() > max_bytes {
        return Outcome::TooLarge;
    }
    // stamp taken *before* reading: if the file changes while we analyse it, the next run notices
    let mtime_ns = md.mtime() * 1_000_000_000 + md.mtime_nsec();
    let hash = match hash_file(path, Some(max_bytes)) {
        Ok(Some(h)) => h,
        Ok(None) => return Outcome::TooLarge,
        Err(e) => return Outcome::Failed(e.to_string()),
    };
    let have_attrs = job.attrs.iter().all(|a| rec.attrs.contains_key(&a.name));
    if !force && rec.analyzed_at.is_some() && rec.hash.as_deref() == Some(hash.as_str()) && have_attrs {
        return Outcome::Unchanged { hash, mtime_ns };
    }
    let req = AnalyzeRequest {
        path: rec.path.clone(),
        mime: rec.mime.clone(),
        categories: (*job.categories).clone(),
        attributes: job.attrs.clone(),
        want_embedding: true,
    };
    match ai.analyze_file(&req) {
        Ok(res) => Outcome::Analyzed { res, hash, mtime_ns },
        Err(e) if e.is_unavailable() => Outcome::Unavailable(e.to_string()),
        Err(e) => Outcome::Failed(e.to_string()),
    }
}

impl App {
    /// Analyse everything under `scope` that is new, stale, or lacks a requested attribute.
    pub fn analyze(&self, scope: &Path, opts: &AnalyzeOpts, progress: &dyn Fn(usize, usize)) -> AppResult<AnalyzeReport> {
        self.require_ai()?;
        let scope = std::fs::canonicalize(scope)?;
        let mut q = FileQuery { under: Some(scope.to_string_lossy().into_owned()), needs_analysis: !opts.force && opts.attributes.is_empty(), ..Default::default() };
        if opts.limit.is_some() && q.needs_analysis {
            q.limit = opts.limit;
        }
        let mut files = self.db.list_files(&q)?;
        if !opts.force && !opts.attributes.is_empty() {
            files.retain(|f| f.needs_analysis() || opts.attributes.iter().any(|a| !f.attrs.contains_key(&a.name)));
        }
        self.analyze_records(files, opts, progress)
    }

    /// The incremental core: only the given records are looked at (the watcher passes just the changed file).
    pub fn analyze_records(&self, mut files: Vec<FileRecord>, opts: &AnalyzeOpts, progress: &dyn Fn(usize, usize)) -> AppResult<AnalyzeReport> {
        self.require_ai()?;
        if let Some(l) = opts.limit {
            files.truncate(l);
        }
        let mut rep = AnalyzeReport { total: files.len(), ..Default::default() };
        if files.is_empty() {
            return Ok(rep);
        }
        let categories: Arc<Vec<String>> = Arc::new(self.db.list_categories()?.into_iter().take(60).map(|(n, _)| n).collect());
        let (ai, abort) = (self.ai_client(), Arc::new(AtomicBool::new(false)));
        let (max_bytes, force) = (self.cfg.index.max_analyze_mb * 1024 * 1024, opts.force);
        let conc = self.cfg.ai.concurrency.max(1);
        let ab = abort.clone();
        let (tx, rx) = fm_core::spawn_pool(conc, conc * 2, move |job: Job| {
            let out = if ab.load(Relaxed) { Outcome::Aborted } else { run_job(&ai, &job, max_bytes, force) };
            (job.rec, out)
        });
        let jobs: Vec<Job> = files.into_iter().map(|rec| Job { rec, attrs: opts.attributes.clone(), categories: categories.clone() }).collect();
        // feed from a separate thread so a full queue never blocks result handling
        std::thread::spawn(move || {
            for j in jobs {
                if tx.send(j).is_err() {
                    break;
                }
            }
        });

        let mut done = 0usize;
        for (rec, out) in rx {
            done += 1;
            match out {
                Outcome::Analyzed { res, hash, mtime_ns } => {
                    for w in &res.warnings {
                        tracing::debug!("{}: {w}", rec.path);
                    }
                    self.db.save_analysis(rec.id, mtime_ns, Some(&hash), &res)?;
                    rep.analyzed += 1;
                }
                Outcome::Unchanged { hash, mtime_ns } => {
                    self.db.touch_analysis(rec.id, mtime_ns, &hash)?;
                    rep.unchanged += 1;
                }
                Outcome::TooLarge => rep.too_large += 1,
                Outcome::Failed(e) => {
                    tracing::warn!("analysis failed for {}: {e}", rec.path);
                    rep.failed += 1;
                    if rep.errors.len() < 5 {
                        rep.errors.push(format!("{}: {e}", rec.path));
                    }
                }
                Outcome::Unavailable(e) => {
                    if !abort.swap(true, Relaxed) {
                        tracing::warn!("AI engine went away, stopping analysis: {e}");
                        rep.errors.push(e);
                    }
                    rep.aborted = true;
                }
                Outcome::Aborted => {}
            }
            progress(done, rep.total);
        }
        Ok(rep)
    }
}
