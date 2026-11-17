//! `probe-agent` as a process: the command line contract (§6 of the S-C plan)
//! and the shutdown path the binary relies on (§5 of the contract).
//!
//! The CLI half runs the real binary as a child, because what it asserts is
//! exactly the process boundary clap owns: which flags are required, which exit
//! codes a usage error produces, and whether a rejected value names the offending
//! path.  The loop half runs [`AgentLoop`] against the in-process mock with a
//! fake data plane, so it needs no privileges and no kernel.

use std::{
    process::{Command, Output},
    time::Duration,
};

use agent::agent_loop::{AgentConfig, AgentLoop};
use agent::clock::ClockOffset;
use agent::mock::{MockConfig, MockControl, RecordingApply, config_document, wait_for};
use agent::pb;
use agent::report::ReportSink;

/// The binary under test, built by cargo for this integration test.
const BIN: &str = env!("CARGO_BIN_EXE_probe-agent");

/// A directory that only this test uses, for PEM placeholders.
fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("probe-agent-cli-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create a scratch directory");
    dir
}

/// Runs the binary with `args` and returns its output.
fn run(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .output()
        .expect("spawn probe-agent")
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn help_lists_the_contract_flags_and_exits_successfully() {
    let output = run(&["--help"]);
    assert!(
        output.status.success(),
        "--help must exit 0, got {:?}: {}",
        output.status,
        stderr_of(&output)
    );
    let stdout = stdout_of(&output);
    for flag in [
        "--iface",
        "--control-endpoint",
        "--ca",
        "--cert",
        "--key",
        "--machine-id",
        "--state-dir",
        "--csv-dir",
        "--window-ms",
        "--grace-ms",
        "--flush-ms",
        "--scan-ms",
    ] {
        assert!(
            stdout.contains(flag),
            "--help does not document {flag}:\n{stdout}"
        );
    }
    assert!(
        stdout.contains("https://127.0.0.1:9090"),
        "--control-endpoint's default must be visible:\n{stdout}"
    );
}

#[test]
fn a_missing_required_flag_is_a_usage_error() {
    // `--ca` is required: without it clap must stop with exit code 2, before any
    // file or kernel access.
    let output = run(&[
        "--iface",
        "lo",
        "--cert",
        "/nonexistent/probe/cert.pem",
        "--key",
        "/nonexistent/probe/key.pem",
    ]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "a missing required flag is a usage error (2), got {:?}: {}",
        output.status,
        stderr_of(&output)
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("--ca"),
        "the message must name --ca:\n{stderr}"
    );
}

#[test]
fn a_window_below_the_contract_minimum_is_rejected() {
    // The window range is checked before the PEM files are read, so placeholder
    // paths are enough: the flag must be rejected on its own.
    let dir = scratch("window");
    let output = run(&[
        "--iface",
        "lo",
        "--ca",
        dir.join("ca.pem").to_str().expect("utf-8 path"),
        "--cert",
        dir.join("cert.pem").to_str().expect("utf-8 path"),
        "--key",
        dir.join("key.pem").to_str().expect("utf-8 path"),
        "--window-ms",
        "999",
    ]);
    assert!(!output.status.success(), "--window-ms 999 must be rejected");
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("--window-ms") && stderr.contains("999"),
        "the message must name the flag and the value:\n{stderr}"
    );
    assert!(
        stderr.contains("1000"),
        "the message must name the accepted minimum:\n{stderr}"
    );
}

#[test]
fn a_missing_certificate_names_its_path() {
    let dir = scratch("cert");
    let ca = dir.join("ca.pem");
    let key = dir.join("key.pem");
    std::fs::write(&ca, b"not really a PEM").expect("write the CA placeholder");
    std::fs::write(&key, b"not really a PEM").expect("write the key placeholder");
    let missing = dir.join("absent-cert.pem");

    let output = run(&[
        "--iface",
        "lo",
        "--ca",
        ca.to_str().expect("utf-8 path"),
        "--cert",
        missing.to_str().expect("utf-8 path"),
        "--key",
        key.to_str().expect("utf-8 path"),
    ]);
    assert!(
        !output.status.success(),
        "a missing certificate must fail, got {:?}",
        output.status
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains(missing.to_str().expect("utf-8 path")),
        "the error must name the path {}, got:\n{stderr}",
        missing.display()
    );
}

/// A mock that heartbeats often enough for a test to finish in a second.
fn fast_mock() -> MockConfig {
    MockConfig {
        heartbeat_interval_ms: 40,
        idle_heartbeat_interval_ms: 40,
        ..MockConfig::default()
    }
}

#[tokio::test]
async fn the_shutdown_path_applies_the_document_and_flushes_open_windows_once() -> anyhow::Result<()>
{
    let mock = MockControl::start(fast_mock()).await?;
    mock.shared().publish(1, config_document());

    let apply = RecordingApply::new();
    let sink = ReportSink::new(ClockOffset::sample()?, 1_000_000_000);
    let client = agent::client::tonic::TonicClient::connect(&mock.client_config())?;
    let mut agent = AgentLoop::new(
        client,
        apply.clone(),
        AgentConfig::new("machine-1", env!("CARGO_PKG_VERSION")),
    )
    .with_reports(sink.queue());

    // The binary's SIGTERM handler resolves this future; from the loop's point of
    // view it is the same "shut down now" edge.
    let (tx, rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        agent
            .run(async move {
                let _ = rx.await;
            })
            .await
    });

    wait_for(Duration::from_secs(5), || !mock.shared().acks().is_empty()).await?;
    let ack = mock.shared().acks()[0].clone();
    assert_eq!(ack.version, 1);
    assert_eq!(ack.result, pb::AckResult::Applied as i32);

    let _ = tx.send(());
    let result = task.await?;
    assert!(
        result.is_ok(),
        "a shutdown signal is a graceful stop, got {result:?}"
    );

    assert_eq!(
        apply.applied().len(),
        1,
        "the published document is applied exactly once"
    );
    assert_eq!(
        apply.flushes(),
        1,
        "the open windows are flushed exactly once before the final report"
    );
    assert!(
        apply.drains() > 0,
        "the loop keeps draining the ring while it waits"
    );

    Ok(())
}
