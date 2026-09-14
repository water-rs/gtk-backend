#!/usr/bin/env bash
# Merges the shards' metrics-*.jsonl files into a markdown table for the run
# summary. jq is preinstalled on GitHub-hosted runners.
set -euo pipefail

dir="${1:?usage: report-metrics.sh <dir containing metrics-*.jsonl>}"

shopt -s nullglob
files=("${dir}"/metrics-*.jsonl)
if ((${#files[@]} == 0)); then
    echo "_No metrics recorded — every shard failed before measuring._"
    exit 0
fi

jq -sr '
    def mib: if . == null then "—" else ((. / 1048576 * 100 | round) / 100 | tostring) + " MiB" end;
    def kib: if . == null then "—" else tostring + " KiB" end;
    def ms: if . == null then "—" else tostring + " ms" end;
    sort_by(.example) as $rows
    | "| example | binary | settled RSS | peak RSS | startup |",
      "|---|---:|---:|---:|---:|",
      ($rows[] | "| \(.example) | \(.binary_bytes | mib) | \(.rss_kib | kib) | \(.peak_rss_kib | kib) | \(.startup_ms | ms) |")
' "${files[@]}"
