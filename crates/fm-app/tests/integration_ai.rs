//! Core + Database + AI engine (real Python process, mock backend) + Sorter, end to end.
mod common;
use common::*;
use fm_app::{AnalyzeOpts, AppError, IndexOpts, RuleSource, SortRequest};
use fm_types::Conflict;

fn none(_: usize, _: usize) {}

fn sort_req(root: &std::path::Path, src: RuleSource) -> SortRequest {
    SortRequest { root: root.to_path_buf(), source: src, conflict: Conflict::Rename, analyze_missing: true, save_as: None }
}

#[test]
fn index_analyze_semantic_search_and_incrementality() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("docs");
    write(&root, "invoice_march.txt", b"Invoice #42: payment due 2026-04-01");
    write(&root, "doc1.txt", b"Recipe: ingredients flour sugar butter");
    write(&root, "notes/meeting.txt", b"Meeting agenda for Monday");
    write(&root, "main.rs", b"fn main() {}");
    let eng = Engine::start(t.path());
    let app = app(t.path(), &eng.socket);

    let ir = app.index(&root, &IndexOpts::default()).unwrap();
    assert_eq!((ir.files, ir.dirs, ir.errors), (4, 2, 0));

    let rep = app.analyze(&root, &AnalyzeOpts::default(), &none).unwrap();
    assert_eq!((rep.total, rep.analyzed, rep.failed), (4, 4, 0), "{rep:?}");
    let cat = |n: &str| app.db.get_file(&root.canonicalize().unwrap().join(n).to_string_lossy()).unwrap().unwrap().category;
    assert_eq!(cat("invoice_march.txt").as_deref(), Some("Finance"));
    assert_eq!(cat("doc1.txt").as_deref(), Some("Cooking"));
    assert_eq!(cat("notes/meeting.txt").as_deref(), Some("Work"));
    assert_eq!(cat("main.rs").as_deref(), Some("Source Code"));

    // vector search
    let hits = app.search_semantic("invoice payment due", 3, None).unwrap();
    assert!(hits[0].0.path.ends_with("invoice_march.txt"), "{hits:?}");
    assert!(hits[0].1 > hits[1].1);
    let scoped = app.search_semantic("invoice payment due", 3, Some(&root.canonicalize().unwrap().join("notes"))).unwrap();
    assert_eq!(scoped.len(), 1);

    // nothing changed → nothing analysed
    assert_eq!(app.analyze(&root, &AnalyzeOpts::default(), &none).unwrap().total, 0);

    // real content change → exactly that one file is re-analysed
    let p = root.join("doc1.txt");
    write(&root, "doc1.txt", b"Meeting agenda: budget review");
    set_mtime(&p, now_plus(100));
    app.index(&root, &IndexOpts::default()).unwrap();
    let rep = app.analyze(&root, &AnalyzeOpts::default(), &none).unwrap();
    assert_eq!((rep.total, rep.analyzed, rep.unchanged), (1, 1, 0));
    assert_eq!(cat("doc1.txt").as_deref(), Some("Work"), "new content, new category");

    // mtime-only change (touch) → hash matches, no model call
    set_mtime(&root.join("notes/meeting.txt"), now_plus(200));
    app.index(&root, &IndexOpts::default()).unwrap();
    let rep = app.analyze(&root, &AnalyzeOpts::default(), &none).unwrap();
    assert_eq!((rep.total, rep.analyzed, rep.unchanged), (1, 0, 1));
    assert_eq!(app.analyze(&root, &AnalyzeOpts::default(), &none).unwrap().total, 0, "stamp refreshed");

    // deleting a file prunes it from the index
    std::fs::remove_file(root.join("main.rs")).unwrap();
    assert_eq!(app.index(&root, &IndexOpts::default()).unwrap().pruned, 1);
}

#[test]
fn sort_by_prompt_dry_run_apply_and_undo() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("pics");
    let a = write(&root, "wedding_01.png", &png("a"));
    let b = write(&root, "trip/beach_02.png", &png("b"));
    set_mtime(&a, 1_559_347_200); // 2019-06-01
    set_mtime(&b, 1_559_347_200);
    write(&root, "readme.txt", b"not a photo");
    let eng = Engine::start(t.path());
    let app = app(t.path(), &eng.socket);
    let croot = root.canonicalize().unwrap();

    let mut req = sort_req(&root, RuleSource::Prompt("разложи фото по годам и событиям".into()));
    req.save_as = Some("photos-by-event".into());
    let sp = app.plan_sort(&req, &none).unwrap();
    assert_eq!(sp.ruleset.rules[0].dest, "Photos/{year}/{attr.event}");
    assert_eq!(sp.analysis.as_ref().unwrap().analyzed, 3, "analysis covers every file under root, not just images");
    assert_eq!(sp.plan.ops.len(), 2, "only the two images match the rule's mime filter");
    let dsts: Vec<_> = sp.plan.ops.iter().map(|o| o.dst.strip_prefix(&croot).unwrap().to_string_lossy().into_owned()).collect();
    for d in &dsts {
        assert!(d.starts_with("Photos/2019/"), "{d}");
    }
    assert_eq!(sp.plan.skipped.len(), 1);
    assert!(sp.plan.skipped[0].src.ends_with("readme.txt"));

    // saved rule set is retrievable by name
    let saved = app.list_rulesets().unwrap();
    assert!(saved.iter().any(|(r, src)| r.name == "photos-by-event" && *src == "ai"));

    // apply, verify on disk + index, then undo restores everything
    let ar = app.apply_plan(&sp).unwrap();
    assert_eq!((ar.exec.applied, ar.exec.failed), (2, 0));
    assert!(!a.exists() && !b.exists());
    for d in &dsts {
        assert!(croot.join(d).exists(), "{d}");
    }
    assert!(app.db.get_file(&a.to_string_lossy()).unwrap().is_none());
    assert!(app.db.get_file(&croot.join(&dsts[0]).to_string_lossy()).unwrap().is_some());

    let us = app.undo(Some(&ar.batch_id)).unwrap();
    assert_eq!((us.report.restored, us.report.blocked, us.report.missing), (2, 0, 0));
    assert!(a.exists() && b.exists());
    assert!(app.db.get_file(&a.to_string_lossy()).unwrap().is_some());
    assert!(app.undo(Some(&ar.batch_id)).is_err(), "nothing left to undo in this batch");
}

#[test]
fn builtin_sort_works_without_ai_and_ai_unavailable_is_reported() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("mixed");
    write(&root, "a.jpg", b"fake jpg");
    write(&root, "b.rs", b"fn f() {}");
    let app = app(t.path(), &dead_socket(t.path()));
    assert!(!app.ai_available());

    let req = sort_req(&root, RuleSource::Named("by-type".into()));
    let sp = app.plan_sort(&req, &none).unwrap();
    assert_eq!(sp.plan.ops.len(), 2, "built-in rules need no AI");

    let bad = sort_req(&root, RuleSource::Prompt("anything".into()));
    assert!(matches!(app.plan_sort(&bad, &none), Err(AppError::AiUnavailable(_))));
    assert!(matches!(app.search_semantic("x", 5, None), Err(AppError::AiUnavailable(_))));
    assert!(matches!(app.analyze(&root, &AnalyzeOpts::default(), &none), Err(AppError::AiUnavailable(_))));
}

#[test]
fn unavailable_ai_degrades_a_rule_that_needs_it_without_failing_the_plan() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("r");
    write(&root, "a.txt", b"hello");
    let app = app(t.path(), &dead_socket(t.path()));
    let req = sort_req(&root, RuleSource::Named("by-category".into()));
    let sp = app.plan_sort(&req, &none).unwrap();
    assert!(sp.plan.ops.is_empty());
    assert_eq!(sp.plan.skipped.len(), 1);
    assert!(sp.notes.iter().any(|n| n.contains("unavailable")));
}
