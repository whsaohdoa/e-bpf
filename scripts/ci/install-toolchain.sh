#!/usr/bin/env bash
# Install everything needed to build and test the probe workspace on a bare
# runner (one with no Docker).
#
# Why this exists: the Gitee Go custom runner has no Docker, so the test stage
# cannot reuse the `toolchain` stage of Dockerfile.ci. This script installs the
# same payload with plain apt/rustup/curl:
#
#   rust stable + rust nightly (with rust-src) + bpf-linker + the test tooling
#   (cargo-nextest, cargo-llvm-cov).
#
# bpf-linker is preferred from the upstream prebuilt release archive: it is a
# statically linked binary, so no apt LLVM packages and no multi-minute
# `cargo install` are needed. That is what aya-rs itself does in its CI
# (`taiki-e/install-action` with `tool: bpf-linker`).
#
# GitHub release downloads are frequently unreachable from mainland runners, so
# the archive comes from one explicitly configured base
# (BPF_LINKER_DOWNLOAD_BASE, set per pipeline as CI_BPF_LINKER_DOWNLOAD_BASE).
# There is no built-in proxy list: walking several hosts in a row turned one
# slow host into a long stall, and the pipeline knows better than the script
# which mirror is alive. If the download still fails, the script falls back to
# building bpf-linker from source with LLVM packages from an apt mirror. The fallback
# costs a `llvm-23` download (~130 MB) plus a few minutes of compiling, but it
# only runs when the prebuilt path is genuinely impossible.
#
# It is idempotent: an image that already ships cc/make/git/curl/zstd skips the
# apt step altogether (GitHub-hosted runners do), apt no-ops on installed
# packages, rustup skips installed
# toolchains, and the bpf-linker / nextest / llvm-cov steps skip once the pinned
# version is on PATH, so a warm CI cache turns this into a few seconds of
# version checks.
#
# Environment knobs (all optional). Every knob also accepts the `CI_*` spelling
# used for Gitee Go pipeline variables, e.g. CI_UBUNTU_MIRROR.
#   UBUNTU_MIRROR                rewrite the distro sources to this mirror
#   RUSTUP_MIRROR                rustup dist server
#   CARGO_REGISTRY_MIRROR        crates.io mirror (e.g. .../crates.io-index/)
#   BPF_LINKER_DOWNLOAD_BASE     base URL serving the release archives as
#                                <base>/v<version>/bpf-linker-<target>.tar.zst,
#                                e.g. https://ghfast.top/https://github.com/
#                                aya-rs/bpf-linker/releases/download .
#                                Default: GitHub upstream.
#   BPF_LINKER_VERSION=0.12.0    bpf-linker version to install
#   BPF_LINKER_SOURCE=0          set to 1 to skip the prebuilt download and
#                                always build from source
#   RUST_VERSION=stable          host toolchain
#   RUST_NIGHTLY=nightly-2026-11-01
#                                toolchain used by aya-build for the eBPF crate.
#                                A dated nightly keeps CI reproducible; the
#                                `nightly` alias aya-build expects is created
#                                below. If that exact nightly does not exist the
#                                script falls back to plain `nightly`.
#   LLVM_VERSION=23              LLVM major used by the source-build path
#   LLVM_MIRROR                  base URL of an apt.llvm.org mirror; unset
#                                means "try apt.llvm.org, then TUNA"
#   LLVM_GPG_KEY_URL             explicit archive key URL (optional)
#
# Supported platforms: Ubuntu/Debian (apt). The runner needs network access to
# the mirrors and needs apt to work (root, or passwordless sudo).
set -euo pipefail

# Pipeline variables are spelled CI_*; the unprefixed names are for local use.
UBUNTU_MIRROR="${UBUNTU_MIRROR:-${CI_UBUNTU_MIRROR:-}}"
RUSTUP_MIRROR="${RUSTUP_MIRROR:-${CI_RUSTUP_MIRROR:-https://static.rust-lang.org}}"
CARGO_REGISTRY_MIRROR="${CARGO_REGISTRY_MIRROR:-${CI_CARGO_REGISTRY_MIRROR:-}}"
BPF_LINKER_DOWNLOAD_BASE="${BPF_LINKER_DOWNLOAD_BASE:-${CI_BPF_LINKER_DOWNLOAD_BASE:-https://github.com/aya-rs/bpf-linker/releases/download}}"
BPF_LINKER_VERSION="${BPF_LINKER_VERSION:-${CI_BPF_LINKER_VERSION:-0.12.0}}"
BPF_LINKER_SOURCE="${BPF_LINKER_SOURCE:-${CI_BPF_LINKER_SOURCE:-0}}"
RUST_VERSION="${RUST_VERSION:-${CI_RUST_VERSION:-stable}}"
RUST_NIGHTLY="${RUST_NIGHTLY:-${CI_RUST_NIGHTLY:-nightly-2026-11-01}}"
LLVM_VERSION="${LLVM_VERSION:-${CI_LLVM_VERSION:-23}}"
LLVM_MIRROR="${LLVM_MIRROR:-${CI_LLVM_MIRROR:-}}"
LLVM_GPG_KEY_URL="${LLVM_GPG_KEY_URL:-${CI_LLVM_GPG_KEY_URL:-}}"

LLVM_PREFIX="/usr/lib/llvm-${LLVM_VERSION}"
CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"

log() { printf '\n[install-toolchain] %s\n' "$*"; }
warn() { printf '[install-toolchain] warning: %s\n' "$*" >&2; }

SUDO=""
if [ "$(id -u)" -ne 0 ]; then
    if command -v sudo >/dev/null 2>&1; then
        SUDO="sudo"
    else
        echo "error: not root and sudo is unavailable" >&2
        exit 1
    fi
fi

if ! command -v apt-get >/dev/null 2>&1; then
    echo "error: this script assumes a Debian/Ubuntu runner (apt-get not found)" >&2
    exit 1
fi

# shellcheck disable=SC1091
. /etc/os-release
CODENAME="${VERSION_CODENAME:-noble}"
ARCH="$(dpkg --print-architecture)"

apt_install() {
    # shellcheck disable=SC2086
    $SUDO env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends "$@"
}

# --------------------------------------------------------------------------
log "apt: bootstrap packages"
# --------------------------------------------------------------------------
# An HTTPS mirror cannot be verified before ca-certificates exists, so: point the
# sources at the mirror first (falling back to its plain-HTTP form while TLS
# trust is missing, which is the state of a bare image), install the small
# bootstrap set from there, then switch back to the requested scheme.
rewrite_sources() {
    local mirror="$1"
    log "apt: distro sources -> ${mirror}"
    if [ -f /etc/apt/sources.list.d/ubuntu.sources ]; then
        $SUDO sed -i "s|^URIs: .*|URIs: ${mirror}|" /etc/apt/sources.list.d/ubuntu.sources
    fi
    if [ -f /etc/apt/sources.list ]; then
        $SUDO sed -i "s|http://[a-z.]*archive.ubuntu.com/ubuntu|${mirror}|g; s|http://security.ubuntu.com/ubuntu|${mirror}|g" /etc/apt/sources.list
    fi
}

if [ -n "${UBUNTU_MIRROR}" ]; then
    BOOTSTRAP_MIRROR="${UBUNTU_MIRROR}"
    if [ ! -f /etc/ssl/certs/ca-certificates.crt ]; then
        BOOTSTRAP_MIRROR="$(printf '%s' "${UBUNTU_MIRROR}" | sed 's|^https://|http://|')"
    fi
    rewrite_sources "${BOOTSTRAP_MIRROR}"
fi

# build-essential provides the `cc` linker rustc calls; zstd unpacks the
# bpf-linker release archive; git is needed for any git-sourced crate.
#
# A stock GitHub-hosted Ubuntu runner already ships all of these, and its image
# carries a cloud-init mirror list whose rewrite leaves `build-essential` with
# "no installation candidate" even after a clean update (see the netcheck
# diagnostic), so an image that already has the tools skips apt entirely. Bare
# images (the Gitee Go runner) still take the apt path.
bootstrap_ready() {
    local tool
    for tool in cc make git curl zstd; do
        command -v "${tool}" >/dev/null 2>&1 || return 1
    done
    [ -f /etc/ssl/certs/ca-certificates.crt ]
}

if bootstrap_ready; then
    log "apt: cc/make/git/curl/zstd/ca-certificates already present; skipping apt"
else
    $SUDO env DEBIAN_FRONTEND=noninteractive apt-get update -qq
    apt_install ca-certificates curl git zstd build-essential

    if [ -n "${UBUNTU_MIRROR}" ] && [ "${BOOTSTRAP_MIRROR}" != "${UBUNTU_MIRROR}" ]; then
        rewrite_sources "${UBUNTU_MIRROR}"
        $SUDO env DEBIAN_FRONTEND=noninteractive apt-get update -qq
    fi
fi

# --------------------------------------------------------------------------
log "rustup: ${RUST_NIGHTLY} (rust-src) then ${RUST_VERSION}"
# --------------------------------------------------------------------------
export RUSTUP_DIST_SERVER="${RUSTUP_MIRROR}"
export RUSTUP_UPDATE_ROOT="${RUSTUP_MIRROR}/rustup"
export RUSTUP_HOME
export CARGO_HOME

if [ ! -x "${CARGO_HOME}/bin/rustup" ]; then
    case "${ARCH}" in
        amd64) RUSTUP_HOST=x86_64-unknown-linux-gnu ;;
        arm64) RUSTUP_HOST=aarch64-unknown-linux-gnu ;;
        *) echo "error: unsupported architecture ${ARCH}" >&2; exit 1 ;;
    esac
    curl --proto '=https' --tlsv1.2 -fsSL \
        --retry 5 --retry-all-errors --retry-delay 2 \
        "${RUSTUP_DIST_SERVER}/rustup/dist/${RUSTUP_HOST}/rustup-init" \
        -o /tmp/rustup-init
    chmod +x /tmp/rustup-init
    /tmp/rustup-init -y --no-modify-path --profile minimal \
        --default-toolchain "${RUST_VERSION}"
    rm -f /tmp/rustup-init
fi

export PATH="${CARGO_HOME}/bin:${PATH}"

# Nightly first, stable last: `rustup default` then points at stable, so a plain
# `cargo build` runs on stable while aya-build explicitly asks for nightly.
if ! rustup toolchain install "${RUST_NIGHTLY}" --profile minimal --no-self-update \
    --component rust-src; then
    if [ "${RUST_NIGHTLY#nightly-}" != "${RUST_NIGHTLY}" ]; then
        warn "pinned toolchain ${RUST_NIGHTLY} is unavailable, falling back to 'nightly'"
        RUST_NIGHTLY=nightly
        rustup toolchain install "${RUST_NIGHTLY}" --profile minimal --no-self-update \
            --component rust-src
    else
        exit 1
    fi
fi

rustup toolchain install "${RUST_VERSION}" --profile minimal --no-self-update
rustup default "${RUST_VERSION}"
# `--profile minimal` omits these; the test stage needs all three.
rustup component add rustfmt clippy llvm-tools-preview --toolchain "${RUST_VERSION}"

# rustfmt.toml sets unstable options, which only nightly rustfmt honours. The
# component is best-effort: a nightly that did not ship rustfmt must not fail the
# whole install, scripts/ci/fmt.sh degrades to stable rustfmt in that case.
if ! rustup component add rustfmt --toolchain "${RUST_NIGHTLY}"; then
    warn "rustfmt is not available for ${RUST_NIGHTLY}; formatting will use stable rustfmt"
fi

# aya-build runs `cargo +nightly`-style invocations through Toolchain::default(),
# which is the literal string "nightly". When the pin is a dated toolchain, alias
# "nightly" to it so that those invocations resolve. Same trick as aya's own
# .github/actions/setup-rust.
if [ "${RUST_NIGHTLY#nightly-}" != "${RUST_NIGHTLY}" ]; then
    if RUST_HOST="$(rustup run "${RUST_NIGHTLY}" rustc --print host-tuple 2>/dev/null)"; then
        :
    else
        RUST_HOST="$(rustup run "${RUST_NIGHTLY}" rustc -vV | sed -n 's/^host: //p')"
    fi
    NIGHTLY_SYSROOT="$(rustup run "${RUST_NIGHTLY}" rustc --print sysroot)"
    NIGHTLY_ALIAS="${RUSTUP_HOME}/toolchains/nightly-${RUST_HOST}"
    if [ "$(readlink -f "${NIGHTLY_ALIAS}" 2>/dev/null || true)" != "$(readlink -f "${NIGHTLY_SYSROOT}")" ]; then
        log "rustup: alias nightly -> ${RUST_NIGHTLY}"
        rm -rf "${NIGHTLY_ALIAS}"
        ln -s "${NIGHTLY_SYSROOT}" "${NIGHTLY_ALIAS}"
    fi
fi

# Optional crates.io mirror, written before anything is fetched so it applies to
# the workspace build and to `cargo install` alike.
if [ -n "${CARGO_REGISTRY_MIRROR}" ] && [ ! -f "${CARGO_HOME}/config.toml" ]; then
    log "cargo: registry mirror ${CARGO_REGISTRY_MIRROR}"
    install -d -m 0755 "${CARGO_HOME}"
    printf '%s\n' \
        '[source.crates-io]' \
        'replace-with = "mirror"' \
        '[source.mirror]' \
        "registry = \"sparse+${CARGO_REGISTRY_MIRROR}\"" \
        > "${CARGO_HOME}/config.toml"
fi

# --------------------------------------------------------------------------
log "test tooling: cargo-nextest, cargo-llvm-cov"
# --------------------------------------------------------------------------
# Installed before bpf-linker on purpose: these come from crates.io and have no
# exotic dependencies, so a failure in the bpf-linker step below cannot leave the
# runner without them.
if ! command -v cargo-nextest >/dev/null 2>&1; then
    cargo install cargo-nextest --locked
fi
if ! command -v cargo-llvm-cov >/dev/null 2>&1; then
    cargo install cargo-llvm-cov --locked
fi

# --------------------------------------------------------------------------
log "bpf-linker ${BPF_LINKER_VERSION}"
# --------------------------------------------------------------------------
# probe-ebpf/build.rs runs `which("bpf-linker")`, so it must be on PATH for every
# later cargo invocation, not just this one (see scripts/ci/run.sh).
bpf_linker_present() {
    command -v bpf-linker >/dev/null 2>&1 \
        && bpf-linker --version 2>/dev/null | grep -q "${BPF_LINKER_VERSION}"
}

# Returns 0 only when the pinned binary ended up in $CARGO_HOME/bin. Every step
# fails closed: the caller treats "anything but 0" as "fall back to source".
install_bpf_linker_prebuilt() {
    local target
    case "${ARCH}" in
        amd64) target=x86_64-unknown-linux-musl ;;
        arm64) target=aarch64-unknown-linux-musl ;;
        *) warn "no prebuilt bpf-linker archive for ${ARCH}"; return 1 ;;
    esac

    # Checksums are pinned for the default version only; other versions are
    # downloaded but not verified.
    local expected_sha256=""
    case "${BPF_LINKER_VERSION}:${ARCH}" in
        0.12.0:amd64) expected_sha256=6789717c8cb6a2a5d920f3b6bfac2bd7efc00f6228ecd149108a99699a75c679 ;;
        0.12.0:arm64) expected_sha256=5e201e69dafac7af98f9788f1c68edd5ccc8fa0805093f899bf02bd2947b3935 ;;
        *) ;;
    esac

    local archive="bpf-linker-${target}.tar.zst"
    local url="${BPF_LINKER_DOWNLOAD_BASE%/}/v${BPF_LINKER_VERSION}/${archive}"
    local tmpdir
    tmpdir="$(mktemp -d)"

    log "bpf-linker: downloading ${url}"
    # --connect-timeout keeps a blackholed host cheap; --max-time bounds a
    # proxy that accepts the connection and then stalls.
    if ! curl -fL --proto '=https,http' --tlsv1.2 \
        --connect-timeout 10 --max-time 600 --retry 2 --retry-delay 2 --retry-all-errors \
        -o "${tmpdir}/${archive}" "${url}"; then
        warn "could not download ${archive} from ${BPF_LINKER_DOWNLOAD_BASE}"
        rm -rf "${tmpdir}"
        return 1
    fi

    if [ -n "${expected_sha256}" ]; then
        if ! printf '%s  %s\n' "${expected_sha256}" "${tmpdir}/${archive}" | sha256sum -c -; then
            warn "checksum mismatch for ${archive}; discarding the download"
            rm -rf "${tmpdir}"
            return 1
        fi
    else
        warn "no pinned checksum for bpf-linker ${BPF_LINKER_VERSION}/${ARCH}"
    fi

    if ! tar --zstd -xf "${tmpdir}/${archive}" -C "${tmpdir}"; then
        warn "could not unpack ${archive}"
        rm -rf "${tmpdir}"
        return 1
    fi

    local binary
    binary="$(find "${tmpdir}" -maxdepth 2 -type f -name bpf-linker | head -n1)"
    if [ -z "${binary}" ]; then
        warn "${archive} does not contain a bpf-linker binary"
        rm -rf "${tmpdir}"
        return 1
    fi

    install -d -m 0755 "${CARGO_HOME}/bin"
    install -m 0755 "${binary}" "${CARGO_HOME}/bin/bpf-linker"
    rm -rf "${tmpdir}"

    if [ ! -x "${CARGO_HOME}/bin/bpf-linker" ]; then
        warn "bpf-linker did not land in ${CARGO_HOME}/bin"
        return 1
    fi
    return 0
}

# First apt.llvm.org mirror that actually serves this release. APT_LLVM.ORG is
# often slow or unreachable from mainland runners, TUNA mirrors the same tree.
resolve_llvm_mirror() {
    local candidates=()
    if [ -n "${LLVM_MIRROR}" ]; then
        candidates+=("${LLVM_MIRROR}")
    else
        candidates+=(
            "https://apt.llvm.org"
            "https://mirrors.tuna.tsinghua.edu.cn/llvm-apt"
        )
    fi
    local mirror url
    for mirror in "${candidates[@]}"; do
        url="${mirror}/${CODENAME}/dists/llvm-toolchain-${CODENAME}-${LLVM_VERSION}/Release"
        log "llvm: probing ${mirror}"
        if curl -fsS --connect-timeout 8 --max-time 60 --retry 1 -o /dev/null "${url}"; then
            printf '%s' "${mirror}"
            return 0
        fi
    done
    return 1
}

install_llvm_dev_packages() {
    apt_install gnupg
    $SUDO install -d -m 0755 /etc/apt/keyrings

    local mirror
    if ! mirror="$(resolve_llvm_mirror)"; then
        echo "error: no LLVM apt mirror serves llvm-toolchain-${CODENAME}-${LLVM_VERSION}" >&2
        return 1
    fi
    log "llvm: using ${mirror}"

    local key_candidates=()
    if [ -n "${LLVM_GPG_KEY_URL}" ]; then
        key_candidates+=("${LLVM_GPG_KEY_URL}")
    fi
    key_candidates+=(
        "${mirror}/llvm-snapshot.gpg.key"
        "https://apt.llvm.org/llvm-snapshot.gpg.key"
    )

    local key_file url trust=""
    key_file="$(mktemp)"
    for url in "${key_candidates[@]}"; do
        log "apt: LLVM key <- ${url}"
        if curl -fsSL --connect-timeout 10 --max-time 60 --retry 2 --retry-all-errors \
            -o "${key_file}" "${url}"; then
            $SUDO gpg --dearmor --yes -o /etc/apt/keyrings/llvm.gpg <"${key_file}"
            trust="signed-by=/etc/apt/keyrings/llvm.gpg"
            break
        fi
        : >"${key_file}"
    done
    rm -f "${key_file}"

    # The key lives on apt.llvm.org only (the TUNA mirror serves the packages but
    # 404s on llvm-snapshot.gpg.key), so it can be unreachable even when the
    # mirror works. Pinning the mirror over HTTPS and trusting it keeps the
    # source-build fallback usable instead of dead.
    if [ -z "${trust}" ]; then
        warn "could not fetch the LLVM archive key; trusting the HTTPS mirror instead"
        trust="trusted=yes"
    fi

    echo "deb [${trust}] ${mirror}/${CODENAME}/ llvm-toolchain-${CODENAME}-${LLVM_VERSION} main" \
        | $SUDO tee /etc/apt/sources.list.d/llvm.list >/dev/null
    $SUDO env DEBIAN_FRONTEND=noninteractive apt-get update -qq

    apt_install \
        "llvm-${LLVM_VERSION}" "llvm-${LLVM_VERSION}-dev" \
        "clang-${LLVM_VERSION}" "libclang-${LLVM_VERSION}-dev"
    # Not needed by every LLVM major / Ubuntu release; missing ones must not
    # abort the whole install.
    apt_install libzstd-dev pkg-config || warn "libzstd-dev/pkg-config unavailable; continuing"
    apt_install "libpolly-${LLVM_VERSION}-dev" \
        || warn "libpolly-${LLVM_VERSION}-dev unavailable; continuing"
}

build_bpf_linker_from_source() {
    # The caller invokes this in a `||` list, which disables errexit for the
    # whole call; re-enable it so a failed apt step aborts instead of carrying on
    # into a doomed `cargo install`.
    set -e
    log "bpf-linker: building ${BPF_LINKER_VERSION} from source against LLVM ${LLVM_VERSION}"
    install_llvm_dev_packages
    (
        export "LLVM_SYS_${LLVM_VERSION}0_PREFIX=${LLVM_PREFIX}"
        export PATH="${LLVM_PREFIX}/bin:${PATH}"
        cargo install bpf-linker \
            --version "${BPF_LINKER_VERSION}" \
            --locked \
            --no-default-features \
            --features "llvm-${LLVM_VERSION}"
    )
}

if bpf_linker_present; then
    log "bpf-linker already present"
elif [ "${BPF_LINKER_SOURCE}" = "1" ] || ! install_bpf_linker_prebuilt; then
    if [ "${BPF_LINKER_SOURCE}" != "1" ]; then
        warn "no prebuilt archive was reachable; falling back to a source build"
        warn "tip: point CI_BPF_LINKER_DOWNLOAD_BASE at a reachable mirror to skip the compile"
    fi
    build_bpf_linker_from_source \
        || { echo "error: could not install bpf-linker (prebuilt unreachable, source build failed)" >&2; exit 1; }
fi

if ! command -v bpf-linker >/dev/null 2>&1; then
    echo "error: bpf-linker is still not on PATH; probe-ebpf/build.rs would panic on which()" >&2
    exit 1
fi

log "installed: $(rustc --version) | $(bpf-linker --version) | $(cargo nextest --version) | $(cargo llvm-cov --version)"
