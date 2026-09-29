# Contributing to PassportSim

**English** | [简体中文](docs/i18n/zh-CN/CONTRIBUTING.md) | [日本語](docs/i18n/ja/CONTRIBUTING.md) | [Français](docs/i18n/fr/CONTRIBUTING.md)

Thank you for helping. PassportSim aims to behave exactly like the real board, so a change is
judged by its evidence: every behavior should trace back to documentation, to source under a
compatible license, or to a measurement on silicon.

Please follow the [Code of Conduct](CODE_OF_CONDUCT.md). Report security issues as described in
[SECURITY.md](SECURITY.md), never in a public issue.

The most valuable report is a **fidelity gap**: firmware that behaves differently in the emulator
than on the board. Include the image (or how to build it), the commands you ran, and what the
emulator and the board each did. Never attach real device data (see [Secrets](#secrets)).

## Setup

You need [Rust](https://rustup.rs) (the toolchain in `rust-toolchain.toml` installs itself),
[Bun](https://bun.sh) 1.4.1 or newer (CI runs the version in `.bun-version`) and
[just](https://github.com/casey/just). Development hosts are macOS on Apple silicon and Windows 10
or newer on x64.

```sh
just setup                        # wasm target and the web page's packages
cargo xtask secrets-check --init  # once per clone
cargo xtask hooks install         # the pre-commit and pre-push secret checks
```

Run `just setup` again in every new worktree: `web/node_modules/` is not shared, and without it
the web tests fail with a missing-package error.

`just` lists every command. The everyday ones are `just run` (build and serve the web page),
`just test`, `just check` and `just ci`.

## Before you open a pull request

```sh
just check              # cargo fmt, clippy, TypeScript type checks
just test               # Rust and web unit tests
cargo xtask codegen     # regenerate tables and docs; the tree must stay clean
just ci                 # the T0 tier: lints, all tests and the repository checks
```

`just ci` runs `cargo xtask ci t0`, which needs no firmware corpus or device data and runs the
same way on macOS and Windows, testing the machine it runs on. `cargo xtask ci t1` and `t2` add
the corpus, goldens, browser runs, oracle diffs and benchmarks; they run on macOS. GitHub Actions
covers both hosts: `pr-check.yml` checks every pull request on macOS and Windows, `release.yml`
publishes a release, and `deploy-web.yml` deploys the web page. To release, run the Release
workflow from the Actions tab, optionally with a version or a bump (patch by default): it tags the
commit, builds both packages, publishes the GitHub Release with generated notes and deploys the web
page.

Checklist:

- [ ] `just ci` passes, and `t1` too if you changed emulation or the web page and have the corpus;
- [ ] a behavior change comes with a test that failed before it;
- [ ] no generated file was edited by hand;
- [ ] no real device data is in the diff.

## Commits

- Titles are short, in English, with a conventional prefix: `feat(scope):`, `fix(scope):`,
  `perf(scope):`, `test(scope):`, `docs(scope):`, `ci:`, `build:`.
- The body says why, and names the evidence for a behavior change: a spec row, a probe capture, a
  document section.
- Commit with plain `git commit`. Never skip or redirect the hooks (`--no-verify`,
  `core.hooksPath`).

## Clean room

PassportSim is MIT-licensed and written from public information. To keep it that way:

- **Never read or copy the source of** QEMU (including Espressif's fork), esp32sim, ESP-EMU,
  NimBLE, Bumble, Zephyr, BlueZ, esptool or serialport-rs, or any other GPL, LGPL, AGPL or
  unlicensed emulator or stack. Do not paraphrase their code, structure or comments either.
- **Other emulators may only be run as black-box oracles:** you may compare their outputs (console
  text, register write streams, traces) with ours, as `tools/oracle/` and `xtask oracle` do, but
  never look inside them.
- **Register and timing facts may come from** Espressif's public documentation (the ESP32-C3
  technical reference manual and datasheets), ESP-IDF sources (Apache-2.0), the bundled ROM ELFs
  (Apache-2.0), and our own probe firmware run on real silicon (`probes/`).
- **Cite the source.** Every row in `specs/` has a `provenance` field, and every model file's
  header names the spec rows or documents it implements. Mark an assumption that is not verified
  yet as `UNVERIFIED`. `cargo xtask provenance` checks this in T0.

## Where things live

| What | Where |
|---|---|
| Design | [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) |
| Unit tests | Next to the code (`#[cfg(test)]`) and in each crate's `tests/` |
| Integration tests that boot real images | `tests/milestones/` (tests named `t1_*` or `t2_*` run in that CI tier) |
| Goldens, scenarios, transcripts | `tests/golden/`, `tests/scenarios/`, `tests/transcripts/` |
| Web unit tests | `web/src/**/*.test.ts` (`bun test`) |
| Browser tests | `web/tests/*.spec.ts` (`just e2e`) |
| Behavior data | `specs/` (see [specs/README.md](specs/README.md)) |

## Conventions

- **Generated files are never edited by hand.** Change the source and run `cargo xtask codegen`
  (register tables, fidelity docs) or `cargo xtask docs` (command reference).
- **Registries are local.** A peripheral changes its own file, not `periph/mod.rs`; a command
  registers itself with `#[command]`.
- **Frozen interfaces change on their own.** The core traits and the wasm ABI listed in
  [ARCHITECTURE.md](docs/ARCHITECTURE.md#frozen-interfaces) change in a separate pull request,
  before code that uses them, with the reason in its description.
- **Dependencies are reviewed.** New crates go in a small change of their own with
  `cargo deny check` passing; only the licenses in `deny.toml` are allowed.
- **Core crates stay host-free.** They build for `wasm32-unknown-unknown` and use no clock,
  thread, file, environment, network or process API. Host-specific code lives in
  `pemu_host::platform`, directory roles in `pemu_host::paths`.
- **Rust edition 2024 and `cargo fmt`.** Code, comments and commit messages are in English.
  Comments are short and say why; the code says what.
- **The tree is LF only** and every path must be legal on Windows; `cargo xtask portable` checks
  both.

## Secrets

Real device data never enters the repository: no MAC addresses, unique IDs, calibration values,
flash or eFuse dumps, backup file names or card contents. Use placeholder MACs with the `02:00:00`
prefix. The hooks installed above refuse commits that contain such data. The policy is in
[docs/secrets.md](docs/secrets.md).

## License

By contributing you agree that your contribution is licensed under the MIT license of this
repository ([LICENSE](LICENSE)).
