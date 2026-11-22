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
it, or set `summary-every 0` to disable summaries. The `yoke` tool can `start`,
`pause`, `resume`, `inject` steering, and `stop` a run. Summaries and the final
result appear as session messages; artifacts are under `.loop/runs/<run-id>/`.
Yoke's worker and judge sessions don't get the supervisor tool.

## Choose the judge's thinking level

To override the judge only, add this optional line to `.loop/yoke.conf`:

```text
judge-thinking auto
```

Yoke passes the value through to `omp --thinking`. If omitted, the judge
inherits `thinking`; if both are omitted, omp chooses its default. The
override does not change the worker or periodic agents.
