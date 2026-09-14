#!/usr/bin/env bash
# Drives this shard's examples through `water run --platform linux --backend
# gtk4`, captures a settled screenshot of each window, and diffs it against the
# golden in e2e/goldens/.
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
# Env: WATERUI_DIR, EXAMPLE_LOG_DIR, SHOTS_DIR, SHARD_INDEX, SHARD_TOTAL;
# optional BASELINES_DIR (default e2e/goldens in this repo), RECORD=1 with
# RECORD_DIR to capture fresh baselines instead of comparing.
set -uo pipefail

repo_root="${GITHUB_WORKSPACE:-$(pwd)}"
waterui_dir="${WATERUI_DIR:?WATERUI_DIR must point at the waterui checkout}"
scripts_dir="${repo_root}/.github/scripts"
log_dir="${EXAMPLE_LOG_DIR:?}"
shots_dir="${SHOTS_DIR:?}"
baselines_dir="${BASELINES_DIR:-${repo_root}/e2e/goldens}"
skip_file="${repo_root}/e2e/skip.txt"
shard_index="${SHARD_INDEX:?}"
shard_total="${SHARD_TOTAL:?}"
record="${RECORD:-0}"
record_dir="${RECORD_DIR:-${repo_root}/e2e-candidates}"

mkdir -p "${log_dir}" "${shots_dir}" "${record_dir}"

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
    xdotool search --onlyvisible --screen 0 --name '.' 2>/dev/null | sort -n
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

run_example() {
    local name=$1
    local log="${log_dir}/${name}.log" shot="${shots_dir}/${name}.png"
    local baseline="${baselines_dir}/${name}.png"

    cd "${waterui_dir}/examples/${name}" || return 1
    : >"${log}"

    local before launcher win
    before=$(toplevel_windows)
    setsid water run --platform linux --backend gtk4 >"${log}" 2>&1 &
    launcher=$!

    win=""
    local deadline=$((SECONDS + WINDOW_APPEAR_DEADLINE))
    while ((SECONDS < deadline)); do
        win=$(new_toplevel "${before}")
        [[ -n ${win} ]] && break
        if ! kill -0 "${launcher}" 2>/dev/null; then
            echo "FAIL ${name}: water run exited before a window appeared (see log)"
            return 1
        fi
        sleep 3
    done
    if [[ -z ${win} ]]; then
        stop_launcher "${launcher}"
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
    stop_launcher "${launcher}"
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
