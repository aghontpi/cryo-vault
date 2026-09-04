<div align="center">

# Cryo Vault

Local, searchable storage for conversations from people and coding agents.

<p>
  <a href="https://github.com/aghontpi/cryo-vault/releases"><img src="https://img.shields.io/github/v/release/aghontpi/cryo-vault?style=flat-square&label=release" alt="release"></a>
  <a href="https://github.com/aghontpi/cryo-vault/blob/main/LICENSE"><img src="https://img.shields.io/github/license/aghontpi/cryo-vault?style=flat-square" alt="license"></a>
</p>

</div>

Cryo Vault is a compact Rust CLI and MCP server for archiving text
conversations. The current release also discovers local coding-agent
transcripts through a default-on nightly collector, while keeping lifecycle
hooks non-blocking and all data local.

## Features

- Automatic local capture with provenance for Codex, Claude Code, GitHub
  Copilot CLI, Cursor, Gemini CLI, and Google Antigravity.
- Explicit JSON, JSONL, and ChatGPT-export imports through `cryo add`.
- Compressed segment storage with Bloom-filter and time-range indexes.
- Resumable capture sessions, content deduplication, and latest-revision reads.
- Native CLI and MCP interfaces with no cloud service or account required.
- Cross-platform binaries for macOS, Linux, and Windows.

Current limitations: the archive is text-oriented, and writes are serialized by
the database lock.

### Efficiency and portability

Cryo Vault is designed for compact, local conversation storage:

- Sessions use a bincode representation with Zstd level-19 compression.
- `flush` and `optimise` use block-oriented storage to reduce per-session
  framing and index overhead.
- Indexes retain time ranges and Bloom filters, allowing `search` to skip
  blocks before exact session matching.
- Readers transparently support `StoredSession::{V1, Block, V2}` across old
  and current archive segments, without an operator migration step.
- The CLI and MCP server are native binaries for macOS, Linux, and Windows.

Search and ID lookup scan index entries before reading selected data blocks;
they are efficient indexed reads, not constant-time lookups. Exact runtime and
memory usage depend on the archive, query, and host.

## Getting started

### Install a release

Use the [published GitHub release](https://github.com/aghontpi/cryo-vault/releases)
or build from source. The installer places versioned binaries under
`~/.cryo-vault`, exposes `cryo` and `cryo-vault-mcp` on `PATH`, and enables the
local 23:00 capture schedule unless opted out.

macOS / Linux:

```bash
curl -fsSL https://raw.githubusercontent.com/aghontpi/cryo-vault/main/install.sh | bash
```

Windows PowerShell:

```powershell
iwr -useb https://raw.githubusercontent.com/aghontpi/cryo-vault/main/install.ps1 | iex
```

The installer also works from a clone:

```bash
./install.sh
./install.ps1
```

Release binaries are built from reviewed `main` commits and published with a
`SHA256SUMS-vX.Y.Z.txt` asset. After downloading a release, verify it with:

```bash
shasum -a 256 -c SHA256SUMS-vX.Y.Z.txt
```

Useful installer options:

| Unix | PowerShell | Purpose |
| --- | --- | --- |
| `--version <vX.Y.Z>` | `-Version <vX.Y.Z>` | Pin a release. |
| `--prefix <path>` | `-Prefix <path>` | Change the install prefix. |
| `--source local\|github` | `-Source local\|github` | Select binary source. |
| `--force` | `-Force` | Reinstall the selected version. |
| `--uninstall` | `-Uninstall` | Remove the install and PATH entry. |
| `--no-path` | `-NoPath` | Skip PATH changes. |
| `--no-capture` | `-NoCapture` | Opt out of the default nightly collector. |

Confirm the installation with `cryo --help`.

### Build from source

```bash
cargo build --release
```

The binaries are `target/release/cryo-vault` and
`target/release/cryo-vault-mcp`. When using a build directly, either invoke the
explicit path or create an alias:

```bash
alias cryo="./target/release/cryo-vault"
```

### First successful capture

After installing, finish a supported coding-agent session, then run:

```bash
cryo capture run --verbose
cryo capture status
cryo last --source claude-code
```

The first observation of a changing transcript is deferred. Run the collector
again after the transcript is unchanged, or use `--settle 1s` for a deliberate
two-observation manual run. The scheduled collector never waits.

## Usage

### Automatic capture runbook

The installer enables a native 23:00 local scheduler. Claude Code, Cursor,
Gemini CLI, GitHub Copilot CLI, and Antigravity can additionally enqueue a
small lifecycle hint; the hook does not parse or write transcript content.
Codex is scanner-only. Generic imports are disabled unless
`CRYO_CAPTURE_IMPORT_ROOTS` is explicitly set.

```bash
cryo capture run                         # scan all supported clients
cryo capture run --platform claude-code  # narrow the scan
cryo capture run --platform generic      # scan configured import roots
cryo capture run --dry-run               # parse and report without writes
cryo capture run --verbose               # show candidate paths and outcomes
cryo capture run --json                  # machine-readable safe diagnostics
cryo capture run --settle 2s              # wait for a second observation
cryo capture status                       # schedule, hooks, state, hints
cryo capture install --time 23:00         # enable scheduler and hooks
cryo capture uninstall                    # remove schedule/hooks, retain data
```

For a static transcript that is not in a native client root:

```bash
export CRYO_CAPTURE_IMPORT_ROOTS="$PWD/my-transcripts"
cryo capture run --platform generic --settle 1s --verbose
cryo search "a phrase from the transcript"
```

The supported platform values are `codex`, `claude-code`, `copilot-cli`,
`cursor`, `gemini-cli`, `antigravity`, and `generic`. Diagnostics include
aggregate counters, platform counts, paths, and safe outcome reasons, never
transcript message text. `cryo show --diagnostics` displays provenance and
extraction metrics; `cryo audit provenance` identifies legacy sessions whose
original source cannot be reconstructed.

Capture is duplicate-safe. A source session ID gives a stable capture ID; when
there is no source ID, a normalized visible-content fingerprint prevents a
moved or re-seen transcript from being imported twice. A resumed transcript
gets a new physical revision under the same logical ID, and readers expose only
the newest revision.

To stop collection, use `cryo capture uninstall`; this does not delete the
archive. To remove archived data, verify the exact path supplied through
`--db` or `CRYO_DB_PATH` before deleting it.

### Manual archival and retrieval

Use the direct CLI or MCP path when you have an explicit file, a client with no
discoverable local transcript, or an immediate import is required. If native
capture is configured and can discover the same transcript, do not also archive
that conversation manually: choose one path to avoid duplicate records.

#### CLI commands

| Command | Purpose | Useful options |
| --- | --- | --- |
| `cryo add [file]` | Import one JSON session, an array, or a ChatGPT export. | Use `-` for stdin and `--stream` for JSONL stream events. |
| `cryo capture run` | Discover and archive supported local transcripts. | `--platform`, `--dry-run`, `--verbose`, `--json`, `--settle`. |
| `cryo capture install` / `uninstall` | Install or remove the local scheduler and marked hooks. | `--time HH:MM`, `--dry-run`. |
| `cryo capture status` | Inspect scheduler, hooks, capture state, and queued hints. | `--json`. |
| `cryo flush` | Finalize completed streaming WAL sessions into the archive. | — |
| `cryo search <query>` | Search visible conversation content. | `--after`, `--before`, `--json`. |
| `cryo first` / `cryo last` | Browse the oldest or newest visible sessions. | `--source <platform>`. |
| `cryo show <id>` | Read a full session. | `--diagnostics` shows provenance without message bodies. |
| `cryo audit provenance` | Find records whose original source cannot be reconstructed. | — |
| `cryo stats` | Report logical counts and physical storage sizes. | — |
| `cryo optimise` | Compact the archive into dense compressed blocks. | `--chunk-kb`, `--yes`. |
| `cryo reindex` | Rebuild indexes from data segments. | `--yes`. |

Run `cryo <command> --help` for the complete option reference.

```bash
cryo add conversation.json       # object, array, or ChatGPT export
cat conversation.json | cryo add -
cryo add --stream events.jsonl   # streaming session events
cryo flush                       # archive finalized WAL sessions
cryo search "database"           # regex-capable content search
cryo search "error" --after 2025-01-01 --json
cryo show <session-id>
cryo show <session-id> --diagnostics
cryo first 10
cryo last 10 --source cursor
cryo stats
cryo optimise --yes
cryo reindex --yes
```

`search` uses index time ranges and Bloom filters to prune blocks before exact
matching. `show` reads the matching data block after resolving the session's
newest revision. `flush` writes finalized streaming sessions as compatible V1
or Block records; `optimise` compacts archive records into dense blocks.
`reindex` rebuilds indexes from data files when needed. See [the architecture
reference](docs/architecture.md) for storage and lookup behavior.

#### JSON input

`cryo add` and MCP `add_log` accept a `ChatSessionInput` object, an array of
objects, or a supported ChatGPT export. The fields are:

| Field | Type | Required | Notes |
| --- | --- | --- | --- |
| `messages` | array | Yes | Defaults to an empty array for compatibility. |
| `id` | string | No | A UUID is generated when omitted. |
| `title` | string | Strongly recommended | Use a specific three-to-seven-word summary. |
| `source` | string | No | For example, `manual-cli` or `claude-code`. |
| `model` | string | No | Originating model identifier. |
| `created_at` | integer | No | Unix timestamp in seconds. |
| additional fields | JSON values | No | Preserved as session metadata. |

Each message requires `role` and `content`. Supported roles are `user`,
`model` (or `assistant`), `system`, `thought`, and `tool`. A message can also
include `id`, `parent_id`, `tool_calls`, `tool_outputs`, and additional
metadata.

A minimal session is:

```json
{
  "title": "JWT auth refresh flow",
  "source": "manual-cli",
  "messages": [
    { "role": "user", "content": "How should token refresh work?" },
    { "role": "model", "content": "Use a short-lived access token..." }
  ]
}
```

`messages` defaults to an empty array for compatibility. A session can also
include `id`, `model`, `created_at`, and additional metadata. Roles are
`user`, `model`, `system`, `thought`, and `tool`. Always provide a specific
3–7-word `title`; do not use `Untitled`, `Chat`, `Conversation`, `New chat`, or
an empty string.

#### End-to-end CLI example

```bash
cat > conversation.json <<'JSON'
{
  "title": "Terminal archive example",
  "source": "manual-cli",
  "model": "example-model",
  "messages": [
    { "role": "user", "content": "Archive this conversation." },
    { "role": "model", "content": "The session is now stored locally." }
  ]
}
JSON

cryo add conversation.json
cryo search "stored locally"
# Copy the returned ID, then inspect the complete record:
cryo show <session-id>
```

### MCP server

`cryo-vault-mcp` exposes the same local archive to MCP-compatible clients. The
installer writes ready-to-paste snippets under `~/.cryo-vault/`:

| File | Top-level key | Use |
| --- | --- | --- |
| `mcp-config.snippet.json` | `mcpServers` | Claude Code, Cursor, Antigravity, Claude Desktop |
| `mcp-config.vscode.snippet.json` | `servers` | VS Code native MCP |

The installer prints the client-specific destination. The essential shape is:

```json
{
  "mcpServers": {
    "cryo-vault": {
      "command": "/absolute/path/to/cryo-vault-mcp",
      "args": [],
      "env": { "CRYO_DB_PATH": "~/.cryo" }
    }
  }
}
```

The MCP `add_log` tool is appropriate for an explicit immediate archive. When
the native collector is enabled for the same client, let the collector own
that transcript instead.

### Agent rule installation

The agent-rules installer writes a marked pointer/instruction block to the
following files and is idempotent:

| Client | Target |
| --- | --- |
| Claude Code | `~/.claude/CLAUDE.md` |
| Antigravity and cross-tool agents | `~/.gemini/AGENTS.md` |
| VS Code Copilot | `./.github/copilot-instructions.md` |

```bash
./install-agent-rules.sh
./install-agent-rules.sh --dry-run
./install-agent-rules.sh --uninstall
```

PowerShell uses the equivalent `-DryRun`, `-Uninstall`, `-SkipClaude`,
`-SkipAgents`, and `-SkipCopilot` flags. The full behavior is defined in the
[auto-capture skill](Skills/auto-capture/SKILL.md), not duplicated in each
client's rule file.

## Architecture

The archive consists of compressed data segments, parallel indexes, a framed
streaming WAL, capture state, and durable hint records. Automatic capture
normalizes eligible local transcripts into sessions with provenance, then
appends data and index records under the database lock. Logical readers resolve
the newest revision while historical physical revisions may remain until
compaction.

![Capture lifecycle](docs/diagrams/capture-lifecycle.png)

![Architecture overview](docs/diagrams/architecture-overview.png)

Read the [canonical architecture document](docs/architecture.md) for component
ownership, supported inputs, storage compatibility, operational lifecycle, and
the embedded editable source for every diagram.

## Validation

Run the repeatable documentation and release gate before publishing changes:

```bash
./scripts/docs-validation.sh
```

It checks embedded diagram source extraction and rendering, local Markdown links and
anchors, stale claims, formatting, tests, the capture smoke test, and CLI help.

## License

This project is licensed under the [GPL-3.0 License](LICENSE).
