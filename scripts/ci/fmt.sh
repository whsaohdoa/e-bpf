#!/usr/bin/env bash
# Formatting gate: `cargo fmt --all -- --check`, on the toolchain that actually
# honours rustfmt.toml.
#
# rustfmt.toml sets three unstable options (`group_imports`,
# `imports_granularity`, `unstable_features`). Stable rustfmt does not apply
# them and instead prints
#   Warning: can't set `group_imports = StdExternalCrate`, unstable features are
#   only available in nightly channel.
# for every file, i.e. the check silently ignores the import layout the project
# asks for. Nightly rustfmt applies them, so prefer nightly and degrade to stable
# only when the nightly component is missing (then the warnings are expected).
#
# Usage: bash scripts/ci/run.sh bash scripts/ci/fmt.sh
set -euo pipefail

NIGHTLY="${RUST_NIGHTLY:-nightly}"

# `nightly` is the alias created by install-toolchain.sh when a dated nightly is
# pinned, and the real channel when the pin was unavailable, so it is the first
# candidate either way.
for toolchain in nightly "${NIGHTLY}"; do
    if rustup run "${toolchain}" rustfmt --version >/dev/null 2>&1; then
        echo "[fmt] rustfmt from ${toolchain}"
        exec cargo "+${toolchain}" fmt --all -- --check
    fi
done

echo "[fmt] warning: rustfmt is not installed for '${NIGHTLY}'; falling back to stable," >&2
echo "[fmt] warning: so rustfmt.toml's unstable options (import grouping) are ignored" >&2
exec cargo fmt --all -- --check
