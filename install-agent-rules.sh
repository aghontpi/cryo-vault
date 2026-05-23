#!/usr/bin/env bash
# Cryo Vault — agent-rules installer (macOS / Linux)
#
# Drops a short auto-capture instruction snippet into the rule-file of every
# AI client you use, so that Claude Code, GitHub Copilot, Antigravity (and any
# other agent that reads the cross-tool AGENTS.md convention) will archive
# every finished conversation to Cryo Vault automatically.
#
# Targets:
#   ~/.claude/CLAUDE.md                       — global, Claude Code
#   ~/.gemini/AGENTS.md                       — global, Antigravity + cross-tool
#   ./.github/copilot-instructions.md         — project-scoped, VSCode Copilot
#                                               (run from each repo where you
#                                                want this active; Copilot has
#                                                no clean global rules path)
#
# Idempotent: re-running this script replaces the previously-written snippet
# in place rather than appending duplicates. Uses HTML-comment markers so the
# rest of each file is left untouched.
#
# Usage:
#   ./install-agent-rules.sh                     # install everywhere applicable
#   ./install-agent-rules.sh --uninstall         # remove the snippet from all targets
#   ./install-agent-rules.sh --skip-copilot      # leave .github/copilot-instructions.md alone
#   ./install-agent-rules.sh --skip-claude       # leave ~/.claude/CLAUDE.md alone
#   ./install-agent-rules.sh --skip-agents       # leave ~/.gemini/AGENTS.md alone
#   ./install-agent-rules.sh --dry-run           # print what would change, do nothing

set -euo pipefail

# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)"
SKILL_REL="Skills/auto-capture/SKILL.md"
SKILL_PATH="${SCRIPT_DIR}/${SKILL_REL}"

MARKER_BEGIN="<!-- cryo-vault:auto-capture start -->"
MARKER_END="<!-- cryo-vault:auto-capture end -->"

CLAUDE_RULES="${HOME}/.claude/CLAUDE.md"
GEMINI_RULES="${HOME}/.gemini/AGENTS.md"
COPILOT_RULES="$(pwd)/.github/copilot-instructions.md"

DO_UNINSTALL=0
DO_DRY_RUN=0
SKIP_CLAUDE=0
SKIP_AGENTS=0
SKIP_COPILOT=0

# ---------------------------------------------------------------------------
# Pretty output
# ---------------------------------------------------------------------------

if [ -t 1 ] && command -v tput >/dev/null 2>&1; then
    BOLD="$(tput bold)"; DIM="$(tput dim)"; RED="$(tput setaf 1)"
    GRN="$(tput setaf 2)"; YLW="$(tput setaf 3)"; BLU="$(tput setaf 4)"; RST="$(tput sgr0)"
else
    BOLD=""; DIM=""; RED=""; GRN=""; YLW=""; BLU=""; RST=""
fi

info()  { printf '%s==>%s %s\n' "${BLU}${BOLD}" "${RST}" "$*"; }
ok()    { printf '%s✓%s %s\n'   "${GRN}${BOLD}" "${RST}" "$*"; }
warn()  { printf '%s!%s %s\n'   "${YLW}${BOLD}" "${RST}" "$*"; }
die()   { printf '%sx%s %s\n'   "${RED}${BOLD}" "${RST}" "$*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------

while [ $# -gt 0 ]; do
    case "$1" in
        --uninstall)     DO_UNINSTALL=1 ;;
        --dry-run)       DO_DRY_RUN=1 ;;
        --skip-claude)   SKIP_CLAUDE=1 ;;
        --skip-agents)   SKIP_AGENTS=1 ;;
        --skip-copilot)  SKIP_COPILOT=1 ;;
        -h|--help)
            sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) die "Unknown argument: $1" ;;
    esac
    shift
done

# ---------------------------------------------------------------------------
# Build the snippet block
# ---------------------------------------------------------------------------

[ -f "$SKILL_PATH" ] || die "Canonical skill missing: $SKILL_PATH"

SNIPPET_BODY="$(cat <<'EOF'
## Cryo Vault — auto-archive conversations

At the end of every conversation, archive the session to Cryo Vault so it
becomes searchable later.

1. **Preferred:** call the `add_log` tool on the `cryo-vault` MCP server.
   The tool description carries the full session schema and the title
   rules — follow them. Nothing else to read.

2. **Fallback (only if the MCP server is not connected):** pipe the
   session JSON to `cryo add -`. Do **not** guess the schema — read
   the `store-conversations` skill or `cryo --help` first.

Always include a `title` of 3–7 words that summarises the session.
Never send placeholders like "Untitled", "Chat", "New chat", or "".

See the full guidance in `Skills/auto-capture/SKILL.md` of the
cryo-vault repo.
EOF
)"

SNIPPET_BLOCK="${MARKER_BEGIN}
${SNIPPET_BODY}
${MARKER_END}"

# ---------------------------------------------------------------------------
# File manipulation helpers
# ---------------------------------------------------------------------------

# Strip any previous snippet block from a file. Safe if file doesn't exist or
# has no marker.
strip_block() {
    local target="$1"
    [ -f "$target" ] || return 0
    # awk: skip lines from MARKER_BEGIN through MARKER_END (inclusive).
    awk -v b="$MARKER_BEGIN" -v e="$MARKER_END" '
        $0 == b { in_block=1; next }
        in_block && $0 == e { in_block=0; next }
        !in_block { print }
    ' "$target" > "${target}.cryo.tmp"
    mv "${target}.cryo.tmp" "$target"
}

# Append the snippet block to a file (after stripping any prior one).
write_block() {
    local target="$1"
    local dir
    dir="$(dirname "$target")"

    if [ "$DO_DRY_RUN" -eq 1 ]; then
        info "would write snippet to: ${target}"
        return 0
    fi

    mkdir -p "$dir"
    if [ -f "$target" ]; then
        strip_block "$target"
        # Make sure there's a blank line before our block if the file is
        # non-empty and doesn't already end with one.
        if [ -s "$target" ]; then
            tail -c1 "$target" | od -An -c | grep -q '\\n' || printf '\n' >> "$target"
            tail -n1 "$target" | grep -q '^[[:space:]]*$' || printf '\n' >> "$target"
        fi
    else
        : > "$target"
    fi
    printf '%s\n' "$SNIPPET_BLOCK" >> "$target"
    ok "wrote: ${target}"
}

remove_block() {
    local target="$1"
    if [ ! -f "$target" ]; then
        warn "not present, skipping: ${target}"
        return 0
    fi
    if [ "$DO_DRY_RUN" -eq 1 ]; then
        info "would strip snippet from: ${target}"
        return 0
    fi
    strip_block "$target"
    ok "stripped: ${target}"
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

info "Cryo Vault — agent-rules installer"
[ "$DO_DRY_RUN" -eq 1 ] && warn "dry-run: no files will be modified"

ACTION="write"
[ "$DO_UNINSTALL" -eq 1 ] && ACTION="remove"

apply() {
    if [ "$ACTION" = "write" ]; then
        write_block "$1"
    else
        remove_block "$1"
    fi
}

if [ "$SKIP_CLAUDE" -eq 0 ]; then
    apply "$CLAUDE_RULES"
else
    warn "skipping Claude Code (${CLAUDE_RULES})"
fi

if [ "$SKIP_AGENTS" -eq 0 ]; then
    apply "$GEMINI_RULES"
else
    warn "skipping Antigravity / AGENTS.md (${GEMINI_RULES})"
fi

if [ "$SKIP_COPILOT" -eq 0 ]; then
    apply "$COPILOT_RULES"
else
    warn "skipping VSCode Copilot (${COPILOT_RULES})"
fi

if [ "$ACTION" = "write" ]; then
    ok "done. agents will now auto-archive conversations to Cryo Vault."
    info "tip: open a new chat in your editor; the rule is loaded at session start."
else
    ok "done. snippet removed from all targets."
fi
