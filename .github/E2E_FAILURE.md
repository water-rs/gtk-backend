---
title: "nightly e2e: a GTK example run failed"
labels: [bug]
---

The scheduled GTK e2e run failed: {{ env.GITHUB_SERVER_URL }}/{{ env.GITHUB_REPOSITORY }}/actions/runs/{{ env.GITHUB_RUN_ID }}

Each failed `example-<name>` job carries its own artifacts (`e2e-shots-<name>`, `e2e-logs-<name>` on the run): the window capture and the `water package` + launcher log for that example. Common causes: the launcher exited before a window appeared (build or startup failure — read the log), the captured frame is blank (window mapped but never drew), or the diff against `e2e/goldens/<name>.png` exceeds the budget — either a rendering regression or an intended change that needs `golden_mode: record` rerun and the candidates reviewed into `e2e/goldens/`.

An example that structurally cannot run under GTK (no display-server service it needs, an engine dependency CI cannot provision) belongs in `e2e/skip.txt` with a reason — not in a permanently failing row.
