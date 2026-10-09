# Everyday commands: `just` lists them, and `just run` builds what changed and serves the web UI.
# The Makefile beside this file offers the same commands on hosts with `make` but no `just`.

set lazy := true
set windows-shell := ["powershell.exe", "-NoLogo", "-NoProfile", "-Command"]

# The data root the demo firmware is read from, where the binary looks for it (`pemu-host` `paths.rs`).
data_root := env("PASSPORTSIM_DATA_ROOT", if os() == "windows" { data_local_directory() / "passportsim" / "data" } else { data_local_directory() / "passportsim" })
host := arch() + if os() == "macos" { "-apple-darwin" } else if os() == "windows" { "-pc-windows-msvc" } else { "-unknown-linux-gnu" }
version := replace_regex(`cargo pkgid -p pemu-cli`, '.*[#@]', '')

# List the commands.
default:
    @just --list --unsorted

# Install what a fresh checkout needs: the wasm target and the page's packages.
setup: setup-web
    rustup target add wasm32-unknown-unknown

[private]
[working-directory: "web"]
setup-web:
    bun install --frozen-lockfile

# Build the core and the page, then serve the web UI on 127.0.0.1 (with the demo when the data root has it, and the play relay).
[working-directory: "web"]
run port="4173" $PASSPORTSIM_DATA_ROOT=data_root: core web
    bun tests/serve.ts {{port}} --play-relay

alias start := run

# Build the wasm core, the web page and the native CLI.
build: core web cli-build

# Build the wasm core the page runs.
core:
    cargo build -p pemu-wasm --lib --target wasm32-unknown-unknown --profile wasm-release

# Build the web page into `web/dist`.
[working-directory: "web"]
web:
    bun run build

# Build the native CLI into `target/release/`.
cli-build:
    cargo build --release -p pemu-cli --bin passportsim

# Build and run the native CLI, for example `just cli start --fw official`.
cli *args:
    cargo run -q --release -p pemu-cli --bin passportsim -- {{args}}

# Unit tests: the Rust workspace, then the page.
test: test-web
    cargo test --workspace

[private]
[working-directory: "web"]
test-web:
    bun test

# Browser tests (Playwright), for example `just e2e --project=firefox`.
[working-directory: "web"]
e2e *args: core web
    bun run e2e {{args}}

# Formatting, lints and type checks.
check: check-web
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings

[private]
[working-directory: "web"]
check-web:
    bun run typecheck
    bun run typecheck:e2e

# The CI tier T0 on this machine.
ci:
    cargo xtask ci t0

# The release package for this host, under `target/package/`.
package:
    cargo xtask package --target {{host}}

# Package, then deploy the web bundle to Cloudflare Workers (`docs/deploy-cloudflare.md`).
[working-directory: "target/package"]
deploy: package
    bunx wrangler@4 deploy --config passportsim-{{version}}-web/wrangler.jsonc

# Remove build output.
clean:
    cargo clean
    bun -e "require('node:fs').rmSync('web/dist', { recursive: true, force: true })"
