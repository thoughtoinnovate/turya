# Turya: The Tools Arsenal & Code Exploration Strategy

To effectively explore and modify massive codebases, an agent needs the right tools. A common mistake in modern agent frameworks is relying heavily on chunk-based Vector databases (RAG) for everything. 

Turya adopts a **Deterministic-First** approach: AST (Abstract Syntax Tree) and Lexical search are vastly superior to probabilistic RAG for writing code.

---

## 1. The Core Arsenal (Lexical Primitives)

These tools are built directly into `turya-tools` and are used for 90% of daily tasks. They are blisteringly fast.

| Tool Name | Engine | Description |
| :--- | :--- | :--- |
| `file_tree` | `ignore` crate | Returns a `.gitignore`-aware hierarchical tree of the project. |
| `grep_search` | `ripgrep` engine | High-speed regex search across the workspace. Far more accurate than RAG for finding specific variable names or error codes. |
| `view_file` | `memmap2` | Memory-mapped file reader. Supports `start_line` and `end_line` for slicing massive files without blowing up context limits. |
| `edit_file` | Unified Diff | Applies targeted diffs rather than rewriting entire files. |
| `run_bash` | `std::process` | Executes shell commands (e.g., `cargo test`, `npm run lint`) inside a PTY. |

---

## 2. The Semantic Arsenal (LSP & AST)

Why guess with Vector math when the compiler knows exactly where the code is? Turya leverages its `turya-lsp` bridge as active exploration tools.

| Tool Name | Protocol | Description |
| :--- | :--- | :--- |
| `goto_definition` | LSP | Ask the LSP where a struct, function, or type is defined, returning the exact file and line number. |
| `find_references` | LSP | Ask the LSP where a specific function is used across the entire monorepo. |
| `document_symbol` | LSP | Returns all the classes, methods, and traits inside a specific file as a structured outline. |

*Best Practice*: When Turya needs to understand how `PermissionBroker` works, it shouldn't "search the web" or "query RAG". It should call `goto_definition("PermissionBroker")`, instantly landing on the exact struct definition.

---

## 3. The Interaction Arsenal (Human-in-the-Loop)

Modern agents do not blindly hallucinate when requirements are ambiguous. They pause and ask.

Because **every tool is a plugin**, the internal `turya-plugin-interaction` registers the following tools to the event bus:

| Tool Name | Engine | Description |
| :--- | :--- | :--- |
| `ask_question` | TUI Modal | Pauses the agentic loop and renders an interactive TUI modal to the user. Supports multiple-choice options (e.g., `options: ["JWT", "Session", "OAuth"]`) or free-text write-ins to clarify underspecified tasks. |
| `request_feedback` | TUI Diff | Presents a generated plan or code diff to the user with a "Proceed / Edit / Reject" prompt before executing. |

---

## 4. Do We Need Vector Databases or RAG?

**The short answer: Not by default, but yes for Monorepos.**

### The Problem with RAG in Coding
Standard RAG pipelines chunk files into 500-token blocks and store them in a Vector DB. If an agent searches for "authentication logic", RAG might return chunk #4 of `auth.rs`. The agent loses the imports at the top of the file and the helper functions at the bottom, leading to hallucinated or broken code edits.

### The Turya Solution: `turya-plugin-rag`
To keep the core system extremely lightweight (running on old machines), Turya does **not** force a Vector DB on you. 

Instead, it is handled via the plugin system:
* If you are working in a giant enterprise codebase (10,000+ files) where you need to ask vague conceptual questions (*"Where is the logic that calculates tax for European customers?"*), you can load the `turya-plugin-rag`.
* This plugin uses `sqlite-vec` (a lightweight vector extension for our existing SQLite `turya.db`) and a fast local embedding model (like `all-MiniLM-L6-v2` via ONNX).
* It provides the `semantic_search` tool to the agent.

**Summary**: Turya relies on **Ripgrep + LSP** for absolute precision, and offers **Local SQLite RAG** as an optional plugin for massive conceptual exploration.
