//! Thin binary wrapper around [`probe::app::main`].
//!
//! The loader itself lives in the library so integration tests can drive the
//! same code with a fake kernel (see `tests/pipeline.rs`).

fn main() -> std::process::ExitCode {
    probe::app::main()
}
