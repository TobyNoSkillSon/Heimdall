---
name: task-skills
description: The skills and tools of the agents you start through T3, one line each. Use when writing a brief (name the skill the task should load) and before telling the user a task cannot do something.
---

# What your tasks can do

Tasks (Claude Code, Codex, OpenCode or Grok through T3) load these skills by themselves when the work matches. Naming
the skill in the brief makes it certain. Tasks also have web search, the shell, and every MCP server the user
connected. Skills the user installed for those agents themselves are theirs to name; ask a task to list its skills
when you need to know.

## Heimdall's task skills (the Heimdall repository's `skills/shared/`)

- `update-skills`: add or change a skill and copy it into every harness; also keeps this list current.
