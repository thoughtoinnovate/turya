# Turya Project Blueprint

Welcome to the comprehensive design, architecture, and implementation documentation for **Turya** — a high-performance, configurable, agentic coding harness.

## Documentation Index

All architectural decisions, UX guides, and implementation steps have been thoroughly documented in this `.plans` directory. Review the files below to understand the system end-to-end:

### 0. 🎯 [Execution Tracker](./EXECUTION_TRACKER.md)
**Your starting point.** A markdown checklist mapping directly to the implementation steps, so you can check off `[x]` as you build the project.

### 1. 🏗️ [Architecture & Roadmap](./turya_architecture_and_roadmap.md)
**Start Here.** This document covers the core philosophy of the project.
- The Microkernel Core Engine.
- The Decoupled Client-Server API (`turya-protocol`).
- How the TUI, WebUI, and Headless modes connect.
- The Crate & Workspace layout.

### 2. 🧩 [Subagents & Plugin Architecture](./plugin_and_subagent_architecture.md)
Deep dive into extensibility and concurrency.
- **Subagents**: Tokio-based async Actor model, Git Worktree isolation, and context economy.
- **Plugins**: Extism (Wasm/WASI) integration for capability-sandboxed community plugins.
- **Self-Improvement Harness**: [Read the philosophical core](./self_improvement_harness.md) of how Turya writes and hot-reloads its own Wasm plugins.
- **Code Intelligence**: Live LSP (Language Server Protocol) integration for instant agent self-correction.

### 3. 📦 [Turya SDK (Python & TypeScript)](./turya_sdk_specification.md)
Programmatic agent orchestration.
- Build custom tools natively using `@tool` decorators.
- Orchestrate parallel background subagents.
- Stream events to your own custom applications.

### 4. 🛠️ [Tools Arsenal & RAG Strategy](./tools_arsenal_and_rag.md)
The deterministic-first approach to code exploration.
- Ripgrep (`grep_search`) and AST-aware LSP queries (`goto_definition`) over probabilistic RAG.
- The optional `turya-plugin-rag` (SQLite vector search) for enterprise monorepos.

### 4. 🧠 [Memory & Self-Improvement System](./memory_and_self_improvement.md)
The 3-tier memory architecture that enables Turya to learn from mistakes.
- **Short-Term**: Context compaction and token budget management.
- **Episodic**: SQLite-backed session persistence (`turya.db`).
- **Long-Term**: Background Reflection subagents that extract rules and prevent repeated mistakes.

### 4. ⚙️ [DevOps, Simulators & Slash Commands](./devops_and_commands.md)
The operational backbone of Turya.
- `turya update` auto-updater mechanics.
- `turya-sim-llm` for deterministic, offline End-to-End testing.
- Modern agent slash commands (`/efforts`, `/thinking`, `/sessions`, `/memory`).

### 5. 🔌 [IDE Integration (VS Code, JetBrains, Neovim)](./ide_integration.md)
How Turya acts as a headless server to power native IDE extensions via WebSockets, stdio JSON-RPC, and MCP.

### 6. 🎨 [TUI UX & Interaction Guide](./tui_ux_and_interaction_guide.md)
The visual and behavioral specification for the Ratatui frontend.
- Non-blocking input loops.
- Progressive disclosure (collapsing messy bash tool logs).
- Split-pane mode (`/plan` vs Build).
- Aggressive, color-coded Permission Modals (`Review-for-me` risk gating).

### 7. 🚀 [Spoon-Fed Implementation Playbook](./spoonfeed_implementation_plan.md)
The exact, step-by-step developer guide to bootstrapping the codebase.
- Exact `Cargo.toml` definitions.
- Full, compilable Rust code for `turya-protocol`, `turya-tools`, `turya-core`, `turya-server`, and `turya-tui`.
- Deterministic mock providers for unit-testing the master loop before connecting to Anthropic/OpenAI APIs.

---

> *These documents represent the complete, approved design specification. When ready to begin writing code, follow `spoonfeed_implementation_plan.md` sequentially from Step 0.*
