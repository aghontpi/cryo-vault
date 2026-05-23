---
name: Cryo Vault CLI Interaction & Log Ingestion
description: Use this skill when asked to add/ingest new logs, flush or compact the database, or when asked to check history, search past chat logs, or retrieve previous conversations from the Cryo Vault database.
version: 0.2.0
---

# Cryo Vault CLI (v0.2.0)

This skill allows you to interact with the Cryo Vault database using its native command-line interface. You can ingest logs, search conversations, compact storage, and view statistics directly from the terminal.

v0.2.0 adds two new pieces of surface area and upgrades the existing ones — see [Notes for v0.2.0](#notes-for-v020) at the bottom for what changed and how it affects day-to-day use.

## Setup

The recommended path is the installer (drops the `cryo` command on `PATH` and removes any older version it finds):

```bash
./install.sh          # macOS / Linux
./install.ps1         # Windows (PowerShell)
```

Or build from source:

```bash
cargo build --release
```

- CLI binary: `target/release/cryo-vault`
- MCP server binary: `target/release/cryo-vault-mcp`

**Alias for convenience (when not using the installer):**
```bash
alias cryo="./target/release/cryo-vault"
```

After install you can invoke `cryo` directly from any terminal.

## Environment Variables

- `CRYO_DB_PATH`: Path to the database directory. Defaults to `~/.cryo`.
- `RUST_LOG`: Control logging verbosity (`error`, `warn`, `info`, `debug`, `trace`). Default is `warn`.

## Commands

### 1. Ingest data (`add`)

Ingest chat logs from a file or standard input.

```bash
cryo add [OPTIONS] [FILE]
```

**Arguments**
- `FILE`: Path to the input file. Use `-` for stdin (default).

**Options**
- `--stream`: Treat input as streaming output (one JSON object per line).

**Supported formats**
- Single session: a `ChatSessionInput` JSON object.
- List of sessions: an array `[ ... ]` of `ChatSessionInput` objects.
- ChatGPT export: JSON exported from ChatGPT (list of conversations).

```bash
# Import a single file
cryo add my_chat_logs.json

# Import from stdin
cat logs.json | cryo add -

# Import streaming newline-delimited JSON
tail -f live_logs.jsonl | cryo add --stream -
```

**Always set `title` when ingesting.** The field is optional in the wire
format for backwards compatibility, but `cryo last` / `cryo first` /
`cryo search` print it as the human label for each session. Omitting it
fills the archive with `Untitled` entries that no one can browse.

When *you* (the model) ingest a session — whether via `cryo add` or the
MCP `add_log` tool — include a 3–7 word summary in `title`:

- Specific enough to find again with `cryo search`.
- A *summary of what the session is about*, not a verbatim copy of the
  first user message.
- Sentence-case or lowercase, no trailing punctuation.
- Good: `JWT auth refresh flow`, `Debug Nginx streaming proxy`,
  `Migrate Postgres to RDS`.
- Bad: `Untitled`, `Chat`, `Conversation`, `New chat`, `""`,
  or pasting the user's literal first message.

If the content genuinely resists a summary (a one-line lookup, a single
test message), use a short topical phrase like `Quick lookup` or
`One-off question` — never a placeholder.

### 2. Flush the WAL (`flush`)

Manually flush completed sessions from the Write-Ahead Log (`pending.bin`) into the active data segment.

```bash
cryo flush
```

In v0.2.0 this no longer writes loose single sessions — pending sessions are packed into a single highly compressed `StoredSession::Block` per flush, so you get dense storage even without running `optimise`.

### 3. Search (`search`)

Search the archive for conversations matching a query.

```bash
cryo search [OPTIONS] <QUERY>
```

**Arguments**
- `QUERY`: The search term (regex supported).

**Options**
- `--after <DATE>`: Filter by date (YYYY-MM-DD or Unix timestamp).
- `--before <DATE>`: Filter by date (YYYY-MM-DD or Unix timestamp).
- `--json`: Output results as raw JSON.

```bash
# Simple search
cryo search "rust optimization"

# Date range
cryo search "database" --after 2023-01-01 --before 2024-01-01

# JSON for downstream processing
cryo search "error" --json | jq .
```

`search` automatically reads every segment on disk (v0.2.0 fix) and transparently handles V1 single-session, new WAL Block, and legacy V2 compacted block formats. Time filters apply per session, not per block — sessions outside `--after`/`--before` are excluded even when they share a block with matches.

### 4. View session (`show`)

Show full details of a specific session by ID.

```bash
cryo show <SESSION_ID>
```

```bash
cryo show 550e8400-e29b-41d4-a716-446655440000
```

Auto-detects and extracts the session from any block format on disk (V1, Block, or legacy V2).

### 5. Browse sessions

```bash
cryo last [COUNT]      # newest N sessions (default 10)
cryo first [COUNT]     # oldest N sessions (default 10)
```

```bash
cryo last 5
```

### 6. Statistics (`stats`)

```bash
cryo stats
```

Reports total sessions, messages, disk usage, and time range. v0.2.0 fixes the regression where `stats` was counting blocks instead of sessions — multi-session blocks are now expanded to real session counts across all segments.

### 7. Compaction (`optimise`)

Compact loose sessions into dense, highly compressed blocks. Use this after a large import (e.g. a ChatGPT export) or periodically to keep storage tight and search fast.

```bash
cryo optimise [OPTIONS]
```

**Options**

| Option | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `--chunk-kb` | `usize` | `256` | Target compressed block size in KB. |
| `--yes` | flag | – | Skip the interactive confirmation prompt. |

```bash
# Default ~256 KB blocks
cryo optimise

# Larger blocks (fewer files, slightly slower random access)
cryo optimise --chunk-kb 512

# Non-interactive (cron / CI)
cryo optimise --yes
```

v0.2.0 fixes three things here: blocks now actually hit `--chunk-kb` (the trial pass uses zstd level 19 like the real write), per-block cost drops from O(N²) to O(N) via a cheap raw-size pre-gate, and pending WAL entries are drained first so recent sessions are included.

### 8. Maintenance (`reindex`)

Rebuild the search index from the raw data files. Useful if the index becomes corrupted, you manually moved data files, or `stats` previously showed wrong counts on a v0.1.0 archive.

```bash
cryo reindex
```

v0.2.0: `reindex` now flushes pending into the archive under the same `CryoLock` used by `add` / `flush` / `optimise`, so a concurrent `cryo add` from another shell can't corrupt anything.

---

## Notes for v0.2.0

What changed that callers of this skill should know:

- **`flush`** packs into a dense `StoredSession::Block` rather than writing loose sessions. No flag needed — this is the new default.
- **`optimise`** is a new top-level command (compaction). Suggest it after bulk imports or when `stats` shows many small loose sessions.
- **All read paths** (`search`, `show`, `last`, `first`, `stats`, `reindex`) are now multi-segment safe. On v0.1.0 archives larger than 1 GB they were silently ignoring `data_002.cryo` and later — if a user reports "I imported X but can't find it", run `cryo reindex` once on a v0.1.0 archive.
- **`stats`** now reports real session counts, not block counts. If a user upgraded from v0.1.0 and `stats` looks wrong (e.g. `Sessions: 5` for an obviously larger archive), run `cryo reindex`.
- **Concurrency**: CLI and MCP server now share the same `CryoLock` (5 s timeout). A `cryo add` from a shell while the MCP server is writing is safe in v0.2.0.
- **Backwards compatibility**: No schema change. Existing `.cryo` files from v0.1.0 keep working — all three formats (V1 single-session, Block, legacy V2) are read transparently.

## Quick decision guide

- User says "import this export" → `cryo add <file>`, then suggest `cryo optimise --yes` if the file is large (>1000 sessions).
- User says "find conversations about X" → `cryo search "X"` (add `--after` / `--before` if they give a timeframe).
- User says "show me that session" → `cryo show <id>`.
- User says "what's in my database" / "stats" → `cryo stats`.
- User says "the database is slow" or "shrink the database" → `cryo optimise`.
- User says "search isn't finding something I know is there" on a large or v0.1.0-era archive → `cryo reindex`.
