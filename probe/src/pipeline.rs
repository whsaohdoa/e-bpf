//! The userspace pipeline: ring drain -> sample de-duplication -> window close
//! -> CSV rows.
//!
//! A [`Session`] owns every bit of state that survives one iteration of the main
//! loop (the merge table, the de-duplication store, the counters) and drives the
//! three ports in [`crate::ports`].  Keeping the state here, instead of in
//! `main()`, is what makes the whole A2/A3/A6/A7 assertion set runnable in two
//! seconds against a fake kernel: `tests/pipeline.rs` builds a `Session` with a
//! scripted ring buffer, a fake `AGG` map and in-memory sinks, then only calls
//! [`Session::drain_ring`], [`Session::report_closed`] and [`Session::shutdown`]
//! with explicit timestamps -- no sleeping, no privileges.

use anyhow::Result;
#[rustfmt::skip]
use log::{debug, info, warn};
use probe_common::{AggKey, ProbeConfig, SampleEvent};

use crate::{
    aggregate::{AggRecord, AggTable, Clock, SampleDedup, SampleIngest, closed_window_watermark},
    ports::{AggStore, Poll, SampleSource, TimeSource},
    sink::{AggSink, Event, Sink, decode_sample},
};

/// Upper bound per periodic drain; keeps the loop responsive when a large
/// backlog is waiting in the ring buffer.
pub const MAX_EVENTS_PER_TICK: usize = 8192;

/// Userspace counters reported in the one `stats:` line on exit.
///
/// `late_dropped` and `samples_deduped` live in [`crate::aggregate`] because the
/// watermark they depend on lives there; [`Summary`] joins both halves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Sample records successfully decoded from `SAMPLES` (before de-dup).
    pub events_received: u64,
    /// Sample CSV rows written (one per de-duplicated key).
    pub sample_rows: u64,
    /// Aggregate CSV rows written.
    pub agg_records: u64,
    /// Ring records with a wrong length or an unknown address family.
    pub malformed: u64,
    /// Ring reservations the data path could not complete, as observed by
    /// userspace while draining (a superset of what `STATS` reports is possible,
    /// the two are summed in [`Summary::stats_line`]).
    pub sample_failed: u64,
}

/// Kernel `STATS` counters, summed over all CPUs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KernelCounters {
    pub packets_total: u64,
    pub sample_failed: u64,
    pub agg_overflow: u64,
    pub config_missing: u64,
}

/// Everything the process reports when it exits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// Userspace counters.
    pub stats: Stats,
    /// Kernel counters, read while the maps were still open.
    pub kernel: KernelCounters,
    /// Keys and samples dropped because their window had been reported already.
    pub late_dropped: u64,
    /// Duplicate samples absorbed into an already pending `(window, key)`.
    pub samples_deduped: u64,
}

impl Summary {
    /// The machine-readable contract: exactly one line, on stdout, with these
    /// tokens in this order.
    ///
    /// The verification scripts grep the tokens, so the spelling and the order
    /// are part of the ABI.  `rows_written` mirrors `sample_rows` (the sample file
    /// has exactly one row per written sample, header excluded) and
    /// `kernel_dropped` mirrors the kernel `STATS` sample-failure counter, both
    /// unchanged from before the pipeline was split out of `main()`.
    pub fn stats_line(&self) -> String {
        format!(
            "stats: packets_total={} sample_rows={} events_received={} rows_written={} agg_records={} kernel_dropped={} sample_failed={} agg_overflow={} malformed={} late_dropped={} samples_deduped={}",
            self.kernel.packets_total,
            self.stats.sample_rows,
            self.stats.events_received,
            self.stats.sample_rows,
            self.stats.agg_records,
            self.kernel.sample_failed,
            self.kernel.sample_failed + self.stats.sample_failed,
            self.kernel.agg_overflow,
            self.stats.malformed,
            self.late_dropped,
            self.samples_deduped,
        )
    }
}

/// One pipeline instance: the ports plus the state that spans the main loop.
pub struct Session<'a> {
    agg: &'a mut dyn AggStore,
    samples: &'a mut dyn SampleSource,
    sink: &'a mut dyn Sink,
    agg_sink: &'a mut dyn AggSink,
    config: ProbeConfig,
    clock: Clock,
    table: AggTable,
    dedup: SampleDedup,
    stats: Stats,
}

impl<'a> Session<'a> {
    /// Wires the ports together.  `config` must be the value read back from
    /// `CONFIG`, and `clock` pairs the wall clock with the monotonic clock.
    pub fn new(
        agg: &'a mut dyn AggStore,
        samples: &'a mut dyn SampleSource,
        sink: &'a mut dyn Sink,
        agg_sink: &'a mut dyn AggSink,
        config: ProbeConfig,
        clock: Clock,
    ) -> Self {
        Self {
            agg,
            samples,
            sink,
            agg_sink,
            config,
            clock,
            table: AggTable::new(),
            dedup: SampleDedup::new(),
            stats: Stats::default(),
        }
    }

    /// The merge table, for assertions and for the metrics line.
    pub fn table(&self) -> &AggTable {
        &self.table
    }

    /// The de-duplication store.
    pub fn dedup(&self) -> &SampleDedup {
        &self.dedup
    }

    /// Userspace counters accumulated so far.
    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    /// The effective configuration (read back from the kernel).
    pub fn config(&self) -> &ProbeConfig {
        &self.config
    }

    /// Number of keys currently in `AGG`; one syscall per key, so this is only
    /// used by the 5s metrics line.
    pub fn agg_len(&self) -> usize {
        self.agg.len()
    }

    /// Keys and samples dropped as late, across both stores.
    pub fn late_dropped(&self) -> u64 {
        self.table.late_dropped() + self.dedup.late_dropped()
    }

    /// Drains at most [`MAX_EVENTS_PER_TICK`] records into the de-duplication
    /// store and returns how many were handled.
    ///
    /// A record whose length is not exactly `size_of::<SampleEvent>()` (for
    /// example the 48-byte record of the previous ABI) or whose address family is
    /// neither IPv4 nor IPv6 is counted in `malformed` and never interpreted with
    /// the wrong layout.
    pub fn drain_ring(&mut self) -> usize {
        let mut handled = 0;
        while handled < MAX_EVENTS_PER_TICK {
            let polled = self.samples.next_record();
            match polled {
                Poll::Empty => break,
                Poll::ReservationFailed => {
                    handled += 1;
                    self.stats.sample_failed += 1;
                    debug!("sample ring reservation failed; sample dropped by the data path");
                }
                Poll::Record(record) => {
                    handled += 1;
                    ingest_record(
                        record,
                        &self.config,
                        self.clock,
                        &self.table,
                        &mut self.dedup,
                        &mut self.stats,
                    );
                }
            }
        }
        handled
    }

    /// Merges and removes every key of a window that is closed at `now_ns`, and
    /// writes both CSVs.
    ///
    /// Only keys of closed windows are merged and removed from `AGG`; keys of open
    /// windows stay in the map so the data path keeps its per-CPU `count` and
    /// `first_seen`/`last_seen` continuous for the whole window.  Removing them
    /// mid-window would restart the count from zero and make the data path report
    /// a second "first packet" sample for the same key.
    ///
    /// Samples of the closed windows are written together with the aggregate
    /// rows: once a window is closed the data path can no longer produce a sample
    /// for it, so the two outputs stay consistent.
    pub fn report_closed(&mut self, now_ns: u64) -> Result<()> {
        let Some(watermark) =
            closed_window_watermark(now_ns, self.config.window_ns, self.config.grace_ns)
        else {
            return Ok(());
        };
        self.scan_agg(Some(watermark));

        // Take the aggregate rows first: `take_closed` also advances the
        // "already reported" watermark, and the sample lookup below must use the
        // same watermark.  Both calls happen after the ring was drained, so no
        // sample of a newly closed window is mistaken for a late one.
        let rows = self.table.take_closed(watermark);
        let closed_samples = self.dedup.take_closed(watermark);
        if closed_samples.is_empty() && rows.is_empty() {
            return Ok(());
        }
        write_aggs(self.agg_sink, &rows, &mut self.stats)?;
        write_samples(self.sink, &closed_samples, self.clock, &mut self.stats)?;
        self.sink.flush()?;
        self.agg_sink.flush()?;
        Ok(())
    }

    /// One periodic iteration: drain first, then report.
    ///
    /// Draining before scanning matters: a sample the kernel produced before a
    /// window closed must be de-duplicated before the window is reported,
    /// otherwise our own report order would make it late.
    pub fn tick(&mut self, time: &dyn TimeSource) -> Result<()> {
        self.drain_ring();
        self.report_closed(time.mono_ns()?)
    }

    /// Final drain, report of everything still pending and the exit summary.
    ///
    /// `kernel_counters` is called after the last drain, while the `STATS` map is
    /// still open, so the counters include samples produced during this drain.
    /// Windows that never closed are flushed with `window_complete = 0`.
    pub fn shutdown(
        &mut self,
        kernel_counters: impl FnOnce() -> KernelCounters,
    ) -> Result<Summary> {
        // Whatever the kernel already queued must still reach disk.
        while self.drain_ring() != 0 {}

        // Last complete `AGG` scan: this is the "all keys" variant, because the
        // data path is about to be detached.  Whatever is left is written with
        // `window_complete = 0`: none of these windows is guaranteed to have seen
        // its close scan (the normal scan tick reports the complete windows).
        self.scan_agg(None);
        let open = self.table.take_pending();
        write_aggs(self.agg_sink, &open, &mut self.stats)?;

        // Every pending sample (one row per key) is written now, including the
        // samples of windows that never closed.
        let pending_samples = self.dedup.take_all();
        write_samples(self.sink, &pending_samples, self.clock, &mut self.stats)?;

        // Flush both files before the teardown starts; nothing may be lost after
        // the program is detached.
        self.sink.flush()?;
        self.agg_sink.flush()?;

        Ok(Summary {
            stats: self.stats,
            kernel: kernel_counters(),
            late_dropped: self.late_dropped(),
            samples_deduped: self.dedup.deduped(),
        })
    }

    /// Enumerates every key of the per-CPU `AGG` store, merges its per-CPU slots
    /// and removes the key.
    ///
    /// `close_before` restricts the scan to closed windows: with
    /// `Some(watermark)` only keys with `window_id <= watermark` are read, merged
    /// and removed, while keys of still-open windows stay in the map so the data
    /// path keeps accumulating them.  `None` is the shutdown scan that drains
    /// every remaining key.  Keys of an already reported window are merged (so
    /// they are counted in `late_dropped`) and removed without being reported
    /// again.
    fn scan_agg(&mut self, close_before: Option<u64>) {
        let mut keys: Vec<AggKey> = Vec::new();
        {
            // Both borrows are of fields, so the closure never captures `self`.
            let table = &mut self.table;
            self.agg.for_each_slot(&mut |key, slot| {
                if !close_before.is_none_or(|watermark| key.window_id <= watermark) {
                    return;
                }
                table.ingest_slot(*key, slot);
                keys.push(*key);
            });
        }
        // One key has one slot per CPU, so it is pushed once per slot.
        keys.dedup();
        for key in &keys {
            self.agg.delete(key);
        }
    }

    /// Logs the running counters with `info!` every `METRICS_PERIOD`.
    ///
    /// `scan_ms` is the measured duration of the last periodic scan and
    /// `scan_interval_ms` the configured period.  This is the operational view;
    /// the machine-readable `stats:` line stays exit-only so scripts can rely on
    /// it appearing exactly once.
    pub fn log_metrics(&self, kernel: KernelCounters, scan_ms: u64, scan_interval_ms: u64) {
        info!(
            "metrics: packets_total={} agg_keys={} agg_overflow={} sample_failed={} late_dropped={} scan_ms={scan_ms} scan_interval_ms={scan_interval_ms}",
            kernel.packets_total,
            self.agg_len(),
            kernel.agg_overflow,
            kernel.sample_failed,
            self.late_dropped(),
        );
    }
}

/// Decodes one ring record and offers it to the de-duplication store.
///
/// The aggregation key is rebuilt exactly like the data path did: the source
/// address is masked with the prefix length from the read-back `CONFIG`, so two
/// sources in one /24 share one sample row.
fn ingest_record(
    record: &[u8],
    config: &ProbeConfig,
    clock: Clock,
    table: &AggTable,
    dedup: &mut SampleDedup,
    stats: &mut Stats,
) {
    let Some(raw) = decode_sample(record) else {
        warn!(
            "ignoring malformed sample record ({} bytes, expected {})",
            record.len(),
            std::mem::size_of::<SampleEvent>()
        );
        stats.malformed += 1;
        return;
    };
    let Some(event) = Event::from_abi(&raw, clock.unix_ms(raw.ts_ns)) else {
        // Unknown family: the record cannot be rendered without guessing which
        // address layout it uses.
        warn!(
            "ignoring sample with unsupported address family {}",
            raw.family
        );
        stats.malformed += 1;
        return;
    };
    stats.events_received += 1;

    let key = AggKey::from_sample(&raw, config);
    let late = table.is_late(raw.window_id);
    if let SampleIngest::Duplicate = dedup.ingest(raw, key, late) {
        debug!(
            "de-duplicated sample for window {} ({} -> {}), keeping the earliest ts_ns",
            raw.window_id, event.src_ip, event.dst_ip
        );
    }
}

/// Writes deduplicated samples as CSV rows and accounts for them.
fn write_samples(
    sink: &mut dyn Sink,
    records: &[SampleEvent],
    clock: Clock,
    stats: &mut Stats,
) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let mut batch: Vec<Event> = Vec::with_capacity(records.len());
    for raw in records {
        // The address family was validated when the record entered the
        // de-duplication store, so this cannot fail here.
        if let Some(event) = Event::from_abi(raw, clock.unix_ms(raw.ts_ns)) {
            batch.push(event);
        }
    }
    let written = batch.len() as u64;
    if written == 0 {
        return Ok(());
    }
    sink.write_batch(&batch)?;
    stats.sample_rows += written;
    Ok(())
}

/// Writes aggregate rows and accounts for them.
fn write_aggs(sink: &mut dyn AggSink, rows: &[AggRecord], stats: &mut Stats) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    sink.write_rows(rows)?;
    stats.agg_records += rows.len() as u64;
    Ok(())
}
