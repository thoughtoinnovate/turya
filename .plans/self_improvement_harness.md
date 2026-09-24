# Turya: The Self-Improvement Harness (Recursive Extensibility)

While typical agent frameworks like Claude Code or OpenCode use a plugin system to allow *human developers* to add new tools, Turya's Wasm Plugin system is designed to allow the **agent itself** to grow its capabilities.

Turya acts as a **Self-Improvement Harness**. 

## How it Works

Because the Plugin Host relies on WebAssembly (Extism), plugins are not deeply coupled to the core Rust codebase. They are standalone binaries loaded at runtime. This enables a recursive loop where Turya can write its own upgrades:

### 1. Capability Discovery
Turya is given a task: *"Convert this 5GB CSV into an optimized Parquet file using Apache Arrow."*
The agent realizes its primitive tools (`run_bash`, `edit_file`) are too slow or memory-intensive for this task inside a bash script.

### 2. Plugin Generation (The "Build" Subagent)
Turya decides to forge a new tool. It spawns a `Build` subagent with instructions to write a Rust (or TypeScript) Extism plugin that reads CSV and writes Parquet.
The subagent writes `src/lib.rs`, runs `cargo build --target wasm32-wasi`, and outputs `csv_to_parquet.wasm`.

### 3. Hot-Reloading
Turya dynamically registers the new `.wasm` file with the internal Extism Plugin Host.
```json
// The agent updates its own tool registry mid-turn
{
  "register_plugin": {
    "name": "csv_to_parquet",
    "wasm_path": "/tmp/turya_plugins/csv_to_parquet.wasm",
    "capabilities": ["fs_read", "fs_write"]
  }
}
```

### 4. Execution & Validation
Turya immediately calls its newly minted tool: `<call_tool name="csv_to_parquet">`. 
If it succeeds, Turya can optionally commit this `.wasm` plugin to its permanent `~/.turya/plugins/` directory.

## Why Extism Wasm is Crucial for this Loop
If Turya tried to self-generate Node.js or Python plugins, executing them would be a massive security risk, as the agent could accidentally (or maliciously) write a script that deletes system files. 

By forcing the agent to compile its new tool to WebAssembly, the Turya Core Engine can **sandbox its own creations**. The agent can give its new plugin read access to `./data/input.csv` and write access to `./data/output.parquet`, guaranteeing that even a hallucinated or flawed tool cannot compromise the host system.
