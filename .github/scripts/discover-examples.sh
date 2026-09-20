#!/usr/bin/env bash
# Print the sorted example names in waterui/examples/: every directory holding
# a Water.toml — the same definition the android and apple e2e suites use, so
# the suite grows with the repository rather than with a hand-kept list.
set -euo pipefail

waterui_dir="${WATERUI_DIR:?WATERUI_DIR must point at the waterui checkout}"

find "${waterui_dir}/examples" -mindepth 2 -maxdepth 2 -name Water.toml -print0 \
    | sort -z \
    | while IFS= read -r -d '' manifest; do
        basename "$(dirname "${manifest}")"
    done
