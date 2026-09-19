#!/usr/bin/env bash
# TEMPORARY (#67 diagnostic): reproduce the intermittent never-presents
# failure. Packages the example once, then repeatedly launches it, picks the
# toplevel with the same lowest-new-XID rule the E2E harness uses, captures a
# handful of frames, and kills it. On a blank capture it dumps the full X
# window state and the app's GDK event stream so the failure can be
# diagnosed instead of retried past.
set -uo pipefail

waterui_dir="${WATERUI_DIR:-${GITHUB_WORKSPACE:?}/waterui}"
log_dir="${EXAMPLE_LOG_DIR:-${GITHUB_WORKSPACE}/repro-logs}"
shots_dir="${SHOTS_DIR:-${GITHUB_WORKSPACE}/repro-shots}"
name="${REPRO_EXAMPLE:-shape}"
loops="${REPRO_LOOPS:-60}"
frames="${REPRO_FRAMES:-8}"
frame_gap="${REPRO_FRAME_GAP:-1}"

mkdir -p "${log_dir}" "${shots_dir}"

BLANK_STDDEV=10
X_TOOL_TIMEOUT=10
CAPTURE_TIMEOUT=60

toplevel_windows() {
    timeout "${X_TOOL_TIMEOUT}" xdotool search --onlyvisible --screen 0 --classname '.*' 2>/dev/null | sort -n
}
new_toplevel() {
    comm -13 <(echo "$1") <(toplevel_windows) | head -1
}
is_blank() {
    local stdev
    stdev=$(timeout "${CAPTURE_TIMEOUT}" identify -format '%[standard-deviation]' "$1" 2>/dev/null || echo 0)
    (($(awk "BEGIN{print (${stdev:-0} <= ${BLANK_STDDEV})}") == 1))
}

cd "${waterui_dir}/examples/${name}" || exit 1

# Package once; the loop only launches the staged artifact.
if ! timeout 1800 water package --platform linux --backend gtk4 --release --yes >"${log_dir}/package.log" 2>&1; then
    echo "repro: package failed (see package.log)"
    exit 1
fi
bin=$(sed -n 's/.*Packaged at //p' "${log_dir}/package.log" | tail -1 | sed 's/\x1b\[[0-9;]*m//g' | tr -d '\r')
if [[ -z ${bin} || ! -x ${bin} ]]; then
    echo "repro: no executable artifact"
    exit 1
fi
echo "repro: binary ${bin}"

hits=0
for ((i = 1; i <= loops; i++)); do
    log="${log_dir}/${name}-${i}.log"
    before=$(toplevel_windows)
    RUST_LOG="${RUST_LOG:-info,waterui_gtk=debug,waterui_graphics=debug}" \
        GDK_DEBUG=events \
        G_MESSAGES_DEBUG=Gdk \
        setsid "${bin}" >"${log}" 2>&1 &
    launcher=$!

    win="" deadline=$((SECONDS + 25))
    while ((SECONDS < deadline)); do
        win=$(new_toplevel "${before}")
        [[ -n ${win} ]] && break
        kill -0 "${launcher}" 2>/dev/null || break
        sleep 0.2
    done
    if [[ -z ${win} ]]; then
        kill -- "-${launcher}" 2>/dev/null; sleep 0.3; kill -9 -- "-${launcher}" 2>/dev/null
        wait "${launcher}" 2>/dev/null
        echo "repro[${i}]: no window"
        continue
    fi

    blank_at=""
    for ((f = 1; f <= frames; f++)); do
        sleep "${frame_gap}"
        shot="${shots_dir}/${name}-${i}-${f}.png"
        timeout "${CAPTURE_TIMEOUT}" import -window "${win}" "${shot}" >>"${log}" 2>&1
        if [[ -s ${shot} ]] && is_blank "${shot}"; then
            blank_at=$f
        else
            blank_at=""
        fi
    done

    if [[ -n ${blank_at} ]]; then
        hits=$((hits + 1))
        {
            echo "== xdiag win=${win} blank hit ${i}/${blank_at} =="
            timeout "${X_TOOL_TIMEOUT}" xwininfo -id "${win}" -all 2>&1
            echo "== xdiag root tree =="
            timeout "${X_TOOL_TIMEOUT}" xwininfo -root -tree 2>&1
            echo "== xdiag xprop =="
            timeout "${X_TOOL_TIMEOUT}" xprop -id "${win}" 2>&1
        } >>"${log}" 2>&1
        cp "${shots_dir}/${name}-${i}-${blank_at}.png" "${shots_dir}/${name}-blank-${i}.png" 2>/dev/null || true
        echo "repro[${i}]: BLANK capture win=${win} frame=${blank_at}"
    else
        rm -f "${shots_dir}/${name}-${i}-"*.png
        echo "repro[${i}]: ok win=${win}"
    fi

    kill -- "-${launcher}" 2>/dev/null
    for _ in 1 2 3; do kill -0 "${launcher}" 2>/dev/null || break; sleep 0.5; done
    kill -9 -- "-${launcher}" 2>/dev/null
    wait "${launcher}" 2>/dev/null
done

echo "repro: ${hits}/${loops} blank hits"
((hits == 0))
