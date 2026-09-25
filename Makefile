.PHONY: help build build-release test install install-release clean nuke e2e

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

install:
	cargo install --path crates/turya-cli --force

install-release:
	cargo install --path crates/turya-cli --release --force

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
