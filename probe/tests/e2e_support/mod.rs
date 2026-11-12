//! Harness shared by the privileged e2e cases (E-01..E-07, TEST-PLAN §5.4).
//!
//! Every case gets its own veth pair and its own artifact directory under
//! `target/e2e/<case>/`, so the cases are independent and `cargo xtask e2e -j 4`
//! can run them in parallel.  Nothing here waits for a fixed time: a case waits
//! for an *event* (a log line, a CSV row, a kernel counter) and fails on timeout,
//! which is what keeps the suite both fast and flake-free.
#![allow(dead_code)]

use std::{
    fs,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use probe_testkit::{
    csv::{AggRow, CsvTable, SampleRow},
    frames::FrameTable,
    net::{self, VethPair},
    probe::ProbeBuilder,
    raw::FrameSender,
    workspace_root,
};

/// Upper bound for one harness step.  A step that takes longer is a bug, not a
/// slow machine, so the case fails instead of waiting longer.
pub const STEP_TIMEOUT: Duration = Duration::from_secs(30);

/// Re-check interval for [`wait_for`].  The cases wait for events, so this only
/// bounds how often an event that has not happened yet is re-read.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Per-case fixture: one veth pair, one fixture table, one artifact directory.
pub struct Env {
    pub pair: VethPair,
    pub table: FrameTable,
    pub dir: PathBuf,
    case: &'static str,
}

impl Env {
    /// Builds the case environment: fresh artifact directory plus a fresh veth
    /// pair.  Both are torn down by `Drop` (or overwritten by the next run).
    pub fn new(case: &'static str) -> Self {
        let dir = workspace_root().join("target/e2e").join(case);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir)
            .unwrap_or_else(|err| panic!("{case}: create artifact dir {}: {err}", dir.display()));
        let pair =
            VethPair::new(case).unwrap_or_else(|err| panic!("{case}: create veth pair: {err}"));
        let table = FrameTable::load(probe_testkit::frames_dir())
            .unwrap_or_else(|err| panic!("{case}: load frame fixtures: {err}"));
        Self {
            pair,
            table,
            dir,
            case,
        }
    }

    pub fn case(&self) -> &'static str {
        self.case
    }

    /// Probe builder for the probe side of the pair, writing this case's CSVs.
    pub fn probe(&self) -> ProbeBuilder {
        self.probe_at(
            self.pair.probe_iface(),
            &self.samples_csv(),
            &self.aggregates_csv(),
        )
    }

    /// Probe builder with explicit interface and output files (E-07 makes two
    /// probes share one CSV pair, so it cannot use the default paths).
    pub fn probe_at(&self, iface: &str, out: &Path, agg_out: &Path) -> ProbeBuilder {
        ProbeBuilder::new(probe_bin(), iface, out, agg_out)
    }

    pub fn samples_csv(&self) -> PathBuf {
        self.dir.join("samples.csv")
    }

    pub fn aggregates_csv(&self) -> PathBuf {
        self.dir.join("aggregates.csv")
    }

    /// AF_PACKET sender on the far end of the pair: frames it sends arrive on the
    /// probe side's ingress.
    pub fn sender(&self) -> FrameSender {
        self.sender_on(self.pair.sender_iface())
    }

    pub fn sender_on(&self, iface: &str) -> FrameSender {
        FrameSender::open(iface)
            .unwrap_or_else(|err| panic!("{}: open sender on {iface}: {err}", self.case))
    }

    /// Raw bytes of one fixture, by file name.
    pub fn frame(&self, file: &str) -> Vec<u8> {
        self.table
            .specs
            .iter()
            .find(|spec| spec.file == file)
            .map(|spec| spec.bytes.clone())
            .unwrap_or_else(|| panic!("{}: fixture {file} is not in index.tsv", self.case))
    }

    /// Aggregate rows as they are on disk right now (post-exit reads).
    pub fn aggs(&self) -> Vec<AggRow> {
        let path = self.aggregates_csv();
        CsvTable::read_or_empty(&path)
            .unwrap_or_else(|err| panic!("{}: read {}: {err}", self.case, path.display()))
            .agg_rows()
            .unwrap_or_else(|err| panic!("{}: parse {}: {err}", self.case, path.display()))
    }

    /// Sample rows as they are on disk right now.
    pub fn samples(&self) -> Vec<SampleRow> {
        let path = self.samples_csv();
        CsvTable::read_or_empty(&path)
            .unwrap_or_else(|err| panic!("{}: read {}: {err}", self.case, path.display()))
            .sample_rows()
            .unwrap_or_else(|err| panic!("{}: parse {}: {err}", self.case, path.display()))
    }
}

/// Path of the `probe` binary under test.
///
/// `PROBE_BIN` wins (that is how `cargo xtask e2e` points the cases at the binary
/// it built for the current feature set); otherwise cargo's own artifact path is
/// used, so a plain `cargo test --test e2e -- --ignored` still runs the right
/// build.
pub fn probe_bin() -> PathBuf {
    let path = match std::env::var_os("PROBE_BIN") {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(env!("CARGO_BIN_EXE_probe")),
    };
    assert!(
        path.is_file(),
        "probe binary {} is missing; build it with `cargo build -p probe`",
        path.display()
    );
    path
}

/// Re-checks `probe` until it returns `Some`, then returns that value.
///
/// The timeout is the assertion: a case never sleeps "long enough", it waits for
/// the event that proves the previous step finished.
pub fn wait_for<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + STEP_TIMEOUT;
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "timed out after {STEP_TIMEOUT:?} waiting for {what}"
        );
        thread::sleep(POLL_INTERVAL);
    }
}

/// Waits until the aggregate CSV holds rows that `ready` accepts.
pub fn wait_aggs(path: &Path, ready: impl Fn(&[AggRow]) -> bool) -> Vec<AggRow> {
    wait_for(&format!("aggregate rows in {}", path.display()), || {
        let rows = try_aggs(path)?;
        ready(&rows).then_some(rows)
    })
}

/// Waits until the sample CSV holds rows that `ready` accepts.
pub fn wait_samples(path: &Path, ready: impl Fn(&[SampleRow]) -> bool) -> Vec<SampleRow> {
    wait_for(&format!("sample rows in {}", path.display()), || {
        let rows = try_samples(path)?;
        ready(&rows).then_some(rows)
    })
}

/// Blocks until the kernel's aggregation clock is at the start of a window.
///
/// `bpf_ktime_get_ns()` shares `CLOCK_MONOTONIC`, so waiting for `now % window`
/// to be small starts a burst just after a boundary.  A burst that is much
/// shorter than a window then provably lands in a single window, which is what
/// makes "100 frames, one row" deterministic instead of a race with the
/// boundary.
pub fn wait_window_start(window_ns: u64) {
    assert!(window_ns > 0, "window must be non-zero");
    let slack = window_ns / 10;
    wait_for("the start of an aggregation window", || {
        (net::monotonic_ns() % window_ns < slack).then_some(())
    });
}

/// Σ count over the given rows.
pub fn total_count(rows: &[AggRow]) -> u64 {
    rows.iter().map(|row| row.count).sum()
}

/// Σ count over the rows written for one interface (E-07 shares a CSV).
pub fn count_for(rows: &[AggRow], iface: &str) -> u64 {
    rows.iter()
        .filter(|row| row.iface == iface)
        .map(|row| row.count)
        .sum()
}

/// Join a child's log lines for assertion messages.
pub fn log_text(logs: &[String]) -> String {
    logs.join("\n")
}

fn try_aggs(path: &Path) -> Option<Vec<AggRow>> {
    CsvTable::read_or_empty(path).ok()?.agg_rows().ok()
}

fn try_samples(path: &Path) -> Option<Vec<SampleRow>> {
    CsvTable::read_or_empty(path).ok()?.sample_rows().ok()
}
