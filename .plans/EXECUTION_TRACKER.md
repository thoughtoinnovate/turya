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

- [ ] **🤖 Agent Instruction (MANDATORY)**: Before writing code for this phase, use your web search tools to explore the best guides and modern practices for lightweight, fast, self-improvable agent state machines and event protocols in Rust.
- [ ] **Step 0: Workspace Initialization**
  - [ ] Create workspace `Cargo.toml`.
  - [ ] Verify: `cargo check` passes.
- [ ] **Step 1: Crate `turya-protocol`**
  - [ ] Create crate structure.
  - [ ] Define `TuryaCommand` and `TuryaEvent` Enums.
  - [ ] Verify: `cargo test -p turya-protocol` passes.
- [ ] **Step 2: Crate `turya-tools`**
  - [ ] Create crate structure.
  - [ ] Implement `ViewFileTool`, `WriteFileTool`, and `RunBashTool`.
  - [ ] Verify: `cargo check -p turya-tools` passes.

---

## Phase 2: Engine & Decoupled Server
*See `spoonfeed_implementation_plan.md` Step 3 - Step 4*

- [ ] **🤖 Agent Instruction (MANDATORY)**: Before writing code for this phase, search the web for the latest architectural best practices in building decoupled, headless agent servers, LLM mock providers, and capability-gating (permission brokers) in Rust.
- [ ] **Step 3: Crate `turya-core`**
  - [ ] Implement `PermissionBroker`.
  - [ ] Implement `MockProvider` (deterministic testing).
  - [ ] Implement `MasterLoop` event dispatcher.
  - [ ] Verify: `cargo test -p turya-core` passes.
- [ ] **Step 4: Crate `turya-server`**
  - [ ] Implement async channel proxy / socket server.
  - [ ] Verify: `cargo check -p turya-server` passes.

---

## Phase 3: The TUI & Binary Glue
*See `spoonfeed_implementation_plan.md` Step 5 - Step 6*

- [ ] **🤖 Agent Instruction (MANDATORY)**: Before writing code for this phase, search for modern `ratatui` best practices, non-blocking async TUI architectures, and how tools like Claude Code or Cursor handle TUI responsiveness and streaming.
- [ ] **Step 5: Crate `turya-tui`**
  - [ ] Setup `ratatui` backend and `crossterm` event stream.
  - [ ] Implement non-blocking chat, tool spinners, and permission modal states.
  - [ ] Verify: `cargo check -p turya-tui` passes.
- [ ] **Step 6: Crate `turya-cli` (The Entrypoint)**
  - [ ] Wire `turya-server`, `turya-core`, and `turya-tui` together.
  - [ ] Verify: `cargo build --workspace` compiles `turya-cli`.
  - [ ] **Milestone achieved:** You can now run `./target/debug/turya-cli` and interact with the mock engine!

---

## Phase 4: The Extensibility Harness (Self-Improvement)
*These require expanding beyond the initial playbook.*

- [ ] **🤖 Agent Instruction (MANDATORY)**: Before implementing this phase, deeply research "LLM self-improvement architectures", "Extism Wasm plugin hosts in Rust", and "LSP agent integrations" to ensure the implementation is cutting-edge and lightweight.
- [ ] **Step 7: Real LLM Integration**
  - [ ] Swap `MockProvider` for `AnthropicProvider` (SSE streaming).
- [ ] **Step 8: `turya-plugin-memory`**
  - [ ] Setup local SQLite database (`turya.db`).
  - [ ] Hook into `pre_turn` and `TurnCompleted`.
- [ ] **Step 9: Wasm Self-Improvement Sandbox**
  - [ ] Embed Extism Wasm runtime.
  - [ ] Allow the agent to dynamically register `plugin.wasm` files mid-turn.
- [ ] **Step 10: LSP Bridge**
  - [ ] Connect `rust-analyzer`/`tsserver` for live compilation diagnostics.
