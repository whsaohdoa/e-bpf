//! Compiles `proto/control.proto` into Rust types plus the gRPC stubs.
//!
//! The contract (`docs/Control-Agent-API-v1.md`, §4) owns the proto text; this
//! crate ships a byte-identical copy so building the proto does not depend on
//! parsing the prose contract.
//!
//! `prost-build` shells out to `protoc`. Instead of relying on whatever the
//! host installed — on Ubuntu `protobuf-compiler` does not ship the well-known
//! type `.proto` files, `libprotobuf-dev` does — this script points the build
//! at the `protoc` binary and include directory bundled by
//! `protoc-bin-vendored`, so CI and developer machines run the same generator.
//!
//! Both halves are generated: the agent needs the client, and the mock control
//! plane (`src/mock/`, `control-mock` bin) needs the server trait. Generating
//! both keeps the two sides provably in sync with the one proto file.

use std::path::PathBuf;

fn main() {
    let proto_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("proto");
    let proto = proto_dir.join("control.proto");

    println!("cargo:rerun-if-changed=proto/control.proto");

    let protoc = protoc_bin_vendored::protoc_bin_path()
        .expect("protoc-bin-vendored has no protoc binary for this platform");
    let protoc_include = protoc_bin_vendored::include_path()
        .expect("protoc-bin-vendored has no include directory for this platform");

    // `prost-build` reads both variables when it invokes `protoc` below.
    // SAFETY: build scripts run as the only thread of their process, and
    // nothing else touches the environment between these writes and the call.
    unsafe {
        std::env::set_var("PROTOC", protoc);
        std::env::set_var("PROTOC_INCLUDE", protoc_include);
    }

    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_protos(&[proto], &[proto_dir])
        .expect("failed to compile proto/control.proto");
}
