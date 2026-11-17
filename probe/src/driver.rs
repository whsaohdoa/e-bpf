//! The attached probe: eBPF object, XDP link, kernel maps and the userspace
//! [`Session`] behind one owned handle.
//!
//! [`crate::app`] and `probe-agent` drive the same object through this module, so
//! neither of them has to know how the object is loaded, how `CONFIG` is written
//! and read back, or which maps the pipeline owns.  [`Probe::attach`] keeps the
//! documented startup order ([`crate::app`] module docs): load the embedded
//! object, write `CONFIG`, read it back, attach, then take the maps.
//!
//! The configuration can change while the probe runs: [`Probe::apply_config`]
//! writes `CONFIG`, reads it back, flushes the rows of the old configuration and
//! only then swaps the userspace view.  That ordering is what makes an old window
//! settle at its own boundary instead of being rendered with the new width.

use std::{
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, ensure};
use aya::{
    Ebpf,
    maps::{Array, MapData, PerCpuArray, PerCpuHashMap, RingBuf},
    programs::{Xdp, XdpMode, xdp::XdpLinkId},
};
#[rustfmt::skip]
use log::{debug, info, warn};
use probe_common::{
    AggKey, AggValue, ProbeConfig, STATS_AGG_OVERFLOW, STATS_CONFIG_MISSING, STATS_PACKETS_TOTAL,
    STATS_SAMPLE_FAILED,
};

use crate::{
    aggregate::Clock,
    pipeline::{KernelCounters, Session, Summary},
    ports::{RingSampleSource, TimeSource, mono_now_ns},
    sink::{AggSink, Sink, SinkKind, create_agg_sink, create_sink},
};

/// Per-CPU aggregation map (`AGG`): one `(window_id, key)` slot per CPU.
type AggMap = PerCpuHashMap<MapData, AggKey, AggValue>;
/// Sample ring buffer (`SAMPLES`).
type SampleRing = RingBuf<MapData>;
/// Per-CPU counters (`STATS`).
type StatsMap = PerCpuArray<MapData, u64>;

/// Everything [`Probe::attach`] needs besides the requested configuration and
/// the two sinks.
///
/// The options mirror the `probe` CLI flags, so a caller that already has them
/// (the `probe` binary, `probe-agent`) can describe both CSV files with one
/// structure and let [`ProbeOptions::create_sink`] /
/// [`ProbeOptions::create_agg_sink`] build them.
#[derive(Clone, Debug)]
pub struct ProbeOptions {
    /// Interface the XDP program is attached to.
    pub iface: String,
    /// Sample sink backend.
    pub sink: SinkKind,
    /// Sample CSV path.
    pub out: PathBuf,
    /// Aggregate CSV path.
    pub agg_out: PathBuf,
    /// Append to the CSV files instead of truncating them on startup.
    pub append: bool,
    /// Ring drain period in milliseconds.
    pub flush_ms: u64,
    /// `AGG` scan period in milliseconds.
    pub scan_ms: u64,
    /// Wall/monotonic clock pair shared by both CSV sinks and the session.
    ///
    /// The CSV writers render `ts_unix_ms`, `first_seen` and the window bounds
    /// with this clock, so it has to be the same object the session converts
    /// kernel timestamps with; otherwise the two files could disagree by the
    /// difference between two clock samples.  `None` lets [`Probe::attach`]
    /// sample it itself, which is only useful for a caller that writes no CSV.
    pub clock: Option<Clock>,
}

impl ProbeOptions {
    /// Builds the sample sink described by these options.
    pub fn create_sink(&self) -> Result<Box<dyn Sink + Send>> {
        create_sink(self.sink, &self.out, &self.iface, self.append)
            .with_context(|| format!("open sample sink {}", self.out.display()))
    }

    /// Builds the aggregate CSV sink described by these options.
    ///
    /// `config` must be the configuration the kernel will run with; the `probe`
    /// binary passes the requested value, which [`Probe::attach`] verifies
    /// against the read-back value before anything is written.
    pub fn create_agg_sink(
        &self,
        config: ProbeConfig,
        clock: Clock,
    ) -> Result<Box<dyn AggSink + Send>> {
        create_agg_sink(&self.agg_out, &self.iface, self.append, config, clock)
            .with_context(|| format!("open aggregate sink {}", self.agg_out.display()))
    }

    /// Ring drain period.
    pub fn flush_interval(&self) -> Duration {
        Duration::from_millis(self.flush_ms)
    }

    /// `AGG` scan period.
    pub fn scan_interval(&self) -> Duration {
        Duration::from_millis(self.scan_ms)
    }
}

/// An attached XDP probe: the object, the link, the kernel counters and the
/// userspace pipeline that owns `AGG` and `SAMPLES`.
pub struct Probe {
    ebpf: Ebpf,
    link_id: XdpLinkId,
    stats_map: StatsMap,
    session: Session,
    flush_ms: u64,
    scan_ms: u64,
}

impl Probe {
    /// Loads the embedded object, applies `requested`, attaches and wires the
    /// pipeline.
    ///
    /// Must be called from a tokio runtime: the optional aya-log pipeline is
    /// pumped by a spawned task.
    pub fn attach(
        opt: &ProbeOptions,
        requested: ProbeConfig,
        sink: Box<dyn Sink + Send>,
        agg_sink: Box<dyn AggSink + Send>,
    ) -> Result<Self> {
        // Older kernels need the memlock bump before the maps are created; newer
        // ones ignore it, so a failure is only a debug event.
        raise_memlock();

        // The object is embedded at build time and always comes from the same
        // build as the binary, so no ABI version handshake is needed.
        let mut ebpf = Ebpf::load(aya::include_bytes_aligned!(concat!(
            env!("OUT_DIR"),
            "/probe"
        )))?;

        init_ebpf_logger(&mut ebpf)?;

        // CONFIG is written before the program is attached and then read back.
        // The read-back value is the only source userspace uses below, so the
        // kernel and userspace can never disagree about window boundaries.
        let config = configure(&mut ebpf, &requested)?;

        let link_id = attach_xdp(&mut ebpf, &opt.iface)?;
        // The `AGG`/`STATS`/`SAMPLES` maps are consumed by the pipeline, which is
        // why they are taken out of the `Ebpf` object after attaching.
        let (agg, samples, stats_map) = take_maps(&mut ebpf)?;

        let clock = match opt.clock {
            Some(clock) => clock,
            None => wall_clock()?,
        };
        let session = Session::new(
            Box::new(agg),
            Box::new(RingSampleSource::new(samples)),
            sink,
            agg_sink,
            config,
            clock,
        );

        Ok(Self {
            ebpf,
            link_id,
            stats_map,
            session,
            flush_ms: opt.flush_ms,
            scan_ms: opt.scan_ms,
        })
    }

    /// The effective configuration (read back from the kernel).
    pub fn config(&self) -> &ProbeConfig {
        self.session.config()
    }

    /// Writes `CONFIG`, verifies the read-back value, flushes the rows of the old
    /// configuration and installs the new one.
    ///
    /// Returns the read-back value: it, not `requested`, is the configuration the
    /// data path actually uses.  Rows already merged into the table are written
    /// *before* the new width is installed, so an old window settles at its old
    /// boundary with `window_complete = 0`.
    pub fn apply_config(&mut self, requested: ProbeConfig) -> Result<ProbeConfig> {
        let readback = configure(&mut self.ebpf, &requested)?;
        self.session.flush_pending()?;
        self.session.set_config(readback);
        Ok(readback)
    }

    /// Drains at most [`crate::pipeline::MAX_EVENTS_PER_TICK`] ring records.
    pub fn drain(&mut self) {
        self.session.drain_ring();
    }

    /// One pipeline iteration at `now_ns` (monotonic): drain, then report every
    /// window that closed at that instant.
    pub fn tick(&mut self, now_ns: u64) -> Result<()> {
        self.session.tick(&FrozenNow(now_ns))
    }

    /// Writes every pending row with `window_complete = false`.
    pub fn flush_pending(&mut self) -> Result<()> {
        self.session.flush_pending()
    }

    /// Reads the kernel `STATS` counters.
    pub fn kernel_counters(&self) -> KernelCounters {
        read_kernel_stats(&self.stats_map)
    }

    /// Logs the periodic metrics line, including the measured scan duration.
    pub fn log_metrics(&self, scan_ms: u64) {
        self.session
            .log_metrics(self.kernel_counters(), scan_ms, self.scan_ms);
    }

    /// Ring drain period from the options.
    pub fn flush_interval(&self) -> Duration {
        Duration::from_millis(self.flush_ms)
    }

    /// `AGG` scan period from the options.
    pub fn scan_interval(&self) -> Duration {
        Duration::from_millis(self.scan_ms)
    }

    /// Final drain, flush of every pending row and the exit summary.
    ///
    /// The kernel counters are read while the maps are still open, after the last
    /// drain; the caller detaches afterwards with [`Probe::detach`].
    pub fn shutdown(&mut self) -> Result<Summary> {
        self.session.shutdown(|| read_kernel_stats(&self.stats_map))
    }

    /// Detaches the XDP program.  Failures are only warnings: everything has been
    /// written and flushed already and the process is about to exit.
    pub fn detach(mut self) {
        detach_xdp(&mut self.ebpf, self.link_id);
    }
}

/// A [`TimeSource`] that always reports the timestamp the caller measured.
///
/// [`Probe::tick`] receives `now_ns` from its caller (the event loop), so the
/// pipeline must not sample the clock a second time: the report boundary has to
/// be the one the loop used.
struct FrozenNow(u64);

impl TimeSource for FrozenNow {
    fn mono_ns(&self) -> Result<u64> {
        Ok(self.0)
    }
}

/// Writes `CONFIG` and reads it back.
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
///
/// Callers that write CSV files sample this once and pass it through
/// [`ProbeOptions::clock`], so the sample file, the aggregate file and the
/// pipeline all convert timestamps with the same offset.
pub fn wall_clock() -> Result<Clock> {
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

    fn options() -> ProbeOptions {
        ProbeOptions {
            iface: "lo".into(),
            sink: SinkKind::Csv,
            out: "/tmp/samples.csv".into(),
            agg_out: "/tmp/aggregates.csv".into(),
            append: false,
            flush_ms: 500,
            scan_ms: 1_000,
            clock: None,
        }
    }

    #[test]
    fn intervals_come_from_the_options() {
        let opt = options();
        assert_eq!(opt.flush_interval(), Duration::from_millis(500));
        assert_eq!(opt.scan_interval(), Duration::from_millis(1_000));
    }

    /// The clock is optional precisely so that a caller without CSV files does
    /// not have to sample it; the probe samples it during attach instead.
    #[test]
    fn clock_is_optional() {
        assert!(options().clock.is_none());
    }

    #[test]
    fn frozen_now_reports_the_given_timestamp() {
        assert_eq!(FrozenNow(42).mono_ns().unwrap(), 42);
    }
}
