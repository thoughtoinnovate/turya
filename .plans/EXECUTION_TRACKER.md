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
