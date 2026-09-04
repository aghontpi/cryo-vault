---
name: Cryo Vault CLI Interaction & Log Ingestion
description: Use this skill to import logs, run capture, flush or compact the database, search history, inspect provenance, or retrieve conversations from Cryo Vault.
version: 0.3.0
---

# Cryo Vault CLI

The installer provides the `cryo` command. From a source checkout, build with
`cargo build --release` and use `target/release/cryo-vault`, or alias it as
`cryo`. The database defaults to `~/.cryo`; override it with `--db` or
`CRYO_DB_PATH`. `RUST_LOG` controls diagnostic logging and defaults to `warn`.

## Automatic capture

The platform installer enables a local 23:00 collector unless the user passes
`--no-capture` / `-NoCapture`. Use the [capture runbook](../../README.md#automatic-capture-runbook)
for the scheduler, hooks, platform filters, dry runs, and settle-time behavior.

```bash
cryo capture run
cryo capture run --platform claude-code --verbose
cryo capture run --dry-run --json
cryo capture run --settle 1s
cryo capture status
cryo capture install --time 23:00
cryo capture uninstall
```

Codex is scanner-only. Claude Code, Cursor, Gemini CLI, GitHub Copilot CLI,
and Antigravity can enqueue non-blocking lifecycle hints. Generic transcript
roots are opt-in through `CRYO_CAPTURE_IMPORT_ROOTS`. If native capture can
discover a transcript, do not archive the same conversation manually as well.
Use `cryo add` or MCP `add_log` when a transcript is not discoverable or an
immediate import is explicitly wanted.

## Commands

### Import (`add`)

```bash
cryo add [OPTIONS] [FILE]
cryo add conversation.json
cat conversation.json | cryo add -
cryo add --stream events.jsonl
```

Input may be one `ChatSessionInput` object, an array of sessions, a ChatGPT
export, or newline-delimited streaming events. Always include a specific
3–7-word `title` in explicit ingests. Valid roles are `user`, `model`,
`system`, `thought`, and `tool`.

### Flush (`flush`)

```bash
cryo flush
```

Reconstructs finalized sessions from `pending.bin`, skips exact replays, and
appends a `StoredSession::V1` for one session or `StoredSession::Block` for
multiple sessions. Unfinished WAL events remain pending.

### Search (`search`)

```bash
cryo search "rust optimization"
cryo search "database" --after 2025-01-01 --before 2025-12-31
cryo search "error" --json
```

Search scans all data/index segments, resolves each ID to its newest revision,
uses time ranges and Bloom filters to prune blocks, then verifies matches after
decoding the selected block. It supports regex queries and per-session time
filters.

### Show and browse

```bash
cryo show <session-id>
cryo show <session-id> --diagnostics
cryo first [COUNT]
cryo last [COUNT] --source cursor
```

`show --diagnostics` displays provenance and extraction metrics instead of
message bodies. `first` and `last` expose logical sessions and can filter by
captured source platform.

### Stats, compaction, and maintenance

```bash
cryo stats
cryo optimise --chunk-kb 256 --yes
cryo reindex --yes
cryo audit provenance
```

`stats` counts newest visible sessions and messages, while physical compressed
and uncompressed bytes can include superseded revisions still on disk.
`optimise` rewrites the active archive into dense Zstd level-19 blocks.
`reindex` rebuilds indexes from all data segments and returns the logical
visible count. `audit provenance` finds legacy sessions whose original source
cannot be reconstructed.

## Input shape

```json
{
  "title": "Migrate Postgres to RDS",
  "source": "manual-cli",
  "model": "gpt-5",
  "created_at": 1706123456,
  "messages": [
    { "role": "user", "content": "Plan the migration" },
    { "role": "model", "content": "Start with a rehearsal..." }
  ]
}
```

`id`, `title`, `source`, `model`, and `created_at` are optional for wire
compatibility; `messages` defaults to an empty array. Extra session fields are
stored as metadata. For explicit archives, titles remain strongly recommended
because list and search output uses them as the human-readable label.

## Storage semantics

Data is stored locally as `data_NNN.cryo` segments with parallel
`index_NNN.cryo` files, a framed `pending.bin` WAL, and capture state under the
database directory. Segments rotate at 1 GiB. Archive records use Zstd level
19 and bincode `StoredSession::{V1, Block, V2}` compatibility wrappers.

Writes are append-oriented. If a transcript resumes, the same stable capture ID
receives a new revision; search, show, browse, stats, and reindex expose the
latest revision while older physical records may remain until compaction.
Indexes store IDs, offsets, sizes, message counts, time ranges, and Bloom
filters. They accelerate reads but are rebuildable from data.

For component ownership and the exact capture/storage pipeline, read the
[canonical architecture document](../../docs/architecture.md).
