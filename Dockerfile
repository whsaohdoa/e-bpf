# Aya eBPF development image, intended for Docker inside WSL2.
#
# Base image: Ubuntu 24.04 LTS ("noble"), explicitly pinned. Do not replace
# this with ubuntu:latest or Alpine. Alpine/musl has known bpf-linker issues,
# and the Ubuntu LLVM packages used below match the supported Aya setup.
FROM ubuntu:24.04

# Fail a RUN step when any command in a pipe fails (for example, a broken
# download piped into gpg or rustup).
SHELL ["/bin/bash", "-o", "pipefail", "-c"]

ARG DEBIAN_FRONTEND=noninteractive

# ---------------------------------------------------------------------------
# Machine-specific endpoints. Defaults below are the upstream public ones, so
# a fresh clone builds anywhere; each machine overrides them through the
# matching variables in .env (see .env.template) instead of editing this file.
#
# The Ubuntu source is first written over HTTP to bootstrap ca-certificates
# (the minimal Ubuntu image may not trust HTTPS yet), then switched to
# UBUNTU_MIRROR before installing the rest. That is why the bootstrap mirror
# is a separate variable: it must be reachable over plain HTTP.
# apt still verifies every repository with its signing key in both phases.
# Do not append a trailing slash to any of these values.
#
# A China-based machine typically sets (see .env.template):
#   UBUNTU_BOOTSTRAP_MIRROR=http://mirrors.tuna.tsinghua.edu.cn/ubuntu
#   UBUNTU_MIRROR=https://mirrors.tuna.tsinghua.edu.cn/ubuntu
#   LLVM_MIRROR=https://mirrors.tuna.tsinghua.edu.cn/llvm-apt
#   RUSTUP_MIRROR=https://mirrors.tuna.tsinghua.edu.cn/rustup
# ---------------------------------------------------------------------------
ARG UBUNTU_BOOTSTRAP_MIRROR=http://archive.ubuntu.com/ubuntu
ARG UBUNTU_MIRROR=https://archive.ubuntu.com/ubuntu
ARG LLVM_MIRROR=https://apt.llvm.org

# Signing key for the apt.llvm.org repository. Kept separate from LLVM_MIRROR
# because mirrors usually serve packages only, not the upstream key file.
ARG LLVM_GPG_KEY_URL=https://apt.llvm.org/llvm-snapshot.gpg.key

# Mirror of the Rust toolchain distribution. RUSTUP_DIST_SERVER (toolchains,
# components, rustup-init) and RUSTUP_UPDATE_ROOT (rustup's own metadata) are
# both derived from it below, so the build never contacts sh.rustup.rs.
ARG RUSTUP_MIRROR=https://static.rust-lang.org

# Value written to git's global http.version. Some networks reset GitHub's
# smart-HTTP endpoint when HTTP/2 is negotiated, so this defaults to HTTP/1.1
# as a workaround; set it to an empty-tolerant value such as HTTP/2 on hosts
# where HTTP/1.1 is not needed or not helpful.
ARG GIT_HTTP_VERSION=HTTP/1.1

# LLVM major version. bpf-linker 0.12.0 has LLVM 21/22/23 backends and uses
# LLVM 23 by default, which is also the LLVM line used by the current Rust
# nightly at the time this image is pinned. This value is used both for the
# apt packages and for the bpf-linker cargo feature, so they cannot drift.
ENV LLVM_VERSION=23 \
    LLVM_PREFIX=/usr/lib/llvm-23
ENV PATH="/usr/local/cargo/bin:${LLVM_PREFIX}/bin:${PATH}"

# Install the LLVM 23 development stack (Ubuntu 24.04's own LLVM is 18, which
# is too old for bpf-linker 0.12.0):
#
# - build-essential: C/C++ compiler and make, needed to build cargo crates
#   with native build scripts (bpf-linker, cargo-generate, openssl-sys, ...).
# - pkg-config: lets Rust build scripts locate system libraries such as OpenSSL.
# - libssl-dev: headers for cargo-generate's TLS/git dependencies.
# - git: cargo-generate, cargo install from git, and normal Aya development.
# - curl: rustup installation and general development convenience.
# - clang-23/llvm-23: C/LLVM toolchain (clang, llvm-objdump, llvm-readelf, ...)
#   exposed through /usr/lib/llvm-23/bin in PATH.
# - llvm-23-dev: LLVM headers, static components, llvm-config and libLLVM.
#   bpf-linker links against LLVM and must be compiled against a matching LLVM.
# - libclang-23-dev: libclang development files required by LLVM-based builds.
# - libpolly-23-dev: Polly optimiser library required by bpf-linker/LLVM when
#   LLVM is linked as separate components (not the single shared library).
# - libzstd-dev: zstd headers/libraries used by LLVM's compression support when
#   linking bpf-linker; this is why it must be installed before cargo install.
# - linux-tools-common + linux-tools-generic: provide bpftool for inspecting
#   BPF programs/maps, dumping BTF, and probing kernel features. On Ubuntu
#   24.04 "bpftool" is only a virtual package, so installing the name directly
#   fails with "no installation candidate". The real binary is shipped by the
#   versioned kernel tools package pulled in by linux-tools-generic. Because
#   uname -r reports the WSL2 kernel (not the Ubuntu package version), locate
#   the installed versioned binary and link it to a stable PATH location.
#
# The LLVM apt repository is added only for LLVM 23; the rest comes from the
# pinned Ubuntu release. apt lists are removed in the same layer to keep the
# image smaller. The LLVM signing key itself is not an apt package and is
# fetched once from LLVM_GPG_KEY_URL over HTTPS; package downloads follow
# LLVM_MIRROR.
RUN printf '%s\n' \
        'Types: deb' \
        "URIs: ${UBUNTU_BOOTSTRAP_MIRROR}" \
        'Suites: noble noble-updates noble-backports noble-security' \
        'Components: main restricted universe multiverse' \
        'Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg' \
        > /etc/apt/sources.list.d/ubuntu.sources \
    && rm -f /etc/apt/sources.list /etc/apt/sources.list.d/*.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        gnupg \
    && sed -i "s|^URIs: .*|URIs: ${UBUNTU_MIRROR}|" \
        /etc/apt/sources.list.d/ubuntu.sources \
    && install -d -m 0755 /etc/apt/keyrings \
    && curl -fsSL "${LLVM_GPG_KEY_URL}" \
        | gpg --dearmor --yes -o /etc/apt/keyrings/llvm.gpg \
    && . /etc/os-release \
    && echo "deb [signed-by=/etc/apt/keyrings/llvm.gpg] ${LLVM_MIRROR}/${VERSION_CODENAME}/ llvm-toolchain-${VERSION_CODENAME}-${LLVM_VERSION} main" \
        > /etc/apt/sources.list.d/llvm.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        pkg-config \
        libssl-dev \
        git \
        curl \
        clang-${LLVM_VERSION} \
        llvm-${LLVM_VERSION} \
        llvm-${LLVM_VERSION}-dev \
        libclang-${LLVM_VERSION}-dev \
        libpolly-${LLVM_VERSION}-dev \
        libzstd-dev \
        linux-tools-common \
        linux-tools-generic \
    && BPFTOOL_BIN="$(find /usr/lib -type f -name bpftool -perm -111 | sort -V | tail -n 1)" \
    && test -x "${BPFTOOL_BIN}" \
    && ln -sf "${BPFTOOL_BIN}" /usr/local/bin/bpftool \
    && rm -rf /var/lib/apt/lists/*

# Rust is installed from the RUSTUP_MIRROR build arg (upstream by default).
# Downloading rustup-init directly avoids sh.rustup.rs, whose flaky connection
# previously caused curl error 56.
# RUSTUP_DIST_SERVER covers toolchain/component downloads and RUSTUP_UPDATE_ROOT
# covers rustup's own update metadata, so both stay on the domestic mirror.
# stable is the default for user-space builds; nightly is required by aya-build
# to build the eBPF crate with -Z build-std=core. That flag compiles core from
# source via the rust-src component, which is why no prebuilt rust-std for
# bpfel-unknown-none is needed (and none is published in the nightly dist).
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    RUSTUP_DIST_SERVER=${RUSTUP_MIRROR} \
    RUSTUP_UPDATE_ROOT=${RUSTUP_MIRROR}/rustup

# rustup-init is architecture-specific. WSL2 is normally amd64, but map arm64
# as well so the same Dockerfile works on ARM Windows hosts.
RUN case "$(dpkg --print-architecture)" in \
        amd64) RUSTUP_HOST=x86_64-unknown-linux-gnu ;; \
        arm64) RUSTUP_HOST=aarch64-unknown-linux-gnu ;; \
        *) echo "Unsupported architecture: $(dpkg --print-architecture)" >&2; exit 1 ;; \
    esac \
    && curl --proto '=https' --tlsv1.2 -fsSL \
        --retry 5 --retry-all-errors --retry-delay 2 \
        "${RUSTUP_DIST_SERVER}/rustup/dist/${RUSTUP_HOST}/rustup-init" \
        -o /tmp/rustup-init \
    && chmod +x /tmp/rustup-init \
    && /tmp/rustup-init -y --no-modify-path --profile minimal --default-toolchain stable \
    && rustup toolchain install nightly --profile minimal --component rust-src \
    && rustup default stable \
    && rm -f /tmp/rustup-init

# Install the BPF linker from source, pinned to the version whose default LLVM
# backend (LLVM 23) matches LLVM_PREFIX above. --no-default-features plus the
# explicit llvm-23 feature avoids accidentally pulling a different llvm-sys.
# --locked uses bpf-linker's own lockfile for reproducible dependencies.
RUN cargo install bpf-linker \
        --version 0.12.0 \
        --locked \
        --no-default-features \
        --features "llvm-${LLVM_VERSION}"

# cargo-generate creates an Aya project from aya-template. It is built from
# source and pinned so the scaffold command does not change silently between
# image rebuilds; OpenSSL and pkg-config above are available for its native
# dependencies if the selected release needs them.
RUN cargo install cargo-generate --version 0.25.1 --locked

# sudo is used by the documented Aya attach commands (for example
# `sudo -E cargo run --release -- --iface lo`). The container already runs as
# root, but keeping sudo makes those commands work verbatim.
#
# http.version defaults to HTTP/1.1 as a mitigation for intermittent
# "tls handshake eof" resets observed on GitHub's smart-HTTP endpoint when the
# client negotiates HTTP/2 through gitoxide/rustls (cargo-generate). gitoxide
# reads git config, so the setting applies to `cargo generate <github-url>`.
# It reduces but does not eliminate those resets: on a lossy network a retry
# may still be needed. Normal git usage is unaffected. The value comes from the
# GIT_HTTP_VERSION build arg (see .env.template) rather than being hardcoded.
#
# libcap2-bin provides capsh (capability inspection), iproute2 and ethtool are
# used when attaching XDP to a real interface, and iputils-ping triggers traffic
# for the XDP program.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        sudo \
        libcap2-bin \
        iproute2 \
        ethtool \
        iputils-ping \
    && rm -rf /var/lib/apt/lists/* \
    && git config --global http.version "${GIT_HTTP_VERSION}" \
    && printf '%s\n' "Defaults secure_path=\"/usr/local/cargo/bin:${LLVM_PREFIX}/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\"" \
        > /etc/sudoers.d/10-cargo-path \
    && chmod 0440 /etc/sudoers.d/10-cargo-path \
    && visudo -c

# cargo-generate expands templates using the current user name and aborts with
# "could not determine the current user, please set $USER" when it is unset.
# Containers run as root but do not inherit $USER, so define it here. This is
# placed after the expensive install layers so changing it does not invalidate
# the toolchain builds.
ENV USER=root

# Source is bind-mounted here by docker-compose.yml. The image still creates
# the directory so the container also works when started without the mount.
WORKDIR /workspace

# Default to an interactive bash session; compose also sets tty/stdin_open.
CMD ["bash"]
