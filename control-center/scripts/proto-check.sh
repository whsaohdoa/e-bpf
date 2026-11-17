#!/usr/bin/env bash
# The Rust side owns the contract: agent/proto/control.proto is the single source of truth and
# control-center/app/src/main/proto/control.proto is a byte-for-byte copy of it. Generated
# Java types only stay compatible because both sides agree on field numbers, so drift has to
# fail CI instead of surfacing as a wrong field during integration.
set -euo pipefail

a="$(dirname "$0")/../../agent/proto/control.proto"
b="$(dirname "$0")/../app/src/main/proto/control.proto"

# `cmp` is present on the CI runner and in Git Bash, but a plain Windows shell may not have it;
# `git diff --no-index --quiet` is the portable fallback (the script only ever runs inside the
# repository).
same_file() {
    if command -v cmp >/dev/null 2>&1; then
        cmp -s "$a" "$b"
    else
        git diff --no-index --quiet -- "$a" "$b"
    fi
}

if ! same_file; then
    echo "control.proto drift: $a vs $b" >&2
    diff -u "$a" "$b" >&2 || true
    exit 1
fi
