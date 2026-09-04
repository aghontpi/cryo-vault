---
name: Cryo Vault Auto-Capture
description: Standing guidance for archiving coding-agent conversations through the native local collector or, when needed, an explicit MCP/CLI import.
version: 0.3.0
---

# Cryo Vault — Auto-Capture

Cryo Vault can collect local coding-agent transcripts at the default 23:00
schedule. The collector supports Codex, Claude Code, GitHub Copilot CLI,
Cursor, Gemini CLI, and Google Antigravity. Codex is scanner-only. Claude Code,
Cursor, Gemini CLI, Copilot CLI, and Antigravity may use marked lifecycle hooks;
those hooks only enqueue a lightweight hint and never parse or write transcript
content.

## Choose one archival path

When native capture is installed and can discover the current client's
transcript, let it own the archive. Do not also call MCP `add_log` or `cryo add`
for the same conversation: that creates a duplicate path.

Use an explicit archive only when the client has no discoverable local
transcript, native capture is not configured, or the user explicitly requests
an immediate import. In that case, use the first available option:

1. Call the `add_log` tool on the `cryo-vault` MCP server.
2. If MCP is unavailable, pipe the session JSON to `cryo add -`.

The MCP tool description and `cryo --help` are authoritative for the input
schema. The [store-conversations skill](../store-conversations/SKILL.md) and
[capture runbook](../../README.md#automatic-capture-runbook) describe the
operational alternatives.

## Lifecycle hooks

If a platform hook is installed, keep it non-blocking. It should enqueue a
hint, for example:

```bash
cryo capture hint --platform <platform> --stdin
```

The hint may contain a source session ID and/or concrete transcript path. The
scheduled collector performs discovery, stability checks, parsing,
deduplication, and database writes under the database lock. Hints are durable
independent JSON records under `capture-hints/`.

## Manual archive payload

For an explicit MCP or CLI archive, always include a useful title:

```json
{
  "title": "JWT auth refresh flow",
  "source": "cursor",
  "messages": [
    { "role": "user", "content": "..." },
    { "role": "model", "content": "..." }
  ]
}
```

Titles should be 3–7 words, sentence-case or lowercase, without trailing
punctuation. Never use `Untitled`, `Chat`, `Conversation`, `New chat`, or an
empty string. Include one message per visible turn in order. Roles may be
`user`, `model`, `system`, `thought`, or `tool`; omit hidden reasoning and
internal orchestration noise that has no value to a future reader.

When known, include `source` and `model`. These fields and capture provenance
make later searches and audits useful.

## Local-only and lifecycle guarantees

The collector reads supported files from the local user profile and writes the
configured local database. It does not upload transcripts. Diagnostics expose
paths, classifications, counters, and extraction metadata, never transcript
message text.

The collector requires two identical file observations unless a valid hint
provides a concrete path. A resumed transcript keeps its stable logical ID and
is stored as a new revision; reads expose the newest revision only. Content
fingerprints prevent duplicate imports when a source ID is unavailable or a
transcript is moved.

Users can opt out during installation with `--no-capture` / `-NoCapture`, or
remove the schedule and marked hooks with `cryo capture uninstall`. Uninstall
retains the archive and unrelated client configuration.
