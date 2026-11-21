//! A completed run persists per-iteration and per-run metrics rows under
//! `~/.yoke/metrics/`, and the rows survive `yoke clean`.

mod common;
use common::*;
use std::fs;

#[test]
fn metrics_rows_persist_under_home_and_survive_clean() {
    let p = Project::new();
    seed_loop(&p, "allow .\n");
    let path = install_mock(&p, MOCK);
    let out = p.run_env(
        &["run", "--no-sandbox"],
        &[("PATH", &path), ("MOCK_WORKER", "cost")],
    );
    ok!(&out, "yoke run");

    let metrics_dir = p.home().join(".yoke/metrics");
    let slugs: Vec<_> = fs::read_dir(&metrics_dir)
        .expect("metrics dir")
        .flatten()
        .map(|e| e.path())
        .collect();
    assert_eq!(slugs.len(), 1, "expected one project slug dir: {slugs:?}");
    let slug_dir = &slugs[0];

    let mut runs_path = None;
    let mut iter_path = None;
    for entry in fs::read_dir(slug_dir).expect("slug dir").flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "runs.ndjson" {
            runs_path = Some(entry.path());
        } else if name.ends_with(".ndjson") {
            iter_path = Some(entry.path());
        }
    }
    let runs_path = runs_path.expect("runs.ndjson");
    let iter_path = iter_path.expect("iteration ndjson");

    let runs = fs::read_to_string(&runs_path).unwrap();
    has!(&runs, "\"outcome\":\"judge_pass\"", "runs.ndjson");
    has!(&runs, "\"iterations\":1", "runs.ndjson");

    let iter = fs::read_to_string(&iter_path).unwrap();
    for needle in [
        "\"mode\":\"brute\"",
        "\"status_done\":true",
        "\"restore_ms\":",
        "\"cost_usd\":0.25",
        "\"num_turns\":1",
    ] {
        has!(&iter, needle, "iteration row");
    }

    ok!(&p.run(&["clean"]), "yoke clean");
    assert!(runs_path.exists(), "runs.ndjson must survive clean");
    assert!(iter_path.exists(), "iteration ndjson must survive clean");

    let stats = p.run(&["stats"]);
    ok!(&stats, "yoke stats");
    let listed = format!("{}{}", stdout(&stats), stderr(&stats));
    let run_id = iter_path.file_stem().unwrap().to_str().unwrap();
    has!(&listed, run_id, "yoke stats");
    has!(&listed, "judge_pass", "yoke stats");

    let detail = p.run(&["stats", "--run", run_id]);
    ok!(&detail, "yoke stats --run");
    let detail = format!("{}{}", stdout(&detail), stderr(&detail));
    has!(&detail, "ITER", "yoke stats --run");
    has!(&detail, "pass", "yoke stats --run");
}
