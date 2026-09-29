# Oracle drivers (`tools/oracle/`)

An oracle is exercised **only as a process**: these drivers start it, collect its output and
hand the output to `pemu-verify`. Nothing here links an oracle, reads its
sources or transcribes its structure (`CONTRIBUTING.md`, "Clean room").

Everything in this directory runs on **macOS only**, on the host that holds the preserved oracle
builds. On any other host `cargo xtask oracle` refuses and says that oracle runs are macOS-only.
The checks that read committed data (`oracle regions`, `oracle known-diffs`, `oracle hist
--check`, `oracle counts --file`) and `oracle help` run anywhere and need no oracle at all: the
gates that consume oracle output consume committed data instead.

## What is here

| File | What it is |
|---|---|
| `qemu-oracle.env.example` | the pinned QEMU oracle configuration (corpus id `qemu-oracle`), as environment variables. Copy it to `qemu-oracle.env` and fill in the local paths; `qemu-oracle.env` is not committed |
| `regen-consoles.sh` | regenerates the oracle consoles the milestone tests compare with: one run per image, writing the console, the memory-region trace and a header block. It checks the pins of the QEMU binary, the ROM image and the eFuse image before any run, passes the pinned eFuse image and the `strap_mode` override on the command line, records the hashes it computed (never the pin variables), and stops at the first image whose run exits non-zero without writing that image's header |
| `trace_calls.py` | a gdb batch script that breaks on each watched function and prints `CALL <name>`, the format of the call-trace diff (`pemu_verify::calltrace`). The bootloader phase takes its oracle call trace from QEMU's own `-d exec,nochain` log instead (`cargo xtask oracle boot-trace`), because that log interleaves the entries with the memory-region accesses; the two methods give the same 1,371 `pk` entries |
| `fixtures/` | committed data the tests and the `--check` gates read, all of it ours; see below |

## Fixtures

None of these files is an oracle capture or device data. They are written by hand so the
verification tooling has something to run on wherever it is checked out.

| File | What it is |
|---|---|
| `hist-sample.trace` | a boot-shaped memory-region trace in the oracle's line format over the register offsets of the `c3_devices!` blocks |
| `hist-sample.hist` | the histogram `hist-sample.trace` must produce; regenerate with `cargo xtask oracle hist --regen` |
| `coverage-reference.console` | a synthetic reference boot, 67 timestamped lines |
| `coverage-emulated.console` | a synthetic emulator boot, 58 timestamped lines, 56 of them shared with the reference: the shape of the `timing_calib` comparison |

The real comparison behind that last pair was run once by hand on the oracle host: `cargo xtask
oracle timing-calib` over the preserved reference boot and the esp32sim R8 run log prints
`reference timestamped lines 67, emulator 58, matched 56 of 67`. Neither log is in the tree.

Every MAC in a fixture is a placeholder MAC (`02:00:00` prefix, `docs/secrets.md`); every version,
timestamp and address is invented.

## Commands

```sh
# checks; any host, no oracle
cargo xtask oracle help
cargo xtask oracle regions --check
cargo xtask oracle known-diffs --check
cargo xtask oracle hist --check
cargo xtask oracle counts --file tools/oracle/fixtures/app-main.counts

# macOS only
cargo xtask oracle hist --regen --trace <trace> --baseline <file> --label "<image> under <oracle>"
cargo xtask oracle timing-calib --ref <reference capture> --emu <emulator capture>
cargo xtask oracle goldens --derive --input <capture> --out "$ROOT/goldens" --image pk \
    --kind device --source device --command '<the command that captured it>' \
    --binary-sha256 <hex> --rom-sha256 <hex> --efuse-sha256 <hex> --strap <value>
bash tools/oracle/regen-consoles.sh tools/oracle/qemu-oracle.env
cargo xtask oracle boot-trace [--images pk,official]   # needs QEMU; writes <data root>/oracles/phase/<id>.boot.qemu

# any host with the records and the corpus (the T2 oracle-diffs step)
cargo xtask oracle diff [--images pk,official]
```

### `boot-trace` and `diff`

`boot-trace` starts the pinned oracle with the configuration `consoles` resolves, adds
`-d exec,nochain,trace:memory_region_ops_read,trace:memory_region_ops_write`, and filters the log
while it streams into a phase record (`pemu_verify::phase`): a `CALL <name>` line for every
translation block that starts at a bootloader function, and every memory-region line verbatim. It
stops the oracle at the app's `call_start_cpu0`. `diff` records our machine over the same image to
the same entry, writes `<id>.boot.pemu` beside the oracle's record for reading, cuts both to the
bootloader phase and compares the SHA, TIMG, EXTMEM, MMU, SPI1, eFuse and RTC_CNTL write streams
and the call trace with `specs/oracle-known-diffs.toml` applied.

### `counts`

`counts` compares the instruction count at `app_main` under `fast` at CPI 1 with esp32sim
(7.02 M for Passport Keys, 9.94 M for the official image), plus or minus 2 %, informational
only. The esp32sim run happens on the oracle host; record what it reports into a file in the
format `pemu_verify::goldens::parse_counts` reads and commit that file, so the comparison runs
on any host with no oracle:

```text
# pemu-verify instruction counts v1
# recorded from <the esp32sim invocation> on <date>
pk app_main 7_020_000
official app_main 9_940_000
```

### Golden derivation is a manual step

`cargo xtask oracle goldens --derive` turns a capture into a golden and is run by hand. It is
pure: bytes in, masked text and a report out. It refuses to write when a MAC-shaped string
survived masking, and it refuses a destination inside the repository unless `--allow-in-repo`
is given, because derived text reaches `tests/golden/` only after `cargo xtask secrets-check`
passes on it.
