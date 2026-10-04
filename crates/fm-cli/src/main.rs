//! `fm`: the CLI. Thin by design — every command is a handful of lines calling `fm_app::App`.
//! This is also where a future GUI would plug in: it would call the same `fm_app` API directly.

mod fmt;
mod ls;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use fm_app::{AnalyzeOpts, App, IndexOpts, RuleSource, SortRequest};
use fm_config::Config;
use fm_db::FileQuery;
use fm_types::Conflict;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "fm", version, about = "Modular file manager with local AI sorting", long_about = None)]
struct Cli {
    /// path to config.toml (default: $FM_CONFIG or ~/.config/fm/config.toml)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// tracing filter, e.g. "info" or "fm_core=debug" (overrides the config's general.log_level)
    #[arg(long, global = true)]
    log_level: Option<String>,
    /// machine-readable JSON output where supported
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List a directory (like `ls -la`, but through fm-core)
    Ls {
        path: Option<PathBuf>,
        #[arg(short = 'a', long)]
        all: bool,
        #[arg(short = 'l', long)]
        long: bool,
        #[arg(long, value_enum, default_value = "name")]
        sort: ls::SortBy,
        #[arg(short = 'r', long)]
        reverse: bool,
    },
    /// Walk a tree and (re)build the index
    Index {
        path: PathBuf,
        #[arg(long)]
        no_prune: bool,
    },
    /// Run AI analysis (category, tags, summary, embeddings) over indexed files
    Analyze {
        path: PathBuf,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Search the index
    Find {
        #[arg(long)]
        under: Option<PathBuf>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        category: Option<String>,
        #[arg(long)]
        tag: Option<String>,
        #[arg(long)]
        mime: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Semantic ("files like ...") search over embeddings
    Search {
        query: String,
        #[arg(long)]
        under: Option<PathBuf>,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// List available sort rule sets (built-in, saved, AI-parsed)
    Rules,
    /// Plan / apply an AI or rule-based sort
    Sort {
        path: PathBuf,
        /// a saved or built-in rule set name (see `fm rules`)
        #[arg(long, conflicts_with = "prompt")]
        rule: Option<String>,
        /// a natural-language instruction, parsed by the AI engine
        #[arg(long, conflicts_with = "rule")]
        prompt: Option<String>,
        /// actually move files (default is dry-run)
        #[arg(long)]
        apply: bool,
        #[arg(long, value_enum, default_value = "rename")]
        conflict: CliConflict,
        /// save an AI-parsed prompt as a named rule set for reuse
        #[arg(long)]
        save_as: Option<String>,
        /// skip AI analysis of files missing required data (they will just be skipped in the plan)
        #[arg(long)]
        no_analyze: bool,
    },
    /// Undo a previous sort batch (defaults to the most recent one)
    Undo {
        batch_id: Option<String>,
    },
    /// List previous sort batches
    History {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Watch directories and keep the index (and optionally analysis) current
    Watch {
        paths: Vec<PathBuf>,
    },
    /// AI engine / Ollama health
    Status,
    /// Print the effective configuration
    Config,
}

#[derive(Clone, clap::ValueEnum)]
enum CliConflict {
    Rename,
    Skip,
    Fail,
}
impl From<CliConflict> for Conflict {
    fn from(c: CliConflict) -> Self {
        match c {
            CliConflict::Rename => Conflict::Rename,
            CliConflict::Skip => Conflict::Skip,
            CliConflict::Fail => Conflict::Fail,
        }
    }
}

fn progress(done: usize, total: usize) {
    if total > 0 {
        eprint!("\ranalyzing {done}/{total}...");
        if done == total {
            eprintln!();
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load(cli.config.as_deref()).context("loading configuration")?;
    let filter = std::env::var("RUST_LOG").ok().or_else(|| cli.log_level.clone()).unwrap_or_else(|| cfg.general.log_level.clone());
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::new(filter)).with_writer(std::io::stderr).init();

    let app = App::new(cfg).context("initializing (is the database path writable?)")?;
    run(&app, cli.cmd, cli.json)
}

fn run(app: &App, cmd: Cmd, json: bool) -> Result<()> {
    match cmd {
        Cmd::Ls { path, all, long, sort, reverse } => ls::run(app, &path.unwrap_or_else(|| ".".into()), all, long, sort, reverse),
        Cmd::Config => {
            print!("{}", app.cfg.to_toml());
            Ok(())
        }
        Cmd::Index { path, no_prune } => {
            let r = app.index(&path, &IndexOpts { prune: !no_prune, threads: None })?;
            if json {
                println!(
                    r#"{{"dirs":{},"files":{},"non_utf8":{},"errors":{},"pruned":{},"elapsed_ms":{}}}"#,
                    r.dirs, r.files, r.non_utf8, r.errors, r.pruned, r.elapsed.as_millis()
                );
            } else {
                println!("indexed {} files in {} directories in {:.2?}", r.files, r.dirs, r.elapsed);
                if r.pruned > 0 {
                    println!("removed {} entries no longer on disk", r.pruned);
                }
                if r.errors > 0 {
                    println!("warning: {} directories could not be read (permissions?)", r.errors);
                }
                if r.non_utf8 > 0 {
                    println!("note: {} entries skipped (non-UTF-8 names are not supported)", r.non_utf8);
                }
            }
            Ok(())
        }
        Cmd::Analyze { path, force, limit } => {
            let r = app.analyze(&path, &AnalyzeOpts { attributes: vec![], force, limit }, &progress)?;
            println!("analyzed {}, unchanged {}, too large {}, failed {}", r.analyzed, r.unchanged, r.too_large, r.failed);
            for e in &r.errors {
                eprintln!("  ! {e}");
            }
            if r.aborted {
                bail!("AI engine became unavailable mid-run; re-run `fm analyze` once it's back");
            }
            Ok(())
        }
        Cmd::Find { under, name, category, tag, mime, limit } => {
            let q = FileQuery {
                under: under.map(|p| p.to_string_lossy().into_owned()),
                name_contains: name,
                category,
                tag,
                mime_prefix: mime,
                needs_analysis: false,
                limit,
            };
            for f in app.find(&q)? {
                fmt::print_file_line(&f, json);
            }
            Ok(())
        }
        Cmd::Search { query, under, limit } => {
            match app.search_semantic(&query, limit, under.as_deref()) {
                Ok(hits) => {
                    for (f, score) in hits {
                        if json {
                            println!(r#"{{"score":{score:.4},"path":{}}}"#, fmt::json_str(&f.path));
                        } else {
                            println!("{:.3}  {}", score, f.path);
                        }
                    }
                    Ok(())
                }
                Err(e) => Err(fmt::friendly(e).into()),
            }
        }
        Cmd::Rules => {
            for (r, src) in app.list_rulesets()? {
                println!("{:<20} [{:<7}] {}", r.name, src, r.description);
            }
            Ok(())
        }
        Cmd::Sort { path, rule, prompt, apply, conflict, save_as, no_analyze } => {
            let source = match (rule, prompt) {
                (Some(r), None) => RuleSource::Named(r),
                (None, Some(p)) => RuleSource::Prompt(p),
                _ => bail!("pass exactly one of --rule <name> or --prompt \"...\" (see `fm rules` for names)"),
            };
            let req = SortRequest { root: path, source, conflict: conflict.into(), analyze_missing: !no_analyze, save_as };
            let sp = app.plan_sort(&req, &progress).map_err(fmt::friendly)?;
            for n in &sp.notes {
                eprintln!("note: {n}");
            }
            if sp.plan.ops.is_empty() && sp.plan.skipped.is_empty() {
                println!("nothing to do: no files under {}", sp.plan.root.display());
                return Ok(());
            }
            for op in &sp.plan.ops {
                let note = op.note.as_deref().map(|n| format!("  ({n})")).unwrap_or_default();
                println!("{} -> {}{}", op.src.display(), op.dst.display(), note);
            }
            if !sp.plan.skipped.is_empty() {
                println!("\nskipped {} file(s):", sp.plan.skipped.len());
                for s in &sp.plan.skipped {
                    println!("  {} ({})", s.src, s.reason);
                }
            }
            if !apply {
                println!("\n{} operation(s) planned. Re-run with --apply to execute (dry-run).", sp.plan.ops.len());
                return Ok(());
            }
            let ar = app.apply_plan(&sp).map_err(fmt::friendly)?;
            println!("\napplied {} move(s), {} failed. batch: {}", ar.exec.applied, ar.exec.failed, ar.batch_id);
            if ar.exec.failed > 0 {
                println!("undo with: fm undo {}", ar.batch_id);
            } else {
                println!("undo with: fm undo");
            }
            Ok(())
        }
        Cmd::Undo { batch_id } => {
            let us = app.undo(batch_id.as_deref()).map_err(fmt::friendly)?;
            println!(
                "batch {}: restored {}, blocked {}, missing {}",
                us.batch_id, us.report.restored, us.report.blocked, us.report.missing
            );
            Ok(())
        }
        Cmd::History { limit } => {
            for b in app.db.list_batches(limit)? {
                let what = b.prompt.map(|p| format!("prompt: \"{p}\"")).unwrap_or_else(|| format!("rule: {}", b.rule_name.unwrap_or_default()));
                println!("{}  {}  {} ({} applied, {} undone, {} failed)", b.batch_id, b.root, what, b.applied, b.undone, b.failed);
            }
            Ok(())
        }
        Cmd::Watch { paths } => {
            let roots = if paths.is_empty() { app.cfg.index.roots.iter().map(PathBuf::from).collect() } else { paths };
            if roots.is_empty() {
                bail!("no paths given and no [index].roots configured");
            }
            println!("watching {} (Ctrl-C to stop)...", roots.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "));
            let stop = Arc::new(AtomicBool::new(false));
            let s = stop.clone();
            ctrlc_once(move || s.store(true, Relaxed));
            app.watch(&roots, &stop, &mut |b| {
                if b.changed + b.removed > 0 || b.rescanned {
                    println!(
                        "changed={} removed={} analyzed={}{}",
                        b.changed,
                        b.removed,
                        b.analyzed,
                        if b.rescanned { " (rescanned: inotify queue overflowed)" } else { "" }
                    );
                }
            })?;
            println!("stopped.");
            Ok(())
        }
        Cmd::Status => {
            print!("core:     ok\n");
            print!("database: ok ({} files indexed)\n", app.db.count_files()?);
            match app.ai_status() {
                Ok(s) => {
                    println!("ai engine: ok (backend={}, version={})", s.backend, s.version);
                    println!("ollama:    {}", if s.ollama_reachable { "reachable" } else { "unreachable" });
                    if !s.missing_models.is_empty() {
                        println!("missing models: {} (run `ollama pull <model>`)", s.missing_models.join(", "));
                    }
                }
                Err(e) => println!("ai engine: unavailable ({e})\n  -> sorting/analysis by rule (non-AI) still works"),
            }
            Ok(())
        }
    }
}

/// Minimal `SIGINT`-once handler with no extra crate: reset to default so a second Ctrl-C kills immediately.
fn ctrlc_once(f: impl Fn() + Send + Sync + 'static) {
    use std::sync::OnceLock;
    static CB: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();
    let _ = CB.set(Box::new(f));
    extern "C" fn handler(_: libc::c_int) {
        if let Some(cb) = CB.get() {
            cb();
        }
        unsafe { libc::signal(libc::SIGINT, libc::SIG_DFL) };
    }
    unsafe { libc::signal(libc::SIGINT, handler as *const () as libc::sighandler_t) };
}
