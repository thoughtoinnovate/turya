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

## 3. 🧬 Microkernel Architecture Constraint (STRICT, NEVER DEVIATE)

> Turya is a microkernel + plugin ecosystem. The core stays very light.
> This constraint binds ALL planning and implementation of new changes.

### Rule 3.1: Core owns traits, registries, and the event loop — nothing else
- **FORBIDDEN in `turya-core`:** provider implementations, vendor strings/URLs,
  auth/credential/keychain/HTTP-OAuth code, UI code, secrets of any kind.
- **ALLOWED in `turya-core`:** `LlmProvider`/`ProviderPlugin`/`UiPlugin` traits,
  `ProviderRegistry`, hook traits (`MemoryHook`, `DiagnosticsHook`),
  `CredentialResolver`/`ResolvedCreds` *shapes* (never resolvers),
  permission broker, event loop, protocol types.
- **ENFORCED** by CI dependency gates (see Rule 3.4).

### Rule 3.2: Internal vs external plugins
- **Internal (native, mandatory, shipped):** protocol, engine loop, primitive tools,
  TUI client, CLI host, auth service, built-in providers, catalog, memory, updater.
  Internal ≠ hardcoded: every internal capability registers through the same
  trait + registry + capability-manifest path an external plugin would use, so
  any of them can be replaced or hot-reloaded without touching core.
- **External (community, Wasm-sandboxed):** community providers, themes, custom
  tools, RAG, UI widgets. Secrets cross into sandboxes as short-lived tokens only.
- Every plugin declares `kind = "internal" | "external"` in its manifest.

### Rule 3.3: Self-improvement loop stays intact
- The registry MUST support runtime `register`/`unregister`; the plugin host MUST
  support hot-reload. Any change that makes capabilities load-time-only is rejected.
- The agent writing its own `.wasm` plugin mid-session is a first-class flow:
  `register_plugin` → capability-gated → callable in the same turn.

### Rule 3.4: CI gates (all must pass)
- `turya-core` dependency allowlist (tokio, serde, serde_json, async-trait + registry
  plumbing only — no auth/vendor/UI crates).
- `turya-tui` imports `turya-protocol` + UI libs only.
- `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`.

### Rule 3.5: Pre-change architecture gate (MANDATORY before ANY code change)
Before writing a single line of implementation, the agent MUST:
1. ASK the user to confirm the change follows this microkernel constraint
   (one explicit question, every time — no silent proceeding).
2. In the plan, declare for the change, in this EXACT block format:
   - **Goes in microkernel:** <crate/items> — why it is essential kernel
     responsibility AND why it does not void the architecture.
   - **Goes in plugin system:** <internal|external, crate/items> — why it
     belongs outside core.
   - **New dependencies (if any):** <crate → dep> — each justified, or "none".
3. Proceed ONLY on user confirmation. If the user rejects, revise the split
   until the declaration is truthful — never relabel core code as "plugin"
   to dodge the gate.

---

## 4. 🔍 Pre-Commit Sanitization Checklist

Before executing any `git commit`, agents must perform this internal checklist:

- [ ] **No Personal Paths**: Check `git diff --cached` for any `/home/<user>`, `/Users/<user>`, or user-specific paths.
- [ ] **No Secrets / Tokens**: Verify that no API keys or private tokens are present in added lines.
- [ ] **Makefile Compliance**: Code was verified using `make test` or `make build`.
- [ ] **Clean Git Status**: Untracked scratch files, temporary binaries, and logs are excluded or ignored.

---

*This document is enforced across all autonomous and interactive agent workflows for the Turya project.*
