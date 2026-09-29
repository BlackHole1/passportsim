# Committed goldens (`tests/golden/`)

A golden pins what a console must say. Goldens are compared as **bytes**, never through a text
layer that could rewrite line endings, and `.gitattributes` normalizes every tracked text file
to LF, so a checkout on macOS and a checkout on Windows compare equal.

## Layout

```text
tests/golden/<image-id>/<name>.console.txt   the golden: header, then the normalized console
tests/golden/<image-id>/bands.toml           timestamp-band anchors for that image
```

## File format

A golden file is a header of `#!` lines followed by the console body verbatim. No console line
can start with `#!`, so the split needs no escaping and no line count.

```text
#!pemu-golden v1
#!kind: oracle
#!source: qemu
#!image: probe-boot-facts
#!command: <the exact command line the regeneration ran>
#!binary-sha256: <hex>
#!rom-sha256: <hex>
#!efuse-sha256: <hex>
#!strap: 0x0a
ESP-ROM:esp32c3-api1-20210207
I (T) boot: ESP-IDF v5.5.3 2nd stage bootloader
...
```

The header fields record how an oracle console was regenerated: the command line, the binary,
ROM and eFuse hashes, and the **strap value read back from the console** rather than assumed.
`pemu_verify::goldens::Header::missing_fields` names any field that is still empty, and
`cargo xtask oracle goldens --derive` refuses to write without them.

## Kinds and classes

| Kind | Source | Fidelity class | Status |
|---|---|---|---|
| `device` | the Passport Keys reference boot | A | authoritative |
| `oracle` | a QEMU or esp32sim console for a probe image | B | authoritative for text on modeled paths |
| `self` | reviewed emulator output | B | `#!provisional: true` until a device capture exists; cannot promote a block to class A |

## The text rule

Each milestone claims a prefix of a golden. The normalized lines of that prefix must be equal
**in sequence**: any insertion, deletion or change fails
(`pemu_verify::goldens::check_prefix`). A milestone may not claim more lines than the golden
holds.

## Normalization

Every golden body is normalized text. `pemu_verify::normalize` performs the four steps in
order: strip CR; rewrite ESP_LOG timestamps to `(T)`, keeping the numbers for the bands; mask
the compile time and date, the app version, the ELF SHA, the Passport Keys version, `boot=`
ids, MAC addresses and the `Saved PC:` value, keeping the line; then select the last boot.

## Timestamp bands

`bands.toml` beside a golden names the anchors compared under the `device` profile from M11:

```toml
schema = 1
# optional, and only ever narrower than the defaults
# delta_floor_ms = 5
# delta_percent = 20
# absolute_floor_ms = 10
# absolute_percent = 20

[[anchor]]
name = "bootloader banner"
line = "boot: ESP-IDF"

[[anchor]]
name = "app loaded"
line = "boot: Loaded app from partition"
```

Deltas between consecutive anchors are checked first, with `max(5 ms, 20 % of dt_dev)`; only
when every delta passes are the absolute timestamps checked, with `max(10 ms, 20 % of t_dev)`.
One late phase therefore reports one failure instead of shifting every later anchor out of band.

## How a golden gets here

1. **Capture.** An oracle console comes from `bash tools/oracle/regen-consoles.sh`; the device
   reference boot is the preserved capture. Both steps are macOS-only.
2. **Derive.** `cargo xtask oracle goldens --derive --input <capture> --out "$ROOT/goldens" …`
   masks the capture and writes the golden with its header. It refuses to write when a
   MAC-shaped string survived masking, and refuses a destination inside this repository unless
   `--allow-in-repo` is given.
3. **Check.** `cargo xtask secrets-check --path <file>` must pass on the derived text. Raw
   captures are never committed; only masked text is.
4. **Commit.** Copy the derived file into `tests/golden/<image-id>/` and add its `bands.toml`.
5. **List what cannot match.** A `boot:` or `chip revision` line the oracle cannot reproduce is
   an entry in `specs/oracle-known-diffs.toml` with its reason, never a silent mask.

## Frame goldens

A frame golden is the PNG `pemu_host::png::encode_rgb565` writes for a settled `raw` frame,
compared byte for byte. It is a `self` golden: a person approves the candidate the milestone test
writes under `<data root>/scratch/candidates/` (`PEMU_WRITE_CANDIDATES=1`), and it is copied here.

| File | Test | Approved | sha256 |
|---|---|---|---|
| `pk/first-screen.png` | `m4.rs` | 2026-09-16 | `8ebb4bb4db2267b62150f428fb0ed0f0d8df6804261a4edb8f8990afcc4e7a16` |
| `official/menu.png` | `m5.rs` | 2026-09-16 | `a7dc7c8b2d7af36cbf369ade5ea2553a816737884b11a8aba0aa5b1c71b02332` |

## The cross-host parity golden

`cross-host/parity.txt` is the cross-host determinism golden: `key = value`
lines of digests only (observation lines of `pemu_machine::determinism`, the SHA-256 of snapshot,
WAV and PNG bytes, the trace digest, the exact bits of the host-side tone analysis) for a scenario
built from code over the bundled ROM and a synthetic eFuse, so it carries no guest secret. It is
produced on macOS arm64 only, by `PEMU_PARITY_RECORD=1 cargo test -p pemu-milestones --test
cross_host`, and checked on every host by the T0 step `cross-host-parity`
(`tests/milestones/cross_host/main.rs`). A mismatch on another host is a defect to fix, never a
reason to re-record; re-record only after a deliberate change of behavior.

## What is not here

Raw captures, device logs, eFuse dumps and anything carrying a real MAC, unique id, calibration
value or cardid byte. The secrets policy (`docs/secrets.md`) and the pre-commit hook
keep them out.

## The baseline to beat

esp32sim R8 reproduces 65 of the 77 device console lines, and 56 of the 67 timestamped ones
(`pemu_verify::goldens::timestamped_coverage`). The target is 77 of 77.
