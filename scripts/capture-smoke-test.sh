#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_dir=$(cd -- "$script_dir/.." && pwd)
if [[ -n "${CRYO_BIN:-}" ]]; then
    cryo_bin=${CRYO_BIN:-}
elif [[ -x "$repo_dir/target/debug/cryo-vault" ]]; then
    cryo_bin=$repo_dir/target/debug/cryo-vault
else
    cryo_bin=$repo_dir/target/release/cryo-vault
fi

if [[ ! -x "$cryo_bin" ]]; then
    echo "capture smoke test: built cryo-vault binary not found at $cryo_bin" >&2
    echo "build it first or set CRYO_BIN to an existing binary" >&2
    exit 1
fi

temp_dir=$(mktemp -d "${TMPDIR:-/tmp}/cryo-capture-smoke.XXXXXX")
trap 'rm -rf "$temp_dir"' EXIT
mkdir -p "$temp_dir/import" "$temp_dir/db"

printf '%s\n' '{"session_id":"capture-smoke","messages":[{"role":"user","content":"capture smoke marker"},{"role":"assistant","content":"captured"}]}' > "$temp_dir/import/session.json"

CRYO_CAPTURE_IMPORT_ROOTS="$temp_dir/import" \
    "$cryo_bin" --db "$temp_dir/db" capture run \
    --platform generic --settle 0s

if ! "$cryo_bin" --db "$temp_dir/db" search "capture smoke marker" | grep -Fq "capture-generic-"; then
    echo "capture smoke test: imported session was not searchable" >&2
    exit 1
fi

echo "capture smoke test: passed"
