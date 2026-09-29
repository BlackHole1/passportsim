---
name: passportsim
description: Use when developing or debugging FoloToy AI Passport firmware without a device, or before asking anyone to flash one - boot the build in PassportSim, drive buttons, read the serial console and guest state, and turn the result into a delivery note. Never a substitute for device tests, and never a way to reach a real device.
---

# PassportSim

`passportsim` is a deterministic emulator of the FoloToy AI Passport (ESP32-C3). It boots
unmodified ESP-IDF images from the mask ROM up. Its results are evidence about firmware, never
about hardware.

This skill is self-contained: everything it refers to is in this directory or comes from the
binary. Add it to an agent with `npx skills add BlackHole1/passportsim`, or copy this directory
into the agent's skills directory (for Claude Code, `~/.claude/skills/passportsim/`).

## Setup

Supported hosts: macOS on Apple silicon and Windows x64. Nothing else runs the CLI.

1. Check that the binary is on `PATH` and runs:
   - macOS: `command -v passportsim && passportsim --version`
   - Windows PowerShell: `Get-Command passportsim; passportsim --version`

   The first line is the version. `payload: embedded, sha256 ...` is a released binary;
   `payload: none: development build ...` is a plain `cargo build`, which has no bundled demo.
2. If it is missing, tell the user and ask before installing anything. Never install silently.
   With their consent, run the installer for their host:
   - macOS (Apple silicon): `curl -fsSL https://passportsim.bugs.cc/install.sh | sh`
   - Windows x64 (PowerShell): `irm https://passportsim.bugs.cc/install.ps1 | iex`

   Each downloads the latest release from GitHub, checks its SHA-256 against the release's
   `SHA256SUMS.txt`, unpacks it into the user's own directories (no administrator rights) and
   prints the full path of the installed binary. Setting `PASSPORTSIM_VERSION` (such as `0.1.0`)
   picks a release.
   Until the user opens a new terminal, call the binary by that full path.
3. If they would rather install by hand: download `passportsim-<version>-macos-arm64.tar.gz` or
   `passportsim-<version>-windows-x64.zip` and `SHA256SUMS.txt` from
   <https://github.com/BlackHole1/passportsim/releases>, compare `shasum -a 256 <archive>`
   (PowerShell: `Get-FileHash <archive>`) with its line in `SHA256SUMS.txt`, unpack it, and add
   the unpacked directory to `PATH`. On macOS, clear the quarantine flag of an archive a browser
   downloaded: `xattr -dr com.apple.quarantine <directory>`.
4. On another host, say that the CLI does not run there. The browser version at
   <https://passportsim.bugs.cc> needs no install, but this skill cannot drive it.

To use the MCP server instead of the CLI, add it to the agent's MCP configuration. Give the
absolute path the installer printed: desktop apps do not see the `PATH` of a terminal.

```json
{ "mcpServers": { "passportsim": { "command": "/absolute/path/to/passportsim", "args": ["mcp"] } } }
```

## Commands

Never call a command from memory. The references are generated from the command registry:

- [references/commands.md](references/commands.md): every command, its MCP tool and its hosts.
- `references/commands/<name>.md`: synopsis, arguments, error codes and examples of one command.
- [references/errors.md](references/errors.md): every error code and what it means.

`passportsim --help` and `passportsim <command> --help` give the same synopsis from the binary you
have. If they differ from the references, the binary is right. A command in neither does not
exist: say so and stop; do not guess a spelling.

Three details that are easy to get wrong:

- `--output json` selects JSON output. `--json @file` or `--json -` is the *input* document.
- `--version` is a top-level flag only; a subcommand refuses it with exit 2.
- Text output is shaped: only the first 10 and last 30 entries survive, with one
  `... N lines elided ...` marker between them, and `--max-nodes` does not change that.
  `ui --output json` carries the whole tree in its `text` field. `serial --output json` does not:
  read a long console in windows with `--max-bytes` and the `next_cursor` each read returns.

## Loop

1. Read `references/commands.md`, then the page of each command you are about to use.
2. Start an instance from the firmware under test: a path to an `idf.py` build directory, a
   merged bin, an ELF or a `.pebundle`, or a corpus id. With no argument, `start` boots the bundled
   demo. A corpus id is a short local name the user defines in `corpus.toml` in the config
   directory (`~/.config/passportsim/` on macOS, `%APPDATA%\passportsim\` on Windows); a fresh
   install has none, and `passportsim doctor` lists the ones this machine has.
3. Act and observe with the commands, and read the receipt each one returns, not only its
   payload. Menu focus is a background-colour change that the pruned UI tree hides: take a
   revision with `ui --include-style`, press the button, then read
   `ui --include-style --diff <revision>`.
4. On a timeout or a fault, read the error's `hint` first; it names the next command.
5. Write the delivery note and stop the instance.

## Rules

1. **Emulator results are never `Device tests`.** See "Delivery note".
2. **Never reach a real device.** See "Device safety".
3. **Never load real device data**: no flash backup, eFuse dump or real NFC card image. The
   defaults (a synthesized eFuse, a blank cardid) need no input. Loading real data taints the
   instance, needs a human confirmation code on the CLI, and is not something an agent arranges.
4. **Never print or copy identity values**: no MAC address, unique id, eFuse calibration word,
   cardid byte, or device backup file name or contents, in any output, file, commit or report.
   Placeholder MACs start `02:00:00`. Name the kind and location of a value (`calib_word` at
   `file:0x1c`), never the value. The full policy:
   <https://github.com/BlackHole1/passportsim/blob/main/docs/secrets.md>.
5. **Wait in virtual time, never in the shell.** Use the commands' own wait arguments (`run`, the
   `start` boot markers), not `sleep`. The same inputs must produce the same bytes.
6. **Say what fidelity you had.** If the run reported a fidelity caveat for a subsystem you
   changed, put it in the delivery note.
7. **Report refusals verbatim.** Quote the error `code` and follow its `hint`; do not retry with
   a different spelling.

## Delivery note

The AI Passport project's delivery block has four fields, each PASS, FAIL or NOT RUN: `Build`,
`Host tests`, `Device tests`, `Unverified`. The emulator adds no field.

- `Build` is the firmware build. Booting an image someone else built does not make it PASS.
- `Host tests` gets one sentence tagged "Emulator (not hardware)" naming the image and the
  `passportsim --version` line. It stays FAIL if the firmware repository's own static validation
  (`./tools/validate.sh --static` there, not part of the emulator) failed.
- `Device tests` stays as the person or the real device run left it.
- `Unverified` keeps every hardware row for the subsystems you touched, marked "emulator-only".

## Device safety

The emulator never opens a serial port, and neither do you. Only the real-device flow
(`plan_flash`, `flash_device`, `device_boot_check`) can: it is off unless a person starts the CLI
or the daemon with `--allow-device`, it asks a human to confirm before any port is opened, and it
is never run by an agent.

Deny the direct tools in the agent harness. [device-deny.json](device-deny.json) is a ready
`permissions.deny` list for Claude Code (`.claude/settings.json`) covering esptool, espefuse and
the `idf.py` flash, erase, monitor and port spellings. It is a guard rail, not the boundary: the planner itself
accepts only `socket://127.0.0.1:*` and `rfc2217://127.0.0.1:*` as a port.

## Known limits

- The CLI finds the app ELF only for the bundled demo and for a corpus id with an `elf` entry,
  not for a merged bin, build directory or `.pebundle` given by path. Without it,
  `start --boot until_ui_settled` runs its whole budget and reports `unobservable`, `inspect` and
  `ui` name the missing ELF, and `--boot-cache` fails with `E_STATE`.
- `start` always boots the bundled ROM; there is no `--rom` flag.
- The bundled demo is present only when the release was built with it; without it, `start` with
  no argument fails with `E_ASSET_MISSING`.

A refusal caused by one of these is not a firmware bug; do not report it as one. The current list
is in <https://github.com/BlackHole1/passportsim/blob/main/docs/quickstart.md#6-known-limits>.
