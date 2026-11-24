//! `--summary-fd` emits token-budgeted summaries on the side descriptor
//! only, and slow summaries apply worker backpressure.

mod common;
use common::*;
use std::fs;
use std::process::Stdio;

#[test]
fn summaries_keep_launch_and_completion_separate_and_flush_partial_text() {
    let p = Project::new();
    seed_loop(&p, "metrics off\nallow .\nsummary-tokens 12\n");
    let path = install_mock(&p, MOCK);
    let log_path = p.path_of(".loop/run.log");
    let (mut child, stream) = spawn_with_summary(
        &p,
        &[
            ("PATH", &path),
            ("MOCK_WORKER", "summary"),
            ("MOCK_SUMMARY", "inspect"),
            ("MOCK_SUMMARY_MODEL", "deepseek/deepseek-flash"),
        ],
        Stdio::from(fs::File::create(&log_path).unwrap()),
    );
    let records = summary_records(stream);
    let status = child.wait().expect("wait yoke run");
    let log = fs::read_to_string(&log_path).unwrap_or_default();
    assert!(status.success(), "run should pass\nlog:\n{log}");

    let start = records
        .iter()
        .find(|r| r["type"] == "run_start")
        .expect("run_start record");
    assert!(start["run_id"].as_str().is_some(), "run_start run id");
    assert_eq!(start["summary_tokens"], 12);

    let summaries: Vec<_> = records.iter().filter(|r| r["type"] == "summary").collect();
    assert_eq!(summaries.len(), 3, "{records:?}");
    for s in &summaries {
        assert_eq!(s["text"], "cheap summary of the batch");
    }
    assert!(summaries[0]["tokens"].as_u64().unwrap() >= 12);
    assert!(summaries[1]["tokens"].as_u64().unwrap() >= 12);
    assert!(summaries[2]["tokens"].as_u64().unwrap() < 12);

    let prompts =
        fs::read_to_string(p.path_of(".loop/.summary-prompts")).expect("summary prompts captured");
    let batches: Vec<_> = prompts.split("\n---PROMPT---\n").skip(1).collect();
    assert_eq!(batches.len(), 3, "{prompts}");
    has!(
        batches[0],
        "checking files\n\nbash cargo test — started bg_1; result pending",
        "launch"
    );
    lacks!(batches[0], "expected build failure", "launch");
    has!(
        batches[1],
        "bash job bg_1 finished — failed; exit 17 (cargo test)\nexpected build failure",
        "completion"
    );
    has!(batches[2], "partial block", "final flush");
    assert_eq!(prompts.matches("expected build failure").count(), 1);
    assert!(
        !prompts.contains("assistantMessageEvent") && !prompts.contains("tool_execution_start"),
        "summary prompt leaked raw JSON: {prompts:?}"
    );

    let end = records
        .iter()
        .find(|r| r["type"] == "run_end")
        .expect("run_end record");
    assert_eq!(end["outcome"], "judge_pass");

    let run_dir = fs::read_dir(p.path_of(".loop/runs"))
        .expect("runs dir")
        .next()
        .expect("one run")
        .unwrap()
        .path();
    assert!(
        !run_dir.join("summaries.ndjson").exists(),
        "summaries must stay out of run artifacts"
    );
}

#[test]
fn slow_summaries_pause_the_worker_and_resume_when_drained() {
    let p = Project::new();
    seed_loop(
        &p,
        "metrics off\nallow .\nsummary-tokens 1\nsummary-model fake-cheap\nsummary-queue-limit 1\n",
    );
    let path = install_mock(&p, MOCK);
    let log_path = p.path_of(".loop/run.log");
    let (mut child, stream) = spawn_with_summary(
        &p,
        &[
            ("PATH", &path),
            ("MOCK_WORKER", "slow-summary"),
            ("MOCK_SUMMARY", "slow"),
        ],
        Stdio::from(fs::File::create(&log_path).unwrap()),
    );
    let records = summary_records(stream);
    let status = child.wait().expect("wait yoke run");
    let log = fs::read_to_string(&log_path).unwrap_or_default();
    assert!(status.success(), "run should pass\nlog:\n{log}");
    has!(&log, "summary backlog reached", "run log");
    has!(&log, "worker paused at block boundary", "run log");
    has!(&log, "worker resumed", "run log");

    let summaries = records.iter().filter(|r| r["type"] == "summary").count();
    assert!(
        summaries >= 6,
        "no summarizer work should be dropped, got {summaries}: {records:?}"
    );
}

#[test]
fn zero_cadence_disables_default_summaries_but_keeps_lifecycle_records() {
    let p = Project::new();
    seed_loop(&p, "metrics off\nallow .\nsummary-tokens 0\n");
    let path = install_mock(&p, MOCK);
    let (mut child, stream) = spawn_with_summary(
        &p,
        &[("PATH", &path), ("MOCK_WORKER", "summary")],
        Stdio::null(),
    );
    let records = summary_records(stream);
    assert!(child.wait().unwrap().success());
    assert_eq!(records.first().unwrap()["type"], "run_start");
    assert_eq!(records.last().unwrap()["type"], "run_end");
    lacks!(&p.read(".loop/.omp-argv"), "--mode\ntext", "omp calls");
}
