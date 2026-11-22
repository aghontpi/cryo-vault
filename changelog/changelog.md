# Changelog

All notable user-facing changes are recorded here. Stable GitHub releases are
cut from reviewed, CI-built artifacts and correspond to a `vX.Y.Z` tag.

## [0.3.0] - 2026-09-04

### Added

- Automatic capture of local coding-agent transcripts for Codex, Claude Code,
  GitHub Copilot CLI, Cursor, Gemini CLI, and Google Antigravity.
- Native hooks and a default-on nightly scheduler to queue and capture eligible
  transcripts without recording transcript content in diagnostics.
- `cryo capture run`, `status`, `install`, `uninstall`, and `hint` commands,
  including platform filters, dry-run mode, verbose output, JSON diagnostics,
  and settle-time support.
- Provenance metadata and `cryo audit provenance` for inspecting archive origin
  and extraction quality.

### Updates

- Capture now uses stable-file gating, source fingerprints, resumable sessions,
  content-based deduplication, and latest-revision semantics.
- The installer enables a 23:00 local capture job by default, with explicit
  `--no-capture` / `-NoCapture` opt-outs.
- Capture support and operational guidance are documented in the README, with
  an end-to-end smoke test for the supported workflow.

### Fixes

- Recovered visible Antigravity event transcripts, including tool context.
- Corrected changed-transcript revision behavior and protected replay
  idempotency across storage and indexed search paths.
- Hardened transcript discovery, state recovery, malformed-input handling, and
  cross-segment search behavior.

### Verification

- `cargo fmt --check`
- `cargo test`
- `./scripts/capture-smoke-test.sh`

**Full Changelog**: https://github.com/aghontpi/cryo-vault/compare/v0.2.0...v0.3.0
