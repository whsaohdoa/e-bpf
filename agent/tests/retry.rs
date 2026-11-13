//! S-B retry behaviour (§2.6, §12.1): retryable statuses back off along the
//! contract's table, `x-retry-after-ms` wins over the local schedule, and a
//! fatal status stops the agent without further requests.

use std::sync::Arc;
use std::time::Duration;

use agent::agent_loop::{AgentConfig, AgentError, AgentLoop};
use agent::client::Rpc;
use agent::client::tonic::TonicClient;
use agent::clock::{ClockOffset, ManualSleeper, Sleeper};
use agent::error::ServerErrorCode;
use agent::mock::{MockConfig, MockControl, RecordingApply, config_document, wait_for};
use agent::report::ReportSink;

const WINDOW_NS: u64 = 1_000_000_000;

fn fast_mock() -> MockConfig {
    MockConfig {
        heartbeat_interval_ms: 40,
        idle_heartbeat_interval_ms: 40,
        ..MockConfig::default()
    }
}

fn agent(
    mock: &agent::mock::MockHandle,
    apply: RecordingApply,
    sleeper: Arc<ManualSleeper>,
) -> anyhow::Result<AgentLoop<TonicClient, RecordingApply>> {
    let sink = ReportSink::new(ClockOffset::sample()?, WINDOW_NS);
    Ok(AgentLoop::new(
        TonicClient::connect(&mock.client_config())?,
        apply,
        AgentConfig::new("machine-1", env!("CARGO_PKG_VERSION")),
    )
    .with_reports(sink.queue())
    // The contract's milliseconds are asserted below, so the §2.6 jitter is
    // off here only; a deployed agent keeps the jitter on.
    .with_deterministic_backoff()
    .with_sleeper(sleeper as Arc<dyn Sleeper>))
}

#[tokio::test]
async fn unavailable_is_retried_along_the_register_schedule() -> anyhow::Result<()> {
    let mock = MockControl::start(fast_mock()).await?;
    mock.shared().publish(1, config_document());
    // The first two Register attempts fail; the third succeeds.
    mock.shared()
        .fail_next(Rpc::Register, tonic::Status::unavailable("try again"));
    mock.shared()
        .fail_next(Rpc::Register, tonic::Status::unavailable("try again"));

    let apply = RecordingApply::new();
    let sleeper = Arc::new(ManualSleeper::new());
    let mut agent = agent(&mock, apply.clone(), sleeper.clone())?;

    let (tx, rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        agent
            .run(async move {
                let _ = rx.await;
            })
            .await?;
        Ok::<(), AgentError>(())
    });

    wait_for(Duration::from_secs(5), || !apply.applied().is_empty()).await?;
    assert_eq!(mock.shared().calls(Rpc::Register), 3);

    // §2.6: Register backs off 1s, 2s, 4s, capped at 10s.
    let delays = sleeper.requested();
    assert_eq!(
        delays,
        vec![Duration::from_secs(1), Duration::from_secs(2)],
        "the register schedule is transcribed from the contract, not invented"
    );

    let _ = tx.send(());
    task.await??;
    Ok(())
}

#[tokio::test]
async fn deadline_exceeded_is_retried_along_the_get_config_schedule() -> anyhow::Result<()> {
    let mock = MockControl::start(fast_mock()).await?;
    mock.shared().publish(1, config_document());
    mock.shared()
        .fail_next(Rpc::GetConfig, tonic::Status::deadline_exceeded("too slow"));

    let apply = RecordingApply::new();
    let sleeper = Arc::new(ManualSleeper::new());
    let mut agent = agent(&mock, apply.clone(), sleeper.clone())?;

    let (tx, rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        agent
            .run(async move {
                let _ = rx.await;
            })
            .await?;
        Ok::<(), AgentError>(())
    });

    wait_for(Duration::from_secs(5), || !apply.applied().is_empty()).await?;
    assert_eq!(mock.shared().calls(Rpc::GetConfig), 2);
    assert_eq!(
        sleeper.requested(),
        vec![Duration::from_secs(2)],
        "GetConfig starts at 2s per §2.6"
    );

    let _ = tx.send(());
    task.await??;
    Ok(())
}

#[tokio::test]
async fn retry_after_ms_wins_over_the_local_schedule() -> anyhow::Result<()> {
    let mock = MockControl::start(fast_mock()).await?;
    mock.shared().publish(1, config_document());

    let mut status = tonic::Status::unavailable("busy, come back later");
    status
        .metadata_mut()
        .insert("x-retry-after-ms", "3000".parse().unwrap());
    mock.shared().fail_next(Rpc::Register, status);

    let apply = RecordingApply::new();
    let sleeper = Arc::new(ManualSleeper::new());
    let mut agent = agent(&mock, apply.clone(), sleeper.clone())?;

    let (tx, rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        agent
            .run(async move {
                let _ = rx.await;
            })
            .await?;
        Ok::<(), AgentError>(())
    });

    wait_for(Duration::from_secs(5), || !apply.applied().is_empty()).await?;
    assert_eq!(
        sleeper.requested().first().copied(),
        Some(Duration::from_secs(3)),
        "the server's hint replaces the local backoff, as §5 of the stage spec requires"
    );

    let _ = tx.send(());
    task.await??;
    Ok(())
}

#[tokio::test]
async fn a_revoked_agent_stops_without_further_requests() -> anyhow::Result<()> {
    let mock = MockControl::start(MockConfig {
        revoked: true,
        ..fast_mock()
    })
    .await?;
    let apply = RecordingApply::new();
    let sleeper = Arc::new(ManualSleeper::new());
    let mut agent = agent(&mock, apply.clone(), sleeper)?;

    let error = agent
        .run(std::future::pending::<()>())
        .await
        .expect_err("a revoked agent must exit non-zero instead of reporting");

    match error {
        AgentError::Fatal { code, .. } => {
            assert_eq!(code, ServerErrorCode::AgentRevoked);
        }
        other => panic!("expected a fatal error, got {other:?}"),
    }
    assert_eq!(mock.shared().calls(Rpc::GetCursor), 0);
    assert_eq!(mock.shared().calls(Rpc::GetConfig), 0);
    assert_eq!(mock.shared().calls(Rpc::Heartbeat), 0);
    assert_eq!(mock.shared().calls(Rpc::ReportWindows), 0);
    assert_eq!(apply.calls(), 0);

    Ok(())
}
