//! Retry schedules, copied from the contract's tables.
//!
//! Every number here comes from `docs/Control-Agent-API-v1.md` §2.6 (unary
//! calls), §5.7 (Watch reconnects, 60s cap) and §8 (Register bootstrap, 5
//! attempts for `AckConfig`). The tables are transcribed as data instead of
//! computed from one growth factor because the contract's first two steps are
//! not geometric: `GetConfig` retries after 2s, 5s, 10s — not 2s, 4s, 8s.
//!
//! Jitter is opt-out: [`Backoff::deterministic`] returns the table values
//! unchanged, which is what the tests assert, while [`Backoff::new`] subtracts
//! up to 1/8 of the delay from a caller-supplied clock. Subtracting (never
//! adding) keeps the contract's cap a real ceiling.

use std::{fmt, sync::Arc, time::Duration};

use crate::client::Rpc;

/// One retry schedule: explicit delays, then doubling until `cap`, and an
/// optional attempt limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Schedule {
    /// The contract's literal delays for the first attempts, in milliseconds.
    steps_ms: &'static [u64],
    /// Hard ceiling in milliseconds.
    cap_ms: u64,
    /// How many retries are allowed before the call is given up; `None` = retry
    /// forever. Only `AckConfig` has a limit (§2.6: "最多 5 次").
    max_attempts: Option<u32>,
}

impl Schedule {
    /// The delay before retry number `attempt` (0 = the first retry).
    ///
    /// Past the end of `steps_ms` the last step doubles, capped at `cap_ms`
    /// (so Register goes 1, 2, 4, 8→10, 10, … and Watch 1, 2, 4, 8, 16, 32,
    /// 64→60, 60, …).
    pub fn delay_ms(&self, attempt: u32) -> u64 {
        let Some(&last) = self.steps_ms.last() else {
            return self.cap_ms;
        };
        let len = self.steps_ms.len() as u32;
        let raw = if attempt < len {
            self.steps_ms[attempt as usize]
        } else {
            // `checked_shl` keeps a pathological attempt count from
            // overflowing; anything that big is capped anyway.
            let shift = attempt - len + 1;
            last.checked_shl(shift).unwrap_or(u64::MAX)
        };
        raw.min(self.cap_ms)
    }

    pub fn cap(&self) -> Duration {
        Duration::from_millis(self.cap_ms)
    }

    pub fn max_attempts(&self) -> Option<u32> {
        self.max_attempts
    }

    /// Register bootstrap: 1/2/4s, cap 10s (§2.6).
    pub const REGISTER: Self = Self {
        steps_ms: &[1_000, 2_000, 4_000],
        cap_ms: 10_000,
        max_attempts: None,
    };

    /// Heartbeat: 1/2/4s, cap 30s (§2.6).
    pub const HEARTBEAT: Self = Self {
        steps_ms: &[1_000, 2_000, 4_000],
        cap_ms: 30_000,
        max_attempts: None,
    };

    /// GetConfig: 2/5/10s, cap 30s (§2.6).
    pub const GET_CONFIG: Self = Self {
        steps_ms: &[2_000, 5_000, 10_000],
        cap_ms: 30_000,
        max_attempts: None,
    };

    /// GetCursor: "同 GetConfig" (§2.6).
    pub const GET_CURSOR: Self = Self::GET_CONFIG;

    /// AckConfig: 1/2/4s, cap 10s, at most 5 attempts, never blocks reports
    /// (§2.6).
    pub const ACK_CONFIG: Self = Self {
        steps_ms: &[1_000, 2_000, 4_000],
        cap_ms: 10_000,
        max_attempts: Some(5),
    };

    /// ReportWindows: 1/2/4s, cap 30s (§2.6).
    pub const REPORT_WINDOWS: Self = Self {
        steps_ms: &[1_000, 2_000, 4_000],
        cap_ms: 30_000,
        max_attempts: None,
    };

    /// Watch reconnect: 1/2/4/8…, cap 60s (§5.7).
    pub const WATCH: Self = Self {
        steps_ms: &[1_000, 2_000, 4_000, 8_000],
        cap_ms: 60_000,
        max_attempts: None,
    };

    /// The schedule of one RPC class, or `None` for calls that are not retried
    /// at all.
    pub fn for_rpc(rpc: Rpc) -> Option<Self> {
        Some(match rpc {
            Rpc::Register => Self::REGISTER,
            Rpc::Heartbeat => Self::HEARTBEAT,
            Rpc::GetConfig => Self::GET_CONFIG,
            Rpc::GetCursor => Self::GET_CURSOR,
            Rpc::AckConfig => Self::ACK_CONFIG,
            Rpc::ReportWindows => Self::REPORT_WINDOWS,
            Rpc::Watch => Self::WATCH,
        })
    }
}

/// Where the jitter comes from: a clock reading in nanoseconds.
///
/// Injected so tests get predictable delays without a `rand` dependency.
pub type JitterClock = Arc<dyn Fn() -> u64 + Send + Sync>;

fn monotonic_ns() -> u64 {
    // The jitter only needs *some* varying value; a coarse fallback would still
    // be correct, so a failed clock read must not fail a retry.
    probe::ports::mono_now_ns().unwrap_or(0)
}

/// One `Schedule` plus the attempt counter and the jitter source.
#[derive(Clone)]
pub struct Backoff {
    schedule: Schedule,
    attempt: u32,
    /// `None` = deterministic (tests); `Some` = jitter up to 1/8 off the delay.
    jitter_clock: Option<JitterClock>,
}

impl fmt::Debug for Backoff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Backoff")
            .field("schedule", &self.schedule)
            .field("attempt", &self.attempt)
            .field("jitter", &self.jitter_clock.is_some())
            .finish()
    }
}

impl Backoff {
    /// Production backoff: jitter driven by the monotonic clock.
    pub fn new(schedule: Schedule) -> Self {
        Self {
            schedule,
            attempt: 0,
            jitter_clock: Some(Arc::new(monotonic_ns)),
        }
    }

    /// Table values, byte for byte, no jitter. Used by the tests.
    pub fn deterministic(schedule: Schedule) -> Self {
        Self {
            schedule,
            attempt: 0,
            jitter_clock: None,
        }
    }

    /// Jitter from an injected clock, so a test can still exercise the jittered
    /// path without asserting wall-clock values.
    pub fn with_clock(schedule: Schedule, clock: JitterClock) -> Self {
        Self {
            schedule,
            attempt: 0,
            jitter_clock: Some(clock),
        }
    }

    /// Retries performed so far.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// Whether the schedule's attempt limit is used up.
    pub fn exhausted(&self) -> bool {
        self.schedule
            .max_attempts
            .is_some_and(|max| self.attempt >= max)
    }

    /// The next delay, advancing the attempt counter.
    ///
    /// Returns `None` once the attempt limit is used up; callers must then stop
    /// retrying (only `AckConfig` has a limit today).
    pub fn next_delay(&mut self) -> Option<Duration> {
        if self.exhausted() {
            return None;
        }
        let base_ms = self.schedule.delay_ms(self.attempt);
        self.attempt = self.attempt.saturating_add(1);
        Some(self.jitter(base_ms))
    }

    /// Contract §2.6 for `RESOURCE_EXHAUSTED`: the server's `x-retry-after-ms`
    /// replaces the local schedule when present.
    ///
    /// The contract writes `max(retry_after, backoff)`; this implementation
    /// takes the server's value as-is, because the trailer is the control
    /// plane's own admission of how long it needs, and the S-B task fixes that
    /// reading ("存在时优先用它，忽略本地退避"). Either way the attempt counter
    /// still advances so a server that keeps asking for more time cannot spin.
    pub fn next_delay_with_retry_after(
        &mut self,
        retry_after: Option<Duration>,
    ) -> Option<Duration> {
        let local = self.next_delay()?;
        Some(retry_after.unwrap_or(local))
    }

    /// Called after a success: the next failure starts from the first step
    /// again.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    /// Applies (negative) jitter to one delay.
    fn jitter(&self, base_ms: u64) -> Duration {
        let Some(clock) = &self.jitter_clock else {
            return Duration::from_millis(base_ms);
        };
        if base_ms == 0 {
            return Duration::ZERO;
        }
        // Up to 1/8 off, so the contract's cap stays a ceiling.
        let span = base_ms / 8;
        let cut = if span == 0 { 0 } else { clock() % (span + 1) };
        Duration::from_millis(base_ms - cut)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The contract table, transcribed once more, independently of the
    /// constants above: a typo in a constant must fail here.
    #[test]
    fn register_and_heartbeat_reach_their_caps() {
        let mut b = Backoff::deterministic(Schedule::REGISTER);
        let seq: Vec<u64> = (0..6)
            .map(|_| b.next_delay().expect("no attempt limit").as_millis() as u64)
            .collect();
        assert_eq!(seq, vec![1_000, 2_000, 4_000, 8_000, 10_000, 10_000]);

        let mut b = Backoff::deterministic(Schedule::HEARTBEAT);
        let seq: Vec<u64> = (0..7)
            .map(|_| b.next_delay().expect("no attempt limit").as_millis() as u64)
            .collect();
        assert_eq!(
            seq,
            vec![1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000]
        );
    }

    #[test]
    fn get_config_uses_the_contract_steps_not_a_geometric_series() {
        let mut b = Backoff::deterministic(Schedule::GET_CONFIG);
        let seq: Vec<u64> = (0..5)
            .map(|_| b.next_delay().expect("no attempt limit").as_millis() as u64)
            .collect();
        assert_eq!(seq, vec![2_000, 5_000, 10_000, 20_000, 30_000]);
        assert_eq!(Schedule::GET_CURSOR.delay_ms(0), 2_000);
    }

    #[test]
    fn watch_backs_off_to_sixty_seconds() {
        let mut b = Backoff::deterministic(Schedule::WATCH);
        let seq: Vec<u64> = (0..7)
            .map(|_| b.next_delay().expect("no attempt limit").as_millis() as u64)
            .collect();
        assert_eq!(
            seq,
            vec![1_000, 2_000, 4_000, 8_000, 16_000, 32_000, 60_000]
        );
    }

    #[test]
    fn ack_config_gives_up_after_five_attempts() {
        let mut b = Backoff::deterministic(Schedule::ACK_CONFIG);
        let seq: Vec<u64> = (0..5)
            .map(|_| b.next_delay().expect("five attempts").as_millis() as u64)
            .collect();
        assert_eq!(seq, vec![1_000, 2_000, 4_000, 8_000, 10_000]);
        assert!(b.exhausted());
        assert_eq!(b.next_delay(), None);
    }

    #[test]
    fn reset_returns_to_the_first_step() {
        let mut b = Backoff::deterministic(Schedule::REPORT_WINDOWS);
        assert_eq!(b.next_delay(), Some(Duration::from_millis(1_000)));
        assert_eq!(b.next_delay(), Some(Duration::from_millis(2_000)));
        b.reset();
        assert_eq!(b.attempt(), 0);
        assert_eq!(b.next_delay(), Some(Duration::from_millis(1_000)));
    }

    #[test]
    fn retry_after_replaces_the_local_delay() {
        let mut b = Backoff::deterministic(Schedule::REPORT_WINDOWS);
        assert_eq!(
            b.next_delay_with_retry_after(Some(Duration::from_millis(7_500))),
            Some(Duration::from_millis(7_500))
        );
        // A shorter server value wins too: it is the hint, the table is only
        // the fallback.
        assert_eq!(
            b.next_delay_with_retry_after(Some(Duration::from_millis(500))),
            Some(Duration::from_millis(500))
        );
        assert_eq!(
            b.next_delay_with_retry_after(None),
            Some(Duration::from_millis(4_000))
        );
    }

    #[test]
    fn jitter_only_shortens_and_never_exceeds_the_cap() {
        // Pin the clock to the largest cut the implementation takes (1/8 of the
        // 1000 ms first step). `u64::MAX` would not do: it is congruent to 15
        // modulo the 126-value range, not to the top of it.
        let clock: JitterClock = Arc::new(|| 125);
        let mut b = Backoff::with_clock(Schedule::REGISTER, clock);
        let first = b.next_delay().expect("no attempt limit");
        assert_eq!(first, Duration::from_millis(1_000 - 125));
        let mut seq = Vec::new();
        for _ in 0..5 {
            seq.push(b.next_delay().expect("no attempt limit"));
        }
        assert!(
            seq.iter().all(|d| *d <= Schedule::REGISTER.cap()),
            "jitter must never push past the cap: {seq:?}"
        );
    }
}
