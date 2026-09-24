# Turya TUI: UX & Interaction Guide

> **Design Philosophy**: The Turya Terminal UI (TUI) must feel instantaneous, keyboard-centric, and entirely non-blocking. It should provide "progressive disclosure"—keeping the main chat clean while allowing deep dives into tool logs, diffs, and subagent states when needed.

---

## 1. Core Visual Language & Palette

To make the CLI feel like a premium IDE rather than a basic script, Turya uses a semantic color palette and consistent typography (using Unicode box-drawing characters and Nerd Fonts if available).

* **Borders**: Clean, single-line rounded borders `╭─╮`, `│`, `╰─╯`. Active panes use a highlighted border color (Cyan), inactive use subdued (Gray).
* **Semantic Colors**:
  * **Brand / System**: Cyan (`#89dceb`)
  * **Success / Approvals**: Green (`#a6e3a1`)
  * **Warnings / Permissions**: Yellow (`#f9e2af`)
  * **Errors / Destructive**: Red (`#f38ba8`)
  * **Muted / Context**: Gray (`#6c7086`)
* **Animations**: Terminal-safe braille spinners `⠋ ⠙ ⠹ ⠸ ⠼ ⠴ ⠦ ⠧ ⠇ ⠏` for active background tasks.

---

## 2. Interaction Paradigms ("How it Feels")

### 2.1 The "Clean Chat" Rule (Handling Tool Spam)
**Problem**: Traditional agent CLIs dump massive `stdout` logs (like `npm install` or huge `grep` results) directly into the chat, making it impossible to read the agent's actual thoughts.
**The Turya Feel**:
* When a tool starts, a single line appears: `⠋ [Bash] Running 'cargo test'...`
* When it finishes successfully, it collapses into a subtle success indicator: `✔ [Bash] 'cargo test' passed (1.2s)`
* If it fails, it shows the error summary, and the user can press `<Enter>` while focusing the log line to pop open a scrollable overlay with the full `stderr`.

### 2.2 Fluid Typing & Submission
* **Input Box**: Acts like a modern editor. 
  * `Enter`: Submits the prompt.
  * `Shift+Enter` (or `Alt+Enter`): Inserts a newline for multi-line prompts.
  * `Ctrl+U`: Clears the current input.
  * `/`: Immediately opens a fuzzy-findable command palette overlay (e.g., `/model`, `/clear`, `/plan`).

### 2.3 Non-Blocking Execution
* When the user hits `Enter`, the prompt moves up to the chat history immediately.
* The input box remains active (though visually muted). The user can continue typing a follow-up thought *while* the agent is generating or running a 30-second build tool. The follow-up is queued.

---

## 3. UI States & Layouts

### State A: Default Conversational Mode
This is the standard interface for quick questions or single-file edits.

```text
╭─ Turya ─────────────────────────────────────────────────────── [Build Mode] ─╮
│ 👤 User                                                                       │
│ Can you update the rate limiter to use a leaky bucket instead?                │
│                                                                               │
│ 🤖 Turya                                                                     │
│ Sure, I'll update the `auth.rs` file to use a leaky bucket implementation.    │
│                                                                               │
│ ⠙ [Tool: edit_file] Applying diff to src/auth.rs...                           │
│ ✔ [Tool: view_file] Read src/config.rs (24 lines)                             │
│                                                                               │
│ I'm modifying the middleware right now. Let me run the tests to verify.       │
│ ⠹ [Tool: bash] cargo test --package auth                                      │
╰───────────────────────────────────────────────────────────────────────────────╯
╭─ Prompt ──────────────────────────────────────────────────────────────────────╮
│ ❯ _                                                                           │
│                                                                               │
╰─ [Enter] Send │ [/] Commands │ [Ctrl+C] Abort │ [?] Help ─────────────────────╯
```

### State B: The Permission Modal (Review-for-Me)
**The Feel**: When the agent attempts a risky action, the UI aggressively grabs focus. The borders turn Yellow or Red (based on risk). Background rendering pauses or dims.

```text
╭─ Turya ─────────────────────────────────────────────────────── [Build Mode] ─╮
│ 👤 User                                                                       │
│ Delete the old python backend and replace it with rust.                       │
│                                                                               │
│ 🤖 Turya                                                                     │
│ I will remove the `backend-py/` directory first.                              │
│ ╭───────────────────────────────────────────────────────────────────────────╮ │
│ │ ⚠️  CRITICAL PERMISSION REQUESTED                                         │ │
│ │                                                                           │ │
│ │ Action:   run_bash                                                        │ │
│ │ Command:  rm -rf ./backend-py                                             │ │
│ │ Reason:   Removing legacy python codebase as requested.                   │ │
│ │                                                                           │ │
│ │ ❯ [y] Allow once                                                          │ │
│ │   [e] Edit command                                                        │ │
│ │   [n] Deny & Explain                                                      │ │
│ ╰───────────────────────────────────────────────────────────────────────────╯ │
╰───────────────────────────────────────────────────────────────────────────────╯
```
* **Interaction**: `Up/Down` to select, `Enter` to confirm, or just press `y`/`n` instantly.

### State C: Plan vs. Build Split View (The "IDE" Feel)
**The Feel**: When the user types `/plan`, the UI splits. The left pane shows a structured checklist (DAG) of the plan. The right pane shows the workspace tree and live diffs of what the agent is proposing.

```text
╭─ 📋 Execution Plan (Tab: 1) ──────────╮╭─ 🛠️ Workspace Diff (Tab: 2) ─────────╮
│ [✔] 1. Init sqlx in Cargo.toml        ││ Active Diff: src/db.rs               │
│ [●] 2. Create Postgres connection pool││ ──────────────────────────────────── │
│ [ ] 3. Write migration script         ││ @@ -10,3 +10,5 @@                    │
│ [ ] 4. Update auth service to use DB  ││ - use sqlite::Connection;            │
│                                       ││ + use sqlx::postgres::PgPoolOptions; │
│ ❯ Turya is writing `src/db.rs`...    ││ + use std::sync::Arc;                │
│                                       ││                                      │
╰───────────────────────────────────────╯╰──────────────────────────────────────╯
╭─ Prompt ──────────────────────────────────────────────────────────────────────╮
│ ❯ Proceed with step 2, but make sure to set the max connections to 50.        │
╰─ [Space] Toggle Step │ [Tab] Switch Pane │ [Enter] Send │ [Esc] Chat Mode ──╯
```
* **Interaction**: `Tab` cycles focus between the Plan, the Diff, and the Input Box.

### State D: Subagent Drawer
**The Feel**: When a subagent is spawned, a drawer slides up from the bottom (or appears as a split). It shows concise status lines for background workers.

```text
╭─ Turya ──────────────────────────────────────────────────────────────────────╮
│ 🤖 Turya                                                                     │
│ I've spawned a research agent to find the best Rust rate-limiting crates      │
│ while I start scaffolding the basic HTTP routes.                              │
├─ Active Subagents (1) ────────────────────────────────────────────────────────┤
│ ⠧ [sub-research-1] Grepping through crates.io docs for "rate limit"...        │
╰───────────────────────────────────────────────────────────────────────────────╯
```
* **Interaction**: Pressing `Ctrl+S` (or a dedicated shortcut) expands the drawer to full screen to view the subagent's isolated context and thoughts.

---

## 4. Keybindings & Shortcuts Reference

| Shortcut | Context | Action |
| :--- | :--- | :--- |
| **`Enter`** | Input Box | Submit prompt |
| **`Shift+Enter`** | Input Box | Insert newline |
| **`Ctrl+C`** | Global | Abort current agent generation/tool call |
| **`Ctrl+D`** | Global | Exit Turya entirely |
| **`Tab`** | Global | Cycle focus between panes (e.g., Chat -> Plan -> Diff -> Input) |
| **`/`** | Input Box | Open slash command palette (autocomplete) |
| **`y` / `n`** | Permission Modal| Quick-approve or deny a permission request |
| **`PageUp/Dn`**| Chat / Logs | Scroll through chat history or expanded tool logs |
| **`Ctrl+O`** | Global | Open current active file (from agent context) in default `$EDITOR` |

## 5. Implementation Notes for Ratatui
To achieve this UX in the Rust backend (`turya-tui`):
1. **Event Loop**: Use `crossterm` `EventStream` intertwined with the `mpsc::Receiver` from `turya-server` using `tokio::select!`. This guarantees the UI never locks up while waiting for the LLM.
2. **State Management**: The UI state machine needs a `Mode` enum (`Chat`, `PlanSplit`, `PermissionModal`) to route keyboard events correctly.
3. **Throttling**: LLM token streaming can happen 100+ times a second. We should throttle the `ratatui` `terminal.draw()` calls to ~60 FPS (approx every 16ms) to prevent excessive CPU usage while maintaining buttery smooth text rendering.

---

## 6. Advanced TUI Features (The "Claude Code" Standard & Beyond)

To compete with and exceed tools like Claude Code and Cursor, the TUI implements the following native features:

### 6.1 Token & Cost Telemetry
* **Per Prompt / Session**: The bottom status bar permanently displays the active token usage (e.g., `In: 4.2k | Out: 312 | Session Cost: $0.14`).
* **Detailed Breakdown**: Typing `/efforts` or `/cost` opens a modal showing exactly which files consumed the most context tokens.

### 6.2 Session Management (Save, Load, Resume)
Because Turya writes every event to the `turya.db` SQLite store in real-time, sessions are inherently persistent.
* **Auto-Save**: You never need to explicitly "save". If your terminal crashes, the session is already on disk.
* **Resume**: Run `turya resume` to load your most recent session exactly where you left off.
* **Session Browser**: Typing `/sessions` opens a fullscreen UI overlay listing all past sessions by date, cost, and summary. You can navigate with arrow keys and hit `Enter` to load any past conversation.
* **Export**: If you need to share a conversation transcript with a coworker, type `/export session_123.md`.

### 6.3 Viewing & Interacting with Subagents
Subagents do not spam the main chat window, but they are fully inspectable:
* **The Subagent Pane**: Pressing `Ctrl+S` (or typing `/subagents`) slides open the Subagent Drawer. This shows a list of all active background agents (`[sub-research-1]`, `[sub-build-2]`).
* **Deep Dive**: Selecting a subagent from the list swaps the main TUI view into that specific subagent's isolated context. You can read its exact chain-of-thought and tool logs.
* **Direct Intervention**: If a subagent is stuck, you don't have to wait for it to fail. You can type `/msg sub-build-2 "Stop using rustc directly, use cargo build"` to inject a message straight into its event loop.

### 6.4 Queue & Steer Modes
* **Queue**: Because input is non-blocking, if Turya is busy writing a 500-line file, you can type *"Also make sure to update the README"* and hit Enter. The prompt is **queued** and appended to the agent's context for its next turn.
* **Steer (Main Agent)**: If the Main Agent goes down the wrong path, press `Ctrl+C` to pause generation, which immediately triggers the Steer prompt to redirect it without losing the session context.
* **Image Rendering**: Using the `Kitty Graphics Protocol` and `Sixel`, Turya can render image previews directly in the terminal.
* **Multimodal Prompts**: If you drag and drop an image into the terminal, or type `/attach design.png`, Turya displays a thumbnail of the image inline before sending it to the Vision model.

### 6.5 Native Copy/Paste & Clipboard
* The input box utilizes the `arboard` (or `copypasta`) crate to natively hook into the OS clipboard.
* `Ctrl+V` (or `Cmd+V` on macOS) pastes multi-line code blocks seamlessly.
* `Ctrl+Shift+C` allows you to copy the agent's exact Markdown response directly to your clipboard without dealing with messy terminal text-selection artifacts.

### 6.6 Theming & UI Extensions (Via Plugins)
True to the Turya microkernel philosophy, themes are **not** hardcoded into the Rust binary. Theming and UI layouts are injected via the Plugin System.
* **Theme Plugins**: A Wasm plugin can register a new color palette dynamically by emitting a `RegisterTheme` event to the TUI (e.g., a community `turya-theme-catppuccin.wasm` plugin).
* **Widget Injection**: Beyond just colors, UI Plugins can inject custom widgets into the status bar or spawn new floating panes using the Extism shared memory bridge.
