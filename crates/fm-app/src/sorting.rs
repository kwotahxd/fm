use crate::analyze::{AnalyzeOpts, AnalyzeReport};
use crate::index::IndexOpts;
use crate::{now_secs, App, AppError, AppResult};
use fm_db::FileQuery;
use fm_sorter::{builtin, ExecReport, Plan, PlannedOp, SortOptions, UndoReport};
use fm_types::*;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

#[derive(Debug, Clone)]
pub enum RuleSource {
    /// a built-in rule set (`by-type`, …) or one saved in the DB
    Named(String),
    /// a natural-language instruction, parsed by the AI engine
    Prompt(String),
}

#[derive(Debug, Clone)]
pub struct SortRequest {
    pub root: PathBuf,
    pub source: RuleSource,
    pub conflict: Conflict,
    /// run AI analysis first for files the rule set needs data for (skipped silently if AI is down)
    pub analyze_missing: bool,
    /// persist an AI-parsed rule set under this name
    pub save_as: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SortPlan {
    pub plan: Plan,
    pub ruleset: RuleSet,
    pub prompt: Option<String>,
    pub analysis: Option<AnalyzeReport>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ApplyReport {
    pub batch_id: String,
    pub exec: ExecReport,
}

#[derive(Debug, Clone)]
pub struct UndoSummary {
    pub batch_id: String,
    pub report: UndoReport,
}

static BATCH_SEQ: AtomicU64 = AtomicU64::new(0);

fn new_batch_id() -> String {
    format!("{:x}-{:x}-{:x}", crate::now_nanos(), std::process::id(), BATCH_SEQ.fetch_add(1, Relaxed))
}

impl App {
    pub fn list_rulesets(&self) -> AppResult<Vec<(RuleSet, &'static str)>> {
        let mut v: Vec<(RuleSet, &'static str)> = builtin::all().into_iter().map(|r| (r, "builtin")).collect();
        v.extend(self.db.list_rules()?.into_iter().map(|r| (r.spec, if r.source == "ai" { "ai" } else { "user" })));
        Ok(v)
    }

    /// Build a sort plan. Nothing on disk is touched — printing this plan *is* the dry-run.
    pub fn plan_sort(&self, req: &SortRequest, progress: &dyn Fn(usize, usize)) -> AppResult<SortPlan> {
        let root = std::fs::canonicalize(&req.root)?;
        let mut notes = vec![];

        let (ruleset, prompt) = match &req.source {
            RuleSource::Named(n) => {
                let rs = match builtin::get(n) {
                    Some(r) => r,
                    None => self.db.get_rule(n)?.map(|s| s.spec).ok_or_else(|| AppError::Invalid(format!("unknown rule set '{n}' (see `fm rules`)")))?,
                };
                (rs, None)
            }
            RuleSource::Prompt(p) => {
                self.require_ai()?;
                let known: Vec<String> = self.db.list_categories()?.into_iter().map(|(n, _)| n).collect();
                (self.ai_client().parse_prompt(p, &known)?, Some(p.clone()))
            }
        };
        if let (Some(name), Some(p)) = (&req.save_as, &prompt) {
            self.db.save_rule(&SavedRule { name: name.clone(), description: ruleset.description.clone(), prompt: Some(p.clone()), source: "ai".into(), spec: RuleSet { name: name.clone(), ..ruleset.clone() }, created_at: now_secs() })?;
        }

        // the plan must reflect what is on disk *now*
        self.index(&root, &IndexOpts::default())?;

        let needs = fm_sorter::requirements(&ruleset)?;
        let mut analysis = None;
        if needs.any() && req.analyze_missing {
            let attributes: Vec<AttrSpec> = needs
                .attrs
                .iter()
                .map(|n| AttrSpec { name: n.clone(), description: ruleset.attributes.iter().find(|a| &a.name == n).map(|a| a.description.clone()).unwrap_or_default() })
                .collect();
            if self.ai_available() {
                analysis = Some(self.analyze(&root, &AnalyzeOpts { attributes, ..Default::default() }, progress)?);
            } else {
                notes.push("AI is unavailable: files that need AI data are skipped (see the skipped list)".into());
            }
        }

        let files = self.db.list_files(&FileQuery { under: Some(root.to_string_lossy().into_owned()), ..Default::default() })?;
        let plan = fm_sorter::plan(&files, &ruleset, &SortOptions { root: root.clone(), conflict: req.conflict })?;
        Ok(SortPlan { plan, ruleset, prompt, analysis, notes })
    }

    /// Execute a plan. Every operation is journaled in `sort_history` the moment it happens, and the
    /// index is kept in sync, so `undo` works even after a crash mid-run.
    pub fn apply_plan(&self, sp: &SortPlan) -> AppResult<ApplyReport> {
        let batch_id = new_batch_id();
        let root = sp.plan.root.to_string_lossy().into_owned();
        let db = &self.db;
        let exec = fm_sorter::execute(&sp.plan, &mut |seq: usize, op: &PlannedOp, out: &fm_sorter::OpOutcome| {
            let (status, error) = match out {
                fm_sorter::OpOutcome::Applied => (OpStatus::Applied, None),
                fm_sorter::OpOutcome::Failed(e) => (OpStatus::Failed, Some(e.clone())),
            };
            let rec = SortOpRecord {
                id: 0,
                batch_id: batch_id.clone(),
                seq: seq as u32,
                rule_name: Some(sp.ruleset.name.clone()),
                prompt: sp.prompt.clone(),
                root: root.clone(),
                src: op.src.to_string_lossy().into_owned(),
                dst: op.dst.to_string_lossy().into_owned(),
                status,
                error,
                applied_at: now_secs(),
                undone_at: None,
            };
            if let Err(e) = db.append_sort_op(&rec) {
                tracing::error!("cannot journal move {} → {}: {e}", rec.src, rec.dst);
            }
            if status == OpStatus::Applied {
                if let Err(e) = db.rename_file(&rec.src, &rec.dst) {
                    tracing::warn!("index not updated for {}: {e}", rec.dst);
                }
            }
        });
        for op in &sp.plan.ops {
            for p in [&op.src, &op.dst] {
                if let Some(parent) = p.parent() {
                    self.cache.invalidate(parent);
                }
            }
        }
        Ok(ApplyReport { batch_id, exec })
    }

    /// Undo a batch (default: the most recent one that still has applied operations).
    pub fn undo(&self, batch: Option<&str>) -> AppResult<UndoSummary> {
        let batch_id = match batch {
            Some(b) => b.to_string(),
            None => self.db.last_undoable_batch()?.ok_or_else(|| AppError::Invalid("nothing to undo".into()))?,
        };
        let ops: Vec<SortOpRecord> = self.db.batch_ops(&batch_id)?.into_iter().filter(|o| o.status == OpStatus::Applied).collect();
        if ops.is_empty() {
            return Err(AppError::Invalid(format!("batch {batch_id} has nothing left to undo")));
        }
        let root = PathBuf::from(&ops[0].root);
        let items: Vec<fm_sorter::exec::UndoItem> =
            ops.iter().map(|o| fm_sorter::exec::UndoItem { id: o.id, src: o.src.clone().into(), dst: o.dst.clone().into() }).collect();
        let db = &self.db;
        let report = fm_sorter::undo(&items, &root, &mut |it, out| {
            use fm_sorter::UndoOutcome::*;
            let (src, dst) = (it.src.to_string_lossy(), it.dst.to_string_lossy());
            let r = match out {
                Restored => db.set_op_status(it.id, OpStatus::Undone, None).and_then(|_| db.rename_file(&dst, &src)),
                // gone for good: mark it so the batch stops being offered for undo
                Missing => db.set_op_status(it.id, OpStatus::Failed, Some("file was missing at undo time")),
                // recoverable (free the spot and retry): stays 'applied'
                Blocked(why) => {
                    tracing::warn!("cannot restore {dst} → {src}: {why}");
                    Ok(())
                }
            };
            if let Err(e) = r {
                tracing::error!("history not updated for {}: {e}", it.dst.display());
            }
            for p in [&it.src, &it.dst] {
                if let Some(parent) = p.parent() {
                    self.cache.invalidate(parent);
                }
            }
        });
        Ok(UndoSummary { batch_id, report })
    }
}
