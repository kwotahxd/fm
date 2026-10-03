//! Black-box CLI tests: no AI engine needed (they exercise the non-AI paths + error hints).
use assert_cmd::Command;
use predicates::str::contains;
use std::fs;

fn cfg(tmp: &std::path::Path) -> std::path::PathBuf {
    let p = tmp.join("config.toml");
    fs::write(
        &p,
        format!(
            "[database]\npath = \"{}\"\n[ai]\nsocket_path = \"{}\"\n",
            tmp.join("index.db").display(),
            tmp.join("no-engine.sock").display()
        ),
    )
    .unwrap();
    p
}

fn fm(tmp: &std::path::Path) -> Command {
    let mut c = Command::cargo_bin("fm").unwrap();
    c.arg("--config").arg(cfg(tmp));
    c
}

#[test]
fn index_ls_find_and_sort_dry_run_work_without_ai() {
    let t = tempfile::tempdir().unwrap();
    let data = t.path().join("data");
    fs::create_dir(&data).unwrap();
    fs::write(data.join("a.jpg"), "x").unwrap();
    fs::write(data.join("b.rs"), "fn f(){}").unwrap();

    fm(t.path()).args(["index"]).arg(&data).assert().success().stdout(contains("indexed 2 files"));
    fm(t.path()).args(["ls", "-l"]).arg(&data).assert().success().stdout(contains("a.jpg")).stdout(contains("b.rs"));
    fm(t.path()).args(["find", "--under"]).arg(&data).assert().success().stdout(contains("a.jpg"));
    fm(t.path())
        .args(["sort"])
        .arg(&data)
        .args(["--rule", "by-type"])
        .assert()
        .success()
        .stdout(contains("Images/a.jpg"))
        .stdout(contains("Code/b.rs"))
        .stdout(contains("dry-run"));
    assert!(data.join("a.jpg").exists(), "dry-run must not move anything");
}

#[test]
fn apply_then_undo_round_trip() {
    let t = tempfile::tempdir().unwrap();
    let data = t.path().join("data");
    fs::create_dir(&data).unwrap();
    fs::write(data.join("a.jpg"), "x").unwrap();
    fm(t.path()).args(["index"]).arg(&data).assert().success();
    fm(t.path()).args(["sort"]).arg(&data).args(["--rule", "by-type", "--apply"]).assert().success().stdout(contains("applied 1 move"));
    assert!(data.join("Images/a.jpg").exists());
    fm(t.path()).args(["undo"]).assert().success().stdout(contains("restored 1"));
    assert!(data.join("a.jpg").exists());
}

#[test]
fn ai_dependent_commands_fail_with_a_helpful_hint_when_engine_is_down() {
    let t = tempfile::tempdir().unwrap();
    let data = t.path().join("data");
    fs::create_dir(&data).unwrap();
    fs::write(data.join("a.txt"), "x").unwrap();
    fm(t.path()).args(["index"]).arg(&data).assert().success();
    fm(t.path())
        .args(["analyze"])
        .arg(&data)
        .assert()
        .failure()
        .stderr(contains("AI is unavailable"))
        .stderr(contains("aiengine"));
    fm(t.path()).args(["search", "invoice"]).assert().failure().stderr(contains("AI is unavailable"));
    fm(t.path())
        .args(["sort"])
        .arg(&data)
        .args(["--prompt", "sort by year"])
        .assert()
        .failure()
        .stderr(contains("AI is unavailable"));
}

#[test]
fn rules_lists_builtins_and_status_reports_ai_down() {
    let t = tempfile::tempdir().unwrap();
    fm(t.path()).args(["rules"]).assert().success().stdout(contains("by-type")).stdout(contains("by-date"));
    fm(t.path()).args(["status"]).assert().success().stdout(contains("ai engine: unavailable"));
}

#[test]
fn invalid_arguments_are_rejected() {
    let t = tempfile::tempdir().unwrap();
    fm(t.path()).args(["sort"]).arg(t.path()).assert().failure().stderr(contains("--rule"));
    fm(t.path()).args(["sort"]).arg(t.path()).args(["--rule", "by-type", "--prompt", "x"]).assert().failure();
}
