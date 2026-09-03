# Repair Antigravity System-Generated Transcript Capture

## Problem statement

`cryo capture run --platform antigravity --verbose` discovers the intended
`.system_generated/logs/transcript.jsonl` files, but most are reported as
`empty (unsupported source record)` and a small number as `failed (malformed
transcript)`.

The scanner is working; the parser is not matched to the real Antigravity
schema. The existing Antigravity fixture is a generic role/content JSONL
sample, while actual transcripts use event records such as:

```json
{"source":"USER_EXPLICIT","type":"USER_INPUT","content":"..."}
{"source":"MODEL","type":"PLANNER_RESPONSE","content":"...","tool_calls":[...]}
{"source":"MODEL","type":"RUN_COMMAND","content":"..."}
```

The generic JSONL parser only recognizes a normalized `role` field or a small
set of generic `type` values. It therefore extracts no messages from valid
Antigravity event records and labels them unsupported. Some live transcripts
also contain an interrupted/truncated physical line; e.g. one observed line
starts in the middle of a task-log path. Because the parser has already
extracted zero messages, that single invalid line escalates the file to a
malformed-transcript failure instead of preserving its valid records.

## Desired outcome

Capture real Antigravity conversations from all currently discovered roots,
archive only visible and useful turns, preserve valid conversation data when
individual JSONL records are damaged, and give diagnostics that distinguish an
empty system-only transcript from an unsupported schema or a wholly malformed
file.

## Implementation plan

1. Add an Antigravity-specific parser in `src/capture.rs` and dispatch
   `Platform::Antigravity` to it from `parse_transcript` before the generic
   JSON/document fallback.
   - Read the file as JSONL and retain the existing per-line recovery model:
     count invalid non-blank records, but continue processing valid records.
   - Map `source: USER_EXPLICIT` plus `type: USER_INPUT` to a user message.
   - Map visible `source: MODEL` planner/model response records to model
     messages. Extract their `content`, omitting empty messages; preserve
     tool-call names/arguments using the existing `ToolCall` representation
     when present, rather than serializing private/internal fields.
   - Map useful model execution/result records (for example
     `LIST_DIRECTORY`, `VIEW_FILE`, `RUN_COMMAND`, `GENERIC`, and task-status
     events) to tool messages when they have visible content. Keep their
     original event type in message metadata so future parsers can evolve
     without changing stored text.
   - Deliberately ignore `SYSTEM` configuration/checkpoint/history records and
     model records with no visible content or tool call. This avoids archiving
     internal instructions, settings changes, and empty orchestration noise.
   - Derive the stable source session ID from the `brain/<UUID>` component of
     the transcript path when the file does not supply a conversation ID;
     otherwise retain the current path fallback. Continue using normal
     capture metadata and visible-content deduplication.
   - Use the recorded timestamps for the session creation time when available,
     falling back to the existing session defaults. Capture a model identifier
     only if an explicit, non-sensitive field is present.

2. Define explicit corruption and classification rules in the capture
   pipeline.
   - A transcript with at least one recognized visible Antigravity record must
     import even if other lines are invalid; expose the invalid-line count in
     existing extraction metrics and do not emit the high-level `failed`
     outcome.
   - A transcript with no recognized visible records and one or more invalid
     lines remains `failed (malformed transcript)`.
   - A syntactically valid transcript containing only excluded system/control
     records becomes an empty transcript with a precise reason such as `no
     visible conversation records`, rather than `unsupported source record`.
   - Keep `unsupported source record` for valid data that does not match any
     supported Antigravity event shape. Update report accounting only as
     needed to make these categories mutually understandable in text and JSON
     output.

3. Replace the synthetic Antigravity fixture with sanitized, schema-faithful
   event records in `tests/fixtures/capture/`, and add focused parser tests in
   `src/capture.rs`.
   - Cover a user input, a model planner response, a model tool/result event,
     a system checkpoint that must be excluded, and a planner record with only
     a tool call.
   - Assert message roles, ordering, visible text, tool-call preservation,
     source session ID/path-derived identity, creation time, and the absence
     of checkpoint/internal text.
   - Add a mixed-validity fixture with a deliberately truncated line between
     valid events. Assert valid messages import and
     `malformed_records` is nonzero.
   - Add negative fixtures/tests for system-only valid JSONL and all-invalid
     JSONL, verifying the two diagnostic outcomes above.

4. Add an end-to-end capture regression using the real discovery layout
   (`brain/<id>/.system_generated/logs/transcript.jsonl`). Run the stable
   two-observation flow against a temporary database, then assert the session
   is archived once, is unchanged on the next pass, and reports Antigravity
   extraction metrics. Retain the existing exclusion test for
   `transcript_full.jsonl`, history, cache, settings, and databases.

5. Update `README.md` capture documentation to describe support for
   Antigravity's system-generated event transcript format and partial-record
   recovery. Document that a `no visible conversation records` result is
   expected for system-only files, while a malformed result means no usable
   records could be recovered.

6. Verify the repair.
   - Run `cargo fmt --check`, `cargo test`, and `cargo clippy --all-targets
     --all-features -- -D warnings`.
   - Run the targeted Antigravity parser and capture tests while iterating.
   - Run `cryo capture run --platform antigravity --dry-run --verbose --json`
     against the local profile and confirm that representative valid
     transcripts report visible-message metrics, mixed-validity files no
     longer fail, and system-only files receive the new explicit reason.

## Scope and non-goals

- This changes local parsing/reporting only; it does not alter hook installation
  or modify any Antigravity transcript.
- Existing legacy Antigravity discovery roots remain supported.
- The repair will not attempt to reconstruct a missing/truncated JSON record;
  it preserves the valid records surrounding it and makes the loss observable
  through extraction metrics.
