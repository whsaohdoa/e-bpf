//! L1 contract tests for the two CSV outputs (TEST-PLAN S3, C-SCHEMA-01..03).
//!
//! These tests treat the CSV files as an external contract: the 7- and 16-column
//! layouts, the closed value sets, and the `--append` guarantees are asserted on
//! the bytes written by the real sink implementations.  No kernel, no root and
//! no privileged container are required.
//!
//! The dialect is deliberately narrow — comma separated, LF line endings, no
//! quoting, no empty fields — so the helpers below reject anything else instead
//! of accepting it leniently.

use std::{
    fs,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::{Path, PathBuf},
    sync::{
        Arc, Barrier,
        atomic::{AtomicU32, Ordering},
    },
};

use probe::{
    aggregate::{AggRecord, Clock},
    sink::{AggCsvSink, AggSink, CsvSink, Event, Sink},
};
use probe_common::{
    ACTION_PASS, AggKey, CONFIG_VERSION, DIRECTION_INGRESS, FAMILY_IPV4, FAMILY_IPV6, ProbeConfig,
};

const SAMPLE_COLUMNS: usize = 7;
const AGG_COLUMNS: usize = 16;
const WINDOW_NS: u64 = 10_000_000_000;

/// Self-cleaning temporary directory (one per test, unique per process).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "probe-csv-schema-{}-{tag}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Parses a CSV file strictly and returns its rows.
fn strict_rows(path: &Path, columns: usize) -> Vec<Vec<String>> {
    let text = fs::read_to_string(path).expect("read CSV file");
    assert!(
        text.ends_with('\n'),
        "{} must end with a newline",
        path.display()
    );
    assert!(
        !text.contains('\r'),
        "{} must use LF line endings",
        path.display()
    );
    assert!(
        !text.contains('"'),
        "{} must not contain CSV quoting",
        path.display()
    );

    let mut rows = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line_no = index + 1;
        assert!(
            !line.is_empty(),
            "blank line {line_no} in {}",
            path.display()
        );
        let fields: Vec<String> = line.split(',').map(str::to_owned).collect();
        assert_eq!(
            fields.len(),
            columns,
            "line {line_no}: expected {columns} columns, got {}: {line}",
            fields.len()
        );
        for field in &fields {
            assert!(!field.is_empty(), "line {line_no}: empty field: {line}");
        }
        rows.push(fields);
    }
    rows
}

fn parse_u64(field: &str, what: &str) -> u64 {
    field
        .parse()
        .unwrap_or_else(|err| panic!("{what} must be an unsigned integer, got {field:?}: {err}"))
}

fn parse_u16(field: &str, what: &str) -> u16 {
    field
        .parse()
        .unwrap_or_else(|err| panic!("{what} must be a 16-bit integer, got {field:?}: {err}"))
}

fn parse_u8(field: &str, what: &str) -> u8 {
    field
        .parse()
        .unwrap_or_else(|err| panic!("{what} must be an 8-bit integer, got {field:?}: {err}"))
}

fn parse_ip(field: &str, what: &str) -> IpAddr {
    field
        .parse()
        .unwrap_or_else(|err| panic!("{what} must be an IP address, got {field:?}: {err}"))
}

/// Checks `a.b.c.d/len` or `v6::addr/len` and returns the address family.
fn assert_cidr(field: &str, max_len: u8) -> u8 {
    let (addr, len) = field
        .split_once('/')
        .unwrap_or_else(|| panic!("src_prefix must be CIDR, got {field:?}"));
    let len: u8 = len
        .parse()
        .unwrap_or_else(|err| panic!("prefix length of {field:?} is not an integer: {err}"));
    assert!(
        len <= max_len,
        "prefix length of {field:?} exceeds /{max_len}"
    );

    if addr.contains(':') {
        addr.parse::<Ipv6Addr>()
            .unwrap_or_else(|err| panic!("bad IPv6 prefix in {field:?}: {err}"));
        FAMILY_IPV6
    } else {
        addr.parse::<Ipv4Addr>()
            .unwrap_or_else(|err| panic!("bad IPv4 prefix in {field:?}: {err}"));
        FAMILY_IPV4
    }
}

fn assert_header_once(rows: &[Vec<String>], header: &str) {
    let rendered: Vec<String> = rows.iter().map(|row| row.join(",")).collect();
    let header_count = rendered.iter().filter(|row| *row == header).count();
    assert_eq!(header_count, 1, "header must appear exactly once");
    assert_eq!(rendered.first().map(String::as_str), Some(header));
}

fn sample_event(
    ts_unix_ms: u64,
    src: &str,
    dst: &str,
    src_port: u16,
    dst_port: u16,
    protocol: u8,
) -> Event {
    Event {
        ts_unix_ms,
        src_ip: src.parse().expect("src IP"),
        dst_ip: dst.parse().expect("dst IP"),
        src_port,
        dst_port,
        protocol,
    }
}

fn test_config() -> ProbeConfig {
    ProbeConfig {
        version: CONFIG_VERSION,
        window_ns: WINDOW_NS,
        grace_ns: 0,
        ipv4_prefix_len: 24,
        ipv6_prefix_len: 64,
        default_action: ACTION_PASS,
        pad: [0; 5],
    }
}

fn agg_record(
    window_id: u64,
    family: u8,
    src_prefix: [u8; 16],
    dst_addr: [u8; 16],
    dst_port: u16,
    protocol: u8,
) -> AggRecord {
    AggRecord {
        window_id,
        key: AggKey {
            window_id,
            rule_id: 0,
            direction: DIRECTION_INGRESS,
            family,
            src_prefix,
            dst_addr,
            dst_port,
            protocol,
            action: ACTION_PASS,
            reason: 0,
            pad: [0; 5],
        },
        count: 3,
        first_seen: 500,
        last_seen: 1_500,
        sample_dropped: false,
        degraded: false,
        window_complete: true,
    }
}

fn ipv4_bytes(a: u8, b: u8, c: u8, d: u8) -> [u8; 16] {
    let mut addr = [0u8; 16];
    addr[..4].copy_from_slice(&[a, b, c, d]);
    addr
}

/// C-SCHEMA-01: the sample CSV is exactly 7 columns with typed fields, one
/// header row, and `append` neither duplicates the header nor drops old rows.
#[test]
fn c_schema_01_sample_csv_is_seven_typed_columns() {
    let dir = TempDir::new("samples");
    let path = dir.path("packets.csv");

    let mut sink = CsvSink::create(&path, "veth0", false).expect("create sample CSV");
    sink.write_batch(&[
        sample_event(1_700_000_000_000, "10.0.0.1", "10.0.0.2", 12345, 80, 6),
        sample_event(1_700_000_000_001, "fd00::1", "fd00::2", 53, 5000, 17),
        // ICMP: both ports are 0, never omitted.
        sample_event(1_700_000_000_002, "192.0.2.9", "192.0.2.10", 0, 0, 1),
    ])
    .expect("write samples");
    sink.flush().expect("flush samples");
    drop(sink);

    let rows = strict_rows(&path, SAMPLE_COLUMNS);
    assert_header_once(&rows, CsvSink::HEADER);
    assert_eq!(rows.len(), 4, "one header + three sample rows");

    for row in &rows[1..] {
        parse_u64(&row[0], "ts_unix_ms");
        assert_eq!(row[1], "veth0");
        let src = parse_ip(&row[2], "src_ip");
        let dst = parse_ip(&row[3], "dst_ip");
        assert_eq!(src.is_ipv4(), dst.is_ipv4(), "one family per row");
        parse_u16(&row[4], "src_port");
        parse_u16(&row[5], "dst_port");
        parse_u8(&row[6], "protocol");
    }

    // `--append` starts a second sink on the same file: no second header, the
    // previous rows survive.
    let mut appended = CsvSink::create(&path, "veth0", true).expect("append sample CSV");
    appended
        .write_batch(&[sample_event(
            1_700_000_000_003,
            "10.0.0.3",
            "10.0.0.4",
            8080,
            443,
            6,
        )])
        .expect("append sample");
    appended.flush().expect("flush appended samples");
    drop(appended);

    let rows = strict_rows(&path, SAMPLE_COLUMNS);
    assert_header_once(&rows, CsvSink::HEADER);
    assert_eq!(rows.len(), 5, "append keeps the old rows and adds one");

    // Without `--append` the file is truncated and starts with a fresh header.
    let mut fresh = CsvSink::create(&path, "veth1", false).expect("truncate sample CSV");
    fresh
        .write_batch(&[sample_event(
            1_700_000_000_004,
            "10.0.0.5",
            "10.0.0.6",
            1,
            2,
            17,
        )])
        .expect("write truncated sample");
    fresh.flush().expect("flush truncated sample");
    drop(fresh);

    let rows = strict_rows(&path, SAMPLE_COLUMNS);
    assert_header_once(&rows, CsvSink::HEADER);
    assert_eq!(rows.len(), 2, "truncate leaves the header and one row");
    assert_eq!(rows[1][1], "veth1");
}

/// C-SCHEMA-02: the aggregate CSV is exactly 16 columns; `src_prefix` is a
/// valid CIDR, `action` is closed to `pass|block`, and the boolean columns are
/// closed to `0|1`.
#[test]
fn c_schema_02_aggregate_csv_is_sixteen_typed_columns() {
    let dir = TempDir::new("aggregate");
    let path = dir.path("aggregates.csv");
    let config = test_config();
    let clock = Clock::new(1_000_000_000, 0);

    let rows_in = [
        agg_record(
            0,
            FAMILY_IPV4,
            ipv4_bytes(10, 99, 0, 0),
            ipv4_bytes(10, 99, 0, 1),
            80,
            6,
        ),
        agg_record(
            1,
            FAMILY_IPV6,
            Ipv6Addr::new(0xfd00, 0x99, 0, 0, 0, 0, 0, 0).octets(),
            Ipv6Addr::new(0xfd00, 0x99, 0, 0, 0, 0, 0, 1).octets(),
            0,
            1,
        ),
    ];

    let mut sink =
        AggCsvSink::create(&path, "veth0", false, config, clock).expect("create aggregate CSV");
    sink.write_rows(&rows_in).expect("write aggregate rows");
    sink.flush().expect("flush aggregate rows");
    drop(sink);

    let rows = strict_rows(&path, AGG_COLUMNS);
    assert_header_once(&rows, AggCsvSink::HEADER);
    assert_eq!(rows.len(), 3, "one header + two aggregate rows");

    for row in &rows[1..] {
        let start = parse_u64(&row[0], "window_start_ms");
        let end = parse_u64(&row[1], "window_end_ms");
        assert_eq!(end - start, WINDOW_NS / 1_000_000, "window width in ms");
        assert_eq!(row[2], "veth0");
        assert_eq!(row[3], "ingress", "direction is a closed set");

        let family = assert_cidr(&row[4], if row[4].contains(':') { 128 } else { 32 });
        let dst = parse_ip(&row[5], "dst_addr");
        assert_eq!(dst.is_ipv4(), family == FAMILY_IPV4, "dst_addr family");
        parse_u16(&row[6], "dst_port");
        let protocol = parse_u8(&row[7], "protocol");
        assert!(protocol == 1 || protocol == 6 || protocol == 17);

        assert!(
            row[8] == "pass" || row[8] == "block",
            "action must be closed, got {:?}",
            row[8]
        );
        parse_u64(&row[9], "rule_id");
        parse_u64(&row[10], "count");
        parse_u64(&row[11], "first_seen_ms");
        parse_u64(&row[12], "last_seen_ms");
        for (index, column) in [
            (13, "sample_dropped"),
            (14, "degraded"),
            (15, "window_complete"),
        ] {
            assert!(
                row[index] == "0" || row[index] == "1",
                "{column} must be 0 or 1, got {:?}",
                row[index]
            );
        }
        assert_eq!(row[15], "1", "closed windows are complete");
    }

    assert_eq!(rows[1][4], "10.99.0.0/24");
    assert_eq!(rows[2][4], "fd00:99::/64");

    // Appending to the aggregate CSV keeps exactly one header as well.
    let mut appended =
        AggCsvSink::create(&path, "veth0", true, config, clock).expect("append aggregate CSV");
    appended
        .write_rows(&rows_in[..1])
        .expect("append aggregate row");
    appended.flush().expect("flush appended aggregate row");
    drop(appended);

    let rows = strict_rows(&path, AGG_COLUMNS);
    assert_header_once(&rows, AggCsvSink::HEADER);
    assert_eq!(rows.len(), 4);
}

/// C-SCHEMA-03: concurrent `--append` writers share one header and every row
/// stays intact (one 7-column line each, counted `writers * rows`).
#[test]
fn c_schema_03_concurrent_append_writers_share_one_header() {
    const WRITERS: usize = 8;
    const ROWS: usize = 50;

    let dir = TempDir::new("append-race");
    let path = dir.path("packets.csv");
    // The file does not exist yet: every writer races to create it, take the
    // header lock and decide whether a header is needed.  Exactly one wins.
    assert!(!path.exists());

    let barrier = Arc::new(Barrier::new(WRITERS));
    let mut handles = Vec::with_capacity(WRITERS);

    for writer in 0..WRITERS {
        let barrier = Arc::clone(&barrier);
        let path = path.clone();
        handles.push(std::thread::spawn(move || {
            // Line every thread up first, so the eight `create` calls (open +
            // flock + length check) overlap instead of running one by one.
            barrier.wait();
            let mut sink = CsvSink::create(&path, "veth0", true).expect("create appending sink");
            let events: Vec<Event> = (0..ROWS)
                .map(|row| {
                    let octet = (writer * ROWS + row) as u8;
                    sample_event(
                        1_700_000_000_000 + octet as u64,
                        "10.1.0.1",
                        "10.1.0.2",
                        0,
                        80,
                        octet.wrapping_add(1),
                    )
                })
                .collect();
            sink.write_batch(&events).expect("concurrent batch");
            sink.flush().expect("concurrent flush");
        }));
    }

    for handle in handles {
        handle.join().expect("writer thread panicked");
    }

    let rows = strict_rows(&path, SAMPLE_COLUMNS);
    assert_header_once(&rows, CsvSink::HEADER);
    assert_eq!(
        rows.len(),
        1 + WRITERS * ROWS,
        "one header plus every concurrent row"
    );

    // Every data row is a full, typed record: no torn writes.
    for row in &rows[1..] {
        parse_u64(&row[0], "ts_unix_ms");
        parse_ip(&row[2], "src_ip");
        parse_ip(&row[3], "dst_ip");
        parse_u16(&row[4], "src_port");
        parse_u16(&row[5], "dst_port");
        parse_u8(&row[6], "protocol");
    }
}
