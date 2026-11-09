//! Window aggregation, sample de-duplication and CSV field rendering.
//!
//! The eBPF side keeps one `AGG` value per CPU for every `(window_id, key)`.
//! This module is the userspace half of that contract:
//!
//! * [`AggTable`] merges the per-CPU slots of one key, decides which windows are
//!   closed (`window end + grace`), emits one [`AggRecord`] per key and window,
//!   and accounts for keys that arrive after their window was already reported.
//! * [`SampleDedup`] keeps at most one sample per `(window_id, key)` and keeps
//!   the record with the smallest `ts_ns`, as required by the Train 0 contract.
//! * [`Clock`] converts `bpf_ktime_get_ns()` timestamps to wall-clock Unix
//!   milliseconds and lays out window bounds, so the aggregate CSV uses exactly
//!   the same time source as the sample CSV.
//!
//! Everything here is plain data manipulation (no `aya`, no `libc`), which
//! keeps the merging, closing and lateness rules unit-testable on the host.
//!
//! # Close and lateness model
//!
//! A window `w` covers `[w * window_ns, (w + 1) * window_ns)` and is *closed*
//! once `(w + 1) * window_ns + grace_ns <= now`.  The highest closed window id
//! is [`closed_window_watermark`].  Every scan reports all keys of every closed
//! window and then advances [`AggTable::last_reported`] to that watermark, even
//! when a closed window had no traffic at all: otherwise a packet arriving
//! after an empty window was scanned would look like a brand-new window and be
//! reported a second time.  Any key or sample whose `window_id` is at or below
//! `last_reported` is *late*: it is counted in `late_dropped` and never
//! re-opens the window.

use std::{
    cmp::Ordering,
    collections::{HashMap, hash_map::Entry},
    net::{Ipv4Addr, Ipv6Addr},
};

use probe_common::{
    ACTION_BLOCK, AggKey, AggValue, DIRECTION_INGRESS, FAMILY_IPV4, FAMILY_IPV6, ProbeConfig,
    SampleEvent,
};

/// Nanoseconds per millisecond; used to build the window bounds in the CSV.
pub const NANOS_PER_MS: u64 = 1_000_000;

/// One merged aggregate row: every per-CPU slot of one `(window_id, key)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AggRecord {
    /// Window index (`ts_ns / window_ns`).
    pub window_id: u64,
    /// The aggregation key shared by all CPUs.
    pub key: AggKey,
    /// Sum of the per-CPU `count` fields.
    pub count: u64,
    /// Smallest `first_seen` among the slots that saw traffic.
    pub first_seen: u64,
    /// Largest `last_seen` among the slots that saw traffic.
    pub last_seen: u64,
    /// OR of the per-CPU `sample_dropped` flags.
    pub sample_dropped: bool,
    /// OR of the per-CPU `degraded` flags.
    pub degraded: bool,
    /// `true` for a window that closed normally; `false` for rows flushed on
    /// shutdown before their window closed.
    pub window_complete: bool,
}

impl AggRecord {
    /// Merges one per-CPU `AGG` slot into this row.
    ///
    /// A per-CPU hash value can exist with `count == 0` (an all-zero value that
    /// lost a race against a concurrent increment).  Such a slot carries no
    /// timestamp at all, so its zero `first_seen`/`last_seen` must not win the
    /// `min`/`max`: only slots with `count > 0` participate in the timestamps.
    /// `count` and the two flags are still accumulated for every slot.
    pub fn merge(&mut self, slot: &AggValue) {
        if slot.count > 0 {
            if self.count == 0 {
                self.first_seen = slot.first_seen;
                self.last_seen = slot.last_seen;
            } else {
                self.first_seen = self.first_seen.min(slot.first_seen);
                self.last_seen = self.last_seen.max(slot.last_seen);
            }
        }
        self.count = self.count.saturating_add(slot.count);
        self.sample_dropped |= slot.sample_dropped != 0;
        self.degraded |= slot.degraded != 0;
    }
}

/// Merged aggregate rows plus the "already reported" watermark.
///
/// A `HashMap` is used because `AggKey` is only `Eq + Hash` (its byte arrays
/// would allow an order, but the map ABI does not promise one); rows are sorted
/// on the way out so the CSV stays deterministic.
#[derive(Debug, Default)]
pub struct AggTable {
    rows: HashMap<(u64, AggKey), AggRecord>,
    last_reported: Option<u64>,
    late_dropped: u64,
}

impl AggTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Highest window id already reported, if any window has been reported.
    #[cfg(test)]
    pub fn last_reported(&self) -> Option<u64> {
        self.last_reported
    }

    /// Number of keys waiting for their window to close.
    #[cfg(test)]
    pub fn pending_len(&self) -> usize {
        self.rows.len()
    }

    /// Keys dropped because their window had already been reported.
    pub fn late_dropped(&self) -> u64 {
        self.late_dropped
    }

    /// Whether `window_id` has already been reported and is therefore closed
    /// for new keys and samples.
    pub fn is_late(&self, window_id: u64) -> bool {
        matches!(self.last_reported, Some(reported) if window_id <= reported)
    }

    /// Merges one slot read from the per-CPU `AGG` map.
    ///
    /// Keys of an already reported window are dropped here (and counted), so a
    /// late packet can never re-open a window that was already written out.
    pub fn ingest_slot(&mut self, key: AggKey, slot: &AggValue) {
        if self.is_late(key.window_id) {
            self.late_dropped += 1;
            return;
        }
        let record = self.rows.entry((key.window_id, key)).or_insert(AggRecord {
            window_id: key.window_id,
            key,
            ..AggRecord::default()
        });
        record.merge(slot);
    }

    /// Removes and returns every row belonging to a window closed at
    /// `watermark` (see [`closed_window_watermark`]).
    ///
    /// Rows of windows that are still open stay in the table.  Rows whose
    /// window is at or below the previous watermark are late: they are dropped
    /// and counted instead of being emitted.  The watermark always advances to
    /// the scanned value, so a later packet for an empty closed window is late
    /// rather than a second report of that window.
    pub fn take_closed(&mut self, watermark: u64) -> Vec<AggRecord> {
        let mut closed = Vec::new();
        let mut late = 0u64;
        let last_reported = self.last_reported;
        self.rows.retain(|(window_id, _key), record| {
            if *window_id > watermark {
                return true; // window still open, keep accumulating
            }
            if matches!(last_reported, Some(reported) if *window_id <= reported) {
                late += 1;
                return false;
            }
            record.window_complete = true;
            closed.push(*record);
            false
        });
        self.late_dropped += late;
        self.last_reported = Some(match last_reported {
            Some(previous) => previous.max(watermark),
            None => watermark,
        });
        sort_records(&mut closed);
        closed
    }

    /// Drains every row that is still pending, for shutdown.
    ///
    /// Windows that never closed are flushed with `window_complete = 0`.
    pub fn take_pending(&mut self) -> Vec<AggRecord> {
        let mut rows: Vec<AggRecord> = self
            .rows
            .drain()
            .map(|(_, mut record)| {
                record.window_complete = false;
                record
            })
            .collect();
        sort_records(&mut rows);
        rows
    }
}

/// Sorts aggregate rows into the deterministic CSV order.
pub fn sort_records(rows: &mut [AggRecord]) {
    rows.sort_unstable_by(record_order);
}

fn record_order(a: &AggRecord, b: &AggRecord) -> Ordering {
    a.window_id
        .cmp(&b.window_id)
        .then_with(|| key_order(&a.key, &b.key))
}

fn key_order(a: &AggKey, b: &AggKey) -> Ordering {
    a.family
        .cmp(&b.family)
        .then_with(|| a.direction.cmp(&b.direction))
        .then_with(|| a.src_prefix.cmp(&b.src_prefix))
        .then_with(|| a.dst_addr.cmp(&b.dst_addr))
        .then_with(|| a.dst_port.cmp(&b.dst_port))
        .then_with(|| a.protocol.cmp(&b.protocol))
        .then_with(|| a.action.cmp(&b.action))
        .then_with(|| a.rule_id.cmp(&b.rule_id))
        .then_with(|| a.reason.cmp(&b.reason))
}

/// Outcome of offering one ring-buffer sample to [`SampleDedup`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleIngest {
    /// First sample for this `(window_id, key)`.
    First,
    /// The key was already sampled; the smallest `ts_ns` was kept.
    Duplicate,
    /// The window was already reported; the sample was dropped.
    LateDropped,
}

/// At most one sample per `(window_id, key)`, keeping the smallest `ts_ns`.
///
/// The eBPF side tries to write a sample on the first packet of a key per
/// window *and per CPU*, and two source addresses can share one masked prefix
/// key, so duplicates are expected and must not produce duplicate CSV rows.
#[derive(Debug, Default)]
pub struct SampleDedup {
    pending: HashMap<(u64, AggKey), SampleEvent>,
    deduped: u64,
    late_dropped: u64,
}

impl SampleDedup {
    pub fn new() -> Self {
        Self::default()
    }

    /// Samples dropped because their window had already been reported.
    pub fn late_dropped(&self) -> u64 {
        self.late_dropped
    }

    /// Duplicate samples absorbed into an already pending key.
    pub fn deduped(&self) -> u64 {
        self.deduped
    }

    /// Number of deduplicated samples waiting to be written.
    #[cfg(test)]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Offers one sample whose aggregation key was rebuilt with
    /// `AggKey::from_sample` (so the key uses the masked source prefix).
    ///
    /// `late` is the caller's [`AggTable::is_late`] answer for
    /// `sample.window_id`; it is passed in so both stores use one watermark.
    pub fn ingest(&mut self, sample: SampleEvent, key: AggKey, late: bool) -> SampleIngest {
        if late {
            self.late_dropped += 1;
            return SampleIngest::LateDropped;
        }
        match self.pending.entry((sample.window_id, key)) {
            Entry::Occupied(mut entry) => {
                self.deduped += 1;
                if sample.ts_ns < entry.get().ts_ns {
                    *entry.get_mut() = sample;
                }
                SampleIngest::Duplicate
            }
            Entry::Vacant(entry) => {
                entry.insert(sample);
                SampleIngest::First
            }
        }
    }

    /// Removes and returns the samples of all windows closed at `watermark`.
    pub fn take_closed(&mut self, watermark: u64) -> Vec<SampleEvent> {
        let mut closed = Vec::new();
        self.pending.retain(|(window_id, _key), sample| {
            if *window_id <= watermark {
                closed.push((*window_id, *sample));
                false
            } else {
                true
            }
        });
        sort_samples(&mut closed);
        closed
            .into_iter()
            .map(|(_window_id, sample)| sample)
            .collect()
    }

    /// Drains every pending sample, for shutdown.
    pub fn take_all(&mut self) -> Vec<SampleEvent> {
        let mut all: Vec<(u64, SampleEvent)> = self
            .pending
            .drain()
            .map(|((window_id, _key), sample)| (window_id, sample))
            .collect();
        sort_samples(&mut all);
        all.into_iter().map(|(_window_id, sample)| sample).collect()
    }
}

fn sort_samples(samples: &mut [(u64, SampleEvent)]) {
    samples.sort_unstable_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.ts_ns.cmp(&b.1.ts_ns))
            .then_with(|| a.1.src_addr.cmp(&b.1.src_addr))
            .then_with(|| a.1.dst_addr.cmp(&b.1.dst_addr))
            .then_with(|| a.1.dst_port.cmp(&b.1.dst_port))
            .then_with(|| a.1.protocol.cmp(&b.1.protocol))
    });
}

/// Highest window id whose close time has been reached, i.e. the window ended
/// at least `grace_ns` ago.
///
/// `window_id` is `ts_ns / window_ns`, so window `w` ends at
/// `(w + 1) * window_ns`; it is closed when
/// `(w + 1) * window_ns + grace_ns <= now_mono`.  Returns `None` while not even
/// window 0 is closed (including the degenerate `window_ns == 0` case, which
/// must never panic and never close anything).
pub fn closed_window_watermark(now_mono: u64, window_ns: u64, grace_ns: u64) -> Option<u64> {
    if window_ns == 0 {
        return None;
    }
    let deadline = now_mono.saturating_sub(grace_ns);
    (deadline / window_ns).checked_sub(1)
}

/// Monotonic start of `window_id` in nanoseconds.
pub fn window_start_ns(window_id: u64, window_ns: u64) -> u64 {
    window_id.saturating_mul(window_ns)
}

/// Monotonic end (exclusive) of `window_id` in nanoseconds.
pub fn window_end_ns(window_id: u64, window_ns: u64) -> u64 {
    window_id.saturating_add(1).saturating_mul(window_ns)
}

/// Converts `bpf_ktime_get_ns()` (CLOCK_MONOTONIC) timestamps to wall-clock
/// Unix milliseconds and lays out window bounds.
///
/// Both clocks are sampled once at startup, so the offset is constant and no
/// per-row syscall is needed.  `bpf_ktime_get_ns` shares the CLOCK_MONOTONIC
/// time base on the kernels supported by aya.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Clock {
    unix_ns: u128,
    mono_ns: u64,
}

impl Clock {
    /// Pairs one wall-clock reading with one monotonic reading taken at
    /// (nearly) the same instant.
    pub const fn new(unix_ns: u128, mono_ns: u64) -> Self {
        Self { unix_ns, mono_ns }
    }

    /// Wall-clock Unix milliseconds for a monotonic timestamp.
    ///
    /// Timestamps *before* the sampled monotonic instant are valid: the window
    /// that contains the probe's first packet may have started a few seconds
    /// before the process did.  Subtracting that delta is what makes
    /// `window_start_ms` a real wall-clock boundary instead of clamping it to
    /// the startup time.  Only a timestamp before the Unix epoch itself is
    /// clamped to 0.
    pub fn unix_ms(&self, mono_ts_ns: u64) -> u64 {
        let ns = if mono_ts_ns >= self.mono_ns {
            self.unix_ns + u128::from(mono_ts_ns - self.mono_ns)
        } else {
            self.unix_ns
                .saturating_sub(u128::from(self.mono_ns - mono_ts_ns))
        };
        (ns / 1_000_000).min(u64::MAX as u128) as u64
    }

    /// Wall-clock start of `window_id` in Unix milliseconds.
    pub fn window_start_ms(&self, window_id: u64, window_ns: u64) -> u64 {
        self.unix_ms(window_start_ns(window_id, window_ns))
    }

    /// Wall-clock end (exclusive) of `window_id` in Unix milliseconds.
    pub fn window_end_ms(&self, window_id: u64, window_ns: u64) -> u64 {
        self.unix_ms(window_end_ns(window_id, window_ns))
    }
}

/// Renders the AGG key's action as the contract's `pass`/`block` text.
pub fn format_action(action: u8) -> &'static str {
    if action == ACTION_BLOCK {
        "block"
    } else {
        "pass"
    }
}

/// Renders the AGG key's direction.  Train 0 only produces
/// [`DIRECTION_INGRESS`]; anything else is rendered as `unknown` so a future
/// direction bug is visible in the CSV instead of being silently mislabelled.
pub fn format_direction(direction: u8) -> &'static str {
    match direction {
        DIRECTION_INGRESS => "ingress",
        _ => "unknown",
    }
}

/// Renders the masked source prefix as CIDR (e.g. `10.99.0.0/24`,
/// `fd00:99::/64`).
///
/// The prefix length comes from the read-back `CONFIG`, which is the only
/// source userspace may use for it.
pub fn format_src_prefix(key: &AggKey, config: &ProbeConfig) -> String {
    match key.family {
        FAMILY_IPV4 => format!("{}/{}", ipv4_from(&key.src_prefix), config.ipv4_prefix_len),
        FAMILY_IPV6 => format!(
            "{}/{}",
            Ipv6Addr::from(key.src_prefix),
            config.ipv6_prefix_len
        ),
        // The data path only writes the two families above; keep a stable
        // fallback instead of panicking on a corrupt key.
        _ => format!("{}/128", Ipv6Addr::from(key.src_prefix)),
    }
}

/// Renders the destination address (dotted quad for IPv4, RFC 5952 compressed
/// form for IPv6).
pub fn format_dst_addr(key: &AggKey) -> String {
    match key.family {
        FAMILY_IPV4 => ipv4_from(&key.dst_addr).to_string(),
        _ => Ipv6Addr::from(key.dst_addr).to_string(),
    }
}

fn ipv4_from(bytes: &[u8; 16]) -> Ipv4Addr {
    Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3])
}

#[cfg(test)]
mod tests {
    use super::*;
    use probe_common::{ACTION_PASS, CONFIG_VERSION, FAMILY_IPV4, FAMILY_IPV6, SampleEvent};

    const WINDOW_NS: u64 = 10_000_000_000;
    const GRACE_NS: u64 = 2_000_000_000;

    fn ipv4_bytes(a: u8, b: u8, c: u8, d: u8) -> [u8; 16] {
        let mut addr = [0u8; 16];
        addr[..4].copy_from_slice(&[a, b, c, d]);
        addr
    }

    fn test_config() -> ProbeConfig {
        ProbeConfig {
            version: CONFIG_VERSION,
            window_ns: WINDOW_NS,
            grace_ns: GRACE_NS,
            ipv4_prefix_len: 24,
            ipv6_prefix_len: 64,
            default_action: ACTION_PASS,
            pad: [0; 5],
        }
    }

    /// The IPCMP-ish key used by most tests: IPv4 /24 prefix, one destination.
    fn key(window_id: u64) -> AggKey {
        AggKey {
            window_id,
            rule_id: 0,
            direction: DIRECTION_INGRESS,
            family: FAMILY_IPV4,
            src_prefix: ipv4_bytes(10, 99, 0, 0),
            dst_addr: ipv4_bytes(10, 99, 0, 1),
            dst_port: 0,
            protocol: 1,
            action: ACTION_PASS,
            reason: 0,
            pad: [0; 5],
        }
    }

    fn slot(count: u64, first_seen: u64, last_seen: u64, dropped: u8, degraded: u8) -> AggValue {
        AggValue {
            count,
            first_seen,
            last_seen,
            sample_dropped: dropped,
            degraded,
            pad: [0; 6],
        }
    }

    fn sample(ts_ns: u64, window_id: u64) -> SampleEvent {
        SampleEvent {
            ts_ns,
            window_id,
            src_addr: ipv4_bytes(10, 99, 0, 2),
            dst_addr: ipv4_bytes(10, 99, 0, 1),
            src_port: 0x1234,
            dst_port: 0x0050,
            protocol: 6,
            family: FAMILY_IPV4,
            action: ACTION_PASS,
            reason: 0,
            rule_id: 0,
            reserved: 0,
        }
    }

    #[test]
    fn zero_count_slot_does_not_pollute_first_seen() {
        // CPU 0 saw traffic, CPU 1 has an all-zero value for the same key (a
        // slot that lost the count race).  The zero timestamps must not win.
        let mut record = AggRecord::default();
        record.merge(&slot(7, 100, 200, 0, 0));
        record.merge(&slot(0, 0, 0, 0, 0));

        assert_eq!(record.count, 7);
        assert_eq!(record.first_seen, 100);
        assert_eq!(record.last_seen, 200);
        assert!(!record.sample_dropped);
        assert!(!record.degraded);
    }

    #[test]
    fn merges_per_cpu_slots_by_summing_and_unioning_flags() {
        let mut table = AggTable::new();
        let k = key(1);
        table.ingest_slot(k, &slot(60, 100, 200, 1, 0));
        table.ingest_slot(k, &slot(40, 90, 250, 0, 1));
        // A third CPU only has an empty slot; it must not change timestamps but
        // still participates in the count/flags accumulation.
        table.ingest_slot(k, &slot(0, 0, 0, 0, 1));

        let rows = table.take_closed(1);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.count, 100);
        assert_eq!(row.first_seen, 90);
        assert_eq!(row.last_seen, 250);
        assert!(row.sample_dropped);
        assert!(row.degraded);
        assert!(row.window_complete);
        assert_eq!(row.key, k);
    }

    #[test]
    fn window_closes_only_after_window_end_plus_grace() {
        // Window 0 ends at WINDOW_NS and is closed once grace has passed too.
        assert_eq!(
            closed_window_watermark(WINDOW_NS + GRACE_NS - 1, WINDOW_NS, GRACE_NS),
            None
        );
        assert_eq!(
            closed_window_watermark(WINDOW_NS + GRACE_NS, WINDOW_NS, GRACE_NS),
            Some(0)
        );
        // Window 1 closes one window later.
        assert_eq!(
            closed_window_watermark(2 * WINDOW_NS + GRACE_NS - 1, WINDOW_NS, GRACE_NS),
            Some(0)
        );
        assert_eq!(
            closed_window_watermark(2 * WINDOW_NS + GRACE_NS, WINDOW_NS, GRACE_NS),
            Some(1)
        );
        // Degenerate input must not panic or close anything.
        assert_eq!(closed_window_watermark(u64::MAX, 0, 0), None);
        assert_eq!(closed_window_watermark(0, WINDOW_NS, GRACE_NS), None);
    }

    #[test]
    fn open_window_rows_stay_pending_until_closed() {
        let mut table = AggTable::new();
        table.ingest_slot(key(3), &slot(5, 1, 2, 0, 0));

        assert!(table.take_closed(2).is_empty());
        assert_eq!(table.pending_len(), 1);
        assert_eq!(table.last_reported(), Some(2));

        let rows = table.take_closed(3);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].window_id, 3);
        assert!(rows[0].window_complete);
        assert_eq!(table.pending_len(), 0);
    }

    #[test]
    fn late_key_after_reported_window_is_dropped_not_reopened() {
        let mut table = AggTable::new();
        table.ingest_slot(key(0), &slot(1, 10, 10, 0, 0));
        assert_eq!(table.take_closed(0).len(), 1);
        assert_eq!(table.last_reported(), Some(0));

        // One more packet for the same (already reported) window: dropped.
        table.ingest_slot(key(0), &slot(1, 20, 20, 0, 0));
        assert_eq!(table.late_dropped(), 1);
        assert_eq!(table.pending_len(), 0);
        assert!(table.is_late(0));
        assert!(!table.is_late(1));

        // The next scan must not emit a second row for window 0.
        assert!(table.take_closed(5).is_empty());
        assert_eq!(table.pending_len(), 0);
    }

    #[test]
    fn watermark_advances_even_when_a_window_had_no_traffic() {
        let mut table = AggTable::new();
        // Nothing scraped yet, but windows 0..=2 are closed and reported empty.
        assert!(table.take_closed(2).is_empty());
        assert!(table.is_late(2));

        table.ingest_slot(key(2), &slot(1, 0, 0, 0, 0));
        assert_eq!(table.late_dropped(), 1);
        assert_eq!(table.pending_len(), 0);
    }

    #[test]
    fn pending_rows_flush_marked_incomplete() {
        let mut table = AggTable::new();
        table.ingest_slot(key(1), &slot(2, 5, 6, 0, 0));

        let rows = table.take_pending();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].count, 2);
        assert!(!rows[0].window_complete);
        assert_eq!(table.pending_len(), 0);
    }

    #[test]
    fn rows_are_sorted_by_window_and_key() {
        let mut table = AggTable::new();
        let mut other = key(0);
        other.dst_port = 443;
        table.ingest_slot(other, &slot(1, 1, 1, 0, 0));
        table.ingest_slot(key(1), &slot(1, 1, 1, 0, 0));
        table.ingest_slot(key(0), &slot(1, 1, 1, 0, 0));

        let rows = table.take_closed(1);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].window_id, 0);
        assert_eq!(rows[0].key.dst_port, 0);
        assert_eq!(rows[1].window_id, 0);
        assert_eq!(rows[1].key.dst_port, 443);
        assert_eq!(rows[2].window_id, 1);
    }

    #[test]
    fn sample_dedup_keeps_earliest_ts_and_counts_duplicates() {
        let config = test_config();
        let mut dedup = SampleDedup::new();

        let first = sample(200, 4);
        let k = AggKey::from_sample(&first, &config);
        assert_eq!(
            dedup.ingest(first, k, false),
            SampleIngest::First,
            "first sample for the key is stored"
        );
        // A second CPU sampled the same key later in the same window.
        assert_eq!(
            dedup.ingest(sample(300, 4), k, false),
            SampleIngest::Duplicate
        );
        // A packet with a smaller timestamp wins.
        assert_eq!(
            dedup.ingest(sample(100, 4), k, false),
            SampleIngest::Duplicate
        );
        assert_eq!(dedup.deduped(), 2);
        assert_eq!(dedup.pending_len(), 1);

        let written = dedup.take_closed(4);
        assert_eq!(written.len(), 1);
        assert_eq!(written[0].ts_ns, 100);
        assert_eq!(dedup.pending_len(), 0);
    }

    #[test]
    fn samples_for_a_reported_window_are_late_dropped() {
        let config = test_config();
        let mut dedup = SampleDedup::new();
        let late = sample(1, 2);
        let k = AggKey::from_sample(&late, &config);

        assert_eq!(dedup.ingest(late, k, true), SampleIngest::LateDropped);
        assert_eq!(dedup.late_dropped(), 1);
        assert_eq!(dedup.pending_len(), 0);
        assert_eq!(dedup.take_all().len(), 0);

        // A different key in the same reported window is dropped as well.
        let mut other = sample(2, 2);
        other.src_addr = ipv4_bytes(10, 99, 7, 7);
        let other_key = AggKey::from_sample(&other, &config);
        assert_eq!(
            dedup.ingest(other, other_key, true),
            SampleIngest::LateDropped
        );
        assert_eq!(dedup.late_dropped(), 2);
    }

    #[test]
    fn sample_dedup_key_uses_the_masked_prefix() {
        let config = test_config();
        let mut dedup = SampleDedup::new();

        // Two source addresses in the same /24 produce one key and therefore
        // exactly one sample row.
        let a = sample(10, 5);
        let b = {
            let mut b = sample(20, 5);
            b.src_addr = ipv4_bytes(10, 99, 0, 99);
            b
        };
        let key_a = AggKey::from_sample(&a, &config);
        let key_b = AggKey::from_sample(&b, &config);
        assert_eq!(key_a, key_b, "/24 collapses both sources onto one key");

        assert_eq!(dedup.ingest(a, key_a, false), SampleIngest::First);
        assert_eq!(dedup.ingest(b, key_b, false), SampleIngest::Duplicate);
        let written = dedup.take_all();
        assert_eq!(written.len(), 1);
        assert_eq!(written[0].ts_ns, 10);
    }

    #[test]
    fn take_closed_only_removes_samples_of_closed_windows() {
        let config = test_config();
        let mut dedup = SampleDedup::new();
        let old = sample(1, 1);
        let open = sample(2, 7);
        let open_key = AggKey::from_sample(&open, &config);
        dedup.ingest(old, AggKey::from_sample(&old, &config), false);
        dedup.ingest(open, open_key, false);

        let closed = dedup.take_closed(3);
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].window_id, 1);
        assert_eq!(dedup.pending_len(), 1);
        assert_eq!(dedup.take_all().len(), 1);
        assert_eq!(dedup.pending_len(), 0);
    }

    #[test]
    fn renders_action_direction_prefix_and_addresses() {
        let config = test_config();
        let k = key(0);

        assert_eq!(format_action(ACTION_PASS), "pass");
        assert_eq!(format_action(ACTION_BLOCK), "block");
        assert_eq!(format_direction(DIRECTION_INGRESS), "ingress");
        assert_eq!(format_src_prefix(&k, &config), "10.99.0.0/24");
        assert_eq!(format_dst_addr(&k), "10.99.0.1");

        // IPv6 uses the RFC 5952 compressed form and the configured /64.
        let mut v6 = key(0);
        v6.family = FAMILY_IPV6;
        v6.src_prefix = Ipv6Addr::new(0xfd00, 0x99, 0, 0, 0, 0, 0, 0).octets();
        v6.dst_addr = Ipv6Addr::new(0xfd00, 0x99, 0, 0, 0, 0, 0, 1).octets();
        assert_eq!(format_src_prefix(&v6, &config), "fd00:99::/64");
        assert_eq!(format_dst_addr(&v6), "fd00:99::1");
    }

    #[test]
    fn clock_maps_monotonic_to_wall_clock_ms_and_window_bounds() {
        // Wall clock 1_000_000_000 ns paired with monotonic 500 ns.
        let clock = Clock::new(1_000_000_000, 500);
        assert_eq!(clock.unix_ms(500), 1_000);
        assert_eq!(clock.unix_ms(1_000_500), 1_001);
        // Timestamps before the sampled monotonic instant keep their exact
        // negative offset: a window that started before the probe did must not
        // be clamped to the startup wall clock.
        assert_eq!(clock.unix_ms(0), 999);
        let old_window = Clock::new(10_000_000, 5_000_000);
        assert_eq!(old_window.window_start_ms(0, 1_000_000), 5);
        assert_eq!(old_window.window_end_ms(0, 1_000_000), 6);

        // Window bounds use the same offset; a clock sampled on a window
        // boundary keeps the arithmetic exact.
        let bounds = Clock::new(1_000_000_000_000, 0);
        assert_eq!(bounds.window_start_ms(0, WINDOW_NS), 1_000_000);
        assert_eq!(bounds.window_end_ms(0, WINDOW_NS), 1_010_000);
        assert_eq!(bounds.window_start_ms(1, WINDOW_NS), 1_010_000);
        assert_eq!(bounds.window_end_ms(1, WINDOW_NS), 1_020_000);
    }
}
