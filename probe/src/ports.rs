//! Ports through which the pipeline reaches the kernel and the clock.
//!
//! [`crate::pipeline::Session`] never touches an `aya` map or a `libc` call
//! directly: it drives the three traits defined here.  Production code passes the
//! thin adapters below ([`AggStore`] on the real per-CPU map,
//! [`RingSampleSource`] on the real ring buffer, [`MonoClock`] for time), while
//! the L2 tests in `tests/pipeline.rs` pass fakes that can inject a failed ring
//! reservation or a map read error without a kernel and without privileges.
//!
//! # Why `next_record` returns [`Poll`] instead of `Option<&[u8]>`
//!
//! `aya::maps::RingBuf::next` hands out a `RingBufItem` that borrows the ring
//! buffer and keeps the consumer position pinned until it is dropped, so the
//! bytes cannot be borrowed straight out of `&mut self` (that would be a
//! self-referential borrow).  [`Poll`] therefore separates the three outcomes the
//! pipeline actually has to distinguish -- a record, a failed reservation (the
//! data path dropped a sample) and "nothing queued" -- and
//! [`RingSampleSource`] copies the record into a reusable buffer.  The copy is
//! 64 bytes against the syscall that produced it.

use anyhow::{Context as _, Result};
use aya::maps::{MapData, PerCpuHashMap, RingBuf};
#[rustfmt::skip]
use log::warn;
use probe_common::{AggKey, AggValue};

/// Outcome of one attempt at reading the sample ring.
#[derive(Debug)]
pub enum Poll<'a> {
    /// One complete record, exactly as the data path wrote it.
    Record(&'a [u8]),
    /// The data path could not reserve space for a sample; the packet was still
    /// counted in `AGG`, only its sample is missing.
    ReservationFailed,
    /// The ring buffer is empty.
    Empty,
}

/// Reads sample records out of the ring buffer.
pub trait SampleSource {
    /// Returns the next queued record, or [`Poll::Empty`] when the ring is
    /// drained.  Once a [`Poll::Record`] is returned the source has consumed the
    /// record: the borrow of `self` ends when the caller drops the returned
    /// value, and the next call advances to the following record.
    fn next_record(&mut self) -> Poll<'_>;
}

/// The per-CPU aggregation map (`AGG`).
///
/// One key can have one value per CPU; [`AggStore::for_each_slot`] visits every
/// `(key, slot)` pair, so the caller merges the slots exactly like the data path
/// accumulated them.  Removal is a second pass ([`AggStore::delete`]) because the
/// visitor holds a borrow of the store while it runs.
pub trait AggStore {
    /// Visits every `(key, per-CPU slot)` pair currently stored.
    fn for_each_slot(&mut self, f: &mut dyn FnMut(&AggKey, &AggValue));

    /// Removes `key` from the store.  Missing keys are not an error: the data
    /// path may have raced another consumer.
    fn delete(&mut self, key: &AggKey);

    /// Number of distinct keys currently stored.
    fn len(&self) -> usize;

    /// Whether the store holds no key at all.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The monotonic clock (`CLOCK_MONOTONIC`), the time base of
/// `bpf_ktime_get_ns()`.
///
/// Reading the clock can fail, so the port returns a `Result` instead of
/// panicking inside the scan path.
pub trait TimeSource {
    fn mono_ns(&self) -> Result<u64>;
}

/// [`TimeSource`] backed by `clock_gettime(CLOCK_MONOTONIC)`.
#[derive(Clone, Copy, Debug, Default)]
pub struct MonoClock;

impl TimeSource for MonoClock {
    fn mono_ns(&self) -> Result<u64> {
        mono_now_ns()
    }
}

/// Monotonic nanoseconds (`CLOCK_MONOTONIC`), the same time base as
/// `bpf_ktime_get_ns()`.
pub fn mono_now_ns() -> Result<u64> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `clock_gettime` only writes into the `timespec` passed by pointer;
    // the pointer is valid for the duration of the call.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("clock_gettime(CLOCK_MONOTONIC) failed");
    }
    Ok(ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64)
}

/// [`SampleSource`] backed by the `SAMPLES` ring buffer.
pub struct RingSampleSource {
    ring: RingBuf<MapData>,
    record: Vec<u8>,
}

impl RingSampleSource {
    pub fn new(ring: RingBuf<MapData>) -> Self {
        Self {
            ring,
            record: Vec::new(),
        }
    }
}

impl SampleSource for RingSampleSource {
    fn next_record(&mut self) -> Poll<'_> {
        let Some(item) = self.ring.next() else {
            return Poll::Empty;
        };
        // Copy out: `item` holds the mutable borrow of the ring buffer until it
        // is dropped at the end of this function, and the pipeline needs the
        // bytes after `next()` returns.  `record` keeps its capacity across
        // calls, so a steady drain does not allocate.
        self.record.clear();
        self.record.extend_from_slice(&item);
        Poll::Record(&self.record)
    }
}

impl AggStore for PerCpuHashMap<MapData, AggKey, AggValue> {
    fn for_each_slot(&mut self, f: &mut dyn FnMut(&AggKey, &AggValue)) {
        // The map iterator holds the borrow of the map, and `get` needs it too,
        // so the keys are collected first.
        let mut keys: Vec<AggKey> = Vec::new();
        for key in self.keys() {
            match key {
                Ok(key) => keys.push(key),
                Err(e) => {
                    warn!("AGG key iteration failed: {e}");
                    break;
                }
            }
        }

        for key in keys {
            match self.get(&key, 0) {
                // `PerCpuValues` derefs to one entry per CPU; visit them all.
                Ok(values) => {
                    for slot in values.iter() {
                        f(&key, slot);
                    }
                }
                Err(e) => warn!("failed to read AGG key: {e}"),
            }
        }
    }

    fn delete(&mut self, key: &AggKey) {
        if let Err(e) = self.remove(key) {
            warn!("failed to remove AGG key: {e}");
        }
    }

    fn len(&self) -> usize {
        let mut count = 0;
        for key in self.keys() {
            match key {
                Ok(_) => count += 1,
                Err(e) => {
                    warn!("AGG key iteration failed while counting: {e}");
                    break;
                }
            }
        }
        count
    }
}
