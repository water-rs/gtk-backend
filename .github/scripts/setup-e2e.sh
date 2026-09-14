#!/usr/bin/env bash
# Prepares a waterui checkout for driving the examples: clones waterui at the
# requested ref (dev for the nightly), initializes the submodules example builds
# resolve (`kit` and `utils/nami` are workspace members, so cargo cannot even
# read the workspace without them), places this repository's tested tree at
# `backends/gtk`, and adds a `[patch.crates-io]` entry so generated backend
# crates resolve `waterui-gtk` from the tested tree instead of the published
# release — the CLI propagates the checkout's patch tables into every generated
# manifest (water-rs/waterui#758).
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

# The checkout declares `waterui-gtk` as a versioned registry dependency;
# redirecting it through [patch.crates-io] makes `local_checkout_dependency`
# prefer the tested tree — the same mechanism extracted crates use to stay on
# one framework graph.
python3 - "${waterui_dir}/Cargo.toml" <<'EOF'
import sys

path = sys.argv[1]
with open(path) as f:
    lines = f.readlines()
entry = 'waterui-gtk = { path = "backends/gtk" }\n'
if entry not in lines:
    start = lines.index("[patch.crates-io]\n")
    end = next(i for i in range(start + 1, len(lines)) if lines[i].startswith("["))
    lines.insert(end - 1 if lines[end - 1] == "\n" else end, entry)
with open(path, "w") as f:
    f.writelines(lines)
EOF
