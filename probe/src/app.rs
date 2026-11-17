//! Process wiring: CLI parsing, the tokio event loop and the exit summary.
//!
//! The eBPF side (load, `CONFIG` handshake, attach, map ownership, detach) lives
//! in [`crate::driver`], so this module and `probe-agent` share exactly the same
//! startup and shutdown sequence.  The order is part of the contract:
//!
//! 1. load the embedded eBPF object,
//! 2. write `CONFIG` **before attach** and read it back; the read-back value is
//!    the only source of truth for `window_ns`/`grace_ns`/prefix lengths,
//! 3. attach the XDP program,
//! 4. scan `AGG` every `--scan-ms` and drain `SAMPLES` every `--flush-ms`.
//!
//! On SIGINT/SIGTERM the loop stops and [`crate::driver::Probe::shutdown`] drains
//! the ring one last time, writes every pending sample, flushes all remaining
//! `AGG` keys (`window_complete=0` for windows that are still open), flushes both
//! CSVs and reads the kernel `STATS` counters.  [`main`] then detaches and prints
//! exactly one machine-readable `stats:` line on stdout.
//!
//! Everything window-related (close watermark, lateness, merging, sample
//! de-duplication, CSV rendering) lives in [`crate::aggregate`],
//! [`crate::pipeline`] and [`crate::sink`]; this module only decides *when* to
//! call them, which is why it holds no testable logic beyond [`validate`].

use std::{
    path::PathBuf,
    process::ExitCode,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail};
use clap::Parser;
use log::info;
use probe_common::{ACTION_PASS, CONFIG_VERSION, ProbeConfig};
use tokio::{
    signal,
    signal::unix::{SignalKind, signal as unix_signal},
    time,
};

use crate::{
    aggregate::NANOS_PER_MS,
    driver::{Probe, ProbeOptions, wall_clock},
    pipeline::Summary,
    ports::mono_now_ns,
    sink::SinkKind,
};

/// Period of the `info!` metrics line.  The machine-readable `stats:` line is
/// still printed only once, on exit.
const METRICS_PERIOD: Duration = Duration::from_secs(5);

#[derive(Debug, Parser)]
#[command(
    about = "Capture IPv4/IPv6 traffic at XDP ingress, aggregate per time window and persist samples plus aggregates"
)]
struct Opt {
    /// Interface to attach the XDP program to.
    #[clap(short, long, default_value = "lo")]
    iface: String,

    /// Sample output file. The parent directory is created if needed.
    #[clap(long, default_value = "/workspace/packets.csv")]
    out: PathBuf,

    /// Storage backend for samples. `csv` writes one row per sample; `db` is reserved.
    #[clap(long, value_enum, default_value = "csv")]
    sink: SinkKind,

    /// Ring drain period in milliseconds; must be greater than 0.
    #[clap(long, default_value_t = 500)]
    flush_ms: u64,

    /// Append to the output files instead of truncating them on startup.
    #[clap(long)]
    append: bool,

    /// Aggregation window width in milliseconds; must be greater than 0.
    #[clap(long, default_value_t = 10_000)]
    window_ms: u64,

    /// Grace period after a window ends before it is reported, in milliseconds.
    #[clap(long, default_value_t = 2_000)]
    grace_ms: u64,

    /// `AGG` scan period in milliseconds; must be greater than 0.
    #[clap(long, default_value_t = 1_000)]
    scan_ms: u64,

    /// Aggregate output file, 16 fixed columns; the parent directory is created.
    #[clap(long, default_value = "/workspace/aggregates.csv")]
    agg_out: PathBuf,

    /// Source IPv4 aggregation prefix length, 0..=32.
    #[clap(long, default_value_t = 24)]
    src_prefix_v4: u8,

    /// Source IPv6 aggregation prefix length, 0..=128.
    #[clap(long, default_value_t = 64)]
    src_prefix_v6: u8,
}

/// Entry point of the `probe` binary: parse, validate, run, report.
///
/// The exit code distinguishes a clean shutdown (0) from a failed startup or a
/// failed write (1).  Argument errors go to stderr on their own, before the
/// logger or any eBPF resource is set up.
pub fn main() -> ExitCode {
    let opt = Opt::parse();

    // Fail fast, before the logger or any eBPF resource is set up: an invalid
    // flag should be reported on its own, not buried in startup logs.
    let requested = match validate(&opt) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("probe: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    env_logger::init();

    match run(&opt, requested) {
        Ok(summary) => {
            // Machine-readable contract: the e2e scripts grep these exact tokens,
            // so the line is printed unconditionally on stdout and is not routed
            // through `log` (its presence must not depend on RUST_LOG).  This is
            // the only stdout write of the process.
            println!("{}", summary.stats_line());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("probe: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Runs the probe until SIGINT/SIGTERM and returns the exit summary.
///
/// `requested` is [`validate`]'s output; the value actually used is the one read
/// back from the kernel inside [`configure`].
#[tokio::main]
async fn run(opt: &Opt, requested: ProbeConfig) -> Result<Summary> {
    // Shutdown handlers are installed before anything is loaded or attached: a
    // `kill`/`systemctl stop` that lands during startup must still take the
    // draining path instead of killing the process with the default disposition
    // and losing the userspace buffers.
    let ctrl_c = signal::ctrl_c();
    tokio::pin!(ctrl_c);
    // Handle SIGTERM as well, so `kill`/container stop still drains and flushes
    // instead of losing the userspace buffers.
    let mut sigterm = unix_signal(SignalKind::terminate()).context("install SIGTERM handler")?;

    // Both CSV writers and the pipeline share one wall/monotonic clock pair, so
    // the two files can never disagree by the difference between two samples.
    let clock = wall_clock()?;
    let options = ProbeOptions {
        iface: opt.iface.clone(),
        sink: opt.sink,
        out: opt.out.clone(),
        agg_out: opt.agg_out.clone(),
        append: opt.append,
        flush_ms: opt.flush_ms,
        scan_ms: opt.scan_ms,
        clock: Some(clock),
    };
    // The aggregate writer renders window bounds and `first_seen`/`last_seen`
    // with the requested prefixes; `Probe::attach` refuses to run unless the
    // kernel reads back exactly these values.
    let sink = options.create_sink()?;
    let agg_sink = options.create_agg_sink(requested, clock)?;

    let mut probe = Probe::attach(&options, requested, sink, agg_sink)?;
    info!(
        "writing samples to {} and aggregates to {} (append={})",
        opt.out.display(),
        opt.agg_out.display(),
        opt.append
    );

    let mut flush_tick = time::interval(probe.flush_interval());
    flush_tick.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    let mut scan_tick = time::interval(probe.scan_interval());
    scan_tick.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    let mut metrics_tick = time::interval(METRICS_PERIOD);
    metrics_tick.set_missed_tick_behavior(time::MissedTickBehavior::Delay);

    // Duration of the most recent periodic scan, in milliseconds.
    let mut last_scan_ms = 0u64;

    info!("Waiting for Ctrl-C...");

    loop {
        tokio::select! {
            _ = &mut ctrl_c => {
                info!("Ctrl-C received, draining and flushing");
                break;
            }
            _ = sigterm.recv() => {
                info!("SIGTERM received, draining and flushing");
                break;
            }
            _ = flush_tick.tick() => {
                probe.drain();
            }
            _ = scan_tick.tick() => {
                let started = Instant::now();
                probe.tick(mono_now_ns()?)?;
                last_scan_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
            }
            _ = metrics_tick.tick() => {
                probe.log_metrics(last_scan_ms);
            }
        }
    }

    // The kernel counters are read while the maps are still open, after the last
    // drain, and the detach happens once the summary is complete.
    let summary = probe.shutdown()?;
    probe.detach();
    Ok(summary)
}

/// Validates the CLI values and turns them into the `ProbeConfig` sent to the
/// kernel.
///
/// Millisecond→nanosecond conversions use `checked_mul`, so a huge
/// `--window-ms`/`--grace-ms` is a clean argument error instead of a silently
/// wrapped window width.
fn validate(opt: &Opt) -> Result<ProbeConfig> {
    if opt.flush_ms == 0 {
        bail!("--flush-ms must be greater than 0");
    }
    if opt.window_ms == 0 {
        bail!("--window-ms must be greater than 0");
    }
    if opt.scan_ms == 0 {
        bail!("--scan-ms must be greater than 0");
    }
    if opt.src_prefix_v4 > 32 {
        bail!("--src-prefix-v4 must be at most 32");
    }
    if opt.src_prefix_v6 > 128 {
        bail!("--src-prefix-v6 must be at most 128");
    }

    let window_ns = opt
        .window_ms
        .checked_mul(NANOS_PER_MS)
        .context("--window-ms is too large to convert to nanoseconds")?;
    let grace_ns = opt
        .grace_ms
        .checked_mul(NANOS_PER_MS)
        .context("--grace-ms is too large to convert to nanoseconds")?;

    Ok(ProbeConfig {
        version: CONFIG_VERSION,
        window_ns,
        grace_ns,
        ipv4_prefix_len: opt.src_prefix_v4,
        ipv6_prefix_len: opt.src_prefix_v6,
        default_action: ACTION_PASS,
        pad: [0; 5],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The CLI defaults, parsed by clap so the test does not restate them.
    fn defaults() -> Opt {
        Opt::parse_from(["probe"])
    }

    #[test]
    fn defaults_are_a_valid_config() {
        let config = validate(&defaults()).expect("the documented defaults are valid");
        assert_eq!(config.version, CONFIG_VERSION);
        assert_eq!(config.window_ns, 10_000 * NANOS_PER_MS);
        assert_eq!(config.grace_ns, 2_000 * NANOS_PER_MS);
        assert_eq!(config.ipv4_prefix_len, 24);
        assert_eq!(config.ipv6_prefix_len, 64);
        assert_eq!(config.default_action, ACTION_PASS);
    }

    #[test]
    fn rejects_zero_periods() {
        for set in [
            (|opt: &mut Opt| opt.flush_ms = 0) as fn(&mut Opt),
            |opt: &mut Opt| opt.window_ms = 0,
            |opt: &mut Opt| opt.scan_ms = 0,
        ] {
            let mut opt = defaults();
            set(&mut opt);
            let error = validate(&opt).unwrap_err().to_string();
            assert!(
                error.ends_with("must be greater than 0"),
                "unexpected error: {error}"
            );
        }
    }

    #[test]
    fn rejects_out_of_range_prefixes() {
        let mut opt = defaults();
        opt.src_prefix_v4 = 33;
        assert_eq!(
            validate(&opt).unwrap_err().to_string(),
            "--src-prefix-v4 must be at most 32"
        );

        let mut opt = defaults();
        opt.src_prefix_v6 = 129;
        assert_eq!(
            validate(&opt).unwrap_err().to_string(),
            "--src-prefix-v6 must be at most 128"
        );
    }

    /// `u64::MAX` milliseconds wrap to a tiny window when multiplied naively; the
    /// checked conversion must refuse them instead.
    #[test]
    fn rejects_a_window_that_overflows_nanoseconds() {
        let mut opt = defaults();
        opt.window_ms = u64::MAX;
        let error = validate(&opt).unwrap_err().to_string();
        assert!(
            error.contains("too large to convert to nanoseconds"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn zero_grace_is_allowed() {
        let mut opt = defaults();
        opt.grace_ms = 0;
        let config = validate(&opt).expect("a zero grace period is a legal configuration");
        assert_eq!(config.grace_ns, 0);
    }
}
