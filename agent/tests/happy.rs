//! S-B happy paths: register → cursor → config → validate → apply → ack, the
//! heartbeat that reports the applied config, and window reporting that does not
//! re-submit what the cursor already covers.

use std::time::Duration;

use agent::{
    agent_loop::{AgentConfig, AgentLoop},
    client::{Rpc, tonic::TonicClient},
    clock::ClockOffset,
    error::AckErrorCode,
    mock::{MockConfig, MockControl, RecordingApply, config_document, wait_for},
    pb,
    report::ReportSink,
};
use probe::{aggregate::AggRecord, sink::AggSink};
use probe_common::AggKey;

const WINDOW_NS: u64 = 1_000_000_000;

/// A closed aggregate row for `window_id`; distinct `dst_port` per key keeps
/// the mock's `(window_id, key_hash)` dedupe honest.
fn row(window_id: u64, dst_port: u16) -> AggRecord {
    AggRecord {
        window_id,
        key: AggKey {
            window_id,
            dst_port,
            ..AggKey::default()
        },
        count: 3,
        first_seen: window_id * WINDOW_NS + 10,
        last_seen: window_id * WINDOW_NS + 900_000_000,
        sample_dropped: false,
        degraded: false,
        window_complete: true,
    }
}

fn fast_mock() -> MockConfig {
    MockConfig {
        heartbeat_interval_ms: 40,
        idle_heartbeat_interval_ms: 40,
        ..MockConfig::default()
    }
}

struct Harness {
    mock: agent::mock::MockHandle,
    apply: RecordingApply,
    sink: ReportSink,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Harness {
    async fn start(mock: agent::mock::MockHandle) -> anyhow::Result<Self> {
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
            Ok(())
        });
        Ok(Self {
            mock,
            apply,
            sink,
            shutdown: Some(tx),
            task,
        })
    }

    fn write(&mut self, rows: &[AggRecord]) {
        self.sink.write_rows(rows).expect("queue rows");
    }

    async fn stop(mut self) -> anyhow::Result<()> {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        self.task.await?
    }
}

#[tokio::test]
async fn applies_the_published_config_and_reports_it_in_the_heartbeat() -> anyhow::Result<()> {
    let mock = MockControl::start(fast_mock()).await?;
    mock.shared().publish(1, config_document());
    let mut harness = Harness::start(mock).await?;

    // §5.3/§5.4: the document is validated, applied, and acknowledged APPLIED.
    wait_for(Duration::from_secs(5), || {
        !harness.mock.shared().acks().is_empty()
    })
    .await?;
    let ack = harness.mock.shared().acks()[0].clone();
    assert_eq!(ack.version, 1);
    assert_eq!(ack.result, pb::AckResult::Applied as i32);
    assert_eq!(
        ack.content_sha256.len(),
        32,
        "sha256 is the published digest"
    );
    assert_eq!(
        harness.apply.applied().first().map(|cfg| cfg.window_ns),
        Some(WINDOW_NS),
        "the applied config is the one served by the control plane"
    );

    // §5.2: once caught up, the heartbeat reports APPLIED and the new version.
    wait_for(Duration::from_secs(5), || {
        harness
            .mock
            .shared()
            .heartbeats()
            .iter()
            .any(|hb| hb.config_state == pb::ConfigState::Applied as i32 && hb.current_version == 1)
    })
    .await?;

    // §5.5: two windows go out, and the response cursor advances so the next
    // batch only carries what the control plane has not seen.
    harness.write(&[row(1, 1000), row(1, 1001), row(2, 1000), row(2, 1001)]);
    wait_for(Duration::from_secs(5), || {
        harness.mock.shared().cursor() == Some(2)
    })
    .await?;
    let first = harness.mock.shared().reports();
    assert_eq!(first.len(), 1, "both windows fit in one batch");
    assert_eq!(first[0].windows.len(), 2);
    assert!(
        first[0].windows.iter().all(|w| w.aggregates.len() == 2),
        "each window keeps its two keys"
    );

    harness.write(&[row(3, 1000)]);
    wait_for(Duration::from_secs(5), || {
        harness.mock.shared().cursor() == Some(3)
    })
    .await?;

    // No window may be submitted twice: the cursor handed back by
    // ReportWindows suppresses the second submission.
    let mut seen: Vec<u64> = Vec::new();
    for report in harness.mock.shared().reports() {
        for window in report.windows {
            if seen.contains(&window.window_id) {
                panic!("window {} was submitted twice", window.window_id);
            }
            seen.push(window.window_id);
        }
    }
    assert_eq!(seen, vec![1, 2, 3]);

    harness.stop().await?;
    Ok(())
}

#[tokio::test]
async fn hash_mismatch_is_rejected_before_apply() -> anyhow::Result<()> {
    let mock = MockControl::start(fast_mock()).await?;
    mock.shared().publish_corrupt_hash(1, config_document());
    let harness = Harness::start(mock).await?;

    wait_for(Duration::from_secs(5), || {
        !harness.mock.shared().acks().is_empty()
    })
    .await?;
    let ack = harness.mock.shared().acks()[0].clone();
    assert_eq!(ack.result, pb::AckResult::Rejected as i32);
    assert_eq!(ack.error_code, AckErrorCode::HashMismatch.as_str());
    assert_eq!(
        harness.apply.calls(),
        0,
        "a document whose hash does not match the envelope must not reach the data path"
    );

    harness.stop().await?;
    Ok(())
}

#[tokio::test]
async fn a_document_not_newer_than_the_applied_one_is_rejected_as_stale() -> anyhow::Result<()> {
    let mock = MockControl::start(fast_mock()).await?;
    mock.shared().publish(2, config_document());
    let harness = Harness::start(mock).await?;

    wait_for(Duration::from_secs(5), || {
        !harness.apply.applied().is_empty()
    })
    .await?;
    let applied_at = harness.apply.calls();
    assert_eq!(applied_at, 1);

    // Announce a newer version while still serving the version 2 document: the
    // agent pulls, and §5.3's first check rejects it without touching the data
    // path.
    harness.mock.shared().announce_version(3);
    wait_for(Duration::from_secs(5), || {
        harness
            .mock
            .shared()
            .acks()
            .iter()
            .any(|ack| ack.error_code == AckErrorCode::StaleVersion.as_str())
    })
    .await?;

    let stale = harness
        .mock
        .shared()
        .acks()
        .into_iter()
        .find(|ack| ack.error_code == AckErrorCode::StaleVersion.as_str())
        .expect("stale ack recorded");
    assert_eq!(stale.result, pb::AckResult::Rejected as i32);
    assert_eq!(stale.version, 2, "the ack describes the document served");
    assert_eq!(
        harness.apply.calls(),
        1,
        "a stale document is never applied"
    );
    assert!(
        harness.mock.shared().calls(Rpc::GetConfig) > 0,
        "the pull happened"
    );

    harness.stop().await?;
    Ok(())
}
