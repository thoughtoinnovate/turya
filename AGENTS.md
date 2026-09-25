# 🤖 AGENTS.md — Mandatory AI Agent Operating Directives

> **CRITICAL DIRECTIVE FOR ALL AI AGENTS & ASSISTANTS**  
> Any AI agent (Claude, Cursor, Antigravity, Copilot, Windsurf, or autonomous subagents) operating on this repository **MUST strictly follow every rule defined in this document**. Zero exceptions.

---

## 1. 🛡️ Absolute Privacy & Zero Personal Path Leaks

### Rule 1.1: Never Commit Personal System Home Paths
- **FORBIDDEN**: Never write, hardcode, or commit personal system filesystem paths such as:
  - `/home/<username>/` (e.g., `/home/user/...`, `/home/developer/...`)
  - `/Users/<username>/`
  - `C:\Users\<username>\`
  - Any local directory structures tied to a specific developer's machine.
- **REQUIRED**:
  - Always use **workspace-relative paths** (e.g., `./crates/...`, `target/...`, `Cargo.toml`).
  - Use standard environment variables where applicable: `$HOME`, `$CARGO_HOME`, `$TURYA_HOME`.
  - In documentation or mock configurations representing standardized development containers, use the standardized container path: `/home/dev/workspace/turya`.

### Rule 1.2: Zero Secrets & Credentials Leakage
- **FORBIDDEN**: Never commit secrets, tokens, or sensitive credentials into git:
  - LLM API keys (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, etc.).
  - GitHub personal access tokens, SSH private keys, AWS/cloud credentials.
  - `.env` files or local configuration files containing live secrets.
- **REQUIRED**:
  - Read secrets strictly through environment variables.
  - Ensure all `.env*` files and local state databases (`turya.db*`) are in `.gitignore`.

### Rule 1.3: Zero System & Hardware Info Leakage
- **FORBIDDEN**: Do not commit personal machine identifiers:
  - Local hostnames, personal IP addresses, MAC addresses, machine IDs.
  - Usernames, personal email addresses (unless explicitly approved for git authorship).
  - Diagnostic logs containing raw local environment dumps.

---

## 2. ⚙️ Adhere Strictly to Makefile Commands

Turya defines a standardized build and test contract in the root [Makefile](./Makefile). All agents must strictly invoke and adhere to these commands instead of improvising arbitrary compilation or testing commands.

| Makefile Command | Purpose | Underlying Action |
| :--- | :--- | :--- |
| `make help` | Show all available dev commands | Displays command catalog |
| `make build` | Build the entire workspace (debug) | `cargo build --workspace` |
| `make build-release` | Build release binaries (ships as `turya`) | `cargo build --workspace --release` |
| `make test` | Run all workspace unit & integration tests | `cargo test --workspace` |
| `make e2e` | Run deterministic E2E simulation tests | `TURYA_SIM_MODE=1 cargo test --workspace -- --nocapture` |
| `make install` | Build & install `turya` CLI (debug) to `~/.cargo/bin` | `cargo install --path crates/turya-cli --force` |
| `make install-release` | Build & install `turya` CLI (release) to `~/.cargo/bin` | `cargo install --path crates/turya-cli --release --force` |
| `make clean` | Clean workspace build artifacts | `cargo clean` |
| `make nuke` | Deep clean target, cargo cache, & reset scratch state | Cleans `target/`, cargo registry cache, and `~/.turya/scratch/*` |

### Agent Rules for Build & Test Execution:
1. **Never bypass Makefile for primary lifecycle actions**: Always prioritize `make build`, `make test`, and `make e2e`.
2. **Never commit broken builds**: Before proposing or committing any code changes, agents must run `make test` to verify zero regression.
3. **Deterministic Simulation First**: Use `make e2e` for testing agentic flows without burning LLM API tokens.

---

## 3. 🔍 Pre-Commit Sanitization Checklist

Before executing any `git commit`, agents must perform this internal checklist:

- [ ] **No Personal Paths**: Check `git diff --cached` for any `/home/<user>`, `/Users/<user>`, or user-specific paths.
- [ ] **No Secrets / Tokens**: Verify that no API keys or private tokens are present in added lines.
- [ ] **Makefile Compliance**: Code was verified using `make test` or `make build`.
- [ ] **Clean Git Status**: Untracked scratch files, temporary binaries, and logs are excluded or ignored.

---

*This document is enforced across all autonomous and interactive agent workflows for the Turya project.*
