#!/usr/bin/env bash
# Prints the `version` of `[workspace.package]` in Cargo.toml without running cargo, so the job
# that computes the release version does not install the pinned toolchain.
set -euo pipefail
manifest="${1:-Cargo.toml}"
version=$(awk '
  /^\[/ { in_block = ($0 == "[workspace.package]"); next }
  in_block && /^version[[:space:]]*=/ {
    sub(/^version[[:space:]]*=[[:space:]]*"/, ""); sub(/".*$/, ""); print; exit
  }
' "$manifest")
if [ -z "$version" ]; then
  echo "no version in [workspace.package] of $manifest" >&2
  exit 1
fi
echo "$version"
