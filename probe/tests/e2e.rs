//! Privileged end-to-end cases E-01..E-07 (TEST-PLAN §5.4).
//!
//! These cases need root, an XDP-capable kernel and the ability to create veth
//! pairs, so they are `#[ignore]`d: a plain `cargo test` never picks them up and
//! the unprivileged L0–L2 gates stay unchanged.  Run them with
//!
//! ```text
//! cargo xtask e2e              # all seven cases, two build passes, -j 4
//! cargo xtask e2e --case e03   # one case
//! ```
//!
//! Every assertion is an event: a case waits for a log line, a CSV row or a
//! kernel counter and fails on timeout, so there is no fixed sleep anywhere and
//! `-j 4` is safe — each case owns its veth pair and its own artifact directory.

mod e2e_support;

use std::{collections::BTreeSet, fs, io, path::Path};

use e2e_support::{
    Env, STEP_TIMEOUT, count_for, log_text, total_count, wait_aggs, wait_for, wait_samples,
    wait_window_start,
};
use probe_common::STATS_CONFIG_MISSING;
use probe_testkit::{frames::FrameSpec, net};

/// Aggregation window used by the cases that compare CSV rows: short enough to
/// close quickly, long enough that a burst cannot straddle it by accident.
const WINDOW_MS: u64 = 200;

/// Length of an Ethernet header: `AF_PACKET`/`SOCK_RAW` rejects anything shorter
/// (`sendto` returns `EINVAL`), so runt frames cannot be injected at all.
const ETH_HEADER_LEN: usize = 14;

/// One millisecond in nanoseconds, for the `--window-ms` → `window_ns` values the
/// `CONFIG` map stores.
const NANOS_PER_MS: u64 = 1_000_000;

#[test]
#[ignore = "privileged: run with `cargo xtask e2e`"]
fn case_e01_attach_lifecycle() {
    let env = Env::new("e01");
    let iface = env.pair.probe_iface().to_owned();

    // Nothing is attached before the probe starts, so "attached afterwards" is
    // really the probe's doing.
    assert_eq!(
        net::xdp_prog_id(&iface).unwrap(),
        None,
        "{iface} already carries an XDP program before the probe starts"
    );
    // `run_cnt` only moves while the kernel maintains program statistics.
    net::enable_bpf_stats().unwrap();

    let probe = env
        .probe()
        .window_ms(WINDOW_MS)
        .grace_ms(WINDOW_MS)
        .scan_ms(50)
        .spawn()
        .unwrap();
    probe.wait_attached(STEP_TIMEOUT).unwrap();

    let id = net::xdp_prog_id(&iface)
        .unwrap()
        .unwrap_or_else(|| panic!("bpftool net shows no XDP program on {iface} after attach"));
    // A program that has never run may not report `run_cnt` at all (bpftool omits
    // a zero counter), so an absent counter before the burst means "zero", not
    // "unsupported".
    let before = net::prog_run_cnt(id).unwrap().unwrap_or(0);

    let sent = env
        .sender()
        .send_repeat(&env.frame("ipv4_udp.bin"), 10)
        .unwrap();
    assert_eq!(sent, 10);

    // The kernel's own invocation counter: it proves the program *runs* on
    // ingress, not merely that it is attached.
    wait_for("run_cnt to grow by 10", || {
        let now = net::prog_run_cnt(id).unwrap();
        now.filter(|&count| count >= before + 10)
    });

    let outcome = probe.finish(STEP_TIMEOUT).unwrap();
    assert!(
        outcome.status.success(),
        "probe exited with {}:\n{}",
        outcome.status,
        log_text(&outcome.logs)
    );
    assert_eq!(
        outcome.stats.get("packets_total"),
        10,
        "{}",
        log_text(&outcome.logs)
    );
    // Exit detaches: the interface keeps existing, without the program.
    assert_eq!(
        net::xdp_prog_id(&iface).unwrap(),
        None,
        "probe left an XDP program attached to {iface}"
    );
    net::ip(&["link", "show", &iface]).expect("the interface must survive the probe");
}

#[test]
#[ignore = "privileged: run with `cargo xtask e2e`"]
fn case_e02_one_hundred_frames_one_window() {
    let env = Env::new("e02");
    let probe = env
        .probe()
        .window_ms(WINDOW_MS)
        .grace_ms(WINDOW_MS)
        .scan_ms(50)
        .spawn()
        .unwrap();
    probe.wait_attached(STEP_TIMEOUT).unwrap();

    let sender = env.sender();
    // A hundred frames take about a millisecond, so starting just after a window
    // boundary puts the whole burst in one kernel window instead of racing it.
    wait_window_start(WINDOW_MS * NANOS_PER_MS);
    assert_eq!(
        sender.send_repeat(&env.frame("ipv4_udp.bin"), 100).unwrap(),
        100
    );

    let rows = wait_aggs(&env.aggregates_csv(), |rows| total_count(rows) >= 100);
    assert_eq!(rows.len(), 1, "expected one window, got {rows:#?}");
    let row = &rows[0];
    assert_eq!(row.count, 100);
    assert!(row.window_complete, "{row:#?}");
    assert_eq!(row.iface.as_str(), env.pair.probe_iface());
    assert_eq!(row.direction, "ingress");
    assert_eq!(row.action, "pass");
    assert_eq!(row.src_prefix, "10.0.0.0/24");
    assert_eq!(row.dst_addr, "10.0.0.2");
    assert_eq!(row.dst_port, 53);
    assert_eq!(row.protocol, 17);

    let outcome = probe.finish(STEP_TIMEOUT).unwrap();
    assert!(
        outcome.status.success(),
        "probe exited with {}:\n{}",
        outcome.status,
        log_text(&outcome.logs)
    );
    let stats = &outcome.stats;
    assert_eq!(stats.get("packets_total"), 100);
    assert_eq!(stats.get("agg_records"), 1);
    assert_eq!(stats.get("events_received"), 1);
    assert_eq!(stats.get("sample_rows"), 1);
    assert_eq!(stats.get("malformed"), 0);
    assert_eq!(stats.get("late_dropped"), 0);
    assert_eq!(stats.get("agg_overflow"), 0);
}

#[test]
#[ignore = "privileged: run with `cargo xtask e2e`"]
fn case_e03_full_frame_matrix() {
    let env = Env::new("e03");
    let probe = env
        .probe()
        .window_ms(WINDOW_MS)
        .grace_ms(WINDOW_MS)
        .scan_ms(50)
        .spawn()
        .unwrap();
    probe.wait_attached(STEP_TIMEOUT).unwrap();

    let accepted = env.table.accepted().count();
    let rejected = env.table.rejected().count();
    // `sendto` on `AF_PACKET`/`SOCK_RAW` refuses frames shorter than an Ethernet
    // header, so `truncated_eth.bin` can never reach XDP: that parse path is
    // covered by the L0 parser tests, and here the sender must report it instead
    // of injecting a zero-padded frame with a different meaning.
    let (runt, injectable): (Vec<&FrameSpec>, Vec<&FrameSpec>) = env
        .table
        .specs
        .iter()
        .partition(|spec| spec.bytes.len() < ETH_HEADER_LEN);
    let sender = env.sender();
    for spec in &runt {
        let err = sender
            .send(&spec.bytes)
            .expect_err("the kernel must refuse a runt frame");
        assert_eq!(
            err.kind(),
            io::ErrorKind::InvalidInput,
            "{}: {err}",
            spec.file
        );
    }
    assert_eq!(
        runt.len() + injectable.len(),
        accepted + rejected,
        "index.tsv holds {accepted} accepted and {rejected} rejected frames"
    );

    let frames: Vec<Vec<u8>> = injectable.iter().map(|spec| spec.bytes.clone()).collect();
    assert_eq!(sender.send_cycle(&frames, 1).unwrap(), frames.len());
    let rows = wait_aggs(&env.aggregates_csv(), |rows| {
        total_count(rows) >= accepted as u64
    });

    // The rejected frames must not create a key at all: the aggregate file holds
    // exactly the accepted fixtures, one row each.
    let mut expected: Vec<(String, u16, u8)> = env
        .table
        .accepted()
        .map(|spec| {
            (
                spec.dst.clone().unwrap_or_default(),
                spec.dport,
                spec.protocol,
            )
        })
        .collect();
    let mut actual: Vec<(String, u16, u8)> = rows
        .iter()
        .map(|row| (row.dst_addr.clone(), row.dst_port, row.protocol))
        .collect();
    expected.sort();
    actual.sort();
    assert_eq!(actual, expected, "aggregate keys differ from index.tsv");
    assert!(
        rows.iter().all(|row| row.count == 1),
        "every fixture is sent exactly once: {rows:#?}"
    );

    let samples = wait_samples(&env.samples_csv(), |rows| rows.len() >= accepted);
    let mut expected_samples: Vec<(String, String, u16, u16, u8)> = env
        .table
        .accepted()
        .map(|spec| {
            (
                spec.src.clone().unwrap_or_default(),
                spec.dst.clone().unwrap_or_default(),
                spec.sport,
                spec.dport,
                spec.protocol,
            )
        })
        .collect();
    let mut actual_samples: Vec<(String, String, u16, u16, u8)> = samples
        .iter()
        .map(|row| {
            (
                row.src_ip.clone(),
                row.dst_ip.clone(),
                row.src_port,
                row.dst_port,
                row.protocol,
            )
        })
        .collect();
    expected_samples.sort();
    actual_samples.sort();
    assert_eq!(
        actual_samples, expected_samples,
        "sample rows differ from index.tsv"
    );

    let outcome = probe.finish(STEP_TIMEOUT).unwrap();
    assert!(
        outcome.status.success(),
        "probe exited with {}:\n{}",
        outcome.status,
        log_text(&outcome.logs)
    );
    let stats = &outcome.stats;
    // `packets_total` counts packets the *kernel* parser accepted, so the seven
    // rejected fixtures never reach it; their absence shows up in the CSV key set
    // compared above, not in a counter.
    assert_eq!(stats.get("packets_total"), accepted as u64);
    assert_eq!(stats.get("agg_records"), accepted as u64);
    assert_eq!(stats.get("sample_rows"), accepted as u64);
    assert_eq!(stats.get("events_received"), accepted as u64);
    assert_eq!(stats.get("malformed"), 0);
    assert_eq!(stats.get("agg_overflow"), 0);
}

#[test]
#[ignore = "privileged: run with `cargo xtask e2e`"]
fn case_e04_sigterm_flushes_partial_window() {
    let env = Env::new("e04");
    // A one-hour window cannot close while the case runs (the monotonic clock is
    // far below an hour in any test environment), so the row written on exit must
    // be the partial flush of a window that is still open.
    let probe = env
        .probe()
        .window_ms(3_600_000)
        .grace_ms(2_000)
        .scan_ms(50)
        .spawn()
        .unwrap();
    probe.wait_attached(STEP_TIMEOUT).unwrap();

    assert_eq!(
        env.sender()
            .send_repeat(&env.frame("ipv4_udp.bin"), 5)
            .unwrap(),
        5
    );
    // `sendto` returns after the peer's XDP ran (veth queues into the local
    // backlog and `local_bh_enable` drains it), so the kernel counters are final
    // when the signal is sent: no sleep needed here.
    let outcome = probe.finish(STEP_TIMEOUT).unwrap();
    assert!(
        outcome.status.success(),
        "probe exited with {}:\n{}",
        outcome.status,
        log_text(&outcome.logs)
    );
    let stats = &outcome.stats;
    assert_eq!(stats.get("packets_total"), 5);
    assert_eq!(stats.get("agg_records"), 1);
    assert_eq!(stats.get("sample_rows"), 1);
    assert_eq!(stats.get("late_dropped"), 0);

    let rows = env.aggs();
    assert_eq!(rows.len(), 1, "{rows:#?}");
    assert_eq!(rows[0].count, 5);
    assert!(
        !rows[0].window_complete,
        "SIGTERM must report the window as incomplete: {:#?}",
        rows[0]
    );
    assert_eq!(
        rows[0].window_end_ms - rows[0].window_start_ms,
        3_600_000,
        "the row must describe the configured one-hour window"
    );
}

#[test]
#[ignore = "privileged: run with `cargo xtask e2e`"]
fn case_e05_unusable_config_falls_back() {
    let env = Env::new("e05");
    let probe = env
        .probe()
        .window_ms(WINDOW_MS)
        .grace_ms(WINDOW_MS)
        .scan_ms(50)
        .spawn()
        .unwrap();
    probe.wait_attached(STEP_TIMEOUT).unwrap();
    // Every case creates the same map names, so the maps are resolved through
    // this probe's program id instead of by name: with `-j` a by-name lookup can
    // hit a concurrent case's maps.
    let prog = net::xdp_prog_id(env.pair.probe_iface())
        .unwrap()
        .expect("the case probe must be attached");
    let maps = net::prog_map_ids(prog).unwrap();
    let config_map = *maps.get("CONFIG").expect("probe loads CONFIG");
    let stats_map = *maps.get("STATS").expect("probe loads STATS");

    // A loaded `Array` map has no deletable element (`map delete` is only for
    // hash-like maps) and the probe never pins its maps, so the reachable failure
    // is an *unusable* `CONFIG`: the version no longer matches `CONFIG_VERSION`,
    // which is the same fallback the data path takes for a missing map.
    // Layout (probe-common/src/lib.rs, `ProbeConfig`): version u32 @0, pad @4,
    // window_ns u64 @8, grace_ns u64 @16, ipv4 u8 @24, ipv6 u8 @25, action @26,
    // pad [u8; 5] @27 — 32 bytes in total.
    let window_ns = WINDOW_MS * NANOS_PER_MS;
    let mut config = Vec::with_capacity(32);
    config.extend_from_slice(&0u32.to_le_bytes()); // version 0 != CONFIG_VERSION
    config.extend_from_slice(&0u32.to_le_bytes()); // padding
    config.extend_from_slice(&window_ns.to_le_bytes());
    config.extend_from_slice(&window_ns.to_le_bytes()); // grace_ns
    config.extend_from_slice(&[24, 64, 0]); // prefix lengths, default action pass
    config.extend_from_slice(&[0u8; 5]); // padding
    assert_eq!(config.len(), 32);
    net::update_config(config_map, &config).unwrap();

    assert_eq!(
        env.sender()
            .send_repeat(&env.frame("ipv4_udp.bin"), 10)
            .unwrap(),
        10
    );

    // `CONFIG_MISSING` is a kernel `STATS` counter and deliberately absent from
    // the exit-only `stats:` line, so read the map while still attached.
    let missing = wait_for("STATS[CONFIG_MISSING] to count the frames", || {
        let seen = net::stat_counter(stats_map, STATS_CONFIG_MISSING).unwrap_or(0);
        (seen >= 10).then_some(seen)
    });

    let outcome = probe.finish(STEP_TIMEOUT).unwrap();
    assert!(
        outcome.status.success(),
        "probe exited with {}:\n{}",
        outcome.status,
        log_text(&outcome.logs)
    );
    assert_eq!(outcome.stats.get("packets_total"), 10);
    assert!(
        missing >= 10,
        "every frame must count as CONFIG_MISSING, saw {missing}"
    );
    // The fallback is transparent: the packets still aggregate normally.
    assert_eq!(total_count(&env.aggs()), 10);
}

#[test]
#[ignore = "privileged: run with `cargo xtask e2e`"]
fn case_e06_small_map_overflows_without_losing_packets() {
    let env = Env::new("e06");
    // Small-map build (`--features small-maps`): `AGG` holds 1024 keys per CPU, so
    // 1100 distinct keys cannot fit.  Pin the sending thread so every insert lands
    // in one per-CPU map instead of spreading over the 20 CPUs of the dev host.
    let cpu = net::pin_current_cpu().expect("pin the sending thread");
    let probe = env
        .probe()
        .window_ms(3_600_000)
        .grace_ms(2_000)
        .scan_ms(50)
        .spawn()
        .unwrap();
    probe.wait_attached(STEP_TIMEOUT).unwrap();

    let base = env.frame("ipv4_udp.bin");
    let frames: Vec<Vec<u8>> = (1..=1100u16)
        .map(|port| with_udp_dst_port(&base, port))
        .collect();
    assert_eq!(env.sender().send_cycle(&frames, 1).unwrap(), 1100);

    let outcome = probe.finish(STEP_TIMEOUT).unwrap();
    assert!(
        outcome.status.success(),
        "probe exited with {}:\n{}",
        outcome.status,
        log_text(&outcome.logs)
    );
    let stats = &outcome.stats;
    let rows = env.aggs();
    let stored = total_count(&rows);
    let overflow = stats.get("agg_overflow");

    assert_eq!(stats.get("packets_total"), 1100);
    assert!(
        overflow > 0,
        "1100 keys must overflow a 1024-entry per-CPU map (cpu {cpu}, stored {stored})"
    );
    // Conservation, not a particular split: a packet is either aggregated once or
    // counted as overflow, never lost and never counted twice.
    assert_eq!(
        stored + overflow,
        1100,
        "stored {stored} + overflow {overflow} must equal the 1100 sent frames"
    );
    assert_eq!(rows.len() as u64, stored, "one row per stored key");
    assert!(
        rows.iter().all(|row| row.count == 1),
        "each destination port is sent exactly once"
    );
    assert!(
        stored <= 1024,
        "the small map holds at most 1024 keys, got {stored}"
    );
}

#[test]
#[ignore = "privileged: run with `cargo xtask e2e`"]
fn case_e07_two_probes_share_one_csv() {
    let env = Env::new("e07");
    let left = env.pair.probe_iface().to_owned();
    let right = env.pair.sender_iface().to_owned();
    let samples = env.dir.join("samples.csv");
    let aggregates = env.dir.join("aggregates.csv");

    // Both probes append to the same two files.  The sink takes an exclusive
    // `flock` around the "is the file still empty" check, so exactly one header is
    // written no matter how the two starts interleave.
    let on_left = env
        .probe_at(&left, &samples, &aggregates)
        .window_ms(WINDOW_MS)
        .grace_ms(WINDOW_MS)
        .scan_ms(50)
        .append(true)
        .spawn()
        .unwrap();
    on_left.wait_attached(STEP_TIMEOUT).unwrap();
    let on_right = env
        .probe_at(&right, &samples, &aggregates)
        .window_ms(WINDOW_MS)
        .grace_ms(WINDOW_MS)
        .scan_ms(50)
        .append(true)
        .spawn()
        .unwrap();
    on_right.wait_attached(STEP_TIMEOUT).unwrap();

    let frame = env.frame("ipv4_udp.bin");
    // Frames sent from one end arrive on the other end's ingress only, so each
    // probe sees its own traffic.
    assert_eq!(env.sender_on(&right).send_repeat(&frame, 3).unwrap(), 3);
    assert_eq!(env.sender_on(&left).send_repeat(&frame, 4).unwrap(), 4);

    let rows = wait_aggs(&aggregates, |rows| total_count(rows) >= 7);
    assert_eq!(rows.len(), 2, "one row per interface: {rows:#?}");
    assert_eq!(count_for(&rows, &left), 3);
    assert_eq!(count_for(&rows, &right), 4);

    let sample_rows = wait_samples(&samples, |rows| rows.len() >= 2);
    let ifaces: BTreeSet<&str> = sample_rows.iter().map(|row| row.iface.as_str()).collect();
    assert_eq!(
        ifaces,
        BTreeSet::from([left.as_str(), right.as_str()]),
        "both interfaces must contribute a sample row"
    );

    let left_outcome = on_left.finish(STEP_TIMEOUT).unwrap();
    let right_outcome = on_right.finish(STEP_TIMEOUT).unwrap();
    assert!(
        left_outcome.status.success(),
        "probe on {left} exited with {}:\n{}",
        left_outcome.status,
        log_text(&left_outcome.logs)
    );
    assert!(
        right_outcome.status.success(),
        "probe on {right} exited with {}:\n{}",
        right_outcome.status,
        log_text(&right_outcome.logs)
    );
    assert_eq!(left_outcome.stats.get("packets_total"), 3);
    assert_eq!(right_outcome.stats.get("packets_total"), 4);

    // No interleaved corruption: one header per file, every row at full width.
    assert_csv_shape(&samples, 7);
    assert_csv_shape(&aggregates, 16);
}

/// Rewrites the UDP destination port of an IPv4 frame.
///
/// The data path never validates checksums (`probe-common/src/parse.rs` treats
/// the IPv4 checksum as unchecked), so changing the port creates a new
/// aggregation key without touching the checksum fields — exactly what the
/// small-map case needs: many keys out of one frame.
fn with_udp_dst_port(frame: &[u8], port: u16) -> Vec<u8> {
    let mut frame = frame.to_vec();
    let ihl = usize::from(frame[ETH_HEADER_LEN] & 0x0f) * 4;
    let offset = ETH_HEADER_LEN + ihl + 2;
    frame[offset..offset + 2].copy_from_slice(&port.to_be_bytes());
    frame
}

/// Asserts a CSV has exactly one header and only rows of `columns` fields.
fn assert_csv_shape(path: &Path, columns: usize) {
    let text =
        fs::read_to_string(path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
    let header = text.lines().next().unwrap_or_default().to_owned();
    assert_eq!(
        header.split(',').count(),
        columns,
        "{}: header {header:?}",
        path.display()
    );
    assert_eq!(
        text.matches(header.as_str()).count(),
        1,
        "{}: the header must appear exactly once",
        path.display()
    );
    for (index, line) in text.lines().enumerate().skip(1) {
        assert_eq!(
            line.split(',').count(),
            columns,
            "{}:{}: {line:?}",
            path.display(),
            index + 1
        );
    }
}
