#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_dir=$(cd -- "$script_dir/.." && pwd)
diagram_dir="$repo_dir/docs/diagrams"
tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/cryo-docs-validation.XXXXXX")
trap 'rm -rf "$tmp_dir"' EXIT

die() {
    echo "docs validation: $*" >&2
    exit 1
}

echo "[1/6] checking embedded diagram source coverage"
generator="${PLANTUML_GENERATOR:-${HOME}/.codex/skills/plantuml-generator/scripts/generate.py}"
[[ -f "$generator" ]] || die "PlantUML generator is not installed at $generator"

echo "[2/6] extracting and regenerating diagrams"
for png in "$diagram_dir"/*.png; do
    base=$(basename "$png" .png)
    source="$tmp_dir/$base.puml"
    python3 "$generator" extract "$png" -o "$source" >/dev/null
    [[ -s "$source" ]] || die "missing embedded PlantUML source for ${png##*/}"
    rg -q '^@startuml' "$source" || die "invalid embedded PlantUML source for ${png##*/}"
    PLANTUML_JAR_PATH="${PLANTUML_JAR_PATH:-${HOME}/.codex/skills/plantuml-generator/resources/plantuml.jar}" \
        python3 "$generator" --refresh-days 0 generate "$source" "$tmp_dir/$base.png" >/dev/null
    [[ -s "$tmp_dir/$base.png" ]] || die "failed to render $source"
done

echo "[3/6] checking local Markdown links and anchors"
python3 - "$repo_dir" <<'PY'
import re
import sys
from pathlib import Path

root = Path(sys.argv[1])
files = [root / "README.md", root / "docs/architecture.md"]
files += sorted((root / "Skills").glob("**/*.md"))

def slugify(value):
    value = re.sub(r"<[^>]+>", "", value).strip().lower()
    value = re.sub(r"[^\w\s-]", "", value)
    return re.sub(r"\s+", "-", value)

headings = {}
for path in files:
    headings[path] = {
        slugify(match.group(2))
        for match in re.finditer(r"^(#+)\s+(.+?)\s*$", path.read_text(), re.M)
    }

pattern = re.compile(r"!?(?:\[[^]]*\])\(([^)]+)\)")
for source in files:
    for target in pattern.findall(source.read_text()):
        target = target.strip().split()[0].strip("<>")
        if target.startswith(("http://", "https://", "mailto:")):
            continue
        path_part, _, anchor = target.partition("#")
        target_path = (source.parent / path_part).resolve() if path_part else source
        if not target_path.exists():
            raise SystemExit(f"{source}: missing link target {target}")
        if anchor and target_path.suffix.lower() == ".md" and anchor not in headings.get(target_path, set()):
            raise SystemExit(f"{source}: missing anchor {target}")
print(f"checked {len(files)} Markdown files")
PY

echo "[4/6] checking stale documentation claims"
if rg -n "single-file architecture|O\(1\)|serialize_as_v2_block|write_v2_block|unsealed V1|no extra background process|no background process is needed" \
    "$repo_dir/README.md" "$repo_dir/Skills" "$repo_dir/docs"; then
    die "stale documentation claim found"
fi
if rg -n "/Users/|/home/" "$repo_dir/README.md" "$repo_dir/Skills" "$repo_dir/docs"; then
    die "user-specific absolute path found in documentation"
fi

echo "[5/6] running Rust and capture verification"
(cd "$repo_dir" && cargo fmt --check && cargo test)
(cd "$repo_dir" && cargo build)
cryo_bin="$repo_dir/target/debug/cryo-vault"
"$cryo_bin" --help >/dev/null
"$cryo_bin" capture run --help >/dev/null
"$cryo_bin" --db "$tmp_dir/db" capture run --dry-run --json >/dev/null
"$cryo_bin" --db "$tmp_dir/db" capture status --json >/dev/null
CRYO_BIN="$cryo_bin" "$repo_dir/scripts/capture-smoke-test.sh"

echo "[6/6] documentation validation passed"
