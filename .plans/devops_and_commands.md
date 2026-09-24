# Turya: DevOps, Simulation & Agent Operations

To make Turya a truly professional, modern agent harness, it requires rigorous build automation, seamless cross-platform distribution, and powerful built-in user commands.

---

## 1. Distribution & Auto-Updates

Turya is distributed as a single statically linked binary. 

### CI/CD Pipeline (GitHub Actions)
The repository uses a Matrix build matrix (`.github/workflows/release.yml`) triggered on every Git tag (e.g., `v0.2.0`). It cross-compiles to:
* `x86_64-unknown-linux-gnu` (Linux)
* `x86_64-apple-darwin` (macOS Intel)
* `aarch64-apple-darwin` (macOS Silicon)
* `x86_64-pc-windows-msvc` (Windows)

### `turya update` & `turya upgrade`
Instead of forcing users to `cargo install` or `brew upgrade` continuously, Turya includes an internal auto-updater powered by the [`self_update`](https://crates.io/crates/self_update) crate.
* Typing `turya update` queries the GitHub Releases API, downloads the latest ZIP artifact for the host OS, verifies the checksum, and overwrites the active binary.

---

## 2. E2E Testing & The `turya-sim-llm`

Testing AI agents is notoriously difficult because LLMs are non-deterministic, slow, and cost money per token. 
Turya solves this by introducing `turya-sim-llm`.

* **How it works**: By passing the `TURYA_SIM_MODE=1` environment variable (or running `make e2e`), Turya swaps out the `AnthropicProvider` for the `MockProvider`.
* **The MockProvider**: Instead of calling a network API, the `MockProvider` consumes a predefined JSON script of events (Tokens, Tool Calls).
* **Benefit**: You can write a test that asserts Turya successfully spawned a Worktree, edited a file, ran `cargo test`, and merged the diff—and that test will run in 50 milliseconds, offline, with 100% reliability, every single time on CI.

---

## 3. Modern Agent Slash Commands

Turya supports industry-standard slash commands invoked directly from the TUI prompt to manage agent operational state.

### 💰 `/efforts` (or `/costs`)
Displays a highly detailed breakdown of the current session:
* Token usage (Input / Output / Cached).
* Context window utilization (e.g., "150k / 200k tokens").
* Exact API cost incurred for the session so far.

### 🧠 `/thinking`
Toggles the visibility of the agent's internal "Chain of Thought" reasoning.
* When ON (default): You see the agent's step-by-step reasoning streamed in gray italics before it takes action.
* When OFF: The TUI only renders the final output and tool calls, reducing screen clutter.

### 🗄️ `/sessions`
Opens an interactive TUI overlay to manage past episodic memory.
* Allows you to browse past debugging sessions, view transcripts, and resume them directly.

### 💾 `/memory`
Opens the Long-Term Semantic Memory editor.
* Lets the user view, edit, or delete the "rules" the Reflection Subagent has learned about the current repository. (e.g., *[Delete Rule]: Always use Python 3.9*).

### 🛠️ `/tools` or `/plugins`
Lists all active built-in tools and loaded Extism Wasm plugins, showing their capability permissions.
