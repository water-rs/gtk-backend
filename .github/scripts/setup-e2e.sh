#!/usr/bin/env bash
# Prepares a waterui checkout for driving the examples: clones waterui at the
# requested ref (dev for the nightly), initializes the submodules example builds
# resolve (`kit` and `utils/nami` are workspace members, so cargo cannot even
# read the workspace without them), places this repository's tested tree at
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
git clone --depth 1 --branch "${waterui_ref}" https://github.com/water-rs/waterui.git "${waterui_dir}"
git -C "${waterui_dir}" submodule update --init --depth 1 kit utils/nami

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
