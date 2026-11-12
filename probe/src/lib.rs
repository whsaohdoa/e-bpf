//! Userspace half of the Train 0 XDP probe.
//!
//! The binary in this package is a thin wrapper around [`app::main`]; everything
//! that decides what a packet becomes lives in a module that can be tested
//! without a kernel, a network interface or root:
//!
//! * [`aggregate`] -- window close watermark, per-CPU slot merging, sample
//!   de-duplication, CSV field rendering,
//! * [`pipeline`] -- the ring drain / window close / write state machine,
//! * [`ports`] -- the three traits the pipeline drives (ring buffer, `AGG` map,
//!   clock) plus the real adapters around them,
//! * [`sink`] -- the CSV writers,
//! * [`app`] -- eBPF loading, `CONFIG` handshake, attach/detach, tokio loop, CLI.
//!
//! Splitting the pipeline out of the binary is what lets `tests/pipeline.rs`
//! drive A2/A3/A6/A7 in-process: it builds a [`pipeline::Session`] with a
//! scripted ring buffer and a fake `AGG` map, so the assertions run in
//! milliseconds instead of requiring a privileged end-to-end run.
pub mod aggregate;
pub mod app;
pub mod pipeline;
pub mod ports;
pub mod sink;
