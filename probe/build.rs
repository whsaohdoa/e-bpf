use anyhow::{Context as _, anyhow};
use aya_build::Toolchain;

fn main() -> anyhow::Result<()> {
    // The eBPF object is built by a nested `cargo build --target bpfel-unknown-none`
    // inside aya-build, and it cannot be instrumented: that target has no profiler
    // runtime, so `cargo llvm-cov`'s `-C instrument-coverage` makes the nested
    // rustc fail with `E0463: can't find crate for profiler_builtins`. The nested
    // invocation inherits those flags in ways the build script cannot strip, so
    // coverage instead builds the object once without instrumentation and points
    // PROBE_EBPF_PREBUILT at it (see scripts/ci/coverage.sh). aya-build copies the
    // object to `$OUT_DIR/<target name>`, which is what main.rs includes.
    println!("cargo:rerun-if-env-changed=PROBE_EBPF_PREBUILT");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_SMALL_MAPS");
    if let Some(prebuilt) = std::env::var_os("PROBE_EBPF_PREBUILT") {
        let prebuilt = std::path::PathBuf::from(prebuilt);
        let out_dir = std::path::PathBuf::from(std::env::var_os("OUT_DIR").context("OUT_DIR")?);
        std::fs::copy(&prebuilt, out_dir.join("probe"))
            .with_context(|| format!("copy {} to {}", prebuilt.display(), out_dir.display()))?;
        return Ok(());
    }

    let cargo_metadata::Metadata { packages, .. } = cargo_metadata::MetadataCommand::new()
        .no_deps()
        .exec()
        .context("MetadataCommand::exec")?;
    let ebpf_package = packages
        .into_iter()
        .find(|cargo_metadata::Package { name, .. }| name.as_str() == "probe-ebpf")
        .ok_or_else(|| anyhow!("probe-ebpf package not found"))?;
    let cargo_metadata::Package {
        name,
        manifest_path,
        ..
    } = ebpf_package;
    // aya-build runs its own cargo invocation for the data path, so a feature
    // enabled on this package does not reach it: forward it explicitly. The
    // `small-maps` feature shrinks the AGG map so the e2e suite can exercise the
    // overflow counter (E-06) with a handful of keys.
    let ebpf_features: Vec<&str> = if std::env::var_os("CARGO_FEATURE_SMALL_MAPS").is_some() {
        vec!["small-maps"]
    } else {
        Vec::new()
    };
    let ebpf_package = aya_build::Package {
        name: name.as_str(),
        root_dir: manifest_path
            .parent()
            .ok_or_else(|| anyhow!("no parent for {manifest_path}"))?
            .as_str(),
        features: &ebpf_features,
        ..Default::default()
    };
    aya_build::build_ebpf([ebpf_package], Toolchain::default())
}
