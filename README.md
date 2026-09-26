# turya

Fast, lightweight, self-improving microkernel agent harness (Rust alternative to Claude Code / OpenCode).

## Quick Install

```bash
# Binary install (Linux/macOS)
curl -fsSL https://raw.githubusercontent.com/thoughtoinnovate/turya/main/install.sh | bash
```

## Installation

### Option A: Install Script (Recommended)

```bash
curl -fsSL https://raw.githubusercontent.com/thoughtoinnovate/turya/main/install.sh | bash
```

The script detects your platform, downloads the matching release binary plus its
`.sha256` checksum, verifies integrity, and installs to `/usr/local/bin`.
It doubles as an updater — re-running it upgrades when a newer release exists:

```bash
./install.sh --check              # report latest vs installed, change nothing
./install.sh --version v0.1.0     # pin a specific version
./install.sh --install-dir ~/.local/bin
```

### Option B: Manual Download

Download from [GitHub Releases](https://github.com/thoughtoinnovate/turya/releases):

| Platform | Binary |
|----------|--------|
| **Linux x64** | `turya-linux-x86_64-musl` |
| **Linux ARM64** | `turya-linux-arm64-musl` |
| **macOS Intel** | `turya-darwin-x86_64` |
| **macOS Apple Silicon** | `turya-darwin-arm64` |
| **Windows x64** | `turya-windows-x86_64.exe` |

```bash
# Example: Linux
curl -L https://github.com/thoughtoinnovate/turya/releases/latest/download/turya-linux-x86_64-musl -o turya
chmod +x turya
sudo mv turya /usr/local/bin/
```

> macOS Gatekeeper: binaries downloaded outside the App Store may be quarantined.
> If macOS refuses to run turya: `xattr -d com.apple.quarantine /usr/local/bin/turya`
> (proper notarization is a future milestone).

### Option C: Build from Source

```bash
cargo install --git https://github.com/thoughtoinnovate/turya.git
# or locally:
make install        # debug binary -> ~/.cargo/bin/turya
make install-release # release binary -> ~/.cargo/bin/turya
```

### Verify Installation

```bash
turya --version
```

## Updating

`turya update` applies the latest patch/minor release within your current major
and refuses to cross a major boundary. `turya upgrade` crosses majors after an
explicit confirmation (breaking changes possible):

```bash
turya update              # patch/minor in-place self-update
turya update --check      # report only, change nothing
turya upgrade             # cross-major (asks for confirmation)
turya upgrade --yes       # cross-major, non-interactive
turya update --version v0.1.4   # explicit pin (may downgrade)
```

Both verify SHA256 checksums, keep a `.bak` backup of the replaced binary, and
re-verify via `turya --version` afterwards.

`turya` is pre-1.0 and makes **no backward-compatibility promise**: every
release may break config, sessions, and internal contracts. State that does not
match the current version is backed up and rebuilt, never migrated.

## Using turya

```bash
turya                       # interactive session
turya sessions              # list stored sessions
turya resume <id>           # continue one (its conversation is replayed)
turya export <id>           # one session as newline-delimited JSON
turya auth login gemini     # store a credential
turya auth status           # what is authenticated, and where it is stored
```

### Providers

| Provider | Credential | Models come from |
|----------|-----------|------------------|
| `gemini` | API key or Google sign-in | the Gemini API |
| `anthropic` | API key | the Anthropic API |
| `ollama` | **none** — it is a server you run | your own daemon, live |

```bash
turya --provider ollama --model qwen3.5:9b
turya auth status            # ollama shows as "not required", never locked
```

A local model needs no API key, so `ollama` works on a machine where nothing
has ever been authenticated. Point it somewhere other than the default with
`OLLAMA_HOST` or `ollama_host` in the config file:

```toml
provider = "ollama"
model = "qwen3.5:9b"
ollama_host = "http://192.168.1.10:11434"
```

`OLLAMA_API_KEY` is optional and only for a reverse proxy or a remote daemon.

**Model detection reads the running server**, not a bundled list, so what you
have pulled is what `/models` shows. Each model's `capabilities` and context
length come from the daemon itself, which means a small model correctly
reports that it cannot call tools. `turya` also pins the context length it
sends, so the window you see is the window in force — Ollama's own default
on a machine with no GPU is 4096, well below what most models advertise.

Local models vary enormously in ability. Anything under a few billion
parameters will stream text and may call a tool, but will not drive a
multi-step task reliably; use a local model for drafts, search and summaries
rather than for agentic work.

### Keys

| Key | Does |
|-----|------|
| `Enter` | Send the prompt |
| `Alt+Enter` | Newline in the prompt |
| `Up` / `Down` | Recall previous prompts (when no popup is open) |
| `@` | Pick a file to attach to the next prompt |
| `PageUp` / `PageDown` | Scroll the transcript |
| Mouse wheel | Scroll, when the terminal supports capture (`/settings mouse`) |
| `Ctrl+E` | Show the full output of the last truncated tool call |
| `Esc` | Close a popup, then stop the running turn. Never quits. |
| `Ctrl+C` / `Ctrl+D` | Quit, from anywhere |

### Slash commands

`/help` `/models` `/auth` `/efforts` `/thinking` `/steps` `/queue` `/compact`
`/context` `/sessions` `/settings` `/skills` `/mcp` `/clear`

| Command | Does |
|---------|------|
| `/models` | Browse providers and switch model |
| `/auth` | Sign in; shows where a credential is stored |
| `/efforts [level\|none]` | Show or set reasoning effort for the current model |
| `/steps [model_calls] [tool_calls]` | Per-turn budgets (default 8 / 32) |
| `/queue [prompt\|clear]` | Queue a follow-up to run after the current turn |
| `/compact [focus]` | Summarise older turns to free context |
| `/context` | Context usage against the model window |
| `/sessions [id]` | List sessions; with an id, resume it |
| `/settings [key value]` | Colour, tints and mouse capture. Saved to the config file |
| `/skills` | Agent Skills found in this project, with the paths |
| `/mcp` | MCP servers, the tools each one added, and why a failed one is missing |

## Extending turya

### Agent Skills

Drop a directory with a `SKILL.md` in `.agents/skills/` (or `.turya/skills/`,
or any path in `skills_paths` in the config file):

```markdown
---
name: release-notes
description: Draft release notes from a range of commits
---

Draft release notes. Resolve relative paths against this file's own directory.
```

Only the name, description, and path are put in front of the model, so a
project with fifty skills does not pay for fifty bodies. The model reads the
`SKILL.md` itself with its normal file tool when a task actually matches, and
`/skills` shows what was found. A malformed file is skipped with a reason
rather than taking the rest of the session's tools away.

### MCP servers

Stdio JSON-RPC servers, configured in the config file:

```toml
[[mcp_servers]]
name = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/srv/data"]
```

`initialize`, `tools/list` and `tools/call` are implemented; HTTP transport,
Tasks and MCP Apps are not, and a server that needs them reports that rather
than failing silently. Discovered tools are callable exactly like the built-in
ones, and every one of them is treated as high risk so the permission broker
asks before it runs — a server can do anything its host can.

### Subagents

The model can hand a self-contained side task to a subagent and get back a
summary, keeping the main context for reasoning about the answer.

**They run in parallel.** Ask for five and you get five at once, not five in a
row — serial delegation costs five turns of latency for what the model could
have done in one. At most four run at once; past that the model is told the
number rather than quietly truncated, because a subagent is a real number of
model calls.

You see one row per delegation plus a live line as each child works, not a
transcript of everything it did. `Ctrl+E` opens the child's own conversation
— its sub-prompt, every tool call, the result — and `Ctrl+E` again folds it
back.

Nesting is capped at one level: a subagent that could spawn subagents could
spawn a fork bomb. `Esc` stops the whole turn, children included.

### Images

An attached image is displayed inline on terminals that can do it — Kitty,
Ghostty, WezTerm, rio, and iTerm2 — and otherwise shown as a line saying why
it is not being displayed. Nothing is emitted when output is redirected, and
`TURYA_IMAGES=off` turns it off deliberately. `NO_COLOR`, or
`/settings no_color on`, removes all colour including tints and dimming.

## Cutting a Release

1. Bump the version in `crates/turya-cli/Cargo.toml` (single source of truth).
2. Commit: `git commit -am "chore(release): vX.Y.Z"`.
3. Tag and push: `git tag -a vX.Y.Z -m "turya vX.Y.Z" && git push origin vX.Y.Z`
   (or Actions → Release → Run workflow → tag input).
4. `release.yml` verifies tag == crate version, builds all 6 platform targets,
   and publishes the GitHub Release with binaries + checksums.
5. Hyphenated tags (`v0.2.0-rc.1`) are published as prereleases; `nightly`
   builds (manual workflow) never affect `turya update` / `install.sh`.

## Development

See [`.plans/README.md`](./.plans/README.md) for architecture docs and
[`.plans/EXECUTION_TRACKER.md`](./.plans/EXECUTION_TRACKER.md) for build progress.

```bash
make build   # debug workspace build
make test    # unit + integration tests
make e2e     # deterministic simulation suite (TURYA_SIM_MODE=1)
```
