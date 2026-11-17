//! Time conversions and sleeping, both behind injectable seams.
//!
//! Contract §9: report timestamps are *monotonic* (`bpf_ktime_get_ns` shares the
//! `CLOCK_MONOTONIC` time base) and must be converted to wall clock before they
//! cross the wire. Sampling both clocks once and keeping the offset constant is
//! what `probe::aggregate::Clock` does for the CSV path; the agent needs the
//! same trick for `google.protobuf.Timestamp`, plus a sleep seam so the state
//! machine's timers can be driven instantly in tests.

use std::{
    future::Future,
    io,
    pin::Pin,
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use probe::ports::mono_now_ns;
use prost_types::Timestamp;

/// Constant `wall_clock - monotonic` offset, sampled once.
///
/// Held as nanoseconds so a monotonic instant *before* the sample still
/// converts correctly: the window containing the probe's first packet may have
/// started before the process did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockOffset {
    offset_ns: i128,
}

impl ClockOffset {
    /// Pairs one wall-clock reading with one monotonic reading taken at
    /// (nearly) the same instant.
    pub const fn new(unix_ns: u128, mono_ns: u64) -> Self {
        Self {
            offset_ns: unix_ns as i128 - mono_ns as i128,
        }
    }

    /// Samples both clocks now. The two readings are a few hundred nanoseconds
    /// apart, which is far below the millisecond precision the wire format
    /// carries (§2.2).
    pub fn sample() -> io::Result<Self> {
        let unix_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| io::Error::other(format!("system clock before the Unix epoch: {e}")))?
            .as_nanos();
        let mono_ns = mono_now_ns().map_err(|e| io::Error::other(e.to_string()))?;
        Ok(Self::new(unix_ns, mono_ns))
    }

    /// Wall-clock Unix nanoseconds for a monotonic timestamp, clamped at the
    /// Unix epoch.
    pub fn unix_ns(&self, mono_ns: u64) -> u128 {
        let ns = self.offset_ns + i128::from(mono_ns);
        ns.max(0) as u128
    }

    /// `Timestamp` for a monotonic timestamp, truncated to milliseconds per
    /// contract §2.2 ("发出时毫秒精度，纳秒位为 0").
    pub fn timestamp(&self, mono_ns: u64) -> Timestamp {
        let ns = self.unix_ns(mono_ns);
        let nanos = (ns % 1_000_000_000) as i32;
        Timestamp {
            seconds: (ns / 1_000_000_000).min(i64::MAX as u128) as i64,
            nanos: nanos - nanos % 1_000_000,
        }
    }
}

/// The sleep seam of the state machine.
///
/// Production uses [`TokioSleeper`]; tests use [`ManualSleeper`] to record the
/// requested delays instead of waiting for them.
pub trait Sleeper: Send + Sync {
    /// Completes after `d`. Boxed rather than `async fn` so the trait stays
    /// object safe and implementors need no `async_trait` dependency.
    fn sleep(&self, d: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

impl<T: Sleeper + ?Sized> Sleeper for std::sync::Arc<T> {
    fn sleep(&self, d: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        (**self).sleep(d)
    }
}

/// Real time, via tokio's timer.
#[derive(Clone, Copy, Debug, Default)]
pub struct TokioSleeper;

impl Sleeper for TokioSleeper {
    fn sleep(&self, d: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(tokio::time::sleep(d))
    }
}

/// Records every requested delay and returns immediately.
///
/// Returning immediately keeps the tests fast but can spin a busy loop; the
/// tests that use it therefore also yield (`yield_now`) and stop the loop from
/// the shutdown future once the expected number of sleeps has been recorded.
#[derive(Debug, Default)]
pub struct ManualSleeper {
    requested: Mutex<Vec<Duration>>,
}

impl ManualSleeper {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every delay requested so far, in call order.
    pub fn requested(&self) -> Vec<Duration> {
        self.requested
            .lock()
            .expect("sleeper mutex poisoned")
            .clone()
    }

    /// How many delays have been requested so far.
    pub fn count(&self) -> usize {
        self.requested.lock().expect("sleeper mutex poisoned").len()
    }
}

impl Sleeper for ManualSleeper {
    fn sleep(&self, d: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.requested
            .lock()
            .expect("sleeper mutex poisoned")
            .push(d);
        Box::pin(tokio::task::yield_now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNIX_NS: u128 = 1_700_000_000_123_456_789;
    const MONO_NS: u64 = 5_000_000_000;

    #[test]
    fn converts_forward_and_backward_of_the_sample() {
        let clock = ClockOffset::new(UNIX_NS, MONO_NS);
        assert_eq!(clock.unix_ns(MONO_NS), UNIX_NS);
        // One second after the sample.
        assert_eq!(
            clock.unix_ns(MONO_NS + 1_000_000_000),
            UNIX_NS + 1_000_000_000
        );
        // The window may start before the process did.
        assert_eq!(
            clock.unix_ns(MONO_NS - 2_000_000_000),
            UNIX_NS - 2_000_000_000
        );
        // Before the Unix epoch clamps instead of wrapping.
        assert_eq!(ClockOffset::new(0, 1_000).unix_ns(0), 0);
    }

    #[test]
    fn timestamps_are_truncated_to_milliseconds() {
        let clock = ClockOffset::new(UNIX_NS, MONO_NS);
        let ts = clock.timestamp(MONO_NS);
        assert_eq!(ts.seconds, 1_700_000_000);
        assert_eq!(ts.nanos, 123_000_000, "§2.2: 发出时毫秒精度");

        let ts = clock.timestamp(MONO_NS + 987_654_321);
        assert_eq!(ts.seconds, 1_700_000_001);
        assert_eq!(ts.nanos, 111_000_000, "456789 + 987654321 ms-truncated");
    }

    #[tokio::test]
    async fn manual_sleeper_records_instantly() {
        let sleeper = ManualSleeper::new();
        sleeper.sleep(Duration::from_millis(7)).await;
        sleeper.sleep(Duration::from_millis(9)).await;
        assert_eq!(
            sleeper.requested(),
            vec![Duration::from_millis(7), Duration::from_millis(9)]
        );
        assert_eq!(sleeper.count(), 2);
    }
}
