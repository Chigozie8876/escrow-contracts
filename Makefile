SHELL := /bin/bash

.PHONY: help build size reproducible docs test-scripts fmt clippy test shellcheck deny audit check ci smoke-local local-network local-network-stop clean

.DEFAULT_GOAL := help

help:
	@printf '%s\n' \
	  'make check        Run the local CI sequence: build, docs, fmt, clippy, tests, shellcheck' \
	  'make ci           Run check plus cargo-deny and cargo-audit (both tools must be installed)' \
	  'make build        Build the optimised Wasm and print its size and SHA-256' \
	  'make size         Build, then fail if the Wasm exceeds 40 KiB' \
	  'make reproducible Build twice from clean targets and compare the hashes' \
	  'make docs         Check README.md and docs/CONTRACT.md against the contract source' \
	  'make test-scripts Run the stubbed deploy-mainnet.sh tests' \
	  'make fmt          Check formatting' \
	  'make clippy       Run clippy with the workspace lint set and -D warnings' \
	  'make test         Run the Rust test suite (builds the Wasm first)' \
	  'make shellcheck   Lint scripts/*.sh and scripts/tests/*.sh' \
	  'make deny         Run cargo-deny against deny.toml' \
	  'make audit        Run cargo-audit' \
	  'make smoke-local  Deploy and smoke-test against a running local Quickstart network' \
	  'make local-network       Start stellar/quickstart on the local network' \
	  'make local-network-stop  Stop the local Quickstart container' \
	  'make clean        Remove Cargo build artifacts'

build:
	./scripts/build.sh

size: build
	bash scripts/check-wasm-size.sh

reproducible:
	bash scripts/reproducible-build.sh

docs:
	python3 scripts/check_contract_docs.py

test-scripts:
	bash scripts/tests/deploy-mainnet.test.sh

fmt:
	cargo fmt --check

# The upgrade test embeds target/wasm32v1-none/release/bounty_escrow.wasm.
clippy: build
	cargo clippy --all-targets --locked -- -D warnings

test: build
	cargo test --locked

shellcheck:
	shellcheck scripts/*.sh scripts/tests/*.sh

deny:
	cargo deny check

audit:
	cargo audit

check: build reproducible docs test-scripts fmt clippy test shellcheck

ci: check deny audit

smoke-local:
	@for role in admin arbiter treasury relayer maintainer contributor; do \
	  stellar keys address "se-local-$$role" >/dev/null 2>&1 || \
	    stellar keys generate "se-local-$$role"; \
	done
	@for role in admin arbiter treasury relayer maintainer contributor; do \
	  stellar keys fund "se-local-$$role" --network local >/dev/null; \
	done
	OVERWRITE_DEPLOYMENT=1 NETWORK=local KEY_PREFIX=se-local- DEPLOYMENT_FILE=deployments/local.json ./scripts/deploy-testnet.sh
	NETWORK=local KEY_PREFIX=se-local- DEPLOYMENT_FILE=deployments/local.json ./scripts/smoke-testnet.sh

local-network:
	stellar container start local

local-network-stop:
	stellar container stop local

clean:
	cargo clean
