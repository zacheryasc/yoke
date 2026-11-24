# Yoke

Yoke runs a coding loop with an omp worker, local guards, and a fresh omp judge.

## Use Yoke from omp

From the Yoke checkout:

```sh
cargo install --path . --locked
omp plugin link .
```

The link is user-scoped and points to this checkout. `omp plugin list` shows
`yoke`. Start a new omp session in the project you want to supervise; no `-e`
flag is needed. Set `YOKE_BIN` to the executable path if `yoke` isn't on `PATH`.

Initialize the project's loop with `yoke init`. Live summaries default to
`deepseek/deepseek-flash`. Set `summary-model` in `.loop/yoke.conf` to override
it. `summary-tokens` defaults to `1024` o200k_base tokens of worker prose and
bounded tool transcripts per batch, or set it to `0` to disable live summaries.
The configured summary model receives worker prose and excerpts from tool
arguments and results; keep it within the same data-sharing boundary.
Replace old `summary-every` settings with `summary-tokens`; the units differ.
In OMP, use the `yoke` tool, not the shell CLI, to `start`, `pause`, `resume`,
`inject` steering, or `stop`.
After `start`, end your turn without polling. Summaries and the final result
wake the session; artifacts are under `.loop/runs/<run-id>/`.
Yoke's worker and judge sessions don't get the supervisor tool.

## Choose the judge's model and thinking level

Set optional judge-only overrides in `.loop/yoke.conf`:

```text
model openai/gpt-5.2
thinking high
judge-model openai/gpt-5.5
judge-thinking auto
```

Both checkpoint and final judges use these overrides. Each omitted `judge-*`
setting inherits its shared counterpart, or OMP's default if neither is set.
Workers and periodic agents keep the shared settings.
