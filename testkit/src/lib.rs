//! Test-only building blocks shared by `xtask` and the privileged e2e suite.
//!
//! Everything here is Linux specific and has no dependency outside `std`: the
//! few libc entry points we need (`socket`, `bind`, `sendto`, `kill`, ...) are
//! declared by hand. Keeping the crate dependency free lets `xtask` link it
//! without pulling in the probe dependency graph.

pub mod csv;
pub mod frames;
pub mod net;
pub mod probe;
pub mod raw;

use std::path::{Path, PathBuf};

/// Returns the Cargo workspace root, i.e. the `probe` checkout directory.
pub fn workspace_root() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .expect("testkit lives one level below the workspace root")
        .to_path_buf()
}

/// Directory holding the `index.tsv` frame matrix and its `*.bin` payloads.
pub fn frames_dir() -> PathBuf {
    workspace_root().join("probe/tests/fixtures/frames")
}
