//! Process wiring: CLI parsing, `CONFIG` handshake, XDP attach/detach, the tokio
//! event loop and the exit summary.
//!
//! Startup order is part of the contract:
//!
//! 1. load the embedded eBPF object,
//! 2. write `CONFIG` **before attach** and read it back; the read-back value is
//!    the only source of truth for `window_ns`/`grace_ns`/prefix lengths,
//! 3. attach the XDP program,
//! 4. scan `AGG` every `--scan-ms` and drain `SAMPLES` every `--flush-ms`.
//!
//! On SIGINT/SIGTERM the loop stops and [`crate::pipeline::Session::shutdown`]
//! drains the ring one last time, writes every pending sample, flushes all
//! remaining `AGG` keys (`window_complete=0` for windows that are still open),
//! flushes both CSVs and reads the kernel `STATS` counters.  [`main`] then
//! detaches and prints exactly one machine-readable `stats:` line on stdout.
//!
//! Everything window-related (close watermark, lateness, merging, sample
//! de-duplication, CSV rendering) lives in [`crate::aggregate`],
//! [`crate::pipeline`] and [`crate::sink`]; this module only decides *when* to
//! call them, which is why it holds no testable logic beyond [`validate`].

use std::{
    path::PathBuf,
    process::ExitCode,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, bail, ensure};
use aya::{
    Ebpf,
    maps::{Array, MapData, PerCpuArray, PerCpuHashMap, RingBuf},
    programs::{Xdp, XdpMode, xdp::XdpLinkId},
};
use clap::Parser;
#[rustfmt::skip]
use log::{debug, info, warn};
use probe_common::{
    ACTION_PASS, AggKey, AggValue, CONFIG_VERSION, ProbeConfig, STATS_AGG_OVERFLOW,
    STATS_CONFIG_MISSING, STATS_PACKETS_TOTAL, STATS_SAMPLE_FAILED,
};
use tokio::{
    signal,
    signal::unix::{SignalKind, signal as unix_signal},
    time,
};

use crate::{
    aggregate::{Clock, NANOS_PER_MS},
    pipeline::{KernelCounters, Session, Summary},
    ports::{MonoClock, RingSampleSource, mono_now_ns},
    sink::{SinkKind, create_agg_sink, create_sink},
};

/// Period of the `info!` metrics line.  The machine-readable `stats:` line is
/// still printed only once, on exit.
const METRICS_PERIOD: Duration = Duration::from_secs(5);

/// Per-CPU aggregation map (`AGG`): one `(window_id, key)` slot per CPU.
type AggMap = PerCpuHashMap<MapData, AggKey, AggValue>;
/// Sample ring buffer (`SAMPLES`).
type SampleRing = RingBuf<MapData>;
/// Per-CPU counters (`STATS`).
type StatsMap = PerCpuArray<MapData, u64>;

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
    raise_memlock();

    // The object is embedded at build time and always comes from the same build
    // as this binary, so no ABI version handshake is needed.
    let mut ebpf = Ebpf::load(aya::include_bytes_aligned!(concat!(
        env!("OUT_DIR"),
        "/probe"
    )))?;

    init_ebpf_logger(&mut ebpf)?;

    // CONFIG is written before the program is attached and then read back.  The
    // read-back value is the only source userspace uses below, so the kernel and
    // userspace can never disagree about window boundaries.
    let config = configure(&mut ebpf, &requested)?;

    let link_id = attach_xdp(&mut ebpf, &opt.iface)?;
    // The `AGG`/`STATS`/`SAMPLES` maps are consumed by the pipeline below, which
    // is why they are taken out of the `Ebpf` object after attaching.
    let (mut agg, samples, stats_map) = take_maps(&mut ebpf)?;

    let clock = wall_clock()?;
    let mut sink = create_sink(opt.sink, &opt.out, &opt.iface, opt.append)?;
    let mut agg_sink = create_agg_sink(&opt.agg_out, &opt.iface, opt.append, config, clock)?;
    info!(
        "writing samples to {} and aggregates to {} (append={})",
        opt.out.display(),
        opt.agg_out.display(),
        opt.append
    );

    let mut samples = RingSampleSource::new(samples);
    let mut session = Session::new(
        &mut agg,
        &mut samples,
        sink.as_mut(),
        agg_sink.as_mut(),
        config,
        clock,
    );

    let mut flush_tick = time::interval(Duration::from_millis(opt.flush_ms));
    flush_tick.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    let mut scan_tick = time::interval(Duration::from_millis(opt.scan_ms));
    scan_tick.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    let mut metrics_tick = time::interval(METRICS_PERIOD);
    metrics_tick.set_missed_tick_behavior(time::MissedTickBehavior::Delay);

    let ctrl_c = signal::ctrl_c();
    tokio::pin!(ctrl_c);
    // Handle SIGTERM as well, so `kill`/container stop still drains and flushes
    // instead of losing the userspace buffers.
    let mut sigterm = unix_signal(SignalKind::terminate()).context("install SIGTERM handler")?;

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
                session.drain_ring();
            }
            _ = scan_tick.tick() => {
                let started = Instant::now();
                session.tick(&MonoClock)?;
                last_scan_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
            }
            _ = metrics_tick.tick() => {
                session.log_metrics(read_kernel_stats(&stats_map), last_scan_ms, opt.scan_ms);
            }
        }
    }

    // The kernel counters are read while the maps are still open, after the last
    // drain, and the detach happens once the summary is complete.
    let summary = session.shutdown(|| read_kernel_stats(&stats_map))?;
    drop(session);
    detach_xdp(&mut ebpf, link_id);
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

/// Writes `CONFIG` and reads it back before attaching.
///
/// The write is fatal when it fails: without a config the data path falls back
/// to `DEFAULT_WINDOW_NS`, and userspace would then disagree with the kernel
/// about every `window_id`.  The read-back values are returned and used
/// everywhere afterwards, so userspace never keeps a second copy of `window_ns`.
fn configure(ebpf: &mut Ebpf, requested: &ProbeConfig) -> Result<ProbeConfig> {
    // Borrow the map instead of taking it: the program is loaded after this
    // point and needs every map to still be owned by `ebpf`.
    let map = ebpf.map_mut("CONFIG").context("map 'CONFIG' not found")?;
    let mut config: Array<&mut MapData, ProbeConfig> =
        Array::try_from(map).context("open array CONFIG")?;
    config.set(0, requested, 0).context("write CONFIG map")?;
    let readback = config.get(&0, 0).context("read back CONFIG map")?;

    ensure!(
        readback.window_ns > 0,
        "CONFIG read-back has window_ns=0: the data path would fall back to the kernel default"
    );
    ensure!(
        readback.window_ns == requested.window_ns
            && readback.grace_ns == requested.grace_ns
            && readback.ipv4_prefix_len == requested.ipv4_prefix_len
            && readback.ipv6_prefix_len == requested.ipv6_prefix_len,
        "CONFIG read-back ({readback:?}) differs from the requested config ({requested:?})"
    );

    info!(
        "CONFIG applied: window_ns={} grace_ns={} ipv4_prefix_len={} ipv6_prefix_len={} default_action={}",
        readback.window_ns,
        readback.grace_ns,
        readback.ipv4_prefix_len,
        readback.ipv6_prefix_len,
        readback.default_action
    );
    Ok(readback)
}

/// Bump the memlock rlimit. This is needed for older kernels that don't use the
/// new memcg based accounting, see https://lwn.net/Articles/837122/
///
/// Failure is not fatal: newer kernels work without the bump, so it only goes
/// to the debug log instead of aborting startup.
fn raise_memlock() {
    let rlim = libc::rlimit {
        rlim_cur: libc::RLIM_INFINITY,
        rlim_max: libc::RLIM_INFINITY,
    };
    // SAFETY: `setrlimit` only reads the `rlimit` struct passed by pointer; the
    // pointer is valid for the duration of the call.
    let ret = unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &rlim) };
    if ret != 0 {
        debug!("remove limit on locked memory failed, ret is: {ret}");
    }
}

/// Sets up the optional aya-log pipeline and pumps it from the tokio runtime.
///
/// The data path uses the `SAMPLES` ring buffer, not aya-log.  Keep the logger
/// optional so kernel-side debug output still reaches userspace if some is ever
/// added; a kernel/program without aya-log support is only a debug event.
fn init_ebpf_logger(ebpf: &mut Ebpf) -> Result<()> {
    match aya_log::EbpfLogger::init(ebpf) {
        Err(e) => debug!("eBPF logger not initialised: {e}"),
        Ok(logger) => {
            // SAFETY: The logger owns its file descriptor, which stays open and unchanged
            // while the AsyncFd owns it. We only flush the logger, never replace it.
            let mut logger = unsafe {
                tokio::io::unix::AsyncFd::register_with_interest(
                    logger,
                    tokio::io::Interest::READABLE,
                )
            }?;
            tokio::task::spawn(async move {
                loop {
                    let mut guard = logger.readable_mut().await.unwrap();
                    guard.get_inner_mut().flush();
                    guard.clear_ready();
                }
            });
        }
    }
    Ok(())
}

/// Loads and attaches the XDP program, returning the link id used on detach.
fn attach_xdp(ebpf: &mut Ebpf, iface: &str) -> Result<XdpLinkId> {
    let program: &mut Xdp = ebpf
        .program_mut("probe")
        .context("eBPF program 'probe' not found")?
        .try_into()?;
    program.load().context("load XDP program")?;
    let link_id = program
        .attach(iface, XdpMode::default())
        .with_context(|| format!("failed to attach XDP program to '{iface}'"))?;
    info!("attached XDP program to '{iface}' (link id {link_id:?})");
    Ok(link_id)
}

/// Takes the three maps the userspace side owns after attaching.
fn take_maps(ebpf: &mut Ebpf) -> Result<(AggMap, SampleRing, StatsMap)> {
    let agg = AggMap::try_from(ebpf.take_map("AGG").context("map 'AGG' not found")?)
        .context("open per-CPU hash AGG")?;
    let samples = SampleRing::try_from(
        ebpf.take_map("SAMPLES")
            .context("map 'SAMPLES' not found")?,
    )
    .context("open ring buffer SAMPLES")?;
    let stats = StatsMap::try_from(ebpf.take_map("STATS").context("map 'STATS' not found")?)
        .context("open per-CPU array STATS")?;
    Ok((agg, samples, stats))
}

/// Reads the kernel `STATS` counters (one slot per index, one value per CPU).
fn read_kernel_stats(stats_map: &StatsMap) -> KernelCounters {
    KernelCounters {
        packets_total: read_counter(stats_map, STATS_PACKETS_TOTAL),
        sample_failed: read_counter(stats_map, STATS_SAMPLE_FAILED),
        agg_overflow: read_counter(stats_map, STATS_AGG_OVERFLOW),
        config_missing: read_counter(stats_map, STATS_CONFIG_MISSING),
    }
}

/// Reads one `STATS` index and sums it over all CPUs.
fn read_counter(stats_map: &StatsMap, index: u32) -> u64 {
    match stats_map.get(&index, 0) {
        Ok(values) => values.iter().copied().sum(),
        Err(e) => {
            warn!("failed to read STATS[{index}]: {e}");
            0
        }
    }
}

/// Pairs the wall clock with the monotonic clock so the kernel's
/// `bpf_ktime_get_ns()` timestamps can be reported as Unix milliseconds.
fn wall_clock() -> Result<Clock> {
    let unix_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_nanos();
    Ok(Clock::new(unix_ns, mono_now_ns()?))
}

/// Detaches the XDP program.  Failures are only warnings: everything has been
/// written and flushed already, and the process is about to exit.
fn detach_xdp(ebpf: &mut Ebpf, link_id: XdpLinkId) {
    let Some(program) = ebpf.program_mut("probe") else {
        warn!("eBPF program 'probe' not found before detach");
        return;
    };
    let program: &mut Xdp = match program.try_into() {
        Ok(program) => program,
        Err(e) => {
            warn!("failed to downcast XDP program for detach: {e}");
            return;
        }
    };
    if let Err(e) = program.detach(link_id) {
        warn!("failed to detach XDP program: {e}");
    }
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
