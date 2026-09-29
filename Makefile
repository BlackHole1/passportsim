# The commands of the justfile beside this one, for hosts with `make` and no `just`: `make` lists
# them, `make run` builds what changed and serves the web UI. Needs a POSIX shell; on Windows use
# `just`. Arguments go in ARGS, for example `make cli ARGS="start --fw official"`.

.DEFAULT_GOAL := help
.PHONY: help setup run start build core web cli-build cli test e2e check ci package deploy clean

PORT ?= 4173
ARGS ?=
# The data root the demo firmware is read from, where the binary looks for it (`pemu-host` `paths.rs`).
ifeq ($(shell uname -s),Darwin)
PASSPORTSIM_DATA_ROOT ?= $(HOME)/Library/Application Support/passportsim
HOST := $(shell uname -m | sed 's/arm64/aarch64/')-apple-darwin
else
PASSPORTSIM_DATA_ROOT ?= $(or $(XDG_DATA_HOME),$(HOME)/.local/share)/passportsim
HOST := $(shell uname -m)-unknown-linux-gnu
endif
VERSION = $(shell cargo pkgid -p pemu-cli | sed 's/.*[\#@]//')

help: ## List the commands.
	@awk 'BEGIN { FS = ":.*## " } /^[a-z0-9-]+:.*## / { printf "  %-10s %s\n", $$1, $$2 }' $(MAKEFILE_LIST)

setup: ## Install what a fresh checkout needs: the wasm target and the page's packages.
	rustup target add wasm32-unknown-unknown
	cd web && bun install --frozen-lockfile

run: core web ## Build the core and the page, then serve the web UI on 127.0.0.1 (PORT=4173).
	cd web && PASSPORTSIM_DATA_ROOT="$(PASSPORTSIM_DATA_ROOT)" bun tests/serve.ts $(PORT)

start: run ## The same as run.

build: core web cli-build ## Build the wasm core, the web page and the native CLI.

core: ## Build the wasm core the page runs.
	cargo build -p pemu-wasm --lib --target wasm32-unknown-unknown --profile wasm-release

web: ## Build the web page into web/dist.
	cd web && bun run build

cli-build: ## Build the native CLI into target/release/.
	cargo build --release -p pemu-cli --bin passportsim

cli: ## Build and run the native CLI with ARGS, for example ARGS="start --fw official".
	cargo run -q --release -p pemu-cli --bin passportsim -- $(ARGS)

test: ## Unit tests: the page, then the Rust workspace.
	cd web && bun test
	cargo test --workspace

e2e: core web ## Browser tests (Playwright) with ARGS, for example ARGS="--project=firefox".
	cd web && bun run e2e $(ARGS)

check: ## Formatting, lints and type checks.
	cd web && bun run typecheck && bun run typecheck:e2e
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings

ci: ## The CI tier T0 on this machine.
	cargo xtask ci t0

package: ## The release package for this host, under target/package/.
	cargo xtask package --target $(HOST)

deploy: package ## Package, then deploy the web bundle to Cloudflare Workers (docs/deploy-cloudflare.md).
	cd target/package && bunx wrangler@4 deploy --config passportsim-$(VERSION)-web/wrangler.jsonc

clean: ## Remove build output.
	cargo clean
	rm -rf web/dist
