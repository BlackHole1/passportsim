#!/usr/bin/env bash
# Checks a deployed web bundle: the page answers 200 with the cross-origin isolation headers, the
# wasm core is served as application/wasm with the bytes of the local bundle (so the new version is
# live, not a cached old one), and the demo firmware is served when the bundle has it.
#
# usage: smoke-check.sh <local web bundle dir> <deployed base URL>
set -euo pipefail
dir="${1:?usage: smoke-check.sh <web bundle dir> <base URL>}"
base="${2:?usage: smoke-check.sh <web bundle dir> <base URL>}"
base="${base%/}/"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

sha256() { openssl dgst -sha256 -r "$1" | cut -d' ' -f1; }

# The value of header $1 in the header dump $2, lowercased, without the trailing CR.
header() { grep -i "^$1:" "$2" | tail -n 1 | cut -d: -f2- | tr -d '\r' | sed 's/^ *//' | tr '[:upper:]' '[:lower:]'; }

# fetch <path> <body file> <header file>: prints the HTTP status.
fetch() {
  curl --silent --show-error --location --retry 3 --retry-all-errors --max-time 300 \
    --dump-header "$3" --output "$2" --write-out '%{http_code}' "$base$1"
}

want_wasm=$(sha256 "$dir/pemu_wasm.wasm")
failures=()
# A new version reaches every edge within seconds to a minute; retry the whole check for a while.
for attempt in $(seq "${SMOKE_ATTEMPTS:-12}"); do
  failures=()
  status=$(fetch "" "$tmp/index.html" "$tmp/index.h" || true)
  [ "$status" = 200 ] || failures+=("GET / answered $status")
  [ "$(header cross-origin-opener-policy "$tmp/index.h")" = "same-origin" ] ||
    failures+=("/ has no Cross-Origin-Opener-Policy: same-origin")
  [ "$(header cross-origin-embedder-policy "$tmp/index.h")" = "require-corp" ] ||
    failures+=("/ has no Cross-Origin-Embedder-Policy: require-corp")

  # A cut-off body still reports 200, so curl's own failure is checked first.
  if ! status=$(fetch "pemu_wasm.wasm" "$tmp/core.wasm" "$tmp/core.h"); then
    failures+=("GET /pemu_wasm.wasm did not complete (HTTP $status)")
  elif [ "$status" != 200 ]; then
    failures+=("GET /pemu_wasm.wasm answered $status")
  else
    case "$(header content-type "$tmp/core.h")" in
      application/wasm*) ;;
      *) failures+=("/pemu_wasm.wasm is not served as application/wasm") ;;
    esac
    [ "$(sha256 "$tmp/core.wasm")" = "$want_wasm" ] ||
      failures+=("/pemu_wasm.wasm is not the deployed bundle's core (an older version is still served)")
  fi

  if [ -f "$dir/official.pebundle" ]; then
    status=$(curl --silent --show-error --location --head --max-time 60 --output /dev/null \
      --write-out '%{http_code}' "${base}official.pebundle" || true)
    [ "$status" = 200 ] || failures+=("HEAD /official.pebundle answered $status")
  fi

  if [ "${#failures[@]}" -eq 0 ]; then
    echo "smoke check passed: $base (core $want_wasm)"
    exit 0
  fi
  echo "attempt $attempt: ${failures[*]}"
  sleep 10
done
for failure in "${failures[@]}"; do echo "::error title=Smoke check $base::$failure"; done
exit 1
