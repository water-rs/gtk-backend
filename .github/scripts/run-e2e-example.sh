#!/usr/bin/env bash
# Drives one example through `water package --platform linux --backend gtk4
# --release`, launches the staged binary directly, captures a settled
# screenshot of its window, and diffs it against the golden in e2e/goldens/.
# The release package keeps the binary a user would ship, so the metrics this
# run records — binary size, settled RSS, and exec-to-first-window latency —
# describe the artifact, not a debug build.
#
# Must run inside an X session with a window manager — GTK4 toplevels only get
# real focus when a WM is running. The workflow wraps this script in
#   xvfb-run -a dbus-run-session -- sh -c 'openbox & sleep 2; exec …'
#
# Failure means: the launcher died or never raised a window, the capture
# stayed unsettled past the deadline, the frame is blank, or the frame
# differs from its golden beyond the diff budget. A `<name>.skip` marker
# beside a golden disables the pixel diff for that example (content check
# only); `e2e/skip.txt` excludes examples that cannot run at all.
#
# Env: WATERUI_DIR, EXAMPLE, EXAMPLE_LOG_DIR, SHOTS_DIR, METRICS_DIR;
# optional BASELINES_DIR (default e2e/goldens in this repo), RECORD=1 with
# RECORD_DIR to capture a fresh baseline instead of comparing.
set -uo pipefail

repo_root="${GITHUB_WORKSPACE:-$(pwd)}"
waterui_dir="${WATERUI_DIR:?WATERUI_DIR must point at the waterui checkout}"
log_dir="${EXAMPLE_LOG_DIR:?}"
shots_dir="${SHOTS_DIR:?}"
metrics_dir="${METRICS_DIR:?}"
baselines_dir="${BASELINES_DIR:-${repo_root}/e2e/goldens}"
skip_file="${repo_root}/e2e/skip.txt"
example="${EXAMPLE:?EXAMPLE must name the example to run}"
record="${RECORD:-0}"
record_dir="${RECORD_DIR:-${repo_root}/e2e-candidates}"

mkdir -p "${log_dir}" "${shots_dir}" "${record_dir}" "${metrics_dir}"

# Diagnostic branch only: the runner image has no gdb; install it so the
# launcher can run every example under a batch backtrace harness.
if ! command -v gdb >/dev/null 2>&1; then
    sudo apt-get update -qq && sudo apt-get install -y -qq gdb >/dev/null
fi

# The generated backend crate shares waterui's dependency graph; the rust
# cache warms it across runs.
export CARGO_TARGET_DIR="${repo_root}/e2e-target"
export LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe

# Multi-GiB examples (stress peaked near 4 GiB RSS) would otherwise spend
# minutes writing cores on every crash.
ulimit -c 0

# The window wait starts after `water package` finishes, so it only covers
# process spawn to first mapped toplevel — a healthy app maps in seconds.
WINDOW_APPEAR_DEADLINE=120
SETTLE_DEADLINE=90            # frames stable before capture
SETTLE_BUDGET=0.005           # normalized RMSE between consecutive frames
DIFF_BUDGET=0.02              # normalized RMSE against the golden
BLANK_STDDEV=10               # Q16 scale; a uniform fill reads ~0

# A crashed client can wedge Xvfb mid-request; every helper call must stay
# bounded or one crash freezes the whole job behind a blocked xdotool or
# import. PACKAGE_DEADLINE is generous — the dependency build can be cold.
PACKAGE_DEADLINE=1800
X_TOOL_TIMEOUT=10
CAPTURE_TIMEOUT=60

skipped() {
    [[ -f ${skip_file} ]] \
        && grep -E '^\s*[^#[:space:]]' "${skip_file}" | awk '{print $1}' | grep -qx "$1"
}

toplevel_windows() {
    # Match on the WM_CLASS res_name (the binary's prgname, always set by GTK)
    # rather than the window title: examples that never assign a title leave
    # WM_NAME empty and would be invisible to a --name filter.
    timeout "${X_TOOL_TIMEOUT}" xdotool search --onlyvisible --screen 0 --classname '.*' 2>/dev/null | sort -n
}

new_toplevel() {
    comm -13 <(echo "$1") <(toplevel_windows) | head -1
}

stop_launcher() {
    # Diag: TERM the inferior alone -- its controlling gdb intercepts the
    # signal, dumps every thread's backtrace, then exits with the process.
    # This captures stacks of wedged apps, not just crashes.
    local inferior
    inferior=$(pgrep -P "$1" | head -1)
    [[ -n ${inferior} ]] && kill -TERM "${inferior}" 2>/dev/null || true
    for _ in $(seq 1 20); do
        kill -0 "$1" 2>/dev/null || break
        sleep 1
    done
    kill -- "-$1" 2>/dev/null || true
    # A wedged app must not stall the job: give SIGTERM a moment, then KILL.
    for _ in 1 2 3 4 5; do
        kill -0 "$1" 2>/dev/null || break
        sleep 1
    done
    kill -9 -- "-$1" 2>/dev/null || true
    wait "$1" 2>/dev/null || true
}

normalized_rmse() {
    local delta
    delta=$(timeout "${CAPTURE_TIMEOUT}" compare -metric RMSE "$1" "$2" null: 2>&1 || true)
    delta="${delta##*\(}"
    delta="${delta%%\)*}"
    echo "${delta:-1}"
}

# A mapped toplevel that has not presented yet captures as a uniform fill.
is_blank() {
    local stdev
    stdev=$(timeout "${CAPTURE_TIMEOUT}" identify -format '%[standard-deviation]' "$1" 2>/dev/null || echo 0)
    (($(awk "BEGIN{print (${stdev:-0} <= ${BLANK_STDDEV})}") == 1))
}

# Appends one JSON object to this example's metrics file; missing values stay
# null rather than reading as real zeros downstream.
record_metric() {
    printf '{"example":"%s","binary_bytes":%s,"rss_kib":%s,"peak_rss_kib":%s,"startup_ms":%s}\n' \
        "$1" "${2:-null}" "${3:-null}" "${4:-null}" "${5:-null}" \
        >>"${metrics_dir}/metrics-${1}.jsonl"
}

run_example() {
    local name=$1
    local log="${log_dir}/${name}.log" shot="${shots_dir}/${name}.png"
    local baseline="${baselines_dir}/${name}.png"

    cd "${waterui_dir}/examples/${name}" || return 1
    : >"${log}"

    # Diagnostic branch only: generated manifests strip the release binary
    # ([profile.release] strip="symbols", debug off), which is why packaged
    # crashes arrive as ?? frames. Keep line tables and the symbol table so
    # gdb resolves Rust frames; opt-level/LTO/codegen-units/panic stay at the
    # production values so the binary keeps ship timing.
    export CARGO_PROFILE_RELEASE_DEBUG=1
    export CARGO_PROFILE_RELEASE_STRIP=none
    # Packaging produces the same binary a user would run; measuring it keeps
    # size/RSS/startup honest instead of reporting debug-profile numbers.
    if ! timeout "${PACKAGE_DEADLINE}" water package --platform linux --backend gtk4 --release --yes >>"${log}" 2>&1; then
        echo "FAIL ${name}: water package failed or exceeded ${PACKAGE_DEADLINE}s (see log)"
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
    # Backend diagnostics are emitted through `tracing`; without RUST_LOG the
    # subscriber only shows errors, so per-example GPU lifecycle detail needs
    # an explicit opt-in here. Output lands in the example's launcher log.
    RUST_LOG="${RUST_LOG:-info,waterui_gtk=debug,waterui_graphics=debug,waterui_media=debug,waterui::gtk::layout=debug}" \
        RUST_BACKTRACE=1 \
        WATERUI_GTK_LAYOUT_DEBUG=1 \
        setsid gdb -batch \
            -ex 'run' \
            -ex 'python
import gdb
# Required evidence first; an unmapped $pc can make disassembly fail, so the
# optional probes run last and every command is individually error-isolated.
for cmd in ["p/x $_siginfo", "info registers", "info proc mappings", "bt 40", "thread apply all bt", "info sharedlibrary", "disassemble $pc-32,$pc+16", "info symbol $rax"]:
    print("\n=== %s ===" % cmd)
    try:
        gdb.execute(cmd)
    except gdb.error as e:
        print("probe failed:", e)
end' \
            -ex 'quit' \
            --args "${bin}" >>"${log}" 2>&1 &
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
    timeout "${CAPTURE_TIMEOUT}" import -window "${win}" "${prev}" >>"${log}" 2>&1
    deadline=$((SECONDS + SETTLE_DEADLINE))
    while ((SECONDS < deadline)); do
        sleep 3
        timeout "${CAPTURE_TIMEOUT}" import -window "${win}" "${shot}" >>"${log}" 2>&1 || break
        # Two blank frames are trivially identical and would read as settled
        # while the window is still on its way to its first present — a blank
        # capture extends the wait, never satisfies it.
        is_blank "${shot}" && continue
        if (($(awk "BEGIN{print ($(normalized_rmse "${prev}" "${shot}") <= ${SETTLE_BUDGET})}") == 1)); then
            settled=1
            break
        fi
        cp "${shot}" "${prev}"
    done
    rm -f "${prev}"

    # Read RSS at the settled frame, then stop the app; the numbers describe
    # the idle-after-render state a user would actually hold open. A process
    # that already crashed has no status to read and is a failure even when a
    # capture exists — the frame is still recorded, but the example is red.
    local crashed=0 rss_kib="" peak_rss_kib="" inferior=""
    inferior=$(pgrep -P "${launcher}" | head -1 || true)
    if [[ -n ${inferior} ]]; then
        # Under gdb a crashed inferior is ptrace-stopped (t/T), not dead --
        # kill -0 alone would miss it. Treat stopped/zombie as crashed.
        local istate
        istate=$(ps -o stat= -p "${inferior}" 2>/dev/null | tr -d ' ')
        [[ -z ${istate} || ${istate} == [TtZ]* ]] && crashed=1
        kill -0 "${inferior}" 2>/dev/null || crashed=1
        if [[ -r /proc/${inferior}/status ]]; then
            rss_kib=$(awk '/^VmRSS/{print $2}' "/proc/${inferior}/status")
            peak_rss_kib=$(awk '/^VmHWM/{print $2}' "/proc/${inferior}/status")
        fi
    else
        kill -0 "${launcher}" 2>/dev/null || crashed=1
    fi
    stop_launcher "${launcher}"
    record_metric "${name}" "${binary_bytes}" "${rss_kib}" "${peak_rss_kib}" \
        "$((window_ms - launch_ms))"
    ((settled)) || echo "WARN ${name}: frame never settled; using the last capture"

    # In record mode the capture is still banked for review even when the run
    # below is red — a crashing or blank example has the most to learn from.
    local recorded=0
    if ((record)) && [[ -s ${shot} ]]; then
        cp "${shot}" "${record_dir}/${name}.png"
        recorded=1
    fi

    if ((crashed)); then
        echo "FAIL ${name}: app exited during capture (see log)"
        return 1
    fi

    if is_blank "${shot}"; then
        echo "FAIL ${name}: captured frame is blank"
        return 1
    fi

    if ((record)); then
        ((recorded)) || cp "${shot}" "${record_dir}/${name}.png"
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

# The workflow's discover job already filters skip.txt out of the matrix; the
# guard stays so a direct local invocation cannot silently run a skip-listed
# example either.
if skipped "${example}"; then
    echo "SKIP ${example}"
    exit 0
fi
run_example "${example}"
