# Turya SDK: Programmatic Agent Orchestration

While Turya provides a best-in-class CLI and TUI, enterprise developers often need to embed agentic workflows directly into their own applications (e.g., CI/CD pipelines, custom automated code reviewers, or bespoke internal platforms).

To solve this, Turya exposes the **`turya-sdk`**, heavily inspired by the best patterns from the Anthropic Claude SDK and the OpenCode SDK.

---

## 1. Multi-Language Support
The core SDK is written in Rust, but we provide native bindings for:
* **TypeScript / Node.js** (via NAPI-RS)
* **Python** (via PyO3)

---

## 2. Core SDK Capabilities

### 2.1 Programmable Agent Loops
Instead of relying on the TUI's chat loop, developers can orchestrate multi-step "Plan $\rightarrow$ Build $\rightarrow$ Run" loops programmatically.

```typescript
import { TuryaClient } from "@turya/sdk";

const client = new TuryaClient({ provider: "claude-3-5-sonnet" });

// Spawn an isolated headless session
const session = await client.createSession({ workspace: "./my-project" });

// Programmatically steer the agent
const result = await session.execute("Refactor the auth controller to use JWT.");
console.log(result.diff);
```

### 2.2 Built-in & Custom Plugin Injection
The SDK allows you to inject custom tools at runtime without needing to compile them into Wasm, using standard native language features.

```python
from turya_sdk import TuryaSession, tool

@tool(description="Fetches live JIRA ticket status")
def fetch_jira_ticket(ticket_id: str) -> str:
    return requests.get(f"https://jira.com/api/{ticket_id}").text

session = TuryaSession("./project")
session.register_tool(fetch_jira_ticket)

session.execute("Fix the bug mentioned in JIRA-1234")
```

### 2.3 Subagent & Background Task Orchestration
Just like OpenCode's advanced harness, the SDK lets you programmatically spawn background subagents and await their completion.

```typescript
const buildAgent = await session.spawnSubagent({ role: "Builder" });
const testAgent = await session.spawnSubagent({ role: "QA Engineer" });

// Let them work in parallel
await Promise.all([
    buildAgent.execute("Compile the rust binary"),
    testAgent.execute("Write unit tests for the auth module")
]);
```

### 2.4 Event Stream Subscription
Because Turya is built on an asynchronous Event Bus, the SDK allows you to subscribe to the live telemetry of the agent, enabling you to build your own custom UIs easily.

```typescript
session.on("TokenDelta", (chunk) => process.stdout.write(chunk));
session.on("ToolCallInitiated", (tool) => console.log(`Running ${tool.name}...`));
```

---

## 3. How It Differs from Claude Code SDK
* **Model Agnostic**: Like OpenCode, the `turya-sdk` is not locked to Anthropic. You can hot-swap to OpenAI, Gemini, or a local Ollama instance via the `turya-core` Provider trait.
* **True Isolation**: When you create a `TuryaSession` in the SDK, it spins up a unique local socket (the A2A protocol) and an isolated Git Worktree. The SDK client guarantees that the agent cannot corrupt your main branch.
