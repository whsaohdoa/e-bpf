#!/usr/bin/env bash
# Run a command with the environment that scripts/ci/install-toolchain.sh set up.
#
# The Gitee Go test stage executes every command in a fresh shell, so the
# exports below cannot live in the install script; they have to be re-applied
# per command. Keeping them here (rather than inlining `export ...` in the YAML)
# means the paths and flags are defined once, next to the script that installs
# the toolchain.
#
# Usage: bash scripts/ci/run.sh cargo nextest run --no-fail-fast
set -euo pipefail

# rustup shims and the tools installed by `cargo install`, plus the prebuilt
# bpf-linker that probe-ebpf/build.rs looks up with `which`.
export PATH="${HOME}/.cargo/bin:${PATH}"

# The prebuilt bpf-linker is statically linked, so LLVM is normally not needed at
# all. Only when someone used the BPF_LINKER_SOURCE=1 escape hatch do these paths
# exist, and then bpf-linker needs the shared LLVM at runtime plus llvm-sys'
# configured prefix.
LLVM_VERSION="${LLVM_VERSION:-${CI_LLVM_VERSION:-23}}"
LLVM_PREFIX="/usr/lib/llvm-${LLVM_VERSION}"
if [ -d "${LLVM_PREFIX}/bin" ]; then
    export PATH="${LLVM_PREFIX}/bin:${PATH}"
    export LD_LIBRARY_PATH="${LLVM_PREFIX}/lib:${LD_LIBRARY_PATH:-}"
    # The name carries the major version (LLVM 23 -> LLVM_SYS_230_PREFIX).
    export "LLVM_SYS_${LLVM_VERSION}0_PREFIX=${LLVM_PREFIX}"
fi

# CI settings that make builds cheaper and logs readable. `target` dominates the
# cache size, and incremental artifacts are useless for a cold, throwaway
# workspace; CI logs get color even though stdout is not a TTY.
export CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}"
export CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-always}"

if [ "$#" -eq 0 ]; then
    echo "usage: $0 <command> [args...]" >&2
    exit 2
fi

exec "$@"
