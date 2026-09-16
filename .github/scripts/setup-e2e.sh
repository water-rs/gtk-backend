#!/usr/bin/env bash
# Prepares a waterui checkout for driving the examples: clones waterui at the
# requested ref (dev for the nightly), initializes whatever submodules that
# revision still records (none, since water-rs/waterui#937; older revisions
# carry workspace members there), places this repository's tested tree at
# `backends/gtk`, and rewrites the checkout's `waterui-gtk` workspace
# dependency to that path so every generated backend crate builds the backend
# under test. The checkout declares `waterui-gtk` as a git dependency pinned to
# a released revision, which `[patch.crates-io]` cannot redirect — only a
# `[patch."<repo-url>"]` table could — so the dependency declaration itself is
# rewritten instead.
set -euo pipefail

repo_root="${GITHUB_WORKSPACE:-$(pwd)}"
waterui_dir="${repo_root}/waterui"
waterui_ref="${WATERUI_REF:-dev}"

echo "Using waterui ref: ${waterui_ref}"
rm -rf "${waterui_dir}"
# Fetch by exact SHA: `git clone --branch` cannot check out a raw commit, and
# this branch pins WATERUI_REF to one. A detached FETCH_HEAD accepts branch
# names and SHAs alike (the server allows reachable-SHA fetches).
git init -q "${waterui_dir}"
git -C "${waterui_dir}" remote add origin https://github.com/water-rs/waterui.git
git -C "${waterui_dir}" fetch -q --depth 1 origin "${waterui_ref}"
git -C "${waterui_dir}" checkout -q --detach FETCH_HEAD
git -C "${waterui_dir}" submodule update --init --depth 1

mkdir -p "${waterui_dir}/backends/gtk"
git -C "${repo_root}" archive HEAD | tar -x -C "${waterui_dir}/backends/gtk"

python3 - "${waterui_dir}/Cargo.toml" <<'EOF'
import re
import sys

path = sys.argv[1]
with open(path) as f:
    lines = f.readlines()
for i, line in enumerate(lines):
    if re.match(r'^waterui-gtk\s*=', line):
        version = re.search(r'version\s*=\s*"([^"]+)"', line)
        suffix = f', version = "{version.group(1)}"' if version else ""
        lines[i] = f'waterui-gtk = {{ path = "backends/gtk"{suffix} }}\n'
        break
else:
    sys.exit("no waterui-gtk dependency declaration found in the workspace manifest")
with open(path, "w") as f:
    f.writelines(lines)
EOF

# The map example builds `waterui-map-gpu` from crates.io, which only ever
# contains released fixes. A nightly exists to catch integration breakage in
# the latest code, so pin it to the map-gpu dev head resolved at run time —
# the same "exact commit on dev" mechanism used for unreleased work
# everywhere else.
map_gpu_rev="${MAP_GPU_REV:-$(git ls-remote https://github.com/water-rs/map-gpu.git dev | cut -f1)}"
if [[ -z ${map_gpu_rev} ]]; then
    echo "could not resolve water-rs/map-gpu dev head" >&2
    exit 1
fi
echo "Using waterui-map-gpu rev: ${map_gpu_rev}"
python3 - "${waterui_dir}/Cargo.toml" "${map_gpu_rev}" <<'EOF'
import re
import sys

path, rev = sys.argv[1], sys.argv[2]
with open(path) as f:
    lines = f.readlines()
for i, line in enumerate(lines):
    if re.match(r'^waterui-map-gpu\s*=', line):
        lines[i] = (
            f'waterui-map-gpu = {{ git = "https://github.com/water-rs/map-gpu.git",'
            f' rev = "{rev}" }}\n'
        )
        break
else:
    sys.exit("no waterui-map-gpu dependency declaration found in the workspace manifest")
with open(path, "w") as f:
    f.writelines(lines)
EOF
