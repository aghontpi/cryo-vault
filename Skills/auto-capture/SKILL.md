---
name: Cryo Vault Auto-Capture (end-of-conversation archival)
description: Standing instruction for any AI agent (Claude Code, GitHub Copilot, Antigravity, Cursor, etc.) to automatically archive every finished conversation into the Cryo Vault database. Read this once at the start of a session; act on it when the session ends.
version: 0.1.0
---

# Cryo Vault — Auto-Capture

At the **end of every conversation**, archive the session to Cryo Vault so it
becomes searchable later via `cryo search` or the `cryo-vault` MCP server.

This file is the canonical source of truth. The installer
(`install-agent-rules.sh` / `.ps1`) drops a short pointer to it into the
rule-file of each AI client (`~/.claude/CLAUDE.md`, `~/.gemini/AGENTS.md`,
`.github/copilot-instructions.md`). All clients converge on the rules below.

## How to archive

Use the **first option that is available** to you:

### 1. Preferred — `cryo-vault` MCP server

If the `cryo-vault` MCP server is connected, call its **`add_log`** tool.

The tool's own description carries the full session schema and the title
rules — follow them. You do not need to consult any other doc; everything
you need to construct a valid payload is in the tool's input schema.

Minimal example payload:

```json
{
  "data": {
    "title": "JWT auth refresh flow",
    "messages": [
      { "role": "user",  "content": "..." },
      { "role": "model", "content": "..." }
    ]
  }
}
```

### 2. Fallback — `cryo add` CLI

Only if the MCP server is **not** connected, fall back to the CLI:

```bash
echo '<session-json>' | cryo add -
```

Do **not** guess the schema. Before constructing the JSON, read one of:

- The `store-conversations` skill: `~/.cryo-vault/Skills/store-conversations/SKILL.md`
  (or, in this repo, `Skills/store-conversations/SKILL.md`)
- `cryo --help` and `cryo add --help`
- The "JSON Structure & Parameters" section of the Cryo Vault README

The CLI accepts the same `ChatSessionInput` shape the MCP tool documents.

## Title rules (apply to both paths)

- **Required in spirit, optional in the wire format.** Always send a `title`.
- **3–7 words**, sentence-case or lowercase, no trailing punctuation.
- A **summary** of what the session was about — not a verbatim copy of the
  first user message.
- **Never** send placeholders: `"Untitled"`, `"Chat"`, `"Conversation"`,
  `"New chat"`, or `""`. If the session genuinely resists summary (a one-line
  lookup, a single test message), use a short topical phrase like
  `"Quick lookup"` or `"One-off question"` instead.

Good titles: `JWT auth refresh flow`, `Debug Nginx streaming proxy`,
`Migrate Postgres to RDS`, `Reproducible build metadata removal`.

## What to include in `messages`

- One entry per turn in the conversation, in order.
- `role` is `"user"`, `"model"`, `"system"`, `"thought"`, or `"tool"`.
- `content` is the text of that turn.
- Skip purely internal tool-orchestration noise that has no informational
  value to a future reader; keep tool calls/results that are part of the
  reasoning trail.

## When to archive

- At the end of the conversation, before the user closes the session.
- If the conversation is long-running and naturally pauses (e.g. user says
  "thanks, that's it"), archive at that pause rather than waiting forever.
- Do **not** archive on every turn — one archive per coherent session.

## Source fields (optional but useful)

When you know them, include:

- `source`: which client you're running in — e.g. `"claude-code"`,
  `"copilot-vscode"`, `"antigravity"`, `"cursor"`.
- `model`: the model ID generating the responses — e.g. `"claude-opus-4-7"`,
  `"gemini-2.5-pro"`, `"gpt-5"`.

These make `cryo search` results easier to filter later.
