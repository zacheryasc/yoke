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

Initialize the project's loop with `yoke init`. Set `summary-model` in
`.loop/yoke.conf` if you want live summaries. The `yoke` tool can `start`,
`pause`, `resume`, `inject` steering, and `stop` a run. Summaries and the final
result appear as session messages; artifacts are under `.loop/runs/<run-id>/`.
Yoke's worker and judge sessions don't get the supervisor tool.

## Choose the judge's thinking level

In `.loop/yoke.conf`:

```text
thinking       low
judge-thinking high
```

`judge-thinking` accepts `off`, `low`, `medium`, `high`, `xhigh`, or `max`.
If omitted, the judge inherits `thinking`; if both are omitted, omp chooses
its default. The override does not change the worker or periodic agents.
