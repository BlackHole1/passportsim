#!/usr/bin/env bash
# Tests release-version.sh: `.github/scripts/release-version.test.sh`, from anywhere.
set -euo pipefail

script="$(dirname "$0")/release-version.sh"
failures=0

# expect <want> <tags, newline separated> <version> <bump>
expect() {
  local want="$1" tags="$2" got
  if got=$(printf '%s' "$tags" | "$script" "$3" "$4" 0.1.0 2> /dev/null); then :; else got="error"; fi
  if [ "$got" = "$want" ]; then
    echo "ok   $want <- tags [${tags//$'\n'/ }] version '$3' bump $4"
  else
    echo "FAIL want $want, got $got <- tags [${tags//$'\n'/ }] version '$3' bump $4"
    failures=$((failures + 1))
  fi
}

tags=$'v0.1.0\nv0.2.0\nv0.10.3\nv0.9.9\nv1.0.0-rc.1\nnightly'

expect 0.1.0 "" "" patch     # no tags: the workspace version
expect 0.1.0 "" "" major     # no tags: the bump is not applied to the first release
expect 0.4.0 "" "0.4.0" patch # no tags, explicit
expect 0.10.4 "$tags" "" patch
expect 0.11.0 "$tags" "" minor
expect 1.0.0 "$tags" "" major
expect 0.10.4 $'v0.10.3\r' "" patch # CRLF from a Windows git
expect 2.0.0 "$tags" "2.0.0" patch  # explicit and higher
expect 0.11.0 "$tags" "0.11.0" major
expect error "$tags" "0.10.2" patch # explicit and lower
expect error "$tags" "0.10.3" patch # the latest tag
expect error "$tags" "0.2.0" patch  # an existing tag
expect error "$tags" "v0.11.0" patch
expect error "$tags" "0.11" patch
expect error "$tags" "0.11.0-rc.1" patch
expect error "$tags" "01.0.0" patch
expect error "$tags" "" sideways

message=$(printf 'v0.2.0\n' | "$script" 0.2.0 patch 0.1.0 2>&1 || true)
case "$message" in
  *"tag v0.2.0 already exists"*) echo "ok   an existing tag is named" ;;
  *)
    echo "FAIL an existing tag: $message"
    failures=$((failures + 1))
    ;;
esac

if [ "$failures" -ne 0 ]; then
  echo "$failures failure(s)"
  exit 1
fi
echo "all passed"
