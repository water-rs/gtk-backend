#!/usr/bin/env bash
# Appends the run's merged metrics to the orphan `metrics` branch. Artifacts
# expire; the branch is the durable trend store — history/<run_id>.jsonl per
# run plus latest.jsonl as the always-current snapshot.
set -euo pipefail

dir="${1:?usage: append-metrics-history.sh <dir containing metrics-*.jsonl>}"
run_id="${GITHUB_RUN_ID:?}"
repo="${GITHUB_REPOSITORY:?}"
: "${GITHUB_TOKEN:?}"

hist=$(mktemp -d)
git -C "${hist}" init -q
git -C "${hist}" remote add origin \
    "https://x-access-token:${GITHUB_TOKEN}@github.com/${repo}.git"
if git -C "${hist}" fetch -q --depth 1 origin metrics; then
    git -C "${hist}" checkout -q -b metrics FETCH_HEAD
else
    git -C "${hist}" checkout -q --orphan metrics
fi

mkdir -p "${hist}/history"
cat "${dir}"/metrics-*.jsonl >"${hist}/history/${run_id}.jsonl"
cat "${dir}"/metrics-*.jsonl >"${hist}/latest.jsonl"

git -C "${hist}" add history latest.jsonl
git -C "${hist}" \
    -c user.name="github-actions[bot]" \
    -c user.email="41898282+github-actions[bot]@users.noreply.github.com" \
    commit -qm "nightly e2e metrics ${run_id}"
git -C "${hist}" push -q origin metrics
