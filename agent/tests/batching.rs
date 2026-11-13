//! S-B batching limits (§2.7, §5.5): ≥1001 windows go out in more than one
//! batch, a window larger than the message limit is dropped instead of killing
//! the agent, and the cursor handed back by `ReportWindows` prevents a window
//! from being submitted twice.

use std::time::Duration;

use agent::agent_loop::{AgentConfig, AgentError, AgentLoop};
use agent::client::tonic::TonicClient;
use agent::clock::ClockOffset;
use agent::mock::{MockConfig, MockControl, RecordingApply, wait_for, window_report};
use agent::report::{MAX_WINDOWS_PER_BATCH, ReportSink};
use probe::aggregate::AggRecord;
use probe::sink::AggSink;
use probe_common::AggKey;

const WINDOW_NS: u64 = 1_000_000_000;

fn fast_mock(initial_cursor: Option<u64>) -> MockConfig {
    MockConfig {
        heartbeat_interval_ms: 40,
        idle_heartbeat_interval_ms: 40,
        initial_cursor,
        ..MockConfig::default()
    }
}

fn row(window_id: u64) -> AggRecord {
    AggRecord {
        window_id,
        key: AggKey {
            window_id,
            dst_port: 1000,
            ..AggKey::default()
        },
        count: 1,
        first_seen: window_id * WINDOW_NS,
        last_seen: window_id * WINDOW_NS + 1,
        sample_dropped: false,
        degraded: false,
        window_complete: true,
    }
}

fn agent(
    mock: &agent::mock::MockHandle,
    apply: RecordingApply,
    sink: &ReportSink,
) -> anyhow::Result<AgentLoop<TonicClient, RecordingApply>> {
    Ok(AgentLoop::new(
        TonicClient::connect(&mock.client_config())?,
        apply,
        AgentConfig::new("machine-1", env!("CARGO_PKG_VERSION")),
    )
    .with_reports(sink.queue()))
}

#[tokio::test]
async fn more_than_a_thousand_windows_are_split_into_batches() -> anyhow::Result<()> {
    let mock = MockControl::start(fast_mock(None)).await?;
    let apply = RecordingApply::new();
    let mut sink = ReportSink::new(ClockOffset::sample()?, WINDOW_NS);
    let mut agent = agent(&mock, apply, &sink)?;

    let (tx, rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        agent
            .run(async move {
                let _ = rx.await;
            })
            .await?;
        Ok::<(), AgentError>(())
    });

    let rows: Vec<AggRecord> = (1..=1001).map(row).collect();
    sink.write_rows(&rows)?;

    wait_for(Duration::from_secs(20), || {
        mock.shared().cursor() == Some(1001)
    })
    .await?;

    let reports = mock.shared().reports();
    assert!(reports.len() >= 2, "1001 windows cannot fit in one batch");
    let mut ids: Vec<u64> = Vec::new();
    for report in &reports {
        assert!(
            report.windows.len() <= MAX_WINDOWS_PER_BATCH,
            "a batch stays within the contract's window count"
        );
        ids.extend(report.windows.iter().map(|window| window.window_id));
    }
    assert_eq!(ids.len(), 1001, "every window is submitted exactly once");
    ids.sort_unstable();
    assert_eq!(ids, (1..=1001).collect::<Vec<_>>());

    let _ = tx.send(());
    task.await??;
    Ok(())
}

#[tokio::test]
async fn an_oversize_window_is_dropped_and_the_rest_still_reports() -> anyhow::Result<()> {
    let mock = MockControl::start(fast_mock(None)).await?;
    let apply = RecordingApply::new();
    let sink = ReportSink::new(ClockOffset::sample()?, WINDOW_NS);
    let mut agent = agent(&mock, apply, &sink)?;

    let (tx, rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        agent
            .run(async move {
                let _ = rx.await;
            })
            .await?;
        Ok::<(), AgentError>(())
    });

    // One window far beyond the 4 MiB message limit: ~150k aggregates of a
    // 32-byte key each.
    let mut oversize = window_report(1, 1000);
    oversize.aggregates = (0..150_000)
        .map(|index| agent::pb::Aggregate {
            key_hash: vec![(index % 251) as u8; 32],
            packets: 1,
            first_seen_offset_ns: 0,
            last_seen_offset_ns: 999_000_000,
            sample_dropped: false,
            degraded: false,
        })
        .collect();
    let encoded = prost::Message::encoded_len(&oversize);
    assert!(
        encoded > 4 * 1024 * 1024,
        "the fixture must exceed the 4 MiB limit, got {encoded} bytes"
    );

    sink.queue().push(oversize);
    sink.queue().push(window_report(2, 1000));

    wait_for(Duration::from_secs(20), || {
        mock.shared().cursor() == Some(2)
    })
    .await?;

    let ids: Vec<u64> = mock
        .shared()
        .reports()
        .iter()
        .flat_map(|report| report.windows.iter().map(|window| window.window_id))
        .collect();
    assert_eq!(
        ids,
        vec![2],
        "the oversize window is dropped, not retried forever and not fatal"
    );

    let _ = tx.send(());
    task.await??;
    Ok(())
}

#[tokio::test]
async fn windows_at_or_below_the_cursor_are_never_resubmitted() -> anyhow::Result<()> {
    let mock = MockControl::start(fast_mock(Some(3))).await?;
    let apply = RecordingApply::new();
    let sink = ReportSink::new(ClockOffset::sample()?, WINDOW_NS);
    let mut agent = agent(&mock, apply, &sink)?;

    let (tx, rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        agent
            .run(async move {
                let _ = rx.await;
            })
            .await?;
        Ok::<(), AgentError>(())
    });

    for window_id in 1..=5 {
        sink.queue().push(window_report(window_id, 1000));
    }

    wait_for(Duration::from_secs(10), || {
        mock.shared().cursor() == Some(5)
    })
    .await?;

    let ids: Vec<u64> = mock
        .shared()
        .reports()
        .iter()
        .flat_map(|report| report.windows.iter().map(|window| window.window_id))
        .collect();
    assert_eq!(
        ids,
        vec![4, 5],
        "§5.6: the control plane's cursor is authoritative, so 1..=3 are skipped"
    );

    let _ = tx.send(());
    task.await??;
    Ok(())
}
