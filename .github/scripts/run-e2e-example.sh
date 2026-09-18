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
# Failure means: the launcher died or never raised a window, the example
# never reached render readiness, the capture stayed unsettled past the
# deadline, the frame is blank, or the frame differs from its golden beyond
# the diff budget. A `<name>.skip` marker beside a golden disables the pixel
# diff for that example (content check only); `e2e/skip.txt` excludes
# examples that cannot run at all.
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

# Readiness configuration per example. The pattern must be an event the
# consumed source already emits — instrumentation adds only the per-surface
# completion events it sequences against. Minimums keep the gate honest
# where the example is known to produce GPU widgets: an empty created set
# would satisfy membership vacuously and read missing instrumentation as
# readiness. Examples with no GPU content take the zero defaults and the
# gate stays vacuous, as before.
case "${example}" in
    map)    READINESS_PATTERN="activated prepared GPU map"
            MIN_GPU_SURFACES=1; MIN_FILTER_HOSTS=0 ;;
    stress) READINESS_PATTERN=""
            MIN_GPU_SURFACES=1; MIN_FILTER_HOSTS=1 ;;
    *)      READINESS_PATTERN=""
            MIN_GPU_SURFACES=0; MIN_FILTER_HOSTS=0 ;;
esac

mkdir -p "${log_dir}" "${shots_dir}" "${record_dir}" "${metrics_dir}"

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
READINESS_DEADLINE=90         # per-surface render completion before capture
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

    # Packaging produces the same binary a user would run; measuring it keeps
    # size/RSS/startup honest instead of reporting debug-profile numbers.
    if ! timeout "${PACKAGE_DEADLINE}" water package --platform linux --backend gtk4 --release --yes >>"${log}" 2>&1; then
        echo "FAIL ${name}: water package failed or exceeded ${PACKAGE_DEADLINE}s (see log)"
        return 1
    fi

    # `water package` names the artifact it produced (`Packaged at <path>`);
    # since water-rs/cli#65 it builds into the CLI's per-user shared target,
    # not under CARGO_TARGET_DIR, so the log line is the only honest source.
    local bin
    bin=$(sed -n 's/.*Packaged at //p' "${log}" | tail -1 | sed 's/\x1b\[[0-9;]*m//g' | tr -d '\r')
    if [[ -z ${bin} || ! -x ${bin} ]]; then
        echo "FAIL ${name}: water package reported no executable artifact (${bin:-no 'Packaged at' line}; see log)"
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
    # WATERUI_GTK_LAYOUT_DEBUG stays off: it emits several lines per measured
    # widget per negotiation — tens of GB per example in run 35286011114 —
    # and the write pressure alone starves the app's main loop.
    RUST_LOG="${RUST_LOG:-info,waterui_gtk=debug,waterui_graphics=debug,waterui_media=debug,waterui_map_gpu=debug}" \
        RUST_BACKTRACE=1 \
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

    # Readiness gate. The backend emits a completion event only after a
    # surface's full render path returns — never at frame entry — keyed by
    # the widget's own pointer, so the gate is set membership of unique
    # identities rather than an event count one widget can satisfy alone.
    # Run 35095065215 showed why this precedes the settle loop: two
    # identical all-black captures "settled" ~3 s after the window mapped,
    # while every surface had been created and none had completed a frame.
    created_surface_ids() {
        grep -oE 'create GLArea widget surface_id=[0-9]+' "${log}" | grep -oE '[0-9]+$' | sort -u
    }
    allocated_surface_ids() {
        grep -oE 'GLArea allocated surface_id=[0-9]+' "${log}" | grep -oE '[0-9]+$' | sort -u
    }
    rendered_surface_ids() {
        grep -oE 'surface render complete surface_id=[0-9]+' "${log}" | grep -oE '[0-9]+$' | sort -u
    }
    created_host_ids() {
        grep -oE 'create filter host host_id=[0-9]+' "${log}" | grep -oE '[0-9]+$' | sort -u
    }
    presented_host_ids() {
        grep -oE 'filtered frame presented host_id=[0-9]+' "${log}" | grep -oE '[0-9]+$' | sort -u
    }

    readiness_met() {
        (($(created_surface_ids | wc -l) >= MIN_GPU_SURFACES)) || return 1
        (($(created_host_ids | wc -l) >= MIN_FILTER_HOSTS)) || return 1
        # A surface owes a completed frame only once GTK gave it pixels:
        # gtk_gl_area_snapshot returns early on a zero allocation, so a
        # created-but-never-allocated GLArea (a surface whose content has
        # not arrived, or one laid out at zero) legitimately never emits
        # render. Counting every created widget made reply and video_player
        # fail readiness on a drawable set of zero.
        comm -23 <(allocated_surface_ids) <(rendered_surface_ids) | grep -q . && return 1
        comm -23 <(created_host_ids) <(presented_host_ids) | grep -q . && return 1
        # Content readiness is sequenced, not just present: map logs
        # "activated prepared GPU map" while building the very frame that
        # consumes the prepared scene, so a completion after that line
        # observes a frame submitted with the activated scene. Submission
        # is all the gate observes — the settle loop and golden diff below
        # remain the visual check.
        if [[ -n ${READINESS_PATTERN} ]]; then
            awk -v pat="${READINESS_PATTERN}" '
                $0 ~ pat { armed = 1; next }
                armed && /surface render complete surface_id=/ { hit = 1 }
                END { exit(hit ? 0 : 1) }
            ' "${log}" || return 1
        fi
        return 0
    }

    # Poll on the same 3 s cadence the settle loop uses; nothing sleeps
    # waiting for content, and no pixel statistic participates here.
    local ready=0
    deadline=$((SECONDS + READINESS_DEADLINE))
    while ((SECONDS < deadline)); do
        if readiness_met; then
            ready=1
            break
        fi
        sleep 3
    done

    if ((!ready)); then
        # Bounded final capture banked for review — an unresolved id bounds
        # where to look (hidden/offscreen widgets may legitimately never
        # render); it is diagnostic evidence, not by itself proof of a
        # runtime fault, but this example cannot be accepted on its frames.
        timeout "${CAPTURE_TIMEOUT}" import -window "${win}" "${shot}" >>"${log}" 2>&1 || true
        if ((record)) && [[ -s ${shot} ]]; then
            cp "${shot}" "${record_dir}/${name}.png"
        fi
        local n_surfaces n_hosts unresolved_surfaces unallocated_surfaces unresolved_hosts detail
        n_surfaces=$(created_surface_ids | wc -l | tr -d ' ')
        n_hosts=$(created_host_ids | wc -l | tr -d ' ')
        unresolved_surfaces=$(comm -23 <(allocated_surface_ids) <(rendered_surface_ids) | tr '\n' ' ')
        unallocated_surfaces=$(comm -23 <(created_surface_ids) <(allocated_surface_ids) | tr '\n' ' ')
        unresolved_hosts=$(comm -23 <(created_host_ids) <(presented_host_ids) | tr '\n' ' ')
        detail="observed surfaces=${n_surfaces} hosts=${n_hosts}; unresolved surface_ids=[${unresolved_surfaces% }] unallocated surface_ids=[${unallocated_surfaces% }] host_ids=[${unresolved_hosts% }]"
        [[ -n ${READINESS_PATTERN} ]] \
            && detail="${detail}, sequence '${READINESS_PATTERN}' + render-complete unmet"
        stop_launcher "${launcher}"
        record_metric "${name}" "${binary_bytes}" "" "" "$((window_ms - launch_ms))"
        echo "FAIL ${name}: readiness deadline ${READINESS_DEADLINE}s reached (${detail})"
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
    local crashed=0 rss_kib="" peak_rss_kib=""
    kill -0 "${launcher}" 2>/dev/null || crashed=1
    if [[ -r /proc/${launcher}/status ]]; then
        rss_kib=$(awk '/^VmRSS/{print $2}' "/proc/${launcher}/status")
        peak_rss_kib=$(awk '/^VmHWM/{print $2}' "/proc/${launcher}/status")
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