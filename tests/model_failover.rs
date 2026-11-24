//! Provider errors switch the live worker session to a fallback model and
//! continue the same iteration instead of aborting the run.

mod common;
use common::*;

#[test]
fn provider_error_switches_model_and_continues_iteration() {
    let p = Project::new();
    seed_loop(
        &p,
        "model openai-codex/gpt-6.2-sol\nfallback-model openai/gpt-5.2\nallow .\n",
    );
    let path = install_mock(&p, MOCK);

    let out = p.run_env(&["run"], &[("PATH", &path), ("MOCK_WORKER", "model-error")]);
    ok!(&out, "yoke run should recover from a provider error");

    let stdin = p.read(".loop/.omp-stdin");
    has!(&stdin, "\"type\":\"set_model\"", "rpc stdin");
    has!(&stdin, "\"provider\":\"openai\"", "rpc stdin");
    has!(&stdin, "\"modelId\":\"gpt-5.2\"", "rpc stdin");
    has!(&stdin, "[YOKE MODEL FAILOVER]", "continuation prompt");
    assert_eq!(
        stdin.matches("\"type\":\"prompt\"").count(),
        2,
        "expected the failed prompt plus one continuation prompt:\n{stdin}"
    );

    let err = stderr(&out);
    has!(&err, "switching model to openai/gpt-5.2", "run stderr");
    has!(&err, "continuing the same iteration", "run stderr");
    has!(&p.read(".loop/notes.md"), "STATUS: DONE", "worker notes");
}

#[test]
fn provider_error_without_policy_still_aborts() {
    let p = Project::new();
    seed_loop(&p, "model openai-codex/gpt-6.2-sol\nallow .\n");
    let path = install_mock(&p, MOCK);

    let out = p.run_env(&["run"], &[("PATH", &path), ("MOCK_WORKER", "model-error")]);
    failed!(&out, "run without a fallback policy");
    lacks!(
        &p.read(".loop/.omp-stdin"),
        "\"type\":\"set_model\"",
        "rpc stdin"
    );
    lacks!(&stderr(&out), "switching model", "run stderr");
}

#[test]
fn rejected_fallbacks_are_tried_in_order_then_abort() {
    let p = Project::new();
    seed_loop(
        &p,
        "model openai-codex/gpt-6.2-sol\n\
         fallback-model openai/gpt-5.2\n\
         fallback-model deepseek/deepseek-v4\n\
         allow .\n",
    );
    let path = install_mock(&p, MOCK);

    let out = p.run_env(
        &["run"],
        &[
            ("PATH", &path),
            ("MOCK_WORKER", "model-error"),
            ("MOCK_SET_MODEL", "reject"),
        ],
    );
    failed!(&out, "run with every fallback rejected");

    let requests = p.read(".loop/.set-model");
    assert_eq!(
        requests.lines().count(),
        2,
        "expected one set_model per fallback:\n{requests}"
    );
    has!(&requests, "\"modelId\":\"gpt-5.2\"", "set_model requests");
    has!(
        &requests,
        "\"modelId\":\"deepseek-v4\"",
        "set_model requests"
    );
    let err = stderr(&out);
    has!(&err, "failed to switch worker model", "run stderr");
    has!(&err, "no fallback models remain", "run stderr");
}
