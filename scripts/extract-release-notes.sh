#!/usr/bin/env bash

set -euo pipefail

if [[ $# -ne 1 || ! "$1" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "Usage: $0 vX.Y.Z" >&2
    exit 2
fi

version="${1#v}"
changelog="CHANGELOG.md"

if [[ ! -f "$changelog" ]]; then
    echo "Missing $changelog" >&2
    exit 1
fi

awk -v heading="## [$version]" '
    index($0, heading) == 1 { found = 1; next }
    found && /^## \[/ { exit }
    found { print }
    END {
        if (!found) {
            exit 1
        }
    }
' "$changelog"
