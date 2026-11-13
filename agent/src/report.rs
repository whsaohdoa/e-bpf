//! The reporting port: `AggRecord` → `WindowReport` and batch cutting.
//!
//! This is the write side of the aggregation pipeline that the probe already
//! drives (`probe::sink::AggSink`): the scan loop hands over merged rows, this
//! module converts them into contract-shaped windows and parks them in a queue,
//! and the state machine drains the queue in batches that respect §2.7.
//!
//! Two conversions deserve attention:
//!
//! - `key_hash` is computed once, here, from the aggregation key
//!   (`crate::keyhash`). The control plane never recomputes it (§5.5): a hash
//!   that differs between the two sides would turn every report into an alert.
//! - `first_seen`/`last_seen` are monotonic nanoseconds. The wire carries them
//!   as *offsets from the window start* (proto comments in `control.proto`), so
//!   they are plain subtractions and need no clock at all. Only
//!   `window_start` — a display value — is converted to wall clock (§9);
//!   alignment always uses `window_id`, which is monotonic bucket arithmetic
//!   (`ts_ns / window_ns`) and therefore stable across the two clocks.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use anyhow::Result;
use probe::{
    aggregate::{AggRecord, window_start_ns},
    sink::AggSink,
};
use prost::Message as _;

use crate::{clock::ClockOffset, keyhash, pb};

/// §2.7: at most 1000 windows per `ReportWindows` call.
pub const MAX_WINDOWS_PER_BATCH: usize = 1_000;

/// §2.7: at most 4 MiB per `ReportWindows` call.
pub const MAX_BATCH_BYTES: usize = 4 * 1024 * 1024;

/// Nanoseconds per millisecond, for `WindowReport.window_ms`.
const NANOS_PER_MS: u64 = 1_000_000;

/// An [`AggSink`] that queues contract windows for the state machine.
///
/// `write_rows` and `flush` are O(rows) and never block on the network: the
/// probe's scan loop must not care whether the control plane is reachable.
pub struct ReportSink {
    queue: ReportQueue,
    clock: ClockOffset,
    window_ns: u64,
}

impl ReportSink {
    /// `window_ns` is the window length of the *applied* configuration; it is
    /// part of the wire format (`window_ms`) and of the offset arithmetic.
    pub fn new(clock: ClockOffset, window_ns: u64) -> Self {
        Self {
            queue: ReportQueue::default(),
            clock,
            window_ns,
        }
    }

    /// A handle the state machine (or a test) uses to drain the queue.
    pub fn queue(&self) -> ReportQueue {
        self.queue.clone()
    }
}

impl AggSink for ReportSink {
    fn write_rows(&mut self, rows: &[AggRecord]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let window_ms = (self.window_ns / NANOS_PER_MS) as u32;
        // Rows for one window are usually adjacent, but nothing in the pipeline
        // promises it, so windows are collected by id. The number of distinct
        // windows per call is tiny (one scan covers one window), so a linear
        // search beats a map's allocation.
        let mut windows: Vec<pb::WindowReport> = Vec::new();
        for row in rows {
            let window_start_mono = window_start_ns(row.window_id, self.window_ns);
            let aggregate = pb::Aggregate {
                key_hash: keyhash::key_hash(&row.key).to_vec(),
                packets: row.count,
                // Offsets from the window start, in monotonic nanoseconds: both
                // sides share the time base, so no clock conversion applies.
                first_seen_offset_ns: row.first_seen.saturating_sub(window_start_mono),
                last_seen_offset_ns: row.last_seen.saturating_sub(window_start_mono),
                sample_dropped: row.sample_dropped,
                degraded: row.degraded,
            };
            match windows.iter_mut().find(|w| w.window_id == row.window_id) {
                Some(window) => window.aggregates.push(aggregate),
                None => windows.push(pb::WindowReport {
                    window_id: row.window_id,
                    window_ms,
                    complete: row.window_complete,
                    // Display value only; alignment uses `window_id` (§9).
                    window_start: Some(self.clock.timestamp(window_start_mono)),
                    aggregates: vec![aggregate],
                    truncated: false,
                    samples: Vec::new(),
                }),
            }
        }
        self.queue.extend(windows);
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        // Windows are queued as they are written; there is nothing buffered
        // locally that a flush could push out. The state machine's own flush
        // point is `ReportQueue::pop_batch`.
        Ok(())
    }
}

/// Shared, ordered queue of windows waiting to be reported.
///
/// Cloneable and `Send + Sync`: the probe thread writes, the agent loop drains.
#[derive(Clone, Debug, Default)]
pub struct ReportQueue {
    queue: Arc<Mutex<VecDeque<pb::WindowReport>>>,
}

impl ReportQueue {
    /// Number of windows waiting (heartbeat's `pending_reports`).
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Largest queued window id, if any.
    pub fn max_window_id(&self) -> Option<u64> {
        self.lock().back().map(|w| w.window_id)
    }

    /// Appends windows in order.
    pub fn extend(&self, windows: impl IntoIterator<Item = pb::WindowReport>) {
        let mut queue = self.lock();
        queue.extend(windows);
    }

    /// Appends one window.
    pub fn push(&self, window: pb::WindowReport) {
        self.lock().push_back(window);
    }

    /// Drops queued windows at or below `cursor`, returning how many were
    /// removed.
    ///
    /// §5.6 makes the control plane's cursor authoritative, so a window at or
    /// below it is already committed: resubmitting it would only produce
    /// `duplicates` and waste a request. The queue is ordered by window id
    /// (`ReportSink` writes windows in scan order), so this drains the front.
    pub fn drop_through(&self, cursor: u64) -> usize {
        let mut queue = self.lock();
        let before = queue.len();
        while queue.front().is_some_and(|w| w.window_id <= cursor) {
            queue.pop_front();
        }
        before - queue.len()
    }

    /// Takes the next batch, respecting §2.7.
    ///
    /// A window is never split: a batch stops *before* the window that would
    /// overflow either limit, and the next call picks it up. A single window
    /// that is already larger than the byte limit can never be sent, so it is
    /// dropped with an error instead of being retried forever (§2.7 has no
    /// other remedy; the control plane would answer `report.batch_too_large`
    /// to every attempt).
    pub fn pop_batch(&self) -> Option<pb::ReportWindowsRequest> {
        let mut queue = self.lock();
        if queue.is_empty() {
            return None;
        }
        let mut windows: Vec<pb::WindowReport> = Vec::new();
        let mut bytes = 0usize;
        while let Some(front) = queue.front() {
            let size = front.encoded_len();
            if size > MAX_BATCH_BYTES {
                let dropped = queue.pop_front().expect("front() just saw it");
                log::error!(
                    "dropping window {}: {} bytes exceeds the {MAX_BATCH_BYTES}-byte report limit",
                    dropped.window_id,
                    size
                );
                continue;
            }
            if !windows.is_empty()
                && (windows.len() >= MAX_WINDOWS_PER_BATCH || bytes + size > MAX_BATCH_BYTES)
            {
                break;
            }
            bytes += size;
            windows.push(queue.pop_front().expect("front() just saw it"));
        }
        if windows.is_empty() {
            // Everything left was oversized; try again next round with
            // whatever arrives in between rather than returning an empty call.
            return None;
        }
        Some(pb::ReportWindowsRequest { windows })
    }

    fn lock(&self) -> MutexGuard<'_, VecDeque<pb::WindowReport>> {
        self.queue.lock().expect("report queue poisoned")
    }
}

/// Deadline for the final partial-window report of a graceful shutdown (§2.6:
/// the whole shutdown has a 5s budget, of which this report may take 2s).
pub const SHUTDOWN_REPORT_DEADLINE: Duration = Duration::from_secs(2);

#[cfg(test)]
mod tests {
    use super::*;
    use probe_common::AggKey;

    fn key(seed: u8) -> AggKey {
        let mut key = AggKey::default();
        key.dst_addr[0] = seed;
        key
    }

    fn row(window_id: u64, seed: u8) -> AggRecord {
        AggRecord {
            window_id,
            key: key(seed),
            count: 3,
            first_seen: window_id * 1_000_000_000 + 10,
            last_seen: window_id * 1_000_000_000 + 900,
            sample_dropped: false,
            degraded: false,
            window_complete: true,
        }
    }

    fn sink() -> ReportSink {
        // 1s windows; wall clock and monotonic sampled at the same instant.
        ReportSink::new(
            ClockOffset::new(1_700_000_000_000_000_000, 0),
            1_000_000_000,
        )
    }

    #[test]
    fn rows_become_one_window_with_offsets() {
        let mut sink = sink();
        AggSink::write_rows(&mut sink, &[row(7, 1), row(7, 2), row(8, 3)]).expect("write_rows");

        let queue = sink.queue();
        assert_eq!(queue.len(), 2);
        let batch = queue.pop_batch().expect("one batch");
        assert_eq!(batch.windows.len(), 2);

        let first = &batch.windows[0];
        assert_eq!(first.window_id, 7);
        assert_eq!(first.window_ms, 1_000);
        assert!(first.complete);
        assert_eq!(first.aggregates.len(), 2);
        assert_eq!(first.aggregates[0].packets, 3);
        assert_eq!(first.aggregates[0].first_seen_offset_ns, 10);
        assert_eq!(first.aggregates[0].last_seen_offset_ns, 900);
        // window 7 starts at 7s monotonic = 1_700_000_007s wall clock.
        let start = first.window_start.as_ref().expect("window_start");
        assert_eq!(start.seconds, 1_700_000_007);
        assert_eq!(start.nanos, 0);

        assert_eq!(batch.windows[1].window_id, 8);
        assert!(queue.is_empty());
    }

    #[test]
    fn key_hash_is_the_thirty_two_byte_digest() {
        let mut sink = sink();
        AggSink::write_rows(&mut sink, &[row(1, 9)]).expect("write_rows");
        let batch = sink.queue().pop_batch().expect("one batch");
        let hash = &batch.windows[0].aggregates[0].key_hash;
        assert_eq!(hash.len(), 32);
        assert_eq!(hash, &keyhash::key_hash(&key(9)));
    }

    #[test]
    fn thousand_and_one_windows_are_two_batches() {
        let queue = ReportQueue::default();
        for window_id in 0..=1_000u64 {
            queue.push(pb::WindowReport {
                window_id,
                window_ms: 1_000,
                ..Default::default()
            });
        }
        let first = queue.pop_batch().expect("first batch");
        assert_eq!(first.windows.len(), MAX_WINDOWS_PER_BATCH);
        let second = queue.pop_batch().expect("second batch");
        assert_eq!(second.windows.len(), 1);
        assert_eq!(second.windows[0].window_id, 1_000);
        assert!(queue.is_empty());
    }

    #[test]
    fn an_oversized_window_is_dropped_and_the_rest_survives() {
        let queue = ReportQueue::default();
        queue.push(pb::WindowReport {
            window_id: 1,
            ..Default::default()
        });
        // One sample whose string alone blows the 4 MiB limit.
        queue.push(pb::WindowReport {
            window_id: 2,
            samples: vec![pb::Sample {
                five_tuple: "x".repeat(MAX_BATCH_BYTES + 1),
                ..Default::default()
            }],
            ..Default::default()
        });
        queue.push(pb::WindowReport {
            window_id: 3,
            ..Default::default()
        });

        let batch = queue.pop_batch().expect("the healthy windows");
        let ids: Vec<u64> = batch.windows.iter().map(|w| w.window_id).collect();
        assert_eq!(ids, vec![1, 3], "window 2 is dropped, not retried");
        assert!(queue.is_empty());
    }

    #[test]
    fn byte_limit_splits_before_a_window_that_would_overflow() {
        let queue = ReportQueue::default();
        let filler = "y".repeat(MAX_BATCH_BYTES / 2);
        for window_id in 1..=3u64 {
            queue.push(pb::WindowReport {
                window_id,
                samples: vec![pb::Sample {
                    five_tuple: filler.clone(),
                    ..Default::default()
                }],
                ..Default::default()
            });
        }
        let first = queue.pop_batch().expect("first batch");
        assert_eq!(first.windows.len(), 1, "two would exceed 4 MiB");
        assert_eq!(first.windows[0].window_id, 1);
        assert_eq!(queue.len(), 2);
    }
}
