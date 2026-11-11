//! Userspace loader for the Train 0 XDP probe.
//!
//! Startup order is part of the contract:
//!
//! 1. load the embedded eBPF object,
//! 2. write `CONFIG` **before attach** and read it back; the read-back value is
//!    the only source of truth for `window_ns`/`grace_ns`/prefix lengths,
//! 3. attach the XDP program,
//! 4. scan `AGG` every `--scan-ms` and drain `SAMPLES` every `--flush-ms`.
//!
//! On SIGINT/SIGTERM the loop stops and the shutdown path drains the ring one
//! last time, writes every pending sample, flushes all remaining `AGG` keys
//! (`window_complete=0` for windows that are still open), flushes both CSVs,
//! sums the kernel `STATS` counters and detaches.  Exactly one machine-readable
//! `stats:` line is printed on stdout.
//!
//! Everything window-related (close watermark, lateness, merging, sample
//! de-duplication, CSV rendering) lives in [`aggregate`] and [`sink`].

mod aggregate;
mod sink;

use std::{
    path::PathBuf,
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
use aggregate::{
    AggRecord, AggTable, Clock, NANOS_PER_MS, SampleDedup, SampleIngest, closed_window_watermark,
};
use probe_common::{
    ACTION_PASS, AggKey, AggValue, CONFIG_VERSION, ProbeConfig, STATS_AGG_OVERFLOW,
    STATS_PACKETS_TOTAL, STATS_SAMPLE_FAILED, SampleEvent,
};
use sink::{AggSink, Event, Sink, SinkKind, create_agg_sink, create_sink, decode_sample};
use tokio::{
    signal,
    signal::unix::{SignalKind, signal as unix_signal},
    time,
};

/// Upper bound per periodic drain; keeps the interval responsive when a large
/// backlog is waiting in the ring buffer.
const MAX_EVENTS_PER_TICK: usize = 8192;

/// Period of the `info!` metrics line.  The machine-readable `stats:` line is
/// still printed only once, on exit.
const METRICS_PERIOD: Duration = Duration::from_secs(5);

/// Per-CPU aggregation map (`AGG`): one `(window_id, key)` slot per CPU.
type AggMap = PerCpuHashMap<MapData, AggKey, AggValue>;
/// Sample ring buffer (`SAMPLES`).
type SampleRing = RingBuf<MapData>;
/// Per-CPU counters (`STATS`).
type StatsMap = PerCpuArray<MapData, u64>;

/// Userspace counters reported in the one `stats:` line on exit.
///
/// `late_dropped` and `samples_deduped` live in [`aggregate`] because the
/// watermark they depend on lives there.
#[derive(Default)]
struct Stats {
    /// Sample records successfully decoded from `SAMPLES` (before de-dup).
    events_received: u64,
    /// Sample CSV rows written (one per de-duplicated key).
    sample_rows: u64,
    /// Aggregate CSV rows written.
    agg_records: u64,
    /// Ring records with a wrong length or an unknown address family.
    malformed: u64,
}

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

#[tokio::main]
async fn main() -> Result<()> {
    let opt = Opt::parse();

    // Fail fast, before the logger or any eBPF resource is set up: an invalid
    // flag should be reported on its own, not buried in startup logs.
    let requested = validate(&opt)?;
    env_logger::init();

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
    // The `AGG`/`STATS`/`SAMPLES` maps are consumed by the loop below, which is
    // why they are taken out of the `Ebpf` object after attaching.
    let (mut agg, mut samples, stats_map) = take_maps(&mut ebpf)?;

    let clock = wall_clock()?;
    let mut sink = create_sink(opt.sink, &opt.out, &opt.iface, opt.append)?;
    let mut agg_sink = create_agg_sink(&opt.agg_out, &opt.iface, opt.append, config, clock)?;
    info!(
        "writing samples to {} and aggregates to {} (append={})",
        opt.out.display(),
        opt.agg_out.display(),
        opt.append
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

    let mut stats = Stats::default();
    let mut table = AggTable::new();
    let mut dedup = SampleDedup::new();
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
                drain_ring(&mut samples, &mut dedup, &table, &config, clock, &mut stats);
            }
            _ = scan_tick.tick() => {
                // Drain before scanning: a sample the kernel produced before a
                // window closed must be deduplicated before the window is
                // reported, otherwise our own report order makes it late.
                drain_ring(&mut samples, &mut dedup, &table, &config, clock, &mut stats);
                let started = Instant::now();
                report_closed(
                    &mut agg,
                    &mut table,
                    &mut dedup,
                    sink.as_mut(),
                    agg_sink.as_mut(),
                    &config,
                    clock,
                    &mut stats,
                )?;
                last_scan_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
            }
            _ = metrics_tick.tick() => {
                log_metrics(
                    &stats_map,
                    &agg,
                    &table,
                    &dedup,
                    last_scan_ms,
                    opt.scan_ms,
                );
            }
        }
    }

    // ---------------------------------------------------------------- shutdown
    // 1. Final drain: whatever the kernel already queued must still reach disk.
    loop {
        if drain_ring(&mut samples, &mut dedup, &table, &config, clock, &mut stats) == 0 {
            break;
        }
    }

    // 2. Last complete `AGG` scan: this is the "all keys" variant, because the
    //    data path is about to be detached.  Whatever is left is written with
    //    `window_complete=0`: none of these windows is guaranteed to have seen
    //    its close scan (the normal scan tick reports the complete windows).
    scan_agg(&mut agg, &mut table, None);
    let open = table.take_pending();
    write_aggs(agg_sink.as_mut(), &open, &mut stats)?;

    // 3. Every pending sample (one row per key) is written now, including the
    //    samples of windows that never closed.
    let pending_samples = dedup.take_all();
    write_samples(sink.as_mut(), &pending_samples, clock, &mut stats)?;

    // 4. Flush both files before the teardown starts; nothing may be lost after
    //    the program is detached.
    sink.flush()?;
    agg_sink.flush()?;

    // 5. Kernel counters are read while the maps are still open, then detach.
    let kernel = read_kernel_stats(&stats_map);
    detach_xdp(&mut ebpf, link_id);

    // 6. Machine-readable contract: the e2e scripts grep these exact tokens, so
    //    the line is printed unconditionally on stdout and is not routed through
    //    `log` (its presence must not depend on RUST_LOG).  This is the only
    //    stdout write of the process.
    let late_dropped = table.late_dropped() + dedup.late_dropped();
    let samples_deduped = dedup.deduped();
    println!(
        "stats: packets_total={} sample_rows={} events_received={} rows_written={} agg_records={} kernel_dropped={} sample_failed={} agg_overflow={} malformed={} late_dropped={} samples_deduped={}",
        kernel.packets_total,
        stats.sample_rows,
        stats.events_received,
        stats.sample_rows,
        stats.agg_records,
        kernel.sample_failed,
        kernel.sample_failed,
        kernel.agg_overflow,
        stats.malformed,
        late_dropped,
        samples_deduped
    );

    Ok(())
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

/// Emits every window that is closed at the current monotonic time.
///
/// Only keys of closed windows are merged and removed from `AGG`; keys of open
/// windows stay in the map so the data path keeps its per-CPU `count` and
/// `first_seen`/`last_seen` continuous for the whole window.  Removing them
/// mid-window would restart the count from zero and make the data path report a
/// second "first packet" sample for the same key.
///
/// Samples of the closed windows are written together with the aggregate rows:
/// once a window is closed the data path can no longer produce a sample for it,
/// so the two outputs stay consistent.
fn report_closed(
    agg: &mut AggMap,
    table: &mut AggTable,
    dedup: &mut SampleDedup,
    sink: &mut dyn Sink,
    agg_sink: &mut dyn AggSink,
    config: &ProbeConfig,
    clock: Clock,
    stats: &mut Stats,
) -> Result<()> {
    let Some(watermark) =
        closed_window_watermark(mono_now_ns()?, config.window_ns, config.grace_ns)
    else {
        return Ok(());
    };
    // Merge and remove the closed-window keys, leaving open-window keys in
    // `AGG` untouched.
    scan_agg(agg, table, Some(watermark));
    // Take the aggregate rows first: `take_closed` also advances the
    // "already reported" watermark, and the sample lookup below must use the
    // same watermark.  Both calls happen after the ring was drained, so no
    // sample of a newly closed window is mistaken for a late one.
    let rows = table.take_closed(watermark);
    let closed_samples = dedup.take_closed(watermark);
    if closed_samples.is_empty() && rows.is_empty() {
        return Ok(());
    }
    write_aggs(agg_sink, &rows, stats)?;
    write_samples(sink, &closed_samples, clock, stats)?;
    sink.flush()?;
    agg_sink.flush()?;
    Ok(())
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

/// Drains the samples currently available in the ring buffer into the
/// de-duplication store.
///
/// Returns how many records were handled.  A record whose length is not exactly
/// `size_of::<SampleEvent>()` (for example the 48-byte record of the previous
/// ABI) or whose address family is neither IPv4 nor IPv6 is counted in
/// `malformed` and never interpreted with the wrong layout.
fn drain_ring(
    samples: &mut SampleRing,
    dedup: &mut SampleDedup,
    table: &AggTable,
    config: &ProbeConfig,
    clock: Clock,
    stats: &mut Stats,
) -> usize {
    let mut handled = 0;
    while handled < MAX_EVENTS_PER_TICK {
        let Some(item) = samples.next() else {
            break;
        };
        handled += 1;

        let Some(raw) = decode_sample(&item) else {
            warn!(
                "ignoring malformed sample record ({} bytes, expected {})",
                item.len(),
                std::mem::size_of::<SampleEvent>()
            );
            stats.malformed += 1;
            continue;
        };
        let Some(event) = Event::from_abi(&raw, clock.unix_ms(raw.ts_ns)) else {
            // Unknown family: the record cannot be rendered without guessing
            // which address layout it uses.
            warn!(
                "ignoring sample with unsupported address family {}",
                raw.family
            );
            stats.malformed += 1;
            continue;
        };
        stats.events_received += 1;

        // Rebuild the aggregation key exactly like the data path did: the
        // source address is masked with the prefix length from the read-back
        // CONFIG, so two sources in one /24 share one sample row.
        let key = AggKey::from_sample(&raw, config);
        let late = table.is_late(raw.window_id);
        if let SampleIngest::Duplicate = dedup.ingest(raw, key, late) {
            debug!(
                "de-duplicated sample for window {} ({} -> {}), keeping the earliest ts_ns",
                raw.window_id, event.src_ip, event.dst_ip
            );
        }
    }
    handled
}

/// Enumerates every key of the per-CPU `AGG` map, merges its per-CPU slots and
/// removes the key.
///
/// Keys are collected before removing: the map iterator holds an immutable
/// borrow while `remove` needs `&mut`.
///
/// `close_before` restricts the scan to closed windows: with
/// `Some(watermark)` only keys with `window_id <= watermark` are read, merged
/// and removed, while keys of still-open windows stay in the map so the data
/// path keeps accumulating them.  `None` is the shutdown scan that drains every
/// remaining key.  Keys of an already reported window are merged (so they are
/// counted in `late_dropped`) and removed without being reported again.
fn scan_agg(agg: &mut AggMap, table: &mut AggTable, close_before: Option<u64>) {
    let mut keys: Vec<AggKey> = Vec::new();
    for key in agg.keys() {
        match key {
            Ok(key) => keys.push(key),
            Err(e) => {
                warn!("AGG key iteration failed: {e}");
                break;
            }
        }
    }

    for key in keys {
        // An open window must keep its per-CPU state in the kernel map: the
        // count would otherwise restart at zero and the data path would emit a
        // second sample for the same key.
        if !close_before.map_or(true, |watermark| key.window_id <= watermark) {
            continue;
        }
        match agg.get(&key, 0) {
            Ok(values) => {
                // `PerCpuValues` derefs to one entry per CPU; merge them all.
                for slot in values.iter() {
                    table.ingest_slot(key, slot);
                }
            }
            Err(e) => warn!("failed to read AGG key: {e}"),
        }
        if let Err(e) = agg.remove(&key) {
            warn!("failed to remove AGG key: {e}");
        }
    }
}

/// Number of keys currently in `AGG`.
///
/// Enumerating the map costs one syscall per key, so this is only used by the
/// 5s metrics line and not by the scan path.
fn count_agg_keys(agg: &AggMap) -> usize {
    let mut count = 0;
    for key in agg.keys() {
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

/// Kernel counters summed over all CPUs.
struct KernelStats {
    packets_total: u64,
    sample_failed: u64,
    agg_overflow: u64,
}

/// Reads the kernel `STATS` counters (one slot per index, one value per CPU).
fn read_kernel_stats(stats_map: &StatsMap) -> KernelStats {
    KernelStats {
        packets_total: read_counter(stats_map, STATS_PACKETS_TOTAL),
        sample_failed: read_counter(stats_map, STATS_SAMPLE_FAILED),
        agg_overflow: read_counter(stats_map, STATS_AGG_OVERFLOW),
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

/// Logs the running counters with `info!` every [`METRICS_PERIOD`].
///
/// `scan_ms` is the measured duration of the last periodic scan and
/// `scan_interval_ms` the configured period; `agg_keys` is the current key count
/// of the `AGG` map.  This is the operational view; the machine-readable
/// `stats:` line stays exit-only so scripts can rely on it appearing exactly
/// once.
fn log_metrics(
    stats_map: &StatsMap,
    agg: &AggMap,
    table: &AggTable,
    dedup: &SampleDedup,
    scan_ms: u64,
    scan_interval_ms: u64,
) {
    let packets_total = read_counter(stats_map, STATS_PACKETS_TOTAL);
    let agg_overflow = read_counter(stats_map, STATS_AGG_OVERFLOW);
    let sample_failed = read_counter(stats_map, STATS_SAMPLE_FAILED);
    info!(
        "metrics: packets_total={packets_total} agg_keys={} agg_overflow={agg_overflow} sample_failed={sample_failed} late_dropped={} scan_ms={scan_ms} scan_interval_ms={scan_interval_ms}",
        count_agg_keys(agg),
        table.late_dropped() + dedup.late_dropped()
    );
}

/// Monotonic nanoseconds (`CLOCK_MONOTONIC`), the same time base as
/// `bpf_ktime_get_ns()`.
fn mono_now_ns() -> Result<u64> {
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
