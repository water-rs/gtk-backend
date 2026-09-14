#!/usr/bin/env bash
# Drives this shard's examples through `water package --platform linux
# --backend gtk4 --release`, launches the staged binary directly, captures a
# settled screenshot of each window, and diffs it against the golden in
# e2e/goldens/. The release package keeps the binary a user would ship, so the
# per-example metrics this run records — binary size, settled RSS, and
# exec-to-first-window latency — describe the artifact, not a debug build.
#
# Must run inside an X session with a window manager — GTK4 toplevels only get
# real focus when a WM is running. The workflow wraps this script in
#   xvfb-run -a dbus-run-session -- sh -c 'openbox & sleep 2; exec …'
#
# Per example, failure means: the launcher died or never raised a window, the
# capture stayed unsettled past the deadline, the frame is blank, or the frame
# differs from its golden beyond the diff budget. A `<name>.skip` marker beside
# a golden disables the pixel diff for that example (content check only);
# `e2e/skip.txt` excludes examples that cannot run at all.
#
# Env: WATERUI_DIR, EXAMPLE_LOG_DIR, SHOTS_DIR, METRICS_DIR, SHARD_INDEX,
# SHARD_TOTAL; optional BASELINES_DIR (default e2e/goldens in this repo),
# RECORD=1 with RECORD_DIR to capture fresh baselines instead of comparing.
set -uo pipefail

repo_root="${GITHUB_WORKSPACE:-$(pwd)}"
waterui_dir="${WATERUI_DIR:?WATERUI_DIR must point at the waterui checkout}"
scripts_dir="${repo_root}/.github/scripts"
log_dir="${EXAMPLE_LOG_DIR:?}"
shots_dir="${SHOTS_DIR:?}"
metrics_dir="${METRICS_DIR:?}"
baselines_dir="${BASELINES_DIR:-${repo_root}/e2e/goldens}"
skip_file="${repo_root}/e2e/skip.txt"
shard_index="${SHARD_INDEX:?}"
shard_total="${SHARD_TOTAL:?}"
record="${RECORD:-0}"
record_dir="${RECORD_DIR:-${repo_root}/e2e-candidates}"

mkdir -p "${log_dir}" "${shots_dir}" "${record_dir}" "${metrics_dir}"

# One target dir serves every generated backend crate in the shard: they share
# the same dependency graph, so the first example's build warms the rest.
export CARGO_TARGET_DIR="${repo_root}/e2e-target"
export LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe

WINDOW_APPEAR_DEADLINE=1500   # the shard's first build is cold
SETTLE_DEADLINE=90            # frames stable before capture
SETTLE_BUDGET=0.005           # normalized RMSE between consecutive frames
DIFF_BUDGET=0.02              # normalized RMSE against the golden
BLANK_STDDEV=10               # Q16 scale; a uniform fill reads ~0

skipped() {
    [[ -f ${skip_file} ]] \
        && grep -E '^\s*[^#[:space:]]' "${skip_file}" | awk '{print $1}' | grep -qx "$1"
}

toplevel_windows() {
    # Match on the WM_CLASS res_name (the binary's prgname, always set by GTK)
    # rather than the window title: examples that never assign a title leave
    # WM_NAME empty and would be invisible to a --name filter.
    xdotool search --onlyvisible --screen 0 --classname '.*' 2>/dev/null | sort -n
}

new_toplevel() {
    comm -13 <(echo "$1") <(toplevel_windows) | head -1
}

stop_launcher() {
    kill -- "-$1" 2>/dev/null || true
    wait "$1" 2>/dev/null || true
    sleep 1
}

normalized_rmse() {
    local delta
    delta=$(compare -metric RMSE "$1" "$2" null: 2>&1 || true)
    delta="${delta##*\(}"
    delta="${delta%%\)*}"
    echo "${delta:-1}"
}

# Appends one JSON object per example to the shard's metrics file; missing
# values stay null rather than reading as real zeros downstream.
record_metric() {
    printf '{"example":"%s","binary_bytes":%s,"rss_kib":%s,"peak_rss_kib":%s,"startup_ms":%s}\n' \
        "$1" "${2:-null}" "${3:-null}" "${4:-null}" "${5:-null}" \
        >>"${metrics_dir}/metrics-${shard_index}.jsonl"
}

run_example() {
    local name=$1
    local log="${log_dir}/${name}.log" shot="${shots_dir}/${name}.png"
    local baseline="${baselines_dir}/${name}.png"

    cd "${waterui_dir}/examples/${name}" || return 1
    : >"${log}"

    # Packaging produces the same binary a user would run; measuring it keeps
    # size/RSS/startup honest instead of reporting debug-profile numbers.
    if ! water package --platform linux --backend gtk4 --release >>"${log}" 2>&1; then
        echo "FAIL ${name}: water package failed (see log)"
        return 1
    fi

    local crate bin
    crate=$(sed -n 's/^name = "\([^"]*\)".*/\1/p' Cargo.toml | head -1)
    bin=$(find "${CARGO_TARGET_DIR}" . -type f -name "${crate}-gtk4" \
        -path "*/release/*" 2>/dev/null | head -1)
    if [[ -z ${bin} || ! -x ${bin} ]]; then
        echo "FAIL ${name}: release binary ${crate}-gtk4 not found under ${CARGO_TARGET_DIR}"
        return 1
    fi

    local binary_bytes
    binary_bytes=$(stat -c%s "${bin}")

    local before launcher win launch_ms window_ms
    before=$(toplevel_windows)
    launch_ms=$(date +%s%3N)
    setsid "${bin}" >>"${log}" 2>&1 &
    launcher=$!

    win=""
    local deadline=$((SECONDS + WINDOW_APPEAR_DEADLINE))
    while ((SECONDS < deadline)); do
        win=$(new_toplevel "${before}")
        [[ -n ${win} ]] && break
        if ! kill -0 "${launcher}" 2>/dev/null; then
            record_metric "${name}" "${binary_bytes}" "" "" ""
            echo "FAIL ${name}: app exited before a window appeared (see log)"
            return 1
        fi
        # Fine-grained poll: startup_ms is only as precise as this loop.
        sleep 0.2
    done
    window_ms=$(date +%s%3N)
    if [[ -z ${win} ]]; then
        stop_launcher "${launcher}"
        record_metric "${name}" "${binary_bytes}" "" "" ""
        echo "FAIL ${name}: no window within ${WINDOW_APPEAR_DEADLINE}s"
        return 1
    fi

    local prev="${shots_dir}/.${name}.prev.png" settled=0
    import -window "${win}" "${prev}" >>"${log}" 2>&1
    deadline=$((SECONDS + SETTLE_DEADLINE))
    while ((SECONDS < deadline)); do
        sleep 3
        import -window "${win}" "${shot}" >>"${log}" 2>&1 || break
        if (($(awk "BEGIN{print ($(normalized_rmse "${prev}" "${shot}") <= ${SETTLE_BUDGET})}") == 1)); then
            settled=1
            break
        fi
        cp "${shot}" "${prev}"
    done
    rm -f "${prev}"

    # Read RSS at the settled frame, then stop the app; the numbers describe
    # the idle-after-render state a user would actually hold open.
    local rss_kib="" peak_rss_kib=""
    if [[ -r /proc/${launcher}/status ]]; then
        rss_kib=$(awk '/^VmRSS/{print $2}' "/proc/${launcher}/status")
        peak_rss_kib=$(awk '/^VmHWM/{print $2}' "/proc/${launcher}/status")
    fi
    stop_launcher "${launcher}"
    record_metric "${name}" "${binary_bytes}" "${rss_kib}" "${peak_rss_kib}" \
        "$((window_ms - launch_ms))"
    ((settled)) || echo "WARN ${name}: frame never settled; using the last capture"

    local stdev
    stdev=$(identify -format '%[standard-deviation]' "${shot}" 2>/dev/null || echo 0)
    if (($(awk "BEGIN{print (${stdev:-0} <= ${BLANK_STDDEV})}") == 1)); then
        echo "FAIL ${name}: captured frame is blank"
        return 1
    fi

    if ((record)); then
        cp "${shot}" "${record_dir}/${name}.png"
        echo "RECORD ${name}"
        return 0
    fi
    if [[ -f ${baseline}.skip || -f ${baseline%.png}.skip ]]; then
        echo "PASS ${name} (content check only)"
        return 0
    fi
    if [[ ! -f ${baseline} ]]; then
        echo "FAIL ${name}: no golden at ${baseline} — record one"
        return 1
    fi
    local delta
    delta=$(normalized_rmse "${baseline}" "${shot}")
    if (($(awk "BEGIN{print (${delta} <= ${DIFF_BUDGET})}") == 1)); then
        echo "PASS ${name} (diff ${delta})"
        return 0
    fi
    echo "FAIL ${name}: diff ${delta} exceeds ${DIFF_BUDGET}"
    return 1
}

mapfile -t examples < <("${scripts_dir}/discover-examples.sh")
failures=()
ran=0
for i in "${!examples[@]}"; do
    ((i % shard_total == shard_index)) || continue
    name=${examples[i]}
    if skipped "${name}"; then
        echo "SKIP ${name}"
        continue
    fi
    ran=$((ran + 1))
    run_example "${name}" || failures+=("${name}")
done

echo "----"
echo "shard ${shard_index}/${shard_total}: ${ran} run, ${#failures[@]} failed"
((${#failures[@]})) && printf 'failed: %s\n' "${failures[@]}"
((${#failures[@]} == 0))
