#!/usr/bin/env bash
# Regenerate the oracle consoles with the pinned configuration: one run per image, writing the
# console, the memory-region trace and a header block. macOS only.
#
# The oracle is a black box: this script starts it, collects its console and its memory-region
# trace and writes a header block beside them (the command line, the binary, ROM and eFuse
# hashes, and the strap value read back from the console). It never links the oracle and never
# reads its sources (`CONTRIBUTING.md`, "Clean room").
#
# usage: bash tools/oracle/regen-consoles.sh <config.env>

set -euo pipefail

if [ "$(uname -s)" != "Darwin" ]; then
  echo "oracle runs are macOS-only" >&2
  exit 2
fi

config="${1:?usage: regen-consoles.sh <config.env>}"
# shellcheck source=/dev/null
. "$config"

require() {
  if [ -z "${2:-}" ]; then
    echo "regen-consoles: $1 is not set in $config" >&2
    exit 2
  fi
}
require PEMU_QEMU_BIN "${PEMU_QEMU_BIN:-}"
require PEMU_QEMU_ROM_DIR "${PEMU_QEMU_ROM_DIR:-}"
require PEMU_QEMU_ROM "${PEMU_QEMU_ROM:-}"
require PEMU_QEMU_EFUSE "${PEMU_QEMU_EFUSE:-}"
require PEMU_QEMU_STRAP_MODE "${PEMU_QEMU_STRAP_MODE:-}"
require PEMU_ORACLE_OUT "${PEMU_ORACLE_OUT:-}"
require PEMU_ORACLE_IMAGES "${PEMU_ORACLE_IMAGES:-}"

# How a run is stopped. Both are bounds, not expectations: the first one reached ends the run and
# the header records which. 20 s past the boot phase every exit reads, and 256 MiB of trace,
# which is about ten seconds of memory-region events.
: "${PEMU_ORACLE_TIMEOUT:=20}"
: "${PEMU_ORACLE_TRACE_MAX:=268435456}"

# The launcher is optional and does not change what runs: the oracle's wrapper exports the
# DYLD_LIBRARY_PATH the preserved Homebrew bottles need and then execs $QEMU_BIN, because a
# SIP-protected launcher strips DYLD_* out of an inherited environment. The pin below is still
# checked against PEMU_QEMU_BIN, never against the wrapper, so the header records the hash of the
# binary that actually ran.
: "${PEMU_QEMU_LAUNCHER:=}"
if [ -n "$PEMU_QEMU_LAUNCHER" ] && [ ! -x "$PEMU_QEMU_LAUNCHER" ]; then
  echo "regen-consoles: PEMU_QEMU_LAUNCHER $PEMU_QEMU_LAUNCHER is not executable" >&2
  exit 2
fi

# The pins are checked before anything runs: a console regenerated with another
# binary, ROM or eFuse image is not the oracle the goldens claim. Every hash is computed here,
# and the header below records the computed value, never the one the operator typed: a pin
# variable is what the hash is checked against, not a substitute for reading the file.
check_pin() {
  local what="$1" path="$2" want="${3:-}"
  local got
  if [ ! -f "$path" ]; then
    echo "regen-consoles: $what file $path does not exist" >&2
    exit 2
  fi
  got="$(shasum -a 256 "$path" | cut -c1-16)"
  if [ -n "$want" ] && [ "$got" != "$want" ]; then
    echo "regen-consoles: $what sha256 prefix is $got, the pinned value is $want" >&2
    exit 2
  fi
  echo "$got"
}
qemu_hash="$(check_pin qemu "$PEMU_QEMU_BIN" "${PEMU_QEMU_SHA256_PREFIX:-}")"
efuse_hash="$(check_pin efuse "$PEMU_QEMU_EFUSE" "${PEMU_QEMU_EFUSE_SHA256_PREFIX:-}")"
rom_hash="$(check_pin rom "$PEMU_QEMU_ROM" "${PEMU_QEMU_ROM_SHA256_PREFIX:-}")"

# The strap override of the pinned `qemu-oracle` configuration. The device boots with boot:0xa, and
# QEMU takes the value as a property of its GPIO device. The device and property names are
# UNVERIFIED, which is why they are configuration rather than literals here: if this QEMU build
# spells them differently it rejects the -global and the run fails loudly (there is no `|| true`
# below), instead of producing a console with default straps under a header that claims 0x0A.
: "${PEMU_QEMU_STRAP_DRIVER:=esp32c3.gpio}"
: "${PEMU_QEMU_STRAP_PROPERTY:=strap_mode}"
# The three -global and -drive options are assembled per image below, as array elements.

# The eFuse image is given to QEMU as a drive the eFuse device reads, so `property=drive` has a
# value to point at. Without both halves the run uses QEMU's default eFuses, and the chip
# revision the bootloader prints is not the one the header claims.


mkdir -p "$PEMU_ORACLE_OUT"

failed=()

# PEMU_ORACLE_IMAGES is one `id=path` pair per line, not a space-separated list: the data root
# is under "Application Support" and every corpus path therefore holds a space. Each path also
# reaches QEMU as one array element rather than through `eval`. The earlier space-separated
# form split those paths and ran the oracle against a file that does not exist, which is the
# silent-wrong-input failure a pinned oracle must not have.
while IFS= read -r pair; do
  [ -n "$pair" ] || continue
  id="${pair%%=*}"
  image="${pair#*=}"
  if [ ! -f "$image" ]; then
    echo "regen-consoles: $id: image $image does not exist" >&2
    exit 2
  fi
  # QEMU's `if=mtd` takes a 2, 4, 8 or 16 MiB flash image and refuses anything else, while
  # several corpus entries are merged app binaries of about 1 MB. A padded copy is made beside
  # the console rather than the corpus file being changed, and 0xFF is the fill because that is
  # what erased flash reads as; the header records that the run used a padded copy.
  image_size="$(stat -f%z "$image")"
  padded=no
  case "$image_size" in
    2097152 | 4194304 | 8388608 | 16777216) ;;
    *)
      if [ "$image_size" -gt 8388608 ]; then
        echo "regen-consoles: $id: image is $image_size bytes, too large to pad to 8 MiB" >&2
        failed+=("$id (image $image_size bytes)")
        continue
      fi
      flash="$PEMU_ORACLE_OUT/$id.flash.bin"
      head -c 8388608 /dev/zero | LC_ALL=C tr '\000' '\377' >"$flash"
      dd if="$image" of="$flash" conv=notrunc bs=1048576 2>/dev/null
      image="$flash"
      padded="8MiB from $image_size bytes"
      ;;
  esac
  console="$PEMU_ORACLE_OUT/$id.console"
  usj="$PEMU_ORACLE_OUT/$id.usj.console"
  trace="$PEMU_ORACLE_OUT/$id.trace"
  header="$PEMU_ORACLE_OUT/$id.header"
  binary_hash="$(shasum -a 256 "$image" | cut -c1-16)"

  argv=()
  if [ -n "$PEMU_QEMU_LAUNCHER" ]; then
    argv+=("$PEMU_QEMU_LAUNCHER")
  else
    argv+=("$PEMU_QEMU_BIN")
  fi
  # shellcheck disable=SC2206
  argv+=($PEMU_QEMU_FLAGS)
  argv+=(-L "$PEMU_QEMU_ROM_DIR")
  argv+=(-drive "file=$image,if=mtd,format=raw")
  argv+=(-drive "file=$PEMU_QEMU_EFUSE,if=none,format=raw,id=efuse")
  argv+=(-global "driver=nvram.esp32c3.efuse,property=drive,value=efuse")
  argv+=(-global "driver=$PEMU_QEMU_STRAP_DRIVER,property=$PEMU_QEMU_STRAP_PROPERTY,value=$PEMU_QEMU_STRAP_MODE")
  # shellcheck disable=SC2206
  argv+=($PEMU_QEMU_TRACE_FLAGS)
  # Three chardevs, in the order the machine wires them: UART0,
  # UART1, then the USB Serial/JTAG endpoint the chardev patch adds. UART0 carries the ROM and
  # bootloader output, the app console is USJ, and UART1 is unused on this board; a run with one
  # -serial captures the ROM banner only and loses every line the firmware printed.
  argv+=(-serial "file:$console")
  argv+=(-serial null)
  argv+=(-serial "file:$usj")

  # What the header records is what ran: the array is printed with one quoted word per element,
  # never reassembled from the unquoted variables above.
  command_line="$(printf '%q ' "${argv[@]}")"

  echo "regen-consoles: $id"
  # The oracle does not stop on its own: a flashed image boots and keeps running, and the
  # memory-region trace grows at tens of MB per second (an unbounded `pk` run wrote 13 GB in ten
  # minutes). Every exit that reads these consoles reads a boot phase, so the run is bounded and
  # the bound is recorded in the header: a golden has to say how its run ended, or a shorter
  # rerun would silently produce a shorter golden.
  status=0
  bound=none
  QEMU_BIN="$PEMU_QEMU_BIN" "${argv[@]}" >"$trace" 2>&1 &
  qemu_pid=$!
  waited=0
  while kill -0 "$qemu_pid" 2>/dev/null; do
    if [ "$waited" -ge "$PEMU_ORACLE_TIMEOUT" ]; then
      bound="timeout ${PEMU_ORACLE_TIMEOUT}s"
      kill -TERM "$qemu_pid" 2>/dev/null
      break
    fi
    size="$(stat -f%z "$trace" 2>/dev/null || echo 0)"
    if [ "$size" -ge "$PEMU_ORACLE_TRACE_MAX" ]; then
      bound="trace cap ${PEMU_ORACLE_TRACE_MAX} bytes"
      kill -TERM "$qemu_pid" 2>/dev/null
      break
    fi
    sleep 1
    waited=$((waited + 1))
  done
  wait "$qemu_pid" || status=$?

  # A run stopped by the bound is the normal case, so its exit status is not a failure. A run
  # that ended by itself with a non-zero status is: the header is not written and whatever the
  # run produced is left in place for reading.
  # One image the oracle refuses (a flash image it will not accept, say) must not cost the
  # consoles of the others, but it must not pass silently either: the id is collected and the
  # script exits non-zero at the end naming every one that failed. A failed image gets no
  # header, so nothing downstream can mistake its leftovers for a regenerated console.
  if [ "$bound" = "none" ] && [ "$status" -ne 0 ]; then
    echo "regen-consoles: $id: the oracle exited $status; see $trace" >&2
    failed+=("$id (exit $status)")
    continue
  fi
  if [ ! -s "$console" ] && [ ! -s "$usj" ]; then
    echo "regen-consoles: $id: the run printed nothing to $console or $usj; see $trace" >&2
    failed+=("$id (empty console)")
    continue
  fi

  # The strap value is read back from the console rather than assumed: the header records what
  # the run actually printed.
  strap="$(sed -n 's/.*boot:\(0x[0-9a-fA-F]*\).*/\1/p' "$console" | head -1)"

  {
    echo "#!pemu-golden v1"
    echo "#!kind: oracle"
    echo "#!source: qemu"
    echo "#!image: $id"
    echo "#!command: $command_line"
    echo "#!binary-sha256: $binary_hash"
    echo "#!rom-sha256: $rom_hash"
    echo "#!efuse-sha256: $efuse_hash"
    echo "#!strap: ${strap:-not printed}"
    echo "#!qemu-sha256: $qemu_hash"
    echo "#!strap-requested: $PEMU_QEMU_STRAP_MODE"
    echo "#!padded: $padded"
    echo "#!uart0-bytes: $(stat -f%z "$console" 2>/dev/null || echo 0)"
    echo "#!usj-bytes: $(stat -f%z "$usj" 2>/dev/null || echo 0)"
    echo "#!bound: $bound"
    echo "#!trace-bytes: $(stat -f%z "$trace" 2>/dev/null || echo 0)"
  } >"$header"
done <<< "$PEMU_ORACLE_IMAGES"

if [ "${#failed[@]}" -ne 0 ]; then
  echo "regen-consoles: no console for: ${failed[*]}" >&2
  echo "regen-consoles: the rest are under $PEMU_ORACLE_OUT" >&2
  exit 4
fi

echo "regen-consoles: wrote consoles, traces and headers under $PEMU_ORACLE_OUT"
echo "regen-consoles: next, normalize each console and, when it is committed, list every"
echo "regen-consoles: difference QEMU cannot reproduce in specs/oracle-known-diffs.toml"
