# 🚀 Turya: Kickstart Guide

Welcome to the **Turya** project. All architectural blueprints, UX designs, and implementation steps have already been generated and are located in the `.plans/` directory.

This file is the trigger. 

---

## 🤖 For AI Agents (How to trigger execution)

If you are an AI assistant (like Claude, Cursor, or an autonomous agent) and the user has pointed you to this file, **execute the following directives immediately**:

1. Read `.plans/EXECUTION_TRACKER.md` to determine the current state of the project.
2. Find the very first unchecked `[ ]` task in the tracker.
3. Read the corresponding step in `.plans/spoonfeed_implementation_plan.md` to get the exact Rust code and instructions.
4. If the tracker mandates "Agent Pre-Flight Research", use your web search tools to research modern best practices for that specific component before writing any code.
5. Generate the code, run the verification commands (e.g., `cargo check`), and check off `[x]` the box in the tracker.
6. Ask the user for permission to proceed to the next step.

**To the User:** *If you want the agent to start building right now, just copy and paste the prompt below into the chat:*

> **"Read `KICKSTART.md` and begin executing Phase 1 from the Execution Tracker."**

---

## 🧑‍💻 For Human Developers

If you are a human developer looking to understand or build this project manually:

1. **Read the Docs:** Start with [`.plans/README.md`](./.plans/README.md) for a complete index of the architecture, UX, and extensibility guides.
2. **Track Progress:** Open [`.plans/EXECUTION_TRACKER.md`](./.plans/EXECUTION_TRACKER.md). This is your source of truth.
3. **Write Code:** Follow the exact, file-by-file instructions in [`.plans/spoonfeed_implementation_plan.md`](./.plans/spoonfeed_implementation_plan.md).

---
*Turya: The fast, lightweight, self-improving microkernel agent harness.*
