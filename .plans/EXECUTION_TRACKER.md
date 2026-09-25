# 🚀 Turya Execution Tracker

This is your single, trackable entrypoint. As you (or an AI agent) complete the code generation phases detailed in the [Spoon-Fed Implementation Playbook](./spoonfeed_implementation_plan.md), check the boxes below by changing `[ ]` to `[x]`.

> **🔄 AGENT RESUMPTION PROTOCOL**
> If you are an AI picking up this task in a new session:
> 1. Scan this file to find the first unchecked `[ ]` task.
> 2. Run `cargo check --workspace` and `cargo test --workspace` to verify the state of the codebase matches the checked boxes. Fix any compilation errors left by the previous session before proceeding.
> 3. Read the corresponding step in `spoonfeed_implementation_plan.md`.
> 4. Continue execution without duplicating already completed work.

---

## Phase 1: Core Foundation (The Microkernel & Types)
*See `spoonfeed_implementation_plan.md` Step 0 - Step 2*

- [x] **🤖 Agent Instruction (MANDATORY)**: Before writing code for this phase, use your web search tools to explore the best guides and modern practices for lightweight, fast, self-improvable agent state machines and event protocols in Rust.
- [x] **Step 0: Workspace Initialization**
  - [x] Create workspace `Cargo.toml`.
  - [x] Verify: `cargo check` passes.
- [x] **Step 1: Crate `turya-protocol`**
  - [x] Create crate structure.
  - [x] Define `TuryaCommand` and `TuryaEvent` Enums.
  - [x] Verify: `cargo test -p turya-protocol` passes.
- [x] **Step 2: Crate `turya-tools`**
  - [x] Create crate structure.
  - [x] Implement `ViewFileTool`, `WriteFileTool`, and `RunBashTool`.
  - [x] Verify: `cargo check -p turya-tools` passes.

---

## Phase 2: Engine & Decoupled Server
*See `spoonfeed_implementation_plan.md` Step 3 - Step 4*

- [x] **🤖 Agent Instruction (MANDATORY)**: Before writing code for this phase, search the web for the latest architectural best practices in building decoupled, headless agent servers, LLM mock providers, and capability-gating (permission brokers) in Rust.
- [x] **Step 3: Crate `turya-core`**
  - [x] Implement `PermissionBroker`.
  - [x] Implement `MockProvider` (deterministic testing).
  - [x] Implement `MasterLoop` event dispatcher.
  - [x] Verify: `cargo test -p turya-core` passes.
- [x] **Step 4: Crate `turya-server`**
  - [x] Implement async channel proxy / socket server.
  - [x] Verify: `cargo check -p turya-server` passes.

---

## Phase 3: The TUI & Binary Glue
*See `spoonfeed_implementation_plan.md` Step 5 - Step 6*

- [x] **🤖 Agent Instruction (MANDATORY)**: Before writing code for this phase, search for modern `ratatui` best practices, non-blocking async TUI architectures, and how tools like Claude Code or Cursor handle TUI responsiveness and streaming.
- [x] **Step 5: Crate `turya-tui`**
  - [x] Setup `ratatui` backend and `crossterm` event stream.
  - [x] Implement non-blocking chat, tool spinners, and permission modal states.
  - [x] Verify: `cargo check -p turya-tui` passes.
- [x] **Step 6: Crate `turya-cli` (The Entrypoint)**
  - [x] Wire `turya-server`, `turya-core`, and `turya-tui` together.
  - [x] Verify: `cargo build --workspace` compiles `turya-cli`.
  - [x] **Milestone achieved:** You can now run `./target/debug/turya-cli` and interact with the mock engine!

---

## Phase 4: The Extensibility Harness (Self-Improvement)
*These require expanding beyond the initial playbook.*

- [x] **🤖 Agent Instruction (MANDATORY)**: Before implementing this phase, deeply research "LLM self-improvement architectures", "Extism Wasm plugin hosts in Rust", and "LSP agent integrations" to ensure the implementation is cutting-edge and lightweight.
- [x] **Step 7: Real LLM Integration**
  - [x] Swap `MockProvider` for `AnthropicProvider` (SSE streaming).
- [x] **Step 8: `turya-plugin-memory`**
  - [x] Setup local SQLite database (`turya.db`).
  - [x] Hook into `pre_turn` and `TurnCompleted`.
- [x] **Step 9: Wasm Self-Improvement Sandbox**
  - [x] Embed Extism Wasm runtime.
  - [x] Allow the agent to dynamically register `plugin.wasm` files mid-turn.
- [x] **Step 10: LSP Bridge**
  - [x] Connect `rust-analyzer`/`tsserver` for live compilation diagnostics.

---

## Phase 5: Provider Auth Framework (Microkernel-Clean)
*Bound by AGENTS.md Rule 3.1–3.5. Every step carries a gate declaration:
microkernel items (why essential + why the architecture holds) vs plugin items.*

- [x] **Gate 0: AGENTS.md amendment** — Rules 3.1 (core owns traits/registries/loop),
  3.2 (internal vs external + manifest `kind`), 3.3 (register/unregister + hot-reload),
  3.4 (CI dep gates), 3.5 (pre-change ask + declare block).
- [x] **Step 0: Hook-trait decoupling** — `MemoryHook`/`DiagnosticsHook` traits in
  core; rusqlite/LSP adapters moved to `turya-memory`/`turya-lsp`; core drops both deps.
- [x] **Step 1: `turya-auth`** — keychain/memory stores, dual-slot status,
  PKCE + refresh, precedence resolver (15 tests, network mocked).
- [x] **Step 2: Core traits + protocol + hot-swap** — `ProviderPlugin`,
  `ProviderRegistry` (register/unregister), `UiPlugin`, `ResolvedCreds`,
  provider/auth/model protocol messages, `set_provider`.
- [x] **Step 3: Provider crates** — Anthropic migrated OUT of core (core got
  lighter); new Gemini SSE provider; per-crate `list_models`.
- [x] **Step 4: `turya-catalog`** — auth-gated loading (locked providers never
  phone home), models.dev scoped metadata + TTL cache, host-fn surface, honest
  `None` pricing until a scoped source exists.
- [x] **Step 5: CLI** — `auth login|logout|status`, `--provider/--model`,
  registry bootstrap + host router (server untouched).
- [x] **Step 6: Slash system** — registry, autocomplete popup, `InputMode`
  state machine (legacy `/`-as-prompt preserved).
- [x] **Step 7: `/auth` + `/models` flows** — badges, lock-pivot, masked key
  prompt, OAuth handoff, optimistic switch + Error surfacing.
- [x] **Step 8: Manifest + gates** — `kind`, `OAuthConfig` validation,
  `storage` scope, CI microkernel gates (Rule 3.4).
- [x] **Milestone:** `make build/test/e2e` green, `fmt` + `clippy -D warnings`
  clean, `turya auth status` renders dual-slot table.
