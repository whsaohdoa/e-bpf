mod sink;

use std::{
    mem,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, bail};
use aya::{
    Ebpf,
    maps::{MapData, PerCpuArray, RingBuf},
    programs::{Xdp, XdpMode, xdp::XdpLinkId},
};
use clap::Parser;
#[rustfmt::skip]
use log::{debug, info, warn};
use probe_common::PacketEvent;
use sink::{Event, Sink, SinkKind, create_sink};
use tokio::{
    signal,
    signal::unix::{SignalKind, signal as unix_signal},
    time,
};

/// Upper bound per periodic drain; keeps the interval responsive when a large
/// backlog is waiting in the ring buffer.
const MAX_EVENTS_PER_TICK: usize = 8192;

/// Userspace event counters, reported in one line on exit.
///
/// `received` and `written` are currently always equal because the sink accepts
/// every decoded event.  They stay separate on purpose: a future sink may
/// reject or coalesce events, and the `stats:` field names are a
/// machine-readable contract used by `verification/run_e2e*.sh`.
#[derive(Default)]
struct Stats {
    received: u64,
    written: u64,
    malformed: u64,
}

#[derive(Debug, Parser)]
#[command(about = "Capture IPv4/IPv6 src/dst at XDP ingress and persist them through a pluggable sink")]
struct Opt {
    /// Interface to attach the XDP program to.
    #[clap(short, long, default_value = "lo")]
    iface: String,

    /// Output file. The parent directory is created if needed.
    #[clap(long, default_value = "/workspace/packets.csv")]
    out: PathBuf,

    /// Storage backend. `csv` writes one row per event; `db` is reserved.
    #[clap(long, value_enum, default_value = "csv")]
    sink: SinkKind,

    /// Batch/flush period in milliseconds.
    #[clap(long, default_value_t = 500)]
    flush_ms: u64,

    /// Append to `--out` instead of truncating it on startup.
    #[clap(long)]
    append: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let opt = Opt::parse();

    // Fail fast, before the logger or any eBPF resource is set up: an invalid
    // flag should be reported on its own, not buried in startup logs.
    if opt.flush_ms == 0 {
        bail!("--flush-ms must be greater than 0");
    }
    env_logger::init();

    raise_memlock();

    // This will include your eBPF object file as raw bytes at compile-time and load it at
    // runtime. This approach is recommended for most real-world use cases. If you would
    // like to specify the eBPF program at runtime rather than at compile-time, you can
    // reach for `Bpf::load_file` instead.
    let mut ebpf = Ebpf::load(aya::include_bytes_aligned!(concat!(
        env!("OUT_DIR"),
        "/probe"
    )))?;

    init_ebpf_logger(&mut ebpf)?;
    let link_id = attach_xdp(&mut ebpf, &opt.iface)?;
    // The EVENTS map is consumed by the reader; DROPS is kept to report the
    // kernel-side loss counter on shutdown.
    let (mut ring, drops) = take_maps(&mut ebpf)?;

    let clock = WallClock::new()?;
    let mut sink = create_sink(opt.sink, &opt.out, &opt.iface, opt.append)?;
    info!(
        "persisting to {} (append={})",
        opt.out.display(),
        opt.append
    );

    let mut ticker = time::interval(Duration::from_millis(opt.flush_ms));
    ticker.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    let ctrl_c = signal::ctrl_c();
    tokio::pin!(ctrl_c);
    // Handle SIGTERM as well, so `kill`/container stop still drains and flushes
    // instead of losing the userspace buffers.
    let mut sigterm =
        unix_signal(SignalKind::terminate()).context("install SIGTERM handler")?;

    let mut stats = Stats::default();
    let mut batch: Vec<Event> = Vec::with_capacity(MAX_EVENTS_PER_TICK);

    info!("Waiting for Ctrl-C...");

    loop {
        tokio::select! {
            _ = &mut ctrl_c => {
                info!("Ctrl-C received, draining ring buffer");
                break;
            }
            _ = sigterm.recv() => {
                info!("SIGTERM received, draining ring buffer");
                break;
            }
            _ = ticker.tick() => {
                let n = drain_ring(&mut ring, &clock, &mut batch, &mut stats);
                if n > 0 {
                    commit_batch(sink.as_mut(), &mut batch, &mut stats, true)?;
                }
            }
        }
    }

    // Final drain: whatever the kernel already queued must still reach disk.
    final_drain(&mut ring, &clock, sink.as_mut(), &mut batch, &mut stats)?;
    drop(sink);

    let kernel_dropped = shutdown(&mut ebpf, link_id, &drops)?;

    // Machine-readable contract: the e2e scripts grep these exact tokens, so
    // the line is printed unconditionally on stdout and is not routed through
    // `log` (its presence must not depend on `RUST_LOG`).
    let Stats { received, written, malformed } = stats;
    println!(
        "stats: events_received={received} rows_written={written} kernel_dropped={kernel_dropped} malformed={malformed}"
    );

    Ok(())
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
    let ret = unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &rlim) };
    if ret != 0 {
        debug!("remove limit on locked memory failed, ret is: {ret}");
    }
}

/// Sets up the optional aya-log pipeline and pumps it from the tokio runtime.
///
/// The data path uses the `EVENTS` ring buffer, not aya-log. Keep the logger
/// optional so kernel-side debug output still reaches userspace if some is
/// ever added; a kernel/program without aya-log support is only a debug event.
/// Registering the logger with the reactor is the only step whose I/O error
/// still propagates; a missing kernel logger is just a `debug!` event above.
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
///
/// Both the load error and the attach error carry context naming the program
/// and the interface involved.
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

/// Takes the two maps the userspace side owns.
///
/// `EVENTS` is consumed by the reader below; `DROPS` is kept to report the
/// kernel-side loss counter on shutdown.
fn take_maps(ebpf: &mut Ebpf) -> Result<(RingBuf<MapData>, PerCpuArray<MapData, u64>)> {
    let ring = RingBuf::try_from(ebpf.take_map("EVENTS").context("map 'EVENTS' not found")?)
        .context("open ring buffer EVENTS")?;
    let drops: PerCpuArray<MapData, u64> = PerCpuArray::try_from(
        ebpf.take_map("DROPS").context("map 'DROPS' not found")?,
    )
    .context("open per-CPU array DROPS")?;
    Ok((ring, drops))
}

/// Writes the accumulated batch, accounts for it and optionally flushes.
///
/// `flush` is `true` on the periodic tick (so rows become visible every
/// `--flush-ms`) and `false` during the final drain, which only flushes once
/// after the ring buffer is empty.  Only call this with a non-empty batch.
fn commit_batch(
    sink: &mut dyn Sink,
    batch: &mut Vec<Event>,
    stats: &mut Stats,
    flush: bool,
) -> Result<()> {
    let n = batch.len() as u64;
    sink.write_batch(batch)?;
    // `received` is kept for a future sink that may drop events; today the sink
    // accepts everything, so it stays equal to `written`.
    stats.received += n;
    stats.written += n;
    batch.clear();
    if flush {
        sink.flush()?;
    }
    Ok(())
}

/// Drains whatever the kernel already queued on shutdown, then flushes once.
fn final_drain(
    ring: &mut RingBuf<MapData>,
    clock: &WallClock,
    sink: &mut dyn Sink,
    batch: &mut Vec<Event>,
    stats: &mut Stats,
) -> Result<()> {
    loop {
        let n = drain_ring(ring, clock, batch, stats);
        if n == 0 {
            break;
        }
        commit_batch(sink, batch, stats, false)?;
    }
    sink.flush()
}

/// Reads the kernel drop counter, then detaches the XDP program.
///
/// DROPS is read first so the counter is reported even if detaching fails, and
/// so the teardown order stays "drain -> write -> flush -> read DROPS ->
/// detach".  Map-read and detach failures are only warnings: there is nothing
/// left to save, and the process is about to exit anyway.
fn shutdown(ebpf: &mut Ebpf, link_id: XdpLinkId, drops: &PerCpuArray<MapData, u64>) -> Result<u64> {
    let kernel_dropped: u64 = match drops.get(&0, 0) {
        Ok(values) => values.iter().copied().sum(),
        Err(e) => {
            warn!("failed to read DROPS map: {e}");
            0
        }
    };

    if let Some(program) = ebpf.program_mut("probe") {
        let program: &mut Xdp = program.try_into()?;
        if let Err(e) = program.detach(link_id) {
            warn!("failed to detach XDP program: {e}");
        }
    }

    Ok(kernel_dropped)
}

/// Drains events currently available in the ring buffer into `batch`.
///
/// Returns the number of decoded events.  Each event is copied out of the ring
/// buffer with `read_unaligned`; `RingBufItem` is dropped immediately after,
/// which releases the record back to the kernel.  Records that cannot be
/// decoded at all — too short for the ABI struct, unknown ABI version or
/// unknown address family — are skipped and counted in `stats.malformed`; they
/// are never interpreted with the wrong layout.
fn drain_ring(
    ring: &mut RingBuf<MapData>,
    clock: &WallClock,
    batch: &mut Vec<Event>,
    stats: &mut Stats,
) -> usize {
    let before = batch.len();
    while batch.len() < MAX_EVENTS_PER_TICK {
        let Some(item) = ring.next() else {
            break;
        };
        if item.len() < mem::size_of::<PacketEvent>() {
            // Same bucket as a version/family mismatch: the record cannot be
            // decoded into an `Event`, so `malformed` counts it and nothing is
            // written to the CSV.
            warn!(
                "ignoring short ring buffer record ({} bytes)",
                item.len()
            );
            stats.malformed += 1;
            continue;
        }

        // SAFETY: length was checked above; `read_unaligned` does not require
        // the ring buffer pointer to be aligned to 8 bytes.
        let raw = unsafe { std::ptr::read_unaligned(item.as_ptr().cast::<PacketEvent>()) };
        match Event::from_abi(&raw, clock.unix_ms(raw.ts_ns)) {
            Some(event) => batch.push(event),
            None => stats.malformed += 1,
        }
    }
    batch.len() - before
}

/// Converts the kernel's `bpf_ktime_get_ns()` (CLOCK_MONOTONIC) timestamps to
/// wall-clock Unix milliseconds.
///
/// Both clocks are sampled once at startup; the offset is then constant, so no
/// per-event syscall is needed.  `bpf_ktime_get_ns` shares the CLOCK_MONOTONIC
/// time base on the kernels supported by aya.
struct WallClock {
    unix_ns: u128,
    mono_ns: u64,
}

impl WallClock {
    fn new() -> Result<Self> {
        let unix_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before the Unix epoch")?
            .as_nanos();

        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("clock_gettime(CLOCK_MONOTONIC) failed");
        }
        let mono_ns = ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64;

        Ok(Self { unix_ns, mono_ns })
    }

    fn unix_ms(&self, mono_ts_ns: u64) -> u64 {
        let delta = mono_ts_ns.saturating_sub(self.mono_ns) as u128;
        ((self.unix_ns + delta) / 1_000_000).min(u64::MAX as u128) as u64
    }
}
