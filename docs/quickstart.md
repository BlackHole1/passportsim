# Quickstart

**English** | [简体中文](i18n/zh-CN/quickstart.md) | [日本語](i18n/ja/quickstart.md) | [Français](i18n/fr/quickstart.md)

A package is one archive with one binary. It needs no ESP-IDF, no `~/.espressif` tools, no
firmware corpus, no device, no Bun, Node, Python or QEMU, and no ROM download.

## Supported hosts

macOS 27 or newer on Apple silicon (`aarch64-apple-darwin`), and Windows 10 1903 or newer on x64
(`x86_64-pc-windows-msvc`, section 8). Linux is not supported.

## 1. Get the package

To install a release, run `curl -fsSL https://passportsim.bugs.cc/install.sh | sh` on macOS or
`irm https://passportsim.bugs.cc/install.ps1 | iex` in PowerShell on Windows; it installs into your
user directory and prints how to run `passportsim`. To build a package from a checkout instead,
with no installer and no `PATH` edit:

<!-- quickstart: not-run - it writes into `target/package/`, which the test does itself with its own output directory -->

```sh
cargo run -q -p xtask -- package --target aarch64-apple-darwin
```

It writes, under `target/package/`:

| Path | What it is |
|---|---|
| `passportsim-0.1.0-macos-arm64/` | the unpacked package |
| `passportsim-0.1.0-macos-arm64.tar.gz` | the same tree as one archive |
| `passportsim-0.1.0-web/` | the static web bundle (section 5) |
| `passportsim-0.1.0-web.tar.gz` | the same bundle as one archive |

No file of the package names the build account: source paths are replaced by fixed tokens, and
packaging fails if a user name or home directory is left anywhere.

Unpack the archive anywhere and change into it. Every command below runs in that directory:

<!-- quickstart: not-run - the directory name carries the version you unpacked -->

```sh
cd passportsim-0.1.0-macos-arm64
```

If packaging fails part way, check free disk space first: it writes two trees and two archives.
Delete `target/package/` and run it again; it always rebuilds from scratch.

### Just the binary

To run the emulator from a checkout without packaging:

<!-- quickstart: not-run - it builds the workspace into the checkout's `target/`, and a package carries no Cargo workspace to build -->

```sh
cargo build -p pemu-cli
./target/debug/passportsim status
```

Read every `./passportsim` below as `./target/debug/passportsim`. Section 2 does not apply, and
such a build carries no payload (no embedded demo), which `--version` says (section 6).

## 2. First launch on macOS

The binary is **not signed and not notarized**. If the archive came through a browser or from
another machine, clear the quarantine attribute once:

<!-- quickstart: run exit=0 -->

```sh
xattr -dr com.apple.quarantine .
```

This installs nothing; on a package that never left this machine it does nothing.

## 3. Check that it runs

<!-- quickstart: run exit=0 -->

```sh
./passportsim status
```

It prints the instances (none on a fresh start), the artifacts directory and the run receipt:

```text
no instance is running
artifacts: ~/Library/Application Support/passportsim/artifacts
profile fast | deterministic
```

Directories are only created when something is written.

Every command answers `--help`; the same text is in [commands/](commands/index.md):

<!-- quickstart: run exit=0 -->

```sh
./passportsim --help
```

Any command gives JSON with `--output json`. (`--json` is how a command takes its *input*.)

Text output is bounded: identical lines collapse, long lines end in `...(+N chars)`, and only the
first 10 and last 30 entries are kept, with one `... N lines elided ...` marker between them. To
read a long console, page through it with `serial read --max-bytes` and the `next_cursor` each read
returns.

<!-- quickstart: run exit=0 -->

```sh
./passportsim status --output json
```

### The daemon, and MCP

Instances live in a daemon. `start` launches one in the background
(`passportsim serve --headless`) when none runs, and later commands (`run`, `serial`, `status`,
`stop`, ...) go to it, so an instance outlives the command that started it. Without a daemon, a
command runs in its own process; `--ephemeral` forces that. A relative firmware path is made
absolute before it is sent (over MCP and HTTP, paths must be absolute). `pk` below is a corpus id
(section 4).

<!-- quickstart: not-run - it needs a firmware corpus entry, and it leaves a background daemon running until the last line -->

```sh
./passportsim start pk --boot none
./passportsim run 'serial:/bsp_i2c/'
./passportsim serial read --cursor 0
./passportsim stop
./passportsim serve --stop
```

The daemon listens on `127.0.0.1:8765` only (or a free port when that is taken). Every request needs
the token it writes, owner-only, beside `serve.json` in `~/.passportsim/`. A headless daemon logs
to `~/.passportsim/logs/serve.log` and exits after ten minutes with no instance.

`passportsim serve` without `--headless` runs in the foreground and prints its URL, the token file
and a `ui:` link to the web page (section 5). The link carries a one-time launch code after `#lc=`,
valid for 60 seconds. `passportsim serve --stop` stops the daemon after stopping its instances and
flushing their artifacts.

`passportsim mcp` is the MCP server for agents: MCP over standard input and output, relayed to the
daemon (started when needed). `--caps audio,nfc` adds tool groups to the core set. In a client
configuration, give the binary and the one argument:

<!-- quickstart: not-run - an MCP server waits on standard input for its client, so it does not return in a shell -->

```sh
./passportsim mcp
```

## 4. What is built in

| Need | Built in | Optional override |
|---|---|---|
| ROM | both Espressif ESP32-C3 mask ROM ELFs, selected by the eFuse chip revision | none that `start` uses (section 6) |
| eFuse | a synthesized image: chip revision v1.1, placeholder MAC `02:00:00:xx:xx:xx`, zero calibration words | `--efuse-dump <dir>`, which taints the machine ([secrets.md](secrets.md)) |
| Firmware | the official BSP demo, when the build host had it (section 6) | a corpus id, or a path to an `idf.py` build directory, a merged bin or a `.pebundle` |
| Tools | this binary, or the web bundle | ESP-IDF only to *build* firmware; esptool only for the USB Serial/JTAG endpoints |

`passportsim doctor` reports what this machine resolved: the bundled ROM pins, any ROM override,
the embedded demo, and each `corpus.toml` entry as found, missing or mismatched. It never prints
file contents.

<!-- quickstart: run exit=0 -->

```sh
./passportsim doctor
```

A report can also be passed in, which is what an agent does over MCP
([commands/doctor.md](commands/doctor.md)):

<!-- quickstart: run exit=0 -->

```sh
printf '%s' '{"report":{"bundled_roms":[],"corpus":[]}}' | ./passportsim doctor --json -
```

No configuration file and no corpus is needed.

### The firmware corpus

A **corpus id** is a short local name for a firmware image, so `start official` can replace a
path. Ids are not built in; each machine defines its own in `corpus.toml` in the config directory:

| Role | macOS | Windows |
|---|---|---|
| config directory (`corpus.toml`, `config.toml`) | `~/.config/passportsim/` | `%APPDATA%\passportsim\` |
| data root (`corpus/`, `artifacts/`, `audio/`) | `~/Library/Application Support/passportsim/` | `%LOCALAPPDATA%\passportsim\data\` |

One table per id. The file keys are `bin` (merged flash image), `elf` (app ELF), `boot_elf`
(bootloader ELF) and `pt` (partition table); only `bin` is required. `sha256` pins each file with
its full 64-character digest (shortened here):

```text
[official]
bin = "corpus/official/FoloToy-AI-Passport-8MB.bin"
elf = "corpus/official/FoloToy-AI-Passport.elf"
boot_elf = "corpus/official/bootloader.elf"
sha256 = { bin = "5802...e163", elf = "dd63...a2de", boot_elf = "5fcf...17a8" }
```

Paths, here and in `config.toml`:

- **absolute** is used as written (a leading `/` counts as absolute on Windows too);
- **`~/...` or `~\...`** is the home directory;
- **anything else is relative to the data root**, never to the current directory, which makes one
  `corpus.toml` portable. A path that climbs out with `..` is refused.

A missing file is `E_ASSET_MISSING`, a digest mismatch `E_ASSET_HASH`; `doctor` names refused
paths.

Environment overrides:

- `PASSPORTSIM_CORPUS_<ID>` replaces one entry's path (`<ID>` uppercased, `-` as `_`:
  `PASSPORTSIM_CORPUS_PROBE_LONG` for `probe-long`). Two ids that map to one name are refused.
- `PASSPORTSIM_DATA_ROOT` moves the data root.
- `PASSPORTSIM_HOME` moves every directory role into `<dir>/<role>/`.

The project's own tests use the ids `official` (official BSP demo), `pk` (Passport Keys),
`goldminer`, `demo`, and the ROM and probe images `rom0`, `probe-long`, `qemu-oracle`, `probe2`,
`scan3` and `pkgatt`. **A fresh install has no corpus ids**, which is fine: `start` with no argument
boots the bundled demo, and a path boots your own image.

<!-- quickstart: not-run - it boots a machine and leaves a background daemon running -->

```sh
./passportsim start
./passportsim start ~/esp/my-project/build
./passportsim stop
./passportsim serve --stop
```

A build directory is read through the `flasher_args.json` that `idf.py build` writes, so each part
lands at the recorded offset. A `cargo build` binary has no demo: `start` with no argument then
fails with `E_ASSET_MISSING` naming `cargo xtask package`.

## 5. The web bundle

`passportsim-0.1.0-web/` is a static site: `index.html`, the stylesheet, the page, worker and
audio worklet scripts, the wasm core with both ROMs, and the demo `.pebundle` when the build host
had it. Serve it with any static server, at the site root or under a subpath (for example `/emu/`).
Opening the page from disk (`file://`) does not work.

The page boots the demo. Drop an `idf.py` build directory, a merged bin or a `.pebundle` to run it
instead; an ELF dropped alone only gives `inspect` its symbols.

**Simple** mode (the default) shows the device, a firmware card and a live log with the page's
own load steps. **Advanced** mode adds run controls, the Console, UI tree, Events, Inspect, Fidelity
and Perf tabs, and the Battery, USB, Audio, NFC, Wi-Fi, BLE and Snapshots cards. The page is in
English, Simplified Chinese, Japanese and French, follows the browser language and the system
theme, and remembers the choices made in its header. `?mode=advanced` (or `simple`) and `?lang=ja`
(or `en`, `zh-CN`, `fr`) override one load; with the `serve` link, put them before the `#`:
`http://127.0.0.1:8765/?mode=advanced&lang=ja#lc=<code>`.

The same bundle is in the package at `payload/web/` and inside the binary, which is what
`passportsim serve` serves.

Another server must send `Cross-Origin-Opener-Policy: same-origin` and
`Cross-Origin-Embedder-Policy: require-corp` (without them there is no `SharedArrayBuffer`), and
serve `.wasm` as `application/wasm`. The bundle is also a ready Cloudflare Workers project; see
[deploy-cloudflare.md](deploy-cloudflare.md) for the commands and the 25 MiB per-file limit.

## 6. Known limits

| What | State |
|---|---|
| app ELF of a firmware given by path | `until_ui_settled`, `inspect`, `ui` and `--boot-cache` need the app ELF's DWARF. The CLI finds one only for the demo and for a corpus id with an `elf` entry. For a merged image, build directory or `.pebundle` given by path, `boot: until_ui_settled` runs its whole budget and reports `unobservable`, the walkers name the missing ELF, and `--boot-cache` fails with `E_STATE`. The web page does read the ELF of a dropped build directory or `.pebundle` |
| ROM override | `doctor` checks `PASSPORTSIM_ROM` and the `rom.rev101` / `rom.rev3` keys of `config.toml`, but `start` always boots the bundled ROM; there is no `--rom` flag |
| embedded demo | present only when the build host has the `official` corpus entry (the image is never committed); the receipt says whether it is there. Without it, `start` with no argument fails with `E_ASSET_MISSING` |

### The payload, and `--version`

The binary carries the whole payload: web assets, wasm core, schemas, documents, skill and demo.
Moving it elsewhere on its own loses nothing. `--version` (or `-V`) prints the payload digest,
which equals `payload.sha256` in `receipt.json`, and re-checks it:

<!-- quickstart: run exit=0 -->

```sh
./passportsim --version
```

```text
passportsim 0.1.0
payload: embedded, sha256 <the payload.sha256 of receipt.json>
```

| `payload:` line | Meaning |
|---|---|
| `embedded, sha256 <digest>` | a packaged binary, intact |
| `package directory beside the binary, sha256 <digest>` | nothing embedded; the verified `payload/` directory beside it is used |
| `none: development build ...` | a plain `cargo build`; the rest of the line says how to get a payload |
| `embedded, damaged: ...` | the executable was altered; replace it |

## 7. The receipt

`receipt.json` records the version, commit, target, the SHA-256 of every payload file and the
payload digest, whether the demo was embedded (and why not), whether each ROM ELF is inside each
artifact, and which secret-guard rules ran. It holds no host path, user name or device identity.

Packaging runs the secret guard ([secrets.md](secrets.md)) over every file and fails on a hit. The
hashed rules need the host's own `~/.config/passportsim/secrets-check.toml`; without it the receipt
says `secrets.hashed_rules: false`.

Packages are not signed and there is no Homebrew, winget or Scoop package; the first-launch steps
of sections 2 and 8 replace that.

## 8. Windows

The Windows package is `passportsim-0.1.0-windows-x64.zip` with `passportsim.exe`. It is built on
a Windows host with the MSVC tools, in PowerShell:

<!-- quickstart: not-run - it builds into `target\package\` on the Windows host, which the test does itself with its own output directory -->

```powershell
cargo run -q -p xtask -- package --target x86_64-pc-windows-msvc --payload-from <macOS package directory>
```

It writes the same four outputs as section 1, with `.zip` archives. The executable links the C
runtime statically, so nothing needs to be installed on the machine that runs it; it declares
long-path awareness and the UTF-8 code page. Packaging fails if either check fails and records both
in the `windows` block of `receipt.json`.

**One payload on both hosts.** The demo exists only on the macOS build host, and the wasm core's
bytes differ between hosts. `--payload-from` takes both from a macOS package of the same commit,
checks the demo, builds the rest, and refuses unless the whole payload matches the macOS package
file for file; both receipts then carry the same `payload.sha256`. Without it, the Windows package
has its own wasm core and no demo. There is no Windows on Arm package.

**First launch on Windows.** The `.exe` is not signed, so SmartScreen shows "Windows protected your
PC": choose **More info**, then **Run anyway**. Or clear the download mark in PowerShell:

<!-- quickstart: run exit=0 -->

```powershell
Get-ChildItem -Recurse | Unblock-File
```

Then the checks of sections 3 and 4 work the same way:

<!-- quickstart: run exit=0 -->

```powershell
.\passportsim.exe --version
```

<!-- quickstart: run exit=0 -->

```powershell
.\passportsim.exe status
```

<!-- quickstart: run exit=0 -->

```powershell
.\passportsim.exe doctor
```

Everything else works as above with `.\passportsim.exe` for `./passportsim`. The background daemon
outlives the console that started it; `passportsim serve --stop` ends it.

Directory roles come from Windows known folders, not from `%USERPROFILE%` or `%LOCALAPPDATA%`.
To move all of them, set `PASSPORTSIM_HOME=<dir>`.

## 9. Next

- [commands/index.md](commands/index.md): every command with its MCP tool, HTTP route and scenario
  step (generated).
- [errors.md](errors.md): every error code (generated).
- [SKILL.md](../skills/passportsim/SKILL.md): the agent skill, with the device-safety deny list.
- [secrets.md](secrets.md): the secrets policy.
- `docs/ARCHITECTURE.md` in the repository: the design (not shipped in a package).
