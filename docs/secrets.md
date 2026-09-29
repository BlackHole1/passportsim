# Secrets policy

**English** | [简体中文](i18n/zh-CN/secrets.md) | [日本語](i18n/ja/secrets.md) | [Français](i18n/fr/secrets.md)

Device data never enters the repository, logs, default exports or agent-visible output. This
document says what counts as a secret, how the emulator handles it at run time, and how
`cargo xtask secrets-check` and the git hooks keep it out of the repository.

## 1. The identity rule

No document, test, golden, log, example, commit message, issue or pull request reproduces a
device's:

- MAC address (base or derived);
- unique ID;
- eFuse calibration word;
- backup file name;
- cardid content.

Examples and fixtures use the placeholder MAC prefix `02:00:00`. Device serial numbers and daemon
tokens are never copied either. To discuss a device value, name its kind and location
(`calib_word` at `file:0x1c`), never the value.

Device-derived files stay under the data root (`~/Library/Application Support/passportsim/` on
macOS, `%LOCALAPPDATA%\passportsim\data\` on Windows), the device subset in an owner-only
directory.

## 2. What counts as secret

| Data | Default handling | Opt-in |
|---|---|---|
| Device flash backups | never loaded except by `--flash <path>`, which taints the machine | `--flash` plus `--allow-tainted` |
| Raw eFuse dumps (MAC, unique ID, calibration) | never read; `--efuse synth` is the default | `--efuse-dump <dir>` taints |
| NVS credentials (Wi-Fi, BLE bond keys, app tokens) | `inspect nvs` shows namespaces, keys and types, never credential values; exports erase NVS pages | `inspect nvs --reveal` with human confirmation |
| cardid partition `[0x356000, 0x35A000)` | synthetic pattern in emulator-created images; never printed, logged or exported; the planner never writes it | none |
| MAC, unique ID, calibration words in outputs | placeholders when synthesized; masked when tainted | `--reveal identity` with human confirmation |
| Real NFC card images | taint on `nfc.load`; `nfc.dump` redacts UID and PWD/PACK | `--include-secrets` |
| Live microphone, bridged network payloads, external HCI | journaled locally for replay, dropped from exports | `--include-secrets` |
| Daemon token and launch codes | in the runtime directory, owner-only, never in outputs, logs or artifacts | none |
| Device serial numbers, backup file names | never copied into the repository or docs | none |

**The bundled ROM ELFs are not secret.** `assets/rom/esp32c3_rev101_rom.elf` and
`assets/rom/esp32c3_rev3_rom.elf` are Espressif's public Apache-2.0 esp-rom-elfs release, committed
with their [LICENSE](../assets/rom/LICENSE), [NOTICE](../assets/rom/NOTICE) and SHA-256 pins in
[pins.toml](../assets/rom/pins.toml). Packages carry all three files.

**The official demo image is never in the repository.** `xtask package` embeds it from the build
host's firmware corpus only when its SHA-256 matches the pin and its MIT license text is present.

**Tainted loading is human-only.** Every input that taints a machine (`--flash` of an image with
non-0xFF cardid bytes or NVS credentials, `--efuse-dump`, `nfc.load` of a real card) is a native CLI
operation behind human confirmation; MCP and HTTP never expose it.

## 3. The secret set

One pure function, `pemu_api::secret_set`, decides what counts as identity. The repository scanner
and the runtime redaction both use it, so they cannot disagree.

Members, with the kind that `secrets-check` reports as `hashed:<kind>`:

| Kind | Member | Forms |
|---|---|---|
| `mac` | base MAC and base+1 to base+3 (Wi-Fi station, soft-AP, BT, Ethernet) | colon, dash and bare hex, both cases, and byte-reversed; derived MACs in both the ESP-IDF last-byte wrap form and the 48-bit carry form |
| `mac_suffix` | 3-byte NIC suffix of each MAC | text forms only |
| `unique_id` | eFuse BLK2 unique ID (128 bits) | raw bytes and bare hex, both byte orders |
| `calib_word` | each non-zero BLK2 calibration word | 4 raw bytes in both byte orders, and hex text |
| `backup_stem` | stems of device backup file names | the stem bytes |
| `cardid` | non-0xFF content of the cardid window | hashed per 32-byte chunk |
| `nvs_credential` | NVS credential values of 6 bytes or more | raw bytes, hex and base64; run time only |
| `nfc_uid`, `nfc_pwd`, `nfc_pack` | NFC tag UID, password and password acknowledge | run time only |
| `canary` | the random canary of section 5.6 | hash file only |

### 3.1 False-positive guards

To keep the guard quiet enough to be trusted, the builder drops members shorter than 4 bytes
(the 2-byte NFC PACK keeps only its hex text), members whose bytes are all equal, calibration words
with fewer than 2 non-zero bytes, and backup stems shorter than 6 bytes. It adds only base+1 to
base+3 as derived MACs. A file is text when its first 8000 bytes hold no NUL byte (git's rule);
text-only members match only in text files.

## 4. Taint and redaction

Run-time behavior of the emulator:

- **Taint.** A machine built from secret-bearing input (the list in section 2, or a live bridge)
  is tainted, and so are its forks, snapshots and boot-cache entries. A tainted machine:
  - refuses snapshot, flash and artifact export with `E_SECRET_REFUSED` unless
    `--include-secrets` comes with a human confirmation code;
  - shows `tainted: true` in its receipt;
  - keeps its boot cache in memory only;
  - masks the cardid window, NVS pages and any secret-set match in raw-memory tools (`mem_read`,
    `watch`, `trace`, `inspect heap`).
- **Redaction** runs on every agent-visible text and JSON output and on every file written to the
  artifacts directory. It matches by value: a match against the machine's secret set becomes
  `<MAC>` or `<SECRET>`, while addresses the agent supplied are left alone. Binary artifacts
  (btsnoop, pcap, dumps) are rewritten the same way.
- **Exports** fill the cardid window with 0xFF, erase NVS partitions, omit eFuse bytes and drop
  live journal payloads. A redacted snapshot boots with factory NVS and a synthetic cardid and says
  so in its receipt.
- **Logs.** No telemetry. Daemon logs are redacted. Crash reports carry no RAM or flash contents
  unless the user asks.
- **Network.** The relay and bridge deny loopback, private, link-local and ULA ranges by default
  and journal every destination.

## 5. Repository hygiene

Normalized, identity-masked text goldens may be committed; the check still scans them.

### 5.1 `.gitignore`

[`.gitignore`](../.gitignore) ignores firmware and device data (`*.bin`, `*.elf`, `efuse_blk*`,
`*flash*.bin`, `cardid*`, `boot_log*`, `GROUND_TRUTH*`), run artifacts (`*.snap`, `*.pebundle`,
`*.pcap`, `*.btsnoop`, `*.wav`, `/artifacts/`, `.passportsim/`) and local configuration
(`*.local.toml`, `secrets-check.toml`). `!` entries re-include only the two bundled ROM ELFs and a
few reviewed test ELFs. `git add -f` bypasses `.gitignore`, so the real guard is
`xtask secrets-check`.

### 5.2 Pattern rules

Pattern rules need no local data (`xtask/src/secrets/`):

| Rule | Rejects |
|---|---|
| `mac-shape` | MAC-shaped text (six hex pairs joined by `:` or `-`) other than the `02:00:00` prefix, the all-zero address and group addresses |
| `efuse-dump` | binary files whose size and structure match raw eFuse block dumps (section 6.1) |
| `nvs-credential` | NVS partitions holding credential keys |
| `cardid-window` | non-0xFF bytes in the cardid window `[0x356000, 0x35A000)` of any binary long enough to hold it |
| `backup-name` | file names matching device backup naming patterns (section 6.2) |
| `rom-pin` | any binary under `assets/rom/` that is not an ELF pinned in `assets/rom/pins.toml`, or any binary there when `LICENSE` or `NOTICE` is missing |

A pinned ROM ELF is exempt from the content rules. When packaging, `cardid-window` also skips the
freshly built `passportsim` binary and `pemu_wasm.wasm`, and only if the file is a structurally
valid executable (PE, ELF, Mach-O or wasm): both are larger than 0x35A000 bytes, so that offset
holds program code. A forged header in front of a flash image is still refused.

### 5.3 Hashed rules

Hashed rules catch device values in forms the patterns miss (bare hex, byte arrays, reversed
bytes, calibration words, cardid chunks). `xtask secrets-check --init` hashes every member of the
secret set with a random salt into `~/.config/passportsim/secrets-check.toml`
(`%APPDATA%\passportsim\secrets-check.toml` on Windows), owner-only (mode 0600 in a 0700 directory
on macOS, a protected DACL on Windows, re-checked on every load). That file:

- never enters the repository and never leaves the host that holds the device data;
- holds salted SHA-256 hashes only, plus the random canary;
- must not be read, printed or copied by agents.

**Output rule.** `secrets-check` never prints matched content, member values or hashes: only rule
names, `file:offset` and counts. A file that trips `backup-name` is reported with its name
withheld.

### 5.4 Where each rule family runs

Pattern rules run on every host. Hashed rules run wherever a device directory exists; each such
host builds its own hash file with `--init`, and must do so before a real flash is made there.

| Where | Pattern rules | Hashed rules |
|---|---|---|
| `cargo xtask secrets-check` (tree or `--paths`) | yes | when the hash file exists; skipped with a note otherwise |
| pre-commit hook (`--staged`) and pre-push hook (`--hook pre-push`) | yes | yes; fail closed |
| T0 on a host with a device directory | yes | yes |
| T0 on a host with no device directory | yes | no; the receipt says "pattern rules only" |
| T1 | yes | yes |

The guard finds its files from the known folders (Windows) or `HOME` (macOS) only, and refuses to
run while `PASSPORTSIM_HOME`, `PASSPORTSIM_CONFIG_DIR` or `PASSPORTSIM_DATA_ROOT` is set unless
`--root` is given, so an override cannot hide the device directory.

It fails closed:

- a hook refuses on a machine with a device directory but no hash file, and says to run
  `cargo xtask secrets-check --init`;
- every mode refuses a hash file that is unreadable, malformed or not owner-only;
- the hooks `exec cargo xtask ...`, so a missing cargo or a failed build refuses instead of
  skipping.

### 5.5 Commands

```text
cargo xtask secrets-check [--root <dir>]                  scan git ls-files -co --exclude-standard
cargo xtask secrets-check [--root <dir>] --paths <files>  scan the given files
cargo xtask secrets-check [--root <dir>] --staged         what the pre-commit hook runs
cargo xtask secrets-check [--root <dir>] --hook pre-push  what the pre-push hook runs
cargo xtask hooks install [--root <dir>] [--force]        install both hooks (maintainer only)
cargo xtask secrets-check --init                          write the hash file (maintainer only)
cargo xtask secrets-check --self-test                     check the hash file (maintainer only)
```

Agents run only the first four. With `--paths`, relative paths resolve against `--root` when given,
else the current directory.

### 5.6 Arming the guard

On a new clone, before the first commit, the maintainer:

1. runs `cargo xtask secrets-check --init`, which builds the secret set from the device directory
   and writes the hash file with a new salt and a random canary (it writes nothing if any read
   fails, so a partial set is never hashed);
2. runs `cargo xtask hooks install`;
3. runs `cargo xtask secrets-check --self-test`, which checks that the scanner detects every
   member form and prints only counts;
4. stages a scratch file containing the canary, confirms that `git commit` is refused, then
   unstages and deletes it.

Worktrees share the hooks. Re-running `--init` replaces the salt and canary, so repeat step 4.

### 5.7 When a commit is refused

The hook prints one line per hit (rule, file, offset). Then:

1. **Do not bypass the guard.** Never use `git commit --no-verify`, never change
   `core.hooksPath`, never edit or delete the hooks, never touch the hash file.
2. **Fix the content, not the check.**
   - `mac-shape` in docs or tests: use a `02:00:00` placeholder.
   - `efuse-dump` or `backup-name` on a fixture: follow section 6.
   - `cardid-window` or `nvs-credential`: the binary does not belong in the repository; generate
     it at test time or keep it under the data root.
   - `rom-pin`: only the pinned ROM ELFs live in `assets/rom/`.
   - `hashed:<kind>`: a real device value reached the file. Remove it and find where it came from.
3. **Report without the value.** Name the rule and `file:offset` only; never quote matched bytes.
4. **A refusal about the hash file** (missing, unreadable, wrong mode) is for the maintainer. Do
   not run `--init` yourself.
5. **A suspected false positive** goes to the maintainer with the rule and `file:offset`. There is
   no allowlist by design; the fix is in the file name or the fixture format.

## 6. Fixture rules

Neither rule has an allowlist: an allowlist is a bypass a real dump could slip through.

### 6.1 Size rule for small binaries

`efuse-dump` rejects:

- any 24-byte or 32-byte binary file (one raw eFuse block) that is not a uniform fill and has no
  container magic (ELF, PNG, GIF, JPEG, gzip, zip, zstd, wasm, PDF), including a raw 32-byte
  SHA-256 digest stored as `.bin`;
- a 336-byte binary file (all eleven blocks) without container magic whose BLK1 MAC field is a
  non-zero unicast address.

Generate fixtures of those sizes at test time or use a container format; store digests as hex
text. Uniform 0x00 or 0xFF fills are fine.

### 6.2 Naming rule for synthetic images

`backup-name` flags a path (case-insensitive) when:

- a directory component is `passport-backups`;
- the file name starts with `efuse_blk`, `cardid`, `boot_log` or `ground_truth`;
- the file is a raw dump (`.bin`, `.img`, `.dump`, `.dmp` or `.raw`, optionally followed by `.gz`,
  `.xz`, `.zst`, `.bz2`, `.zip` or `.7z`) whose stem contains `backup`, `dump`, `flash`, `full`,
  `efuse`, `nvs`, `cardid`, `readback`, `passport`, `4m`, `8m` or `16m`;
- any path component carries a MAC: a separated MAC shape, or a 12-hex-digit token with at least
  one digit and one letter (placeholder, all-zero and group addresses excepted).

Name synthetic images without those words, for example `synthetic_image.bin` or
`seeded_card_erased.img`. The same words are in `.gitignore`, so a binary fixture also needs a
reviewed `!` entry there. Exact backup stems are matched by value by the hashed rules.

## 7. Hook scopes

- **pre-commit** scans the staged blobs, not the work tree. Stage the file again after a fix.
- **pre-push** scans every blob in `remote..local`. For a new or unknown remote tip it scans the
  pushed tree plus the blobs of commits on no remote-tracking ref, so history cannot leak through a
  new branch. A manual `--hook pre-push` scans `@{upstream}..HEAD`, or the `HEAD` tree.
- **Installation.** Hooks go to `git rev-parse --git-path hooks`, so worktrees share them. An
  existing hook without the xtask marker is kept unless `--force` is given. The scripts are `sh`
  scripts, run by the shell Git ships on both hosts.
- **`--init` inputs.** The device directory is `[paths] data_root` of the local configuration,
  else the default data root. Backups are the `.bin` files directly in the backups directory.
- **Tests** of `xtask` refuse the device and hook modes and scan with an empty `HOME`, so no test
  can read device data or the real hash file.

## 8. Device safety

**Who may open the device.**

- The emulator, the daemon, its endpoints, CI, tests and agents never open `/dev/cu.*`,
  `/dev/tty.*` or a `COM` port, and never run `esptool`, `espefuse`, `idf.py flash` or
  `idf.py monitor`.
- Only `pemu-planner` with feature `device` opens the port, and only after human confirmation
  (`docs/ARCHITECTURE.md`, "Flashing a real device"). Discovery enumerates by VID/PID without
  opening a port, since esptool's default reset would put the app into download mode. Flashing a
  real device is native CLI only, never from the browser.

**Development captures** on a real device (goldens, probe runs, calibration):

1. back up first;
2. write only the bootloader, partition table and app segments;
3. never write the cardid range 0x356000 to 0x359FFF;
4. never erase all flash and never burn eFuses;
5. restore Passport Keys from the backup afterwards and compare the cardid MD5.

Any other use of device data (such as `--efuse-dump`) needs the device owner's explicit approval.

**Human confirmation** stops mistaken tool calls by a cooperating agent. It does not stop a hostile
agent with a shell as the same user, which could run esptool itself; the guardrails below address
that. Confirmation paths, in order:

1. MCP elicitation, answered by the user in the client;
2. a native dialog raised by the daemon (in a desktop session);
3. a one-time code on the controlling terminal, only for a foreground command in an interactive
   console. The code is never shown in the web UI, returned in a tool result or written to a file.

The same paths gate `inspect nvs --reveal`, `--reveal identity`, `--include-secrets` and tainted
loading.

**Shipped guardrails.** The [skill](../skills/passportsim/SKILL.md) and
[`device-deny.json`](../skills/passportsim/device-deny.json) (for the `permissions.deny` list of a
Claude Code `.claude/settings.json`) deny the usual spellings that reach a real device: `esptool`,
`esptool.py`, `python -m esptool`, `py -m esptool`, a virtual-environment `python.exe` path,
`esptool.exe`, `espefuse`, `espefuse.exe`, `idf.py flash`, `idf.py -p /dev/cu.*`,
`idf.py -p COM*` and `\\.\COM*` paths, and route flashing through the planner. That list is
best effort, since a pattern list cannot cover every shell and quoting style. **The enforced
allowlist is where the argument is parsed**: the planner and the skill's tools accept only
`socket://127.0.0.1:*` and `rfc2217://127.0.0.1:*` as a port, before any process is spawned.

This document ships in every package beside the skill. Its link to `.gitignore` does not resolve
there, because a package carries no repository files.
