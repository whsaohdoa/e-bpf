//! Compiles `proto/control.proto` into Rust types.
//!
//! The contract (`docs/Control-Agent-API-v1.md`, §4) owns the proto text; this
//! crate ships a byte-identical copy so a checkout without the contract can
//! still build. `prost-build` shells out to `protoc`, which is not part of the
//! Rust toolchain — see CONTRIBUTING.md.

use std::path::PathBuf;

fn main() {
    let proto_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("proto");
    let proto = proto_dir.join("control.proto");

    println!("cargo:rerun-if-changed=proto/control.proto");
    println!("cargo:rerun-if-env-changed=PROTOC");

    prost_build::Config::new()
        .compile_protos(&[proto], &[proto_dir])
        .expect("failed to compile proto/control.proto (is protoc on PATH?)");
}
