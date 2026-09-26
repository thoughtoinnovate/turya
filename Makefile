.PHONY: help build build-release test install install-release clean nuke e2e

# The dev image exports CARGO_HOME=/opt/cargo globally, but that dir is an
# empty root-owned skeleton (no registry cache, not writable), while the
# pre-populated registry cache and write permission live in ~/.cargo. Pin it
# here so every target works out of the box (otherwise even `make build`
# fails re-resolving + downloading into /opt/cargo, os error 13).
# Override per-invocation if you know what you are doing:
#   make CARGO_HOME=/custom/path install
export CARGO_HOME := $(HOME)/.cargo

# Default target
help:
	@echo "Turya Dev Make Commands:"
	@echo "  make build           - Build the workspace (debug)"
	@echo "  make build-release   - Build release binaries (ships as turya)"
	@echo "  make test            - Run all unit tests"
	@echo "  make e2e             - Run End-to-End tests using turya-sim-llm (Mock Provider)"
	@echo "  make install         - Install turya (debug) to ~/.cargo/bin"
	@echo "  make install-release - Install turya (release) to ~/.cargo/bin"
	@echo "  make clean           - Standard cargo clean"
	@echo "  make nuke            - Deep clean (removes target/, cargo cache, and resets state)"

build:
	cargo build --workspace

build-release:
	cargo build --workspace --release

test:
	cargo test --workspace

# NOTE: relies on the CARGO_HOME pin above: the image-wide
# CARGO_HOME=/opt/cargo is an empty root-owned skeleton that is not writable,
# so resolving dependencies into it fails with os error 13.
#
# Installed from the workspace build rather than with `cargo install`.
#
# `cargo install --path` re-resolves against the registry index even with
# `--locked`, so it needs network on a machine whose DNS is broken, and it
# compiles a second copy of everything into its own target dir. It was also
# free to pick versions the tests had never verified.
#
# Building in-workspace and copying the binary:
#   - uses the committed Cargo.lock, so the installed binary is the exact
#     dependency set `make test` passed against,
#   - never contacts the registry once the cache is warm, so it works offline,
#   - reuses ./target, so it is incremental instead of a second full build.
#
# It still fails if a dependency is genuinely absent from the local cache,
# which is the honest failure: the bytes needed to build were never here.
INSTALL_DIR ?= $(HOME)/.cargo/bin
INSTALL_FLAGS = --locked

install:
	cargo build -p turya-cli $(INSTALL_FLAGS)
	install -d $(INSTALL_DIR)
	install -m 0755 target/debug/turya $(INSTALL_DIR)/turya
	@$(INSTALL_DIR)/turya --version

install-release:
	cargo build --release -p turya-cli $(INSTALL_FLAGS)
	install -d $(INSTALL_DIR)
	install -m 0755 target/release/turya $(INSTALL_DIR)/turya
	@$(INSTALL_DIR)/turya --version

clean:
	cargo clean

nuke: clean
	@echo "Nuking all build caches and temporary states..."
	rm -rf target/
	rm -rf ~/.cargo/registry/cache/github.com-*
	rm -rf ~/.turya/scratch/*
	@echo "Nuke complete. Next build will be completely fresh."

e2e:
	@echo "Starting E2E simulation with turya-sim-llm..."
	# We pass TURYA_SIM_MODE to force the engine to use the deterministic MockProvider
	# rather than reaching out to Anthropic/OpenAI, ensuring tests are free and fast.
	TURYA_SIM_MODE=1 cargo test --workspace -- --nocapture
