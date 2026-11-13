//! S-B watch behaviour (§5.7): the real Train 0 control plane has no push
//! channel, so the agent must converge by polling; where the channel exists, a
//! notification must trigger a pull within one heartbeat.

use std::time::Duration;

use agent::agent_loop::{AgentConfig, AgentLoop};
use agent::client::tonic::TonicClient;
use agent::client::{ControlClient, Rpc};
use agent::clock::ClockOffset;
use agent::mock::{MockConfig, MockControl, RecordingApply, config_document, wait_for};
use agent::pb;
use agent::report::ReportSink;

const WINDOW_NS: u64 = 1_000_000_000;

fn fast_mock(watch_supported: bool) -> MockConfig {
    MockConfig {
        watch_supported,
        heartbeat_interval_ms: 40,
        idle_heartbeat_interval_ms: 40,
        ..MockConfig::default()
    }
}

#[tokio::test]
async fn polling_converges_when_the_control_plane_has_no_watch() -> anyhow::Result<()> {
    let mock = MockControl::start(fast_mock(false)).await?;
    let apply = RecordingApply::new();
    let sink = ReportSink::new(ClockOffset::sample()?, WINDOW_NS);
    let client = TonicClient::connect(&mock.client_config())?;
    let mut agent = AgentLoop::new(
        client,
        apply.clone(),
        AgentConfig::new("machine-1", env!("CARGO_PKG_VERSION")),
    )
    .with_reports(sink.queue());

    let (tx, rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        agent
            .run(async move {
                let _ = rx.await;
            })
            .await?;
        Ok::<(), agent::agent_loop::AgentError>(())
    });

    // Let the agent reach its main loop, then publish. Only the periodic
    // heartbeat can carry the news, so this observes the polling path.
    wait_for(Duration::from_secs(5), || {
        mock.shared().calls(Rpc::Heartbeat) > 0
    })
    .await?;
    let watch_calls = mock.shared().calls(Rpc::Watch);
    assert!(watch_calls > 0, "the agent attempts Watch once");

    mock.shared().publish(1, config_document());
    wait_for(Duration::from_secs(5), || !apply.applied().is_empty()).await?;
    assert_eq!(
        apply.applied().first().map(|cfg| cfg.window_ns),
        Some(WINDOW_NS)
    );
    assert!(
        mock.shared().calls(Rpc::Watch) == watch_calls,
        "UNIMPLEMENTED must stop the reconnect loop instead of hammering the control plane"
    );

    let _ = tx.send(());
    task.await??;
    Ok(())
}

#[tokio::test]
async fn watch_returns_none_when_the_control_plane_does_not_implement_it() -> anyhow::Result<()> {
    let mock = MockControl::start(fast_mock(false)).await?;
    let client = TonicClient::connect(&mock.client_config())?;

    let stream = client
        .watch(pb::WatchRequest { current_version: 0 })
        .await
        .expect("UNIMPLEMENTED is not a failure for the S-B client");
    assert!(stream.is_none(), "no push channel means Ok(None)");

    Ok(())
}

#[tokio::test]
async fn a_push_notification_triggers_a_pull() -> anyhow::Result<()> {
    // A one-minute heartbeat makes polling physically unable to explain the
    // apply below: only the notification can.
    let mock = MockControl::start(MockConfig {
        watch_supported: true,
        heartbeat_interval_ms: 60_000,
        idle_heartbeat_interval_ms: 60_000,
        ..MockConfig::default()
    })
    .await?;
    let apply = RecordingApply::new();
    let sink = ReportSink::new(ClockOffset::sample()?, WINDOW_NS);
    let client = TonicClient::connect(&mock.client_config())?;
    let mut agent = AgentLoop::new(
        client,
        apply.clone(),
        AgentConfig::new("machine-1", env!("CARGO_PKG_VERSION")),
    )
    .with_reports(sink.queue());

    let (tx, rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        agent
            .run(async move {
                let _ = rx.await;
            })
            .await?;
        Ok::<(), agent::agent_loop::AgentError>(())
    });

    wait_for(Duration::from_secs(5), || {
        mock.shared().calls(Rpc::Watch) > 0
    })
    .await?;
    let pulls_before = mock.shared().calls(Rpc::GetConfig);

    mock.shared().publish(1, config_document());
    wait_for(Duration::from_secs(5), || !apply.applied().is_empty()).await?;
    assert!(
        mock.shared().calls(Rpc::GetConfig) > pulls_before,
        "a notification only carries a version; the document comes from GetConfig"
    );

    let _ = tx.send(());
    task.await??;
    Ok(())
}
