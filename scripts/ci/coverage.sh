#!/usr/bin/env bash
# Coverage report for the host crates, with an optional line-coverage gate.
#
# probe-ebpf is deliberately not covered: it is #![no_std] #![no_main] and only
# builds for a bpf target, so it cannot be instrumented for a host test run.
# main.rs is excluded by filename because it is process wiring (747 lines, no
# tests) and would drown the ratio.
#
# The eBPF object itself is built *before* instrumentation and handed to
# probe/build.rs through PROBE_EBPF_PREBUILT: aya-build drives a nested
# `cargo build --target bpfel-unknown-none`, and instrumenting that target fails
# with `E0463: can't find crate for profiler_builtins` because it has no
# profiler runtime. The uninstrumented build is usually already in `target/`
# from the earlier nextest step, so this normally costs nothing.
#
# The gate is opt-in. Most upstream Rust projects (aya, libbpf-rs, nextest) do
# not gate coverage at all; they publish the report. Set COVERAGE_MIN_LINES once
# you know what the real baseline is, e.g. 55.
#
# Knobs:
#   COVERAGE_MIN_LINES   if non-empty, fail when line coverage is below this
#   COVERAGE_LCOV        lcov output path (default target/lcov.info)
set -euo pipefail

find_ebpf_object() {
    # aya-build copies the object to `target/<profile>/build/probe-*/out/probe`;
    # take the newest one if several stale build dirs exist.
    ls -t target/debug/build/probe-*/out/probe target/release/build/probe-*/out/probe \
        2>/dev/null | head -n1 || true
}

ebpf_object="$(find_ebpf_object)"
if [ -z "${ebpf_object}" ]; then
    printf '[coverage] no eBPF object in target/, building it once without instrumentation\n'
    cargo build -p probe
    ebpf_object="$(find_ebpf_object)"
fi
if [ -z "${ebpf_object}" ]; then
    echo "error: could not find the eBPF object (target/*/build/probe-*/out/probe)" >&2
    exit 1
fi
# Absolute path: the build script of the probe crate runs with its crate
# directory as CWD, so a relative path would be resolved against probe/.
ebpf_object="$(cd "$(dirname "${ebpf_object}")" && pwd)/$(basename "${ebpf_object}")"
printf '[coverage] eBPF object: %s\n' "${ebpf_object}"

args=(
    -p probe
    -p probe-common
    --ignore-filename-regex '(^|/)main\.rs$'
    --lcov
    --output-path "${COVERAGE_LCOV:-target/lcov.info}"
)

if [ -n "${COVERAGE_MIN_LINES:-}" ]; then
    args+=(--fail-under-lines "${COVERAGE_MIN_LINES}")
    printf '[coverage] gate on: line coverage must be >= %s%%\n' "${COVERAGE_MIN_LINES}"
else
    printf '[coverage] gate off: COVERAGE_MIN_LINES is empty, report only\n'
fi

PROBE_EBPF_PREBUILT="${ebpf_object}" exec cargo llvm-cov "${args[@]}"
