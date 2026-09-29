#!/usr/bin/env bash
# Prints the version the Release workflow publishes next.
#
#   git tag -l 'v*' | release-version.sh <version> <bump> <first>
#
# <version> is an explicit X.Y.Z or empty; <bump> is patch, minor or major; <first> is the version
# of the first release when no vX.Y.Z tag exists yet (the workspace version). Tags come on stdin,
# one per line; tags that are not vX.Y.Z are ignored. An explicit version must be new and greater
# than the latest tag; otherwise the latest tag is bumped.
set -euo pipefail

explicit="${1-}"
bump="${2:-patch}"
first="${3:?usage: release-version.sh <version> <bump> <first>}"

fail() {
  echo "release-version: $*" >&2
  exit 1
}

is_version() { [[ "$1" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; }

# Succeeds when X.Y.Z $1 is greater than X.Y.Z $2.
greater() {
  local -a a b
  IFS=. read -r -a a <<< "$1"
  IFS=. read -r -a b <<< "$2"
  for i in 0 1 2; do
    if ((a[i] != b[i])); then
      ((a[i] > b[i]))
      return
    fi
  done
  return 1
}

latest=""
tags=()
while IFS= read -r tag || [ -n "$tag" ]; do
  tag="${tag%$'\r'}"
  if [[ "$tag" != v* ]] || ! is_version "${tag#v}"; then continue; fi
  tags+=("${tag#v}")
  if [ -z "$latest" ] || greater "${tag#v}" "$latest"; then latest="${tag#v}"; fi
done

if [ -n "$explicit" ]; then
  is_version "$explicit" || fail "'$explicit' is not a version: expected X.Y.Z, for example 0.2.0"
  for tag in ${tags[@]+"${tags[@]}"}; do
    [ "$tag" != "$explicit" ] || fail "tag v$explicit already exists"
  done
  if [ -n "$latest" ] && ! greater "$explicit" "$latest"; then
    fail "$explicit is not greater than the latest release v$latest"
  fi
  echo "$explicit"
  exit 0
fi

if [ -z "$latest" ]; then
  is_version "$first" || fail "the first version '$first' is not X.Y.Z"
  echo "$first"
  exit 0
fi

IFS=. read -r major minor patch <<< "$latest"
case "$bump" in
  patch) echo "$major.$minor.$((patch + 1))" ;;
  minor) echo "$major.$((minor + 1)).0" ;;
  major) echo "$((major + 1)).0.0" ;;
  *) fail "bump '$bump' is not patch, minor or major" ;;
esac
