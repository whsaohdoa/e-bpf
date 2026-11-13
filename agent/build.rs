//! Compiles `proto/control.proto` into Rust types plus the gRPC stubs.
//!
//! The contract (`docs/Control-Agent-API-v1.md`, §4) owns the proto text; this
//! crate ships a byte-identical copy so a checkout without the contract can
//! still build. `prost-build` shells out to `protoc`, which is not part of the
//! Rust toolchain — see CONTRIBUTING.md.
//!
//! Both halves are generated: the agent needs the client, and the mock control
//! plane (`src/mock/`, `control-mock` bin) needs the server trait. Generating
//! both keeps the two sides provably in sync with the one proto file.

use std::path::PathBuf;

fn main() {
    let proto_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("proto");
    let proto = proto_dir.join("control.proto");

    println!("cargo:rerun-if-changed=proto/control.proto");
    println!("cargo:rerun-if-env-changed=PROTOC");

    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_protos(&[proto], &[proto_dir])
        .expect("failed to compile proto/control.proto (is protoc on PATH?)");
}
