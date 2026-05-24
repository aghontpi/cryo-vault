#!/usr/bin/env bash
# Cryo Vault installer (macOS / Linux)
#
# Installs the `cryo` CLI (and `cryo-vault-mcp` MCP server) into
# ~/.cryo-vault/bin and makes them available on $PATH so that typing
# `cryo` in a fresh terminal Just Works.
#
# Sources, in order:
#   1) Local dist/ folder if this script is run from inside the repo
#      (uses cryo-vault-v<version>-<os>-<arch> and matching MCP binary).
#   2) GitHub releases for the requested version otherwise.
#
# Removes any prior install it finds (older versions, stale symlinks,
# previous PATH entry) before laying down the new one.
#
# Usage:
#   ./install.sh                       # install latest known version
#   ./install.sh --version v0.2.0      # pin a specific version
#   ./install.sh --uninstall           # remove any installed copy
#   ./install.sh --prefix ~/.local     # install under ~/.local/cryo-vault
#   ./install.sh --no-path             # skip editing shell rc
#   ./install.sh --force               # reinstall even if same version present

set -euo pipefail

# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------

DEFAULT_VERSION="v0.2.0"
GITHUB_REPO="aghontpi/cryo-vault"
DEFAULT_PREFIX="${HOME}/.cryo-vault"

VERSION="${DEFAULT_VERSION}"
PREFIX="${DEFAULT_PREFIX}"
DO_UNINSTALL=0
EDIT_PATH=1
FORCE=0
SOURCE_OVERRIDE=""   # empty = auto, "local" or "github" forces

PATH_MARKER_BEGIN="# >>> cryo-vault >>>"
PATH_MARKER_END="# <<< cryo-vault <<<"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)"

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

usage() {
    cat <<EOF
Cryo Vault installer

Usage: install.sh [options]

Options:
  --version <vX.Y.Z>   Version to install (default: ${DEFAULT_VERSION})
  --prefix  <path>     Install prefix (default: ${DEFAULT_PREFIX})
                       Binaries go to <prefix>/bin.
  --source <local|github>
                       Force binary source. Default: local if dist/ found,
                       otherwise github.
  --uninstall          Remove any installed cryo-vault and clean PATH entry.
  --no-path            Don't modify shell rc files (~/.zshrc, ~/.bashrc).
  --force              Reinstall even if the same version is already present.
  -h, --help           Show this help.
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version)    VERSION="${2:?missing value for --version}"; shift 2;;
        --prefix)     PREFIX="${2:?missing value for --prefix}"; shift 2;;
        --source)     SOURCE_OVERRIDE="${2:?missing value for --source}"; shift 2;;
        --uninstall)  DO_UNINSTALL=1; shift;;
        --no-path)    EDIT_PATH=0; shift;;
        --force)      FORCE=1; shift;;
        -h|--help)    usage; exit 0;;
        *)            die "Unknown option: $1 (try --help)";;
    esac
done

# Normalise version: accept "0.2.0" as well as "v0.2.0".
case "$VERSION" in
    v*) ;;
    *)  VERSION="v${VERSION}";;
esac

BIN_DIR="${PREFIX}/bin"
VERSIONS_DIR="${PREFIX}/versions"
INSTALL_DIR="${VERSIONS_DIR}/${VERSION}"

# ---------------------------------------------------------------------------
# Platform detection
# ---------------------------------------------------------------------------

detect_platform() {
    local uname_s uname_m os arch
    uname_s="$(uname -s)"
    uname_m="$(uname -m)"

    case "$uname_s" in
        Darwin)            os="macos"   ;;
        Linux)             os="linux"   ;;
        MINGW*|MSYS*|CYGWIN*)
            die "On Windows, please run install.ps1 from PowerShell instead.";;
        *) die "Unsupported OS: $uname_s";;
    esac

    case "$uname_m" in
        x86_64|amd64)      arch="x64"   ;;
        arm64|aarch64)     arch="arm64" ;;
        *) die "Unsupported architecture: $uname_m";;
    esac

    PLATFORM="${os}-${arch}"
}

# ---------------------------------------------------------------------------
# Source selection: local dist/ vs GitHub releases
# ---------------------------------------------------------------------------

pick_source() {
    local dist_dir="${SCRIPT_DIR}/dist"
    local local_cli="${dist_dir}/cryo-vault-${VERSION}-${PLATFORM}"
    local local_mcp="${dist_dir}/cryo-vault-mcp-${VERSION}-${PLATFORM}"

    case "$SOURCE_OVERRIDE" in
        local)
            [ -f "$local_cli" ] || die "Local source requested but $local_cli not found."
            [ -f "$local_mcp" ] || die "Local source requested but $local_mcp not found."
            SOURCE="local"
            ;;
        github)
            SOURCE="github"
            ;;
        "")
            if [ -f "$local_cli" ] && [ -f "$local_mcp" ]; then
                SOURCE="local"
            else
                SOURCE="github"
            fi
            ;;
        *) die "--source must be 'local' or 'github' (got: $SOURCE_OVERRIDE)";;
    esac

    SRC_CLI="$local_cli"
    SRC_MCP="$local_mcp"
}

# ---------------------------------------------------------------------------
# Detect existing installs (this prefix + stray copies on PATH)
# ---------------------------------------------------------------------------

current_installed_version() {
    # Reads the version marker file if present.
    local marker="${PREFIX}/.version"
    if [ -f "$marker" ]; then
        cat "$marker"
    fi
}

list_stray_cryo_on_path() {
    # Anything called `cryo` on PATH that isn't our managed shim.
    local managed="${BIN_DIR}/cryo"
    local IFS=:
    for d in $PATH; do
        [ -n "$d" ] || continue
        local cand="$d/cryo"
        if [ -x "$cand" ] && [ "$cand" != "$managed" ]; then
            echo "$cand"
        fi
    done
}

# ---------------------------------------------------------------------------
# Uninstall — remove install dir + PATH entry + nothing else.
# ---------------------------------------------------------------------------

remove_path_block_from() {
    local rc="$1"
    [ -f "$rc" ] || return 0
    if grep -qF "$PATH_MARKER_BEGIN" "$rc"; then
        local tmp
        tmp="$(mktemp)"
        awk -v b="$PATH_MARKER_BEGIN" -v e="$PATH_MARKER_END" '
            $0 == b { skip=1; next }
            $0 == e { skip=0; next }
            !skip   { print }
        ' "$rc" > "$tmp"
        mv "$tmp" "$rc"
        ok "Removed PATH block from $rc"
    fi
}

uninstall() {
    info "Uninstalling cryo-vault from ${PREFIX}"

    if [ -d "$PREFIX" ]; then
        rm -rf "$PREFIX"
        ok "Removed $PREFIX"
    else
        warn "No install found at $PREFIX (nothing to remove there)"
    fi

    if [ "$EDIT_PATH" -eq 1 ]; then
        for rc in "${HOME}/.zshrc" "${HOME}/.bashrc" "${HOME}/.bash_profile" "${HOME}/.profile"; do
            remove_path_block_from "$rc"
        done
    fi

    local strays
    strays="$(list_stray_cryo_on_path || true)"
    if [ -n "$strays" ]; then
        warn "Other 'cryo' binaries are still on your PATH (not managed by this installer):"
        printf '    %s\n' $strays
        warn "Remove them manually if you don't want them shadowing future installs."
    fi
}

# ---------------------------------------------------------------------------
# Fetch helpers
# ---------------------------------------------------------------------------

download() {
    local url="$1" out="$2"
    if command -v curl >/dev/null 2>&1; then
        curl --fail --location --progress-bar -o "$out" "$url"
    elif command -v wget >/dev/null 2>&1; then
        wget -q --show-progress -O "$out" "$url"
    else
        die "Need 'curl' or 'wget' to download release assets."
    fi
}

verify_executable() {
    local f="$1"
    [ -f "$f" ] || die "Missing binary after install: $f"
    [ -s "$f" ] || die "Downloaded binary is empty: $f"
    chmod +x "$f"
}

# ---------------------------------------------------------------------------
# Stage binaries into versioned dir
# ---------------------------------------------------------------------------

stage_binaries() {
    info "Staging ${VERSION} binaries into ${INSTALL_DIR}"
    mkdir -p "$INSTALL_DIR"

    local dst_cli="${INSTALL_DIR}/cryo-vault"
    local dst_mcp="${INSTALL_DIR}/cryo-vault-mcp"

    if [ "$SOURCE" = "local" ]; then
        cp "$SRC_CLI" "$dst_cli"
        cp "$SRC_MCP" "$dst_mcp"
        ok "Copied local binaries from dist/"
    else
        local base="https://github.com/${GITHUB_REPO}/releases/download/${VERSION}"
        info "Downloading from GitHub release ${VERSION}"
        download "${base}/cryo-vault-${VERSION}-${PLATFORM}"      "$dst_cli"
        download "${base}/cryo-vault-mcp-${VERSION}-${PLATFORM}"  "$dst_mcp"
    fi

    verify_executable "$dst_cli"
    verify_executable "$dst_mcp"
}

# ---------------------------------------------------------------------------
# Wire up <prefix>/bin to point at the active version
# ---------------------------------------------------------------------------

activate_version() {
    mkdir -p "$BIN_DIR"

    # `cryo` is the friendly name. `cryo-vault` and `cryo-vault-mcp` are also
    # exposed so MCP configs that point at the full name keep working.
    local targets=( "cryo:cryo-vault" "cryo-vault:cryo-vault" "cryo-vault-mcp:cryo-vault-mcp" )

    for spec in "${targets[@]}"; do
        local link_name="${spec%%:*}"
        local real_name="${spec##*:}"
        local link="${BIN_DIR}/${link_name}"
        local real="${INSTALL_DIR}/${real_name}"
        rm -f "$link"
        ln -s "$real" "$link"
    done

    echo "$VERSION" > "${PREFIX}/.version"
    ok "Linked ${BIN_DIR}/cryo -> ${INSTALL_DIR}/cryo-vault"
}

# ---------------------------------------------------------------------------
# Prune old versions under <prefix>/versions
# ---------------------------------------------------------------------------

prune_old_versions() {
    [ -d "$VERSIONS_DIR" ] || return 0
    local removed=0
    for d in "$VERSIONS_DIR"/*; do
        [ -d "$d" ] || continue
        local name
        name="$(basename "$d")"
        if [ "$name" != "$VERSION" ]; then
            rm -rf "$d"
            ok "Removed old version: $name"
            removed=$((removed + 1))
        fi
    done
    if [ "$removed" -eq 0 ]; then
        printf '%s    %s%s\n' "${DIM}" "no older versions to clean" "${RST}"
    fi
}

# ---------------------------------------------------------------------------
# Shell PATH wiring
# ---------------------------------------------------------------------------

shell_rc_files() {
    # Prefer the user's actual shell, but also touch the others if they exist
    # — saves people from "it works in zsh but not bash" surprises.
    local rcs=()
    case "${SHELL:-}" in
        */zsh)  rcs+=("${HOME}/.zshrc");;
        */bash) rcs+=("${HOME}/.bashrc");;
    esac
    for extra in "${HOME}/.zshrc" "${HOME}/.bashrc" "${HOME}/.bash_profile" "${HOME}/.profile"; do
        [ -f "$extra" ] || continue
        local seen=0
        for r in "${rcs[@]}"; do [ "$r" = "$extra" ] && seen=1; done
        [ "$seen" -eq 0 ] && rcs+=("$extra")
    done
    # If nothing exists yet, create ~/.profile as a safe default.
    if [ "${#rcs[@]}" -eq 0 ]; then
        rcs+=("${HOME}/.profile")
        : > "${HOME}/.profile"
    fi
    printf '%s\n' "${rcs[@]}"
}

ensure_path_in_rc() {
    local rc="$1"
    local block
    # Build the block as a multi-line string. We append it via `printf >>` so
    # awk never has to carry the newlines (awk's -v doesn't handle them).
    block="${PATH_MARKER_BEGIN}
# Added by cryo-vault installer. Safe to remove by deleting the lines
# between these markers, or by re-running install.sh --uninstall.
case \":\$PATH:\" in
    *\":${BIN_DIR}:\"*) ;;
    *) export PATH=\"${BIN_DIR}:\$PATH\";;
esac
${PATH_MARKER_END}"

    local refreshed=0
    if [ -f "$rc" ] && grep -qF "$PATH_MARKER_BEGIN" "$rc"; then
        # Strip the existing block in place — the new one is appended below.
        # Doing it in two steps avoids passing a multi-line replacement
        # through `awk -v`, which doesn't accept literal newlines.
        local tmp
        tmp="$(mktemp)"
        awk -v b="$PATH_MARKER_BEGIN" -v e="$PATH_MARKER_END" '
            $0 == b { skip=1; next }
            $0 == e { skip=0; next }
            !skip   { print }
        ' "$rc" > "$tmp"
        # Trim trailing blank lines so repeated --force refreshes don't
        # accumulate a growing run of empty lines above the block.
        awk 'NF { for (i=0; i<blanks; i++) print ""; blanks=0; print; next } { blanks++ }' \
            "$tmp" > "$tmp.2"
        mv "$tmp.2" "$rc"
        rm -f "$tmp"
        refreshed=1
    fi

    printf '\n%s\n' "$block" >> "$rc"
    if [ "$refreshed" -eq 1 ]; then
        ok "Refreshed PATH block in $rc"
    else
        ok "Added PATH block to $rc"
    fi
}

# ---------------------------------------------------------------------------
# Write MCP config snippets the user can paste into their AI client
# ---------------------------------------------------------------------------
#
# We deliberately do NOT auto-edit each AI client's MCP config — paths and
# formats vary (Claude Code: ~/.claude.json, Cursor: ~/.cursor/mcp.json,
# VSCode: user mcp.json with `servers` key not `mcpServers`, Antigravity:
# IDE-managed). A single overwrite of these snippet files under $PREFIX is
# idempotent by construction: re-running the installer just rewrites them
# in place, never duplicates.

write_mcp_snippets() {
    local cryo_mcp_bin="${BIN_DIR}/cryo-vault-mcp"
    local default_db="${HOME}/.cryo"

    local mcp_snippet="${PREFIX}/mcp-config.snippet.json"
    local vscode_snippet="${PREFIX}/mcp-config.vscode.snippet.json"

    info "Writing MCP config snippets"

    # mcpServers schema — Claude Code, Cursor, Antigravity, Claude Desktop.
    cat > "$mcp_snippet" <<EOF
{
  "mcpServers": {
    "cryo-vault": {
      "command": "${cryo_mcp_bin}",
      "args": [],
      "env": {
        "CRYO_DB_PATH": "${default_db}"
      }
    }
  }
}
EOF
    ok "Wrote ${mcp_snippet}"

    # VSCode native MCP uses top-level "servers", not "mcpServers".
    cat > "$vscode_snippet" <<EOF
{
  "servers": {
    "cryo-vault": {
      "command": "${cryo_mcp_bin}",
      "args": [],
      "env": {
        "CRYO_DB_PATH": "${default_db}"
      }
    }
  }
}
EOF
    ok "Wrote ${vscode_snippet}"
}

print_mcp_paste_guide() {
    local mcp_snippet="${PREFIX}/mcp-config.snippet.json"
    local vscode_snippet="${PREFIX}/mcp-config.vscode.snippet.json"

    printf '\n%sTo wire the MCP server into an AI client, paste the relevant snippet into:%s\n' "${BOLD}" "${RST}"
    printf '    %sClaude Code%s   ~/.claude.json                     (or: %sclaude mcp add cryo-vault %s --scope user%s)\n' \
        "${BOLD}" "${RST}" "${DIM}" "${BIN_DIR}/cryo-vault-mcp" "${RST}"
    printf '    %sCursor%s        ~/.cursor/mcp.json\n' "${BOLD}" "${RST}"
    printf '    %sAntigravity%s   IDE → Manage MCP Servers → View raw config\n' "${BOLD}" "${RST}"
    printf '    %sVSCode%s        Cmd-Shift-P → MCP: Open User Configuration  %s(use the vscode snippet — key is %s"servers"%s, not %s"mcpServers"%s)%s\n' \
        "${BOLD}" "${RST}" "${DIM}" "${BOLD}" "${DIM}" "${BOLD}" "${DIM}" "${RST}"
    printf '\n    Snippet:        %s%s%s\n' "${GRN}" "$mcp_snippet" "${RST}"
    printf '    VSCode snippet: %s%s%s\n' "${GRN}" "$vscode_snippet" "${RST}"
}

# ---------------------------------------------------------------------------
# Main install flow
# ---------------------------------------------------------------------------

install() {
    detect_platform
    pick_source

    info "Cryo Vault installer"
    printf '    %sversion:%s  %s\n' "${BOLD}" "${RST}" "$VERSION"
    printf '    %splatform:%s %s\n' "${BOLD}" "${RST}" "$PLATFORM"
    printf '    %sprefix:%s   %s\n' "${BOLD}" "${RST}" "$PREFIX"
    printf '    %ssource:%s   %s\n' "${BOLD}" "${RST}" "$SOURCE"

    local prev
    prev="$(current_installed_version || true)"
    if [ -n "$prev" ]; then
        if [ "$prev" = "$VERSION" ] && [ "$FORCE" -eq 0 ]; then
            ok "Cryo Vault ${VERSION} is already installed at ${PREFIX}."
            warn "Re-run with --force to reinstall, or --uninstall to remove."
            return 0
        fi
        info "Replacing existing install (${prev} -> ${VERSION})"
    fi

    local strays
    strays="$(list_stray_cryo_on_path || true)"
    if [ -n "$strays" ]; then
        warn "Found 'cryo' on PATH outside this installer:"
        printf '    %s\n' $strays
        warn "These will shadow the new install. Remove them if that's not what you want."
    fi

    stage_binaries
    activate_version
    prune_old_versions
    write_mcp_snippets

    if [ "$EDIT_PATH" -eq 1 ]; then
        info "Wiring PATH"
        while IFS= read -r rc; do
            [ -n "$rc" ] || continue
            ensure_path_in_rc "$rc"
        done < <(shell_rc_files)
    else
        warn "Skipping PATH edit (--no-path). Add manually:"
        printf '    export PATH="%s:$PATH"\n' "$BIN_DIR"
    fi

    echo
    ok "Done."
    printf '    Run %s%scryo --help%s to confirm.\n' "${BOLD}" "${GRN}" "${RST}"
    if [ "$EDIT_PATH" -eq 1 ]; then
        printf '    Open a new terminal, or %ssource%s your shell rc, to pick up the PATH change.\n' "${BOLD}" "${RST}"
    fi
    print_mcp_paste_guide
}

# ---------------------------------------------------------------------------
# Entry
# ---------------------------------------------------------------------------

if [ "$DO_UNINSTALL" -eq 1 ]; then
    uninstall
else
    install
fi
