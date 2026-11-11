//! L2 pipeline tests: the userspace half of the probe runs in-process against a
//! fake kernel, so every assertion of the aggregation/sample contract completes
//! in milliseconds without privileges, network namespaces or `sleep`.
//!
//! The three ports from [`probe::ports`] are implemented here:
//!
//! * [`FakeKernel`] is the `AGG` map: per-key "CPU slots" are plain values, so a
//!   test can ask for a two-CPU key without a two-CPU machine.
//! * [`ScriptedSource`] is the `SAMPLES` ring buffer: it replays exact ABI bytes,
//!   including a failed reservation, and never blocks.
//! * [`MemSink`] is both CSV sinks: rows are kept in memory and a write/flush
//!   failure can be switched on to test the exit path.
//!
//! Time is always explicit: [`Harness::session`] hands out a [`Session`] and the
//! tests call [`probe::pipeline::Session::report_closed`] with the monotonic
//! timestamp they want.  Nothing here depends on wall-clock scheduling.

use std::{
    cell::RefCell,
    collections::VecDeque,
    net::{IpAddr, Ipv4Addr},
    rc::Rc,
};

use anyhow::{Result, bail};
use probe::{
    aggregate::{AggRecord, Clock, NANOS_PER_MS},
    pipeline::{KernelCounters, Session, Summary},
    ports::{AggStore, Poll, SampleSource, TimeSource},
    sink::{AggSink, Event, Sink},
};
use probe_common::{
    ACTION_PASS, AggKey, AggValue, CONFIG_VERSION, DIRECTION_INGRESS, FAMILY_IPV4, FAMILY_IPV6,
    ProbeConfig, SampleEvent,
};

/// Wall clock paired with monotonic 0 in [`Harness::new`]; the absolute value is
/// irrelevant, the tests only assert differences.
const CLOCK_UNIX_NS: u128 = 1_700_000_000_000_000_000;

/// A 1ms timestep in nanoseconds, the unit the tests speak in.
fn ms(value: u64) -> u64 {
    value * NANOS_PER_MS
}

/// Configuration with explicit window and grace periods in milliseconds.
fn config(window_ms: u64, grace_ms: u64) -> ProbeConfig {
    ProbeConfig {
        version: CONFIG_VERSION,
        window_ns: ms(window_ms),
        grace_ns: ms(grace_ms),
        ipv4_prefix_len: 24,
        ipv6_prefix_len: 64,
        default_action: ACTION_PASS,
        pad: [0; 5],
    }
}

fn ipv4(a: u8, b: u8, c: u8, d: u8) -> [u8; 16] {
    let mut addr = [0u8; 16];
    addr[..4].copy_from_slice(&[a, b, c, d]);
    addr
}

/// One `SampleEvent` as the data path would produce it.
fn sample(
    ts_ns: u64,
    window_ns: u64,
    src: [u8; 16],
    dst: [u8; 16],
    dst_port: u16,
    protocol: u8,
) -> SampleEvent {
    SampleEvent {
        ts_ns,
        window_id: ts_ns / window_ns,
        src_addr: src,
        dst_addr: dst,
        src_port: 0,
        dst_port: dst_port.to_be(),
        protocol,
        family: FAMILY_IPV4,
        action: ACTION_PASS,
        reason: 0,
        rule_id: 0,
        reserved: 0,
    }
}

/// Serialises a `SampleEvent` to the exact bytes the ring buffer delivers.
fn record_of(sample: &SampleEvent) -> Vec<u8> {
    // SAFETY: `SampleEvent` is `#[repr(C)]`, has no pointers and is fully
    // initialised, so its object representation is readable.
    unsafe {
        std::slice::from_raw_parts(
            (sample as *const SampleEvent).cast::<u8>(),
            std::mem::size_of::<SampleEvent>(),
        )
        .to_vec()
    }
}

fn slot(count: u64, first_seen: u64, last_seen: u64) -> AggValue {
    AggValue {
        count,
        first_seen,
        last_seen,
        sample_dropped: 0,
        degraded: 0,
        pad: [0; 6],
    }
}

/// The `AGG` map: a flat list of `(key, per-CPU slot)` pairs.
///
/// Several entries for one key model the per-CPU layout of the real map, which is
/// the only way to exercise the "sum counts, min/max timestamps, OR flags" merge
/// without pinning the test to several CPUs.
#[derive(Default)]
struct FakeKernel {
    slots: Vec<(AggKey, AggValue)>,
}

impl FakeKernel {
    fn insert(&mut self, key: AggKey, value: AggValue) {
        self.slots.push((key, value));
    }

    /// Number of distinct keys still stored.
    fn distinct_keys(&self) -> usize {
        let mut keys: Vec<&AggKey> = self.slots.iter().map(|(key, _)| key).collect();
        keys.sort_by_key(|key| (key.window_id, key.dst_port));
        keys.dedup();
        keys.len()
    }
}

impl AggStore for FakeKernel {
    fn for_each_slot(&mut self, f: &mut dyn FnMut(&AggKey, &AggValue)) {
        for (key, value) in &self.slots {
            f(key, value);
        }
    }

    fn delete(&mut self, key: &AggKey) {
        self.slots.retain(|(existing, _)| existing != key);
    }

    fn len(&self) -> usize {
        self.distinct_keys()
    }
}

/// A cloneable handle to one [`FakeKernel`].
///
/// The ownership is shared so a test can still insert a "late key" while the
/// session that reads the store is alive; without that, the late-key scenario
/// would need a second session and lose the reported-window watermark.
#[derive(Clone, Default)]
struct KernelHandle(Rc<RefCell<FakeKernel>>);

impl KernelHandle {
    fn insert(&self, key: AggKey, value: AggValue) {
        self.0.borrow_mut().insert(key, value);
    }

    fn distinct_keys(&self) -> usize {
        self.0.borrow().distinct_keys()
    }
}

impl AggStore for KernelHandle {
    fn for_each_slot(&mut self, f: &mut dyn FnMut(&AggKey, &AggValue)) {
        let inner = self.0.borrow();
        for (key, value) in &inner.slots {
            f(key, value);
        }
    }

    fn delete(&mut self, key: &AggKey) {
        self.0
            .borrow_mut()
            .slots
            .retain(|(existing, _)| existing != key);
    }

    fn len(&self) -> usize {
        self.distinct_keys()
    }
}

/// One queued ring-buffer event.
enum Script {
    Record(Vec<u8>),
    ReservationFailed,
}

/// The `SAMPLES` ring buffer: a scripted queue of events.
///
/// The queue is shared (not owned) so a test can append a record while the
/// session is draining, which is how the late-sample scenario is scripted.
#[derive(Clone, Default)]
struct ScriptedSource {
    queue: Rc<RefCell<VecDeque<Script>>>,
    current: Vec<u8>,
}

impl ScriptedSource {
    fn push(&self, record: Vec<u8>) {
        self.queue.borrow_mut().push_back(Script::Record(record));
    }

    fn push_reservation_failed(&self) {
        self.queue.borrow_mut().push_back(Script::ReservationFailed);
    }
}

impl SampleSource for ScriptedSource {
    fn next_record(&mut self) -> Poll<'_> {
        let next = self.queue.borrow_mut().pop_front();
        match next {
            None => Poll::Empty,
            Some(Script::ReservationFailed) => Poll::ReservationFailed,
            Some(Script::Record(record)) => {
                // Copy into the reusable buffer: the real `RingBufItem` also
                // borrows the ring, so a `&[u8]` cannot be handed out directly.
                self.current = record;
                Poll::Record(&self.current)
            }
        }
    }
}

/// Both CSV sinks, in memory.  `fail_write`/`fail_flush` make the exit path take
/// its error branch, which must still name the file.
#[derive(Default)]
struct MemSink {
    events: Vec<Event>,
    rows: Vec<AggRecord>,
    flushes: u32,
    fail_write: bool,
    fail_flush: bool,
    path: String,
}

impl MemSink {
    fn failing(path: &str) -> Self {
        Self {
            fail_write: true,
            path: path.to_owned(),
            ..Self::default()
        }
    }
}

impl Sink for MemSink {
    fn write_batch(&mut self, events: &[Event]) -> Result<()> {
        if self.fail_write {
            bail!("write sample CSV row to {}: simulated failure", self.path);
        }
        self.events.extend_from_slice(events);
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if self.fail_flush {
            bail!("flush sample CSV file {}: simulated failure", self.path);
        }
        self.flushes += 1;
        Ok(())
    }
}

impl AggSink for MemSink {
    fn write_rows(&mut self, rows: &[AggRecord]) -> Result<()> {
        if self.fail_write {
            bail!(
                "write aggregate CSV row to {}: simulated failure",
                self.path
            );
        }
        self.rows.extend_from_slice(rows);
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if self.fail_flush {
            bail!("flush aggregate CSV file {}: simulated failure", self.path);
        }
        self.flushes += 1;
        Ok(())
    }
}

/// A fixed monotonic reading, so `tick` never consults the real clock.
struct FixedTime(u64);

impl TimeSource for FixedTime {
    fn mono_ns(&self) -> Result<u64> {
        Ok(self.0)
    }
}

/// The fake kernel plus the ports and the static configuration of one run.
struct Harness {
    config: ProbeConfig,
    clock: Clock,
    kernel: KernelHandle,
    source: ScriptedSource,
    sink: MemSink,
    agg_sink: MemSink,
}

/// What a finished run produced.
#[derive(Debug)]
struct Finished {
    summary: Summary,
    samples: Vec<Event>,
    aggs: Vec<AggRecord>,
    /// Copied from the harness so rows can be rendered after the session is gone.
    clock: Clock,
    window_ns: u64,
}

impl Harness {
    fn new(config: ProbeConfig) -> Self {
        Self::with_clock(config, Clock::new(CLOCK_UNIX_NS, 0))
    }

    fn with_clock(config: ProbeConfig, clock: Clock) -> Self {
        Self {
            config,
            clock,
            kernel: KernelHandle::default(),
            source: ScriptedSource::default(),
            sink: MemSink::default(),
            agg_sink: MemSink::default(),
        }
    }

    /// Builds a session over the fake ports.
    ///
    /// The session borrows the harness, so the state that must survive several
    /// ticks (the merge table, the de-duplication store) lives in the session
    /// while the ports stay outside: no self-referential struct is needed.
    fn session(&mut self) -> Session<'_> {
        Session::new(
            &mut self.kernel,
            &mut self.source,
            &mut self.sink,
            &mut self.agg_sink,
            self.config,
            self.clock,
        )
    }

    /// Cloneable ports for injecting traffic while a session is alive.
    fn handles(&self) -> (KernelHandle, ScriptedSource) {
        (self.kernel.clone(), self.source.clone())
    }

    /// Queues one sample record, as the data path would have produced it.
    fn sample(&mut self, ts_ns: u64) -> &mut Self {
        let event = sample(
            ts_ns,
            self.config.window_ns,
            ipv4(10, 99, 0, 2),
            ipv4(10, 99, 0, 1),
            443,
            6,
        );
        self.source.push(record_of(&event));
        self
    }

    /// Queues an arbitrary raw record (used by the malformed-record tests).
    fn raw(&mut self, record: Vec<u8>) -> &mut Self {
        self.source.push(record);
        self
    }

    fn reservation_failed(&mut self) -> &mut Self {
        self.source.push_reservation_failed();
        self
    }

    /// Adds one per-CPU slot for the key the default sample would map to.
    fn slot(&mut self, ts_ns: u64, value: AggValue) -> &mut Self {
        let event = sample(
            ts_ns,
            self.config.window_ns,
            ipv4(10, 99, 0, 2),
            ipv4(10, 99, 0, 1),
            443,
            6,
        );
        let key = AggKey::from_sample(&event, &self.config);
        self.kernel.insert(key, value);
        self
    }

    /// Runs a session to completion and returns the summary plus every row.
    fn finish(&mut self) -> Result<Finished> {
        self.finish_with(KernelCounters::default())
    }

    fn finish_with(&mut self, kernel: KernelCounters) -> Result<Finished> {
        let summary = {
            let mut session = self.session();
            session.shutdown(|| kernel)?
        };
        Ok(self.snapshot(summary))
    }

    /// Runs one session, applies `before` (ticks, reports) and then shuts down.
    ///
    /// Everything happens in one session on purpose: the watermark that decides
    /// whether a key is late only exists for the lifetime of the session.
    fn finish_after(
        &mut self,
        before: impl FnOnce(&mut Session<'_>) -> Result<()>,
    ) -> Result<Finished> {
        let summary = {
            let mut session = self.session();
            before(&mut session)?;
            session.shutdown(KernelCounters::default)?
        };
        Ok(self.snapshot(summary))
    }

    fn snapshot(&self, summary: Summary) -> Finished {
        Finished {
            summary,
            samples: self.sink.events.clone(),
            aggs: self.agg_sink.rows.clone(),
            clock: self.clock,
            window_ns: self.config.window_ns,
        }
    }

    fn aggs(&self) -> &[AggRecord] {
        &self.agg_sink.rows
    }

    fn samples(&self) -> &[Event] {
        &self.sink.events
    }
}

/// P-01: three packets of one flow in one window become one aggregate row whose
/// count is the sum, whose `first_seen`/`last_seen` are the min/max, and exactly
/// one sample row.
#[test]
fn p01_one_window_one_key() -> Result<()> {
    let mut harness = Harness::new(config(10, 1));
    harness
        .sample(ms(1)) // 1ms -> window 0
        .sample(ms(5))
        .sample(ms(9))
        .slot(ms(1), slot(3, ms(1), ms(9)));

    let finished = harness
        .finish_after(|session| {
            // Drain first, then close window 0 at 11ms (10ms window + 1ms grace).
            session.drain_ring();
            session.report_closed(ms(11))
        })
        .expect("a well-formed run must finish without an error");

    assert_eq!(finished.aggs.len(), 1, "one window, one key: one row");
    let row = finished.aggs[0];
    assert_eq!(row.count, 3, "count is the merged per-CPU sum");
    assert_eq!(row.first_seen, ms(1));
    assert_eq!(row.last_seen, ms(9));
    assert!(row.window_complete, "a closed window is complete");
    assert_eq!(finished.samples.len(), 1, "three records, one sample key");
    assert_eq!(finished.summary.stats.events_received, 3);
    assert_eq!(finished.summary.stats.agg_records, 1);
    assert!(
        harness.sink.flushes >= 2,
        "both sinks are flushed on a normal exit"
    );
    Ok(())
}

/// P-02: two CPU slots of one key merge into one row: counts add up, `first_seen`
/// is the minimum, `last_seen` the maximum, and the sticky flags are OR-ed.
#[test]
fn p02_two_cpu_slots_merge() -> Result<()> {
    let mut harness = Harness::new(config(10, 1));
    let mut dropped = slot(2, ms(2), ms(4));
    dropped.sample_dropped = 1;
    let mut degraded = slot(3, ms(1), ms(8));
    degraded.degraded = 1;
    harness
        .slot(ms(2), dropped)
        .slot(ms(1), degraded)
        .sample(ms(2));

    let finished = harness.finish()?;

    assert_eq!(finished.aggs.len(), 1);
    let row = finished.aggs[0];
    assert_eq!(row.count, 5, "2 + 3");
    assert_eq!(row.first_seen, ms(1), "minimum of both slots");
    assert_eq!(row.last_seen, ms(8), "maximum of both slots");
    assert!(row.sample_dropped, "flag from the first slot");
    assert!(row.degraded, "flag from the second slot");
    Ok(())
}

/// P-03: two sources in one /24 share the aggregate key, so they produce one row
/// whose `src_prefix` is the masked network and whose count is the sum.
#[test]
fn p03_two_sources_share_one_prefix() -> Result<()> {
    let mut harness = Harness::new(config(10, 1));
    let mut first = sample(
        ms(1),
        harness.config.window_ns,
        ipv4(10, 99, 0, 2),
        ipv4(10, 99, 0, 1),
        443,
        6,
    );
    let second = SampleEvent {
        src_addr: ipv4(10, 99, 0, 3),
        ..sample(
            ms(2),
            harness.config.window_ns,
            ipv4(10, 99, 0, 3),
            ipv4(10, 99, 0, 1),
            443,
            6,
        )
    };
    // Three packets per source, already merged by the data path.
    first.ts_ns = ms(1);
    harness.raw(record_of(&first)).raw(record_of(&second));
    harness
        .slot(ms(1), slot(3, ms(1), ms(3)))
        .slot(ms(2), slot(3, ms(2), ms(4)));

    let finished = harness.finish()?;

    assert_eq!(finished.aggs.len(), 1, "one masked key for both sources");
    let row = finished.aggs[0];
    assert_eq!(row.key.src_prefix, ipv4(10, 99, 0, 0));
    assert_eq!(row.count, 6);
    assert_eq!(finished.summary.stats.events_received, 2);
    Ok(())
}

/// P-04: two adjacent windows produce two rows, the windows touch without
/// overlapping, and the number of packets is conserved.
#[test]
fn p04_adjacent_windows() -> Result<()> {
    let mut harness = Harness::new(config(10, 1));
    harness.sample(ms(1)).sample(ms(12));
    let first = sample(
        ms(1),
        harness.config.window_ns,
        ipv4(10, 99, 0, 2),
        ipv4(10, 99, 0, 1),
        443,
        6,
    );
    let second = sample(
        ms(12),
        harness.config.window_ns,
        ipv4(10, 99, 0, 2),
        ipv4(10, 99, 0, 1),
        443,
        6,
    );
    harness
        .slot(ms(1), slot(1, ms(1), ms(1)))
        .slot(ms(12), slot(1, ms(12), ms(12)));
    assert_ne!(
        AggKey::from_sample(&first, &harness.config).window_id,
        AggKey::from_sample(&second, &harness.config).window_id
    );

    let finished = harness.finish()?;

    assert_eq!(finished.aggs.len(), 2);
    assert_eq!(finished.aggs[0].window_id, 0);
    assert_eq!(finished.aggs[1].window_id, 1);
    assert_eq!(
        finished.aggs[0].window_id + 1,
        finished.aggs[1].window_id,
        "no window was skipped"
    );
    let total: u64 = finished.aggs.iter().map(|row| row.count).sum();
    assert_eq!(total, 2, "every packet is counted exactly once");
    Ok(())
}

/// P-05: a window with no traffic produces no row, and the watermark still
/// advances so the next window is reported normally.
#[test]
fn p05_empty_window_writes_nothing() -> Result<()> {
    let mut harness = Harness::new(config(10, 1));
    {
        let mut session = harness.session();
        // Window 0 is closed at 11ms, but it saw no traffic. Driving the
        // watermark through `tick` also covers the `TimeSource` port.
        session.tick(&FixedTime(ms(11)))?;
    }
    assert!(harness.aggs().is_empty(), "no traffic, no row");
    assert!(harness.samples().is_empty());

    harness.sample(ms(12)).slot(ms(12), slot(1, ms(12), ms(12)));
    {
        let mut session = harness.session();
        session.report_closed(ms(21))?;
    }
    assert_eq!(harness.aggs().len(), 1, "window 1 is reported normally");
    assert_eq!(harness.aggs()[0].window_id, 1);
    Ok(())
}

/// P-06: a sample and a key that arrive after their window was reported are
/// counted as late, dropped, and must not reopen the old window.
#[test]
fn p06_late_sample_and_key_are_dropped() -> Result<()> {
    let mut harness = Harness::new(config(10, 1));
    harness.sample(ms(1)).slot(ms(1), slot(1, ms(1), ms(1)));
    let (kernel, source) = harness.handles();
    let cfg = harness.config;
    let (summary, late) = {
        let mut session = harness.session();
        session.drain_ring();
        session.report_closed(ms(11))?;
        assert_eq!(session.stats().agg_records, 1);

        // A sample and a key of the already reported window 0 arrive late.
        let late_sample = sample(
            ms(5),
            cfg.window_ns,
            ipv4(10, 99, 0, 2),
            ipv4(10, 99, 0, 1),
            443,
            6,
        );
        source.push(record_of(&late_sample));
        kernel.insert(
            AggKey::from_sample(&late_sample, &cfg),
            slot(9, ms(5), ms(5)),
        );

        let summary = session.shutdown(KernelCounters::default)?;
        let late = session.late_dropped();
        (summary, late)
    };
    assert_eq!(late, 2, "one late sample and one late key slot");
    assert_eq!(summary.late_dropped, 2);
    assert_eq!(
        summary.stats.events_received, 2,
        "the late sample is decoded (and counted) before it is dropped as late"
    );
    assert_eq!(
        summary.stats.agg_records, 1,
        "only the completed window 0 row; the late key adds none"
    );
    assert_eq!(
        harness.aggs().len(),
        1,
        "the shutdown scan added no row for the late window"
    );
    Ok(())
}

/// P-07: on SIGINT an unclosed window is flushed with `window_complete = 0`, and
/// the stats line keeps every token the scripts grep for.
#[test]
fn p07_sigint_flushes_partial_window() -> Result<()> {
    let mut harness = Harness::new(config(10, 1));
    harness.sample(ms(1)).slot(ms(1), slot(1, ms(1), ms(1)));

    let finished = harness.finish()?;

    assert_eq!(finished.aggs.len(), 1);
    assert!(
        !finished.aggs[0].window_complete,
        "a window flushed at shutdown never saw its close scan"
    );
    assert_eq!(
        finished.samples.len(),
        1,
        "the pending sample is still written"
    );

    let line = finished.summary.stats_line();
    let tokens: Vec<&str> = line
        .strip_prefix("stats: ")
        .expect("the line starts with the stats token")
        .split(' ')
        .collect();
    assert_eq!(tokens.len(), 11, "eleven key=value tokens: {line}");
    for token in [
        "packets_total=",
        "sample_rows=",
        "events_received=",
        "rows_written=",
        "agg_records=",
        "kernel_dropped=",
        "sample_failed=",
        "agg_overflow=",
        "malformed=",
        "late_dropped=",
        "samples_deduped=",
    ] {
        assert!(line.contains(token), "missing {token} in {line}");
    }
    assert!(line.contains("rows_written=1"), "one sample row: {line}");
    Ok(())
}

/// P-08: a failed ring reservation is counted in `sample_failed`, drops exactly
/// one sample and never changes the aggregate count.
#[test]
fn p08_failed_reservation_is_counted() -> Result<()> {
    let mut harness = Harness::new(config(10, 1));
    harness
        .sample(ms(1))
        .reservation_failed()
        .sample(ms(2))
        .slot(ms(1), slot(3, ms(1), ms(9)));

    let (summary, handled) = {
        let mut session = harness.session();
        let handled = session.drain_ring();
        assert_eq!(session.stats().sample_failed, 1);
        (session.shutdown(KernelCounters::default)?, handled)
    };
    assert_eq!(handled, 3, "two records and one failed reservation");

    let finished = Finished {
        summary,
        samples: harness.samples().to_vec(),
        aggs: harness.aggs().to_vec(),
        clock: harness.clock,
        window_ns: harness.config.window_ns,
    };

    assert_eq!(finished.summary.stats.sample_failed, 1);
    assert_eq!(finished.summary.stats.events_received, 2);
    assert_eq!(finished.aggs.len(), 1);
    assert_eq!(finished.aggs[0].count, 3, "count comes from the kernel");
    let line = finished.summary.stats_line();
    assert!(line.contains("sample_failed=1"), "{line}");
    assert!(line.contains("kernel_dropped=0"), "{line}");
    Ok(())
}

/// P-09: an `AGG` insertion failure is reported by the kernel counter; the
/// pipeline produces no row for it and does not panic.
#[test]
fn p09_agg_overflow_is_reported() -> Result<()> {
    let mut harness = Harness::new(config(10, 1));
    let counters = KernelCounters {
        packets_total: 5,
        sample_failed: 0,
        agg_overflow: 1,
        config_missing: 0,
    };

    let finished = harness.finish_with(counters)?;

    assert_eq!(finished.summary.kernel.agg_overflow, 1);
    assert!(finished.aggs.is_empty(), "the failed key never appeared");
    assert!(finished.samples.is_empty());
    let line = finished.summary.stats_line();
    assert!(line.contains("agg_overflow=1"), "{line}");
    assert!(line.contains("packets_total=5"), "{line}");
    Ok(())
}

/// P-10: records with the wrong length or an unknown address family are counted
/// as malformed, are never written to a CSV and never crash the pipeline.
#[test]
fn p10_malformed_records() -> Result<()> {
    let mut harness = Harness::new(config(10, 1));
    let valid = record_of(&sample(
        ms(1),
        harness.config.window_ns,
        ipv4(10, 99, 0, 2),
        ipv4(10, 99, 0, 1),
        443,
        6,
    ));
    for len in [48usize, 63, 65] {
        let mut record = vec![0u8; len];
        let copied = len.min(valid.len());
        record[..copied].copy_from_slice(&valid[..copied]);
        harness.raw(record);
    }
    for family in [5u8, 0] {
        let mut event = sample(
            ms(2),
            harness.config.window_ns,
            ipv4(10, 99, 0, 2),
            ipv4(10, 99, 0, 1),
            443,
            6,
        );
        event.family = family;
        harness.raw(record_of(&event));
    }

    let finished = harness.finish()?;

    assert_eq!(finished.summary.stats.malformed, 5);
    assert_eq!(finished.summary.stats.events_received, 0);
    assert!(finished.aggs.is_empty(), "nothing may be written");
    assert!(finished.samples.is_empty());
    Ok(())
}

/// P-11: every length in `1..=256` is rejected except the exact ABI size, and of
/// the 64-byte records only family 4 and 6 enter the pipeline.
#[test]
fn p11_only_the_exact_abi_record_enters_the_pipeline() -> Result<()> {
    let mut harness = Harness::new(config(10, 1));
    let template = sample(
        ms(1),
        harness.config.window_ns,
        ipv4(10, 99, 0, 2),
        ipv4(10, 99, 0, 1),
        443,
        6,
    );
    let full = record_of(&template);
    for len in 1..=256usize {
        let mut record = vec![0u8; len];
        let copied = len.min(full.len());
        record[..copied].copy_from_slice(&full[..copied]);
        harness.raw(record);
    }
    for family in 0..=255u8 {
        let mut event = template;
        event.family = family;
        harness.raw(record_of(&event));
    }

    let finished = harness.finish()?;

    assert_eq!(
        finished.summary.stats.events_received, 3,
        "one 64-byte record of length 64 plus family 4 and 6"
    );
    assert_eq!(
        finished.summary.stats.malformed,
        255 + 254,
        "255 wrong lengths and 254 unsupported families"
    );
    Ok(())
}

/// P-12: two writers appending to the same file keep every row and never
/// interleave a partial line.
#[test]
fn p12_two_writers_append_without_losing_rows() -> Result<()> {
    use std::sync::{Arc, Barrier};

    use probe::sink::{SinkKind, create_sink};

    let path = std::env::temp_dir().join(format!(
        "probe-p12-{}-{}.csv",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let event = Event {
        ts_unix_ms: 1,
        src_ip: IpAddr::V4(Ipv4Addr::new(10, 99, 0, 2)),
        dst_ip: IpAddr::V4(Ipv4Addr::new(10, 99, 0, 1)),
        src_port: 0,
        dst_port: 443,
        protocol: 6,
    };
    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for (iface, rows) in [("veth0", 500usize), ("veth1", 300)] {
        let path = path.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || -> Result<()> {
            let mut sink = create_sink(SinkKind::Csv, &path, iface, true)?;
            barrier.wait();
            // One batch per simulated flush tick.  A batch of 64 rows stays under
            // the `BufWriter` capacity, so every `writeln!` reaches the kernel in
            // one `write(2)` and `O_APPEND` makes it atomic: a row is never split
            // by the other writer.
            let batch = vec![event; 64];
            let mut written = 0;
            while written < rows {
                let take = (rows - written).min(64);
                sink.write_batch(&batch[..take])?;
                sink.flush()?;
                written += take;
            }
            Ok(())
        }));
    }
    for handle in handles {
        handle.join().expect("writer thread must not panic")?;
    }

    let content = std::fs::read_to_string(&path)?;
    std::fs::remove_file(&path)?;
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 1 + 500 + 300, "header plus every row");
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.starts_with("ts_unix_ms"))
            .count(),
        1
    );
    assert!(lines.iter().all(|line| line.split(',').count() == 7));
    assert_eq!(
        lines.iter().filter(|line| line.contains(",veth0,")).count(),
        500
    );
    assert_eq!(
        lines.iter().filter(|line| line.contains(",veth1,")).count(),
        300
    );
    Ok(())
}

/// P-13: a sink write failure aborts the run, and the error names the output
/// file instead of hiding behind a bare `io::Error`.
#[test]
fn p13_sink_failure_names_the_file() -> Result<()> {
    let path = "/tmp/probe-packets.csv";
    let mut harness = Harness::new(config(10, 1));
    harness.sink = MemSink::failing(path);
    harness.sample(ms(1)).slot(ms(1), slot(1, ms(1), ms(1)));

    let error = harness
        .finish()
        .expect_err("a failing sink must fail the run");
    let message = format!("{error:#}");
    assert!(
        message.contains(path),
        "the error must name the output file: {message}"
    );
    Ok(())
}

/// P-14: packets that predate the probe start still get a real wall-clock
/// window, and the window span stays exactly one window.
#[test]
fn p14_clock_offset_goes_backwards() -> Result<()> {
    // Monotonic 5ms at startup; the sample at 2ms is 3ms older than the process.
    let clock = Clock::new(CLOCK_UNIX_NS, ms(5));
    let mut harness = Harness::with_clock(config(10, 1), clock);
    harness.sample(ms(2)).slot(ms(2), slot(1, ms(2), ms(2)));

    let finished = harness.finish()?;

    assert_eq!(finished.aggs.len(), 1);
    let row = finished.aggs[0];
    let start = finished
        .clock
        .window_start_ms(row.window_id, finished.window_ns);
    let end = finished
        .clock
        .window_end_ms(row.window_id, finished.window_ns);
    assert_eq!(end - start, 10, "the window is exactly 10ms wide");
    assert_eq!(
        finished.samples.len(),
        1,
        "the sample of the partial window is still written"
    );
    // Window 0 starts 5ms before the sampled wall clock instant, so a naive
    // implementation that clamped to the startup time would report 0ms span.
    let startup_ms = (CLOCK_UNIX_NS / 1_000_000) as u64;
    assert!(
        start < startup_ms,
        "window starts before startup: start={start} startup={startup_ms}"
    );
    assert_eq!(row.first_seen, ms(2));
    Ok(())
}

/// P-15: `--window-ms 1 --grace-ms 0` must not divide by zero and must not report
/// the same window twice when the scan tick fires again inside it.
#[test]
fn p15_one_millisecond_window() -> Result<()> {
    let mut harness = Harness::new(config(1, 0));
    harness
        .sample(ms(1) / 2) // 0.5ms -> window 0
        .slot(ms(1) / 2, slot(1, ms(1) / 2, ms(1) / 2));
    {
        let mut session = harness.session();
        session.report_closed(ms(1))?;
        session.report_closed(ms(1))?; // same scan tick, must be a no-op
        session.report_closed(ms(2))?;
    }
    assert_eq!(harness.aggs().len(), 1, "window 0 is reported exactly once");
    assert_eq!(harness.aggs()[0].window_id, 0);
    Ok(())
}

/// P-16: the aggregate row counts sum to the number of injected packets.
#[test]
fn p16_counts_sum_to_injected_packets() -> Result<()> {
    const PACKETS: u64 = 20;
    let mut harness = Harness::new(config(10, 1));
    harness
        .slot(ms(1), slot(7, ms(1), ms(9)))
        .slot(ms(12), slot(13, ms(11), ms(19)))
        .sample(ms(1))
        .sample(ms(12));

    let finished = harness.finish()?;

    let total: u64 = finished.aggs.iter().map(|row| row.count).sum();
    assert_eq!(total, PACKETS, "every packet is accounted for once");
    assert_eq!(
        finished.summary.stats.agg_records,
        finished.aggs.len() as u64
    );
    Ok(())
}

/// The sample CSV uses the same `Clock` offset as the aggregate file, so the two
/// outputs line up.
#[test]
fn window_rendering_is_consistent_between_files() -> Result<()> {
    let mut harness = Harness::new(config(15, 1));
    harness.sample(ms(4)).slot(ms(4), slot(1, ms(4), ms(4)));
    let finished = harness.finish()?;
    assert_eq!(finished.aggs.len(), 1);
    assert_eq!(finished.samples.len(), 1);
    let row = finished.aggs[0];
    let startup_ms = (CLOCK_UNIX_NS / 1_000_000) as u64;
    assert_eq!(finished.samples[0].ts_unix_ms, startup_ms + 4);
    assert_eq!(
        finished
            .clock
            .window_start_ms(row.window_id, finished.window_ns),
        startup_ms
    );
    assert_eq!(
        finished
            .clock
            .window_end_ms(row.window_id, finished.window_ns),
        startup_ms + 15
    );
    Ok(())
}

/// Family must not leak an IPv6 address into an IPv4-shaped row: the key keeps
/// the family byte and the sink renders the corresponding format.
#[test]
fn ipv6_samples_keep_their_family() -> Result<()> {
    let mut harness = Harness::new(config(10, 1));
    let mut event = sample(
        ms(1),
        harness.config.window_ns,
        [0u8; 16],
        [0u8; 16],
        443,
        6,
    );
    event.family = FAMILY_IPV6;
    event.src_addr = "fd00:99::2".parse::<std::net::Ipv6Addr>().unwrap().octets();
    event.dst_addr = "fd00:99::1".parse::<std::net::Ipv6Addr>().unwrap().octets();
    harness.raw(record_of(&event));
    harness.slot(ms(1), slot(1, ms(1), ms(1)));

    let finished = harness.finish()?;

    assert_eq!(finished.summary.stats.malformed, 0);
    assert_eq!(finished.summary.stats.events_received, 1);
    assert_eq!(finished.samples.len(), 1, "the record keeps its own key");
    assert!(matches!(finished.samples[0].src_ip, IpAddr::V6(_)));
    Ok(())
}

/// Direction/action rendering of a `block` key must not change the fixed
/// 16-column schema.
#[test]
fn block_keys_keep_the_sixteen_column_schema() -> Result<()> {
    let mut harness = Harness::new(config(10, 1));
    let mut event = sample(
        ms(1),
        harness.config.window_ns,
        ipv4(10, 99, 0, 2),
        ipv4(10, 99, 0, 1),
        443,
        6,
    );
    event.action = probe_common::ACTION_BLOCK;
    event.rule_id = 7;
    harness.raw(record_of(&event));
    let key = AggKey::from_sample(&event, &harness.config);
    harness.kernel.insert(key, slot(1, ms(1), ms(1)));

    let finished = harness.finish()?;

    assert_eq!(finished.aggs.len(), 1);
    let row = finished.aggs[0];
    assert_eq!(row.key.action, probe_common::ACTION_BLOCK);
    assert_eq!(row.key.rule_id, 7);
    assert_eq!(row.key.direction, DIRECTION_INGRESS);
    assert_eq!(row.key.family, FAMILY_IPV4);
    Ok(())
}
