//! CSV storage backends: one row per unique sample record and one row per
//! merged aggregate window key.
//!
//! Two independent files are produced:
//!
//! ```text
//! samples:    ts_unix_ms,iface,src_ip,dst_ip,src_port,dst_port,protocol
//! aggregates: window_start_ms,window_end_ms,iface,direction,src_prefix,dst_addr,
//!             dst_port,protocol,action,rule_id,count,first_seen_ms,last_seen_ms,
//!             sample_dropped,degraded,window_complete
//! ```
//!
//! Both schemas have a fixed column count.  Ports are always integers (0 for
//! ICMP/ICMPv6) so a parser can index by column number; aggregate counters and
//! flags are integers as well.  Addresses use `Display`, so IPv6 rows use the
//! standard compressed form (`fd00:99::2`) and IPv4 prefixes are CIDR
//! (`10.99.0.0/24`).
//!
//! The eBPF side stops at `probe_common`; everything that decides *where* rows
//! go lives here.  Adding a database backend means adding a new [`Sink`]/
//! [`AggSink`] implementation and a new arm in the `create_*` functions.
//!
//! Both writers always keep the file open in `O_APPEND` mode: two probes may
//! write the same CSV concurrently (one per veth end, as reproduced by e2e case
//! E-07 in `probe/tests/e2e.rs`).  Without `O_APPEND` a truncating writer
//! keeps its own file offset and would silently overwrite rows appended by the
//! other process.  `--append` only controls whether the file is emptied once on
//! startup; the header is (re)written whenever the file is empty.

use std::{
    fs::{File, OpenOptions},
    io::{BufWriter, Write},
    mem::size_of,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};
use clap::ValueEnum;
use probe_common::{FAMILY_IPV4, FAMILY_IPV6, ProbeConfig, SampleEvent};

use crate::aggregate::{
    AggRecord, Clock, format_action, format_direction, format_dst_addr, format_src_prefix,
};

/// A decoded sample, independent of the ring buffer ABI.
#[derive(Clone, Copy, Debug)]
pub struct Event {
    pub ts_unix_ms: u64,
    pub src_ip: IpAddr,
    pub dst_ip: IpAddr,
    pub src_port: u16,
    pub dst_port: u16,
    pub protocol: u8,
}

impl Event {
    /// Converts one `SampleEvent` into a userspace sample row.
    ///
    /// Returns `None` when the address family is neither IPv4 nor IPv6; the
    /// caller counts those records as malformed instead of interpreting the
    /// addresses with the wrong layout.  Addresses are stored as raw
    /// network-order bytes by the eBPF side, so no byte swapping happens here;
    /// the ports are big-endian on the wire, hence `from_be`.
    pub fn from_abi(raw: &SampleEvent, ts_unix_ms: u64) -> Option<Self> {
        let (src_ip, dst_ip) = match raw.family {
            FAMILY_IPV4 => (
                IpAddr::V4(Ipv4Addr::new(
                    raw.src_addr[0],
                    raw.src_addr[1],
                    raw.src_addr[2],
                    raw.src_addr[3],
                )),
                IpAddr::V4(Ipv4Addr::new(
                    raw.dst_addr[0],
                    raw.dst_addr[1],
                    raw.dst_addr[2],
                    raw.dst_addr[3],
                )),
            ),
            FAMILY_IPV6 => (
                IpAddr::V6(Ipv6Addr::from(raw.src_addr)),
                IpAddr::V6(Ipv6Addr::from(raw.dst_addr)),
            ),
            _ => return None,
        };

        Some(Self {
            ts_unix_ms,
            src_ip,
            dst_ip,
            src_port: u16::from_be(raw.src_port),
            dst_port: u16::from_be(raw.dst_port),
            protocol: raw.protocol,
        })
    }
}

/// Decodes exactly one ring-buffer record into the shared ABI struct.
///
/// The record length must be *exactly* `size_of::<SampleEvent>()`: a shorter
/// (e.g. the 48-byte record of the previous ABI) or longer record means loader
/// and object disagree and is rejected as malformed instead of being read with
/// the wrong layout.  `read_unaligned` is used because the ring-buffer consumer
/// position is only guaranteed to be 4-byte aligned, not 8-byte aligned.
pub fn decode_sample(record: &[u8]) -> Option<SampleEvent> {
    if record.len() != size_of::<SampleEvent>() {
        return None;
    }
    // SAFETY: the length check above proves the slice contains one complete
    // `SampleEvent`; `read_unaligned` does not require an aligned pointer and
    // the returned value is owned (no reference into the ring buffer).
    Some(unsafe { std::ptr::read_unaligned(record.as_ptr().cast::<SampleEvent>()) })
}

/// Pluggable persistence layer for sample rows.
pub trait Sink: Send {
    fn write_batch(&mut self, events: &[Event]) -> Result<()>;
    fn flush(&mut self) -> Result<()>;
}

/// Pluggable persistence layer for merged aggregate rows.
pub trait AggSink: Send {
    fn write_rows(&mut self, rows: &[AggRecord]) -> Result<()>;
    fn flush(&mut self) -> Result<()>;
}

/// Available `--sink` values.  A future `Db` variant only needs a new arm in
/// [`create_sink`]; the CLI and the eBPF side stay untouched.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum SinkKind {
    Csv,
}

/// Builds the sample sink selected on the command line.
pub fn create_sink(
    kind: SinkKind,
    path: &Path,
    iface: &str,
    append: bool,
) -> Result<Box<dyn Sink>> {
    match kind {
        SinkKind::Csv => Ok(Box::new(CsvSink::create(path, iface, append)?)),
    }
}

/// Builds the aggregate CSV sink.
///
/// The window bounds and `first_seen`/`last_seen` columns are rendered with the
/// same [`Clock`] as the sample `ts_unix_ms`, and the prefix lengths come from
/// the read-back `ProbeConfig`.
pub fn create_agg_sink(
    path: &Path,
    iface: &str,
    append: bool,
    config: ProbeConfig,
    clock: Clock,
) -> Result<Box<dyn AggSink>> {
    Ok(Box::new(AggCsvSink::create(
        path, iface, append, config, clock,
    )?))
}

/// Opens `path` for appending, creating parent directories as needed.
///
/// Without `append` the file is truncated once; afterwards the writer only ever
/// appends, so a concurrent writer cannot lose rows.
fn open_append(path: &Path, append: bool) -> Result<BufWriter<File>> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create output directory {}", parent.display()))?;
    }
    // `OpenOptions` rejects `truncate + append`, so the non-append case empties
    // the file explicitly and then appends from row 0.
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open output file {}", path.display()))?;
    if !append {
        file.set_len(0)
            .with_context(|| format!("truncate output file {}", path.display()))?;
    }
    Ok(BufWriter::new(file))
}

/// Advisory exclusive lock on the output file, held only while the header is
/// checked and written.
///
/// Two probes may share one CSV (`--append` on both ends of a veth pair, see
/// e2e case E-07).  Without the lock both open an empty file,
/// both see length 0 and both write a header, so the file starts with two header
/// rows and every row counter is off by one.
#[cfg(unix)]
struct FileLock(std::os::fd::RawFd);

#[cfg(unix)]
impl FileLock {
    fn exclusive(file: &File, path: &Path) -> Result<Self> {
        use std::os::fd::AsRawFd as _;

        let fd = file.as_raw_fd();
        // SAFETY: `fd` stays open for as long as the borrowed `File` lives, and
        // `flock` only changes this process's advisory lock on that description.
        if unsafe { libc::flock(fd, libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("lock output file {}", path.display()));
        }
        Ok(Self(fd))
    }
}

#[cfg(unix)]
impl Drop for FileLock {
    fn drop(&mut self) {
        // SAFETY: the guard cannot outlive its `File`, so the descriptor is open.
        unsafe { libc::flock(self.0, libc::LOCK_UN) };
    }
}

#[cfg(not(unix))]
struct FileLock;

#[cfg(not(unix))]
impl FileLock {
    fn exclusive(_file: &File, _path: &Path) -> Result<Self> {
        Ok(Self)
    }
}

/// Writes `header` when the file is still empty (fresh file or `--append` run
/// that found no previous rows).
fn write_header_if_empty(writer: &mut BufWriter<File>, path: &Path, header: &str) -> Result<()> {
    // The lock covers the check and the write: a second process starting at the
    // same moment either writes the header first or finds it already there.
    let _lock = FileLock::exclusive(writer.get_ref(), path)?;
    let empty = writer
        .get_ref()
        .metadata()
        .with_context(|| format!("stat output file {}", path.display()))?
        .len()
        == 0;
    if empty {
        writeln!(writer, "{header}").context("write CSV header")?;
        // Make the header visible immediately, even if no packet arrives.
        writer.flush().context("flush CSV header")?;
    }
    Ok(())
}

/// Sample CSV backend: one header row, then the fixed 7-column sample rows.
pub struct CsvSink {
    writer: BufWriter<File>,
    iface: String,
    /// Kept so write failures name the file the operator asked for; the
    /// `io::Error` alone cannot say which of the two CSVs broke.
    path: PathBuf,
}

impl CsvSink {
    pub const HEADER: &'static str = "ts_unix_ms,iface,src_ip,dst_ip,src_port,dst_port,protocol";

    pub fn create(path: &Path, iface: &str, append: bool) -> Result<Self> {
        let mut writer = open_append(path, append)?;
        write_header_if_empty(&mut writer, path, Self::HEADER)?;
        Ok(Self {
            writer,
            iface: iface.to_owned(),
            path: path.to_path_buf(),
        })
    }
}

impl Sink for CsvSink {
    fn write_batch(&mut self, events: &[Event]) -> Result<()> {
        let path = &self.path;
        for event in events {
            // Ports are always integers, so the row always has 7 columns;
            // ICMP/ICMPv6 rows simply carry 0 for both ports.
            writeln!(
                self.writer,
                "{},{},{},{},{},{},{}",
                event.ts_unix_ms,
                self.iface,
                event.src_ip,
                event.dst_ip,
                event.src_port,
                event.dst_port,
                event.protocol
            )
            .with_context(|| format!("write sample CSV row to {}", path.display()))?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        let path = &self.path;
        self.writer
            .flush()
            .with_context(|| format!("flush sample CSV file {}", path.display()))
    }
}

impl Drop for CsvSink {
    fn drop(&mut self) {
        // Best effort: the explicit flush on the normal exit path has already
        // run, but Drop covers early returns and panics.
        let _ = self.writer.flush();
    }
}

/// Aggregate CSV backend: one header row, then the fixed 16-column rows.
pub struct AggCsvSink {
    writer: BufWriter<File>,
    iface: String,
    config: ProbeConfig,
    clock: Clock,
    /// Same reason as [`CsvSink::path`]: failures must name the output file.
    path: PathBuf,
}

impl AggCsvSink {
    pub const HEADER: &'static str = "window_start_ms,window_end_ms,iface,direction,src_prefix,dst_addr,dst_port,protocol,action,rule_id,count,first_seen_ms,last_seen_ms,sample_dropped,degraded,window_complete";

    pub fn create(
        path: &Path,
        iface: &str,
        append: bool,
        config: ProbeConfig,
        clock: Clock,
    ) -> Result<Self> {
        let mut writer = open_append(path, append)?;
        write_header_if_empty(&mut writer, path, Self::HEADER)?;
        Ok(Self {
            writer,
            iface: iface.to_owned(),
            config,
            clock,
            path: path.to_path_buf(),
        })
    }
}

impl AggSink for AggCsvSink {
    fn write_rows(&mut self, rows: &[AggRecord]) -> Result<()> {
        let path = &self.path;
        for row in rows {
            let key = &row.key;
            writeln!(
                self.writer,
                "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
                self.clock
                    .window_start_ms(row.window_id, self.config.window_ns),
                self.clock
                    .window_end_ms(row.window_id, self.config.window_ns),
                self.iface,
                format_direction(key.direction),
                format_src_prefix(key, &self.config),
                format_dst_addr(key),
                // The ABI carries the L4 ports as they appear on the wire
                // (network byte order), so the CSV needs the `from_be` decode —
                // the same one `SampleEvent` gets above.
                u16::from_be(key.dst_port),
                key.protocol,
                format_action(key.action),
                key.rule_id,
                row.count,
                self.clock.unix_ms(row.first_seen),
                self.clock.unix_ms(row.last_seen),
                u8::from(row.sample_dropped),
                u8::from(row.degraded),
                u8::from(row.window_complete),
            )
            .with_context(|| format!("write aggregate CSV row to {}", path.display()))?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        let path = &self.path;
        self.writer
            .flush()
            .with_context(|| format!("flush aggregate CSV file {}", path.display()))
    }
}

impl Drop for AggCsvSink {
    fn drop(&mut self) {
        let _ = self.writer.flush();
    }
}

#[cfg(test)]
mod tests {
    use probe_common::{ACTION_PASS, AggKey, CONFIG_VERSION, DIRECTION_INGRESS, ProbeConfig};

    use super::*;
    use crate::aggregate::{AggTable, Clock};

    fn raw_event(family: u8, src: [u8; 16], dst: [u8; 16], protocol: u8) -> SampleEvent {
        SampleEvent {
            ts_ns: 1,
            window_id: 0,
            src_addr: src,
            dst_addr: dst,
            src_port: 0,
            dst_port: 0,
            protocol,
            family,
            action: ACTION_PASS,
            reason: 0,
            rule_id: 0,
            reserved: 0,
        }
    }

    fn ipv4_bytes(a: u8, b: u8, c: u8, d: u8) -> [u8; 16] {
        let mut addr = [0u8; 16];
        addr[..4].copy_from_slice(&[a, b, c, d]);
        addr
    }

    /// Serialises a `SampleEvent` to its 64-byte in-memory form, the way the
    /// ring buffer delivers it.
    fn abi_bytes(sample: &SampleEvent) -> Vec<u8> {
        // SAFETY: `SampleEvent` is `#[repr(C)]`, has no pointers and is fully
        // initialised, so its whole object representation is readable.
        unsafe {
            std::slice::from_raw_parts(
                (sample as *const SampleEvent).cast::<u8>(),
                size_of::<SampleEvent>(),
            )
            .to_vec()
        }
    }

    #[test]
    fn decodes_a_full_64_byte_ipv4_record() {
        let mut raw = raw_event(
            FAMILY_IPV4,
            ipv4_bytes(10, 99, 0, 2),
            ipv4_bytes(10, 99, 0, 1),
            17,
        );
        raw.src_port = 0x1234u16.to_be();
        raw.dst_port = 0x00ddu16.to_be();

        let record = abi_bytes(&raw);
        assert_eq!(record.len(), 64, "the current ABI record is 64 bytes");
        let decoded = decode_sample(&record).expect("64-byte record is a SampleEvent");
        assert_eq!(decoded.ts_ns, 1);

        let event = Event::from_abi(&decoded, 1234).expect("valid IPv4 record");
        assert_eq!(event.src_ip, IpAddr::V4(Ipv4Addr::new(10, 99, 0, 2)));
        assert_eq!(event.dst_ip, IpAddr::V4(Ipv4Addr::new(10, 99, 0, 1)));
        assert_eq!(event.src_port, 0x1234);
        assert_eq!(event.dst_port, 0x00dd);
        assert_eq!(event.protocol, 17);
        assert_eq!(event.ts_unix_ms, 1234);
        assert_eq!(event.src_ip.to_string(), "10.99.0.2");
    }

    #[test]
    fn decodes_ipv6_from_the_full_sixteen_bytes() {
        let src: Ipv6Addr = "fd00:99::2".parse().unwrap();
        let dst: Ipv6Addr = "fd00:99::1".parse().unwrap();
        let raw = raw_event(FAMILY_IPV6, src.octets(), dst.octets(), 58);
        let event = Event::from_abi(&raw, 1).expect("valid IPv6 record");
        assert_eq!(event.src_ip, IpAddr::V6(src));
        assert_eq!(event.dst_ip, IpAddr::V6(dst));
        assert_eq!(event.protocol, 58);
        assert_eq!(event.src_ip.to_string(), "fd00:99::2");
    }

    /// A leftover 48-byte record from the previous ABI must be rejected, not
    /// decoded with the current (64-byte) layout.
    #[test]
    fn rejects_the_old_48_byte_record_as_malformed() {
        let mut old_abi = vec![0u8; 48];
        // First eight bytes of the old record were `ts_ns`; they are non-zero
        // so a wrong-layout decode would produce a bogus timestamp.
        old_abi[..8].copy_from_slice(&u64::MAX.to_ne_bytes());

        assert!(
            decode_sample(&old_abi).is_none(),
            "48-byte record must not decode as SampleEvent"
        );

        // One byte too long is just as malformed.
        let mut too_long = abi_bytes(&raw_event(FAMILY_IPV4, [0; 16], [0; 16], 1));
        too_long.push(0);
        assert!(decode_sample(&too_long).is_none());

        // A truncated record as well.
        let mut truncated = abi_bytes(&raw_event(FAMILY_IPV4, [0; 16], [0; 16], 1));
        truncated.pop();
        assert!(decode_sample(&truncated).is_none());
    }

    #[test]
    fn rejects_unknown_address_family() {
        let wrong_family = raw_event(5, [0; 16], [0; 16], 1);
        assert!(
            Event::from_abi(&wrong_family, 0).is_none(),
            "family=5 must be malformed"
        );
        // Family 0 and 255 are equally unsupported.
        for family in [0u8, 255] {
            assert!(Event::from_abi(&raw_event(family, [0; 16], [0; 16], 1), 0).is_none());
        }
    }

    #[test]
    fn sample_sink_writes_the_fixed_seven_columns() {
        let path = temp_path("probe-samples");
        let event = Event {
            ts_unix_ms: 1_700_000_000_000,
            src_ip: IpAddr::V4(Ipv4Addr::new(10, 99, 0, 2)),
            dst_ip: IpAddr::V4(Ipv4Addr::new(10, 99, 0, 1)),
            src_port: 0,
            dst_port: 0,
            protocol: 1,
        };
        {
            let mut sink = CsvSink::create(&path, "veth0", false).unwrap();
            sink.write_batch(std::slice::from_ref(&event)).unwrap();
            sink.flush().unwrap();
        }
        let rows = read_lines(&path);
        remove(&path);
        assert_eq!(rows.len(), 2, "header + one row: {rows:?}");
        assert_eq!(
            rows[0],
            "ts_unix_ms,iface,src_ip,dst_ip,src_port,dst_port,protocol"
        );
        // Ports stay as integers even when they are 0 (ICMP).
        assert_eq!(rows[1], "1700000000000,veth0,10.99.0.2,10.99.0.1,0,0,1");
        assert_eq!(rows[1].split(',').count(), 7);
    }

    #[test]
    fn aggregate_sink_writes_the_fixed_sixteen_columns() {
        let path = temp_path("probe-aggregates");
        let config = ProbeConfig {
            version: CONFIG_VERSION,
            window_ns: 10_000_000_000,
            grace_ns: 2_000_000_000,
            ipv4_prefix_len: 24,
            ipv6_prefix_len: 64,
            default_action: ACTION_PASS,
            pad: [0; 5],
        };
        // Wall clock 1_000_000_000_000 ns at monotonic 0 → window 0 of 10s.
        let clock = Clock::new(1_000_000_000_000, 0);

        let mut table = AggTable::new();
        let mut key = AggKey {
            window_id: 0,
            rule_id: 7,
            direction: DIRECTION_INGRESS,
            family: FAMILY_IPV4,
            src_prefix: ipv4_bytes(10, 99, 0, 0),
            dst_addr: ipv4_bytes(10, 99, 0, 1),
            dst_port: 8080u16.to_be(),
            protocol: 6,
            action: ACTION_PASS,
            reason: 0,
            pad: [0; 5],
        };
        key.src_prefix = ipv4_bytes(10, 99, 0, 0);
        let mut slot = probe_common::AggValue {
            count: 123,
            first_seen: 1_000_000_000,
            last_seen: 9_000_000_000,
            sample_dropped: 1,
            degraded: 0,
            pad: [0; 6],
        };
        table.ingest_slot(key, &slot);
        slot.count = 0;
        table.ingest_slot(key, &slot); // count=0 slot must not disturb first/last

        let rows = table.take_closed(0);
        assert_eq!(rows.len(), 1);
        {
            let mut sink = AggCsvSink::create(&path, "veth0", false, config, clock).unwrap();
            sink.write_rows(&rows).unwrap();
            sink.flush().unwrap();
        }
        let lines = read_lines(&path);
        remove(&path);

        assert_eq!(lines.len(), 2, "header + one row: {lines:?}");
        assert_eq!(
            lines[0],
            "window_start_ms,window_end_ms,iface,direction,src_prefix,dst_addr,dst_port,protocol,action,rule_id,count,first_seen_ms,last_seen_ms,sample_dropped,degraded,window_complete"
        );
        let columns: Vec<&str> = lines[1].split(',').collect();
        assert_eq!(columns.len(), 16, "16 fixed columns: {columns:?}");
        assert_eq!(columns[0], "1000000", "window_start_ms");
        assert_eq!(columns[1], "1010000", "window_end_ms");
        assert_eq!(columns[2], "veth0");
        assert_eq!(columns[3], "ingress");
        assert_eq!(columns[4], "10.99.0.0/24");
        assert_eq!(columns[5], "10.99.0.1");
        assert_eq!(columns[6], "8080");
        assert_eq!(columns[7], "6");
        assert_eq!(columns[8], "pass");
        assert_eq!(columns[9], "7");
        assert_eq!(columns[10], "123", "count is summed across slots");
        assert_eq!(columns[11], "1001000", "first_seen_ms of the nonzero slot");
        assert_eq!(columns[12], "1009000", "last_seen_ms of the nonzero slot");
        assert_eq!(columns[13], "1", "sample_dropped");
        assert_eq!(columns[14], "0", "degraded");
        assert_eq!(columns[15], "1", "window_complete");
    }

    /// Regression test for two probes writing one CSV: one truncating instance
    /// plus one `--append` instance, as done by e2e case E-07.
    /// The truncating writer must not clobber rows the appending writer already
    /// flushed (it used to seek back to its own offset).
    #[test]
    fn truncating_sink_does_not_clobber_appended_rows() {
        let path = temp_path("probe-csv-append");
        let event = Event {
            ts_unix_ms: 1,
            src_ip: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            dst_ip: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)),
            src_port: 0,
            dst_port: 0,
            protocol: 1,
        };

        {
            let mut truncating = CsvSink::create(&path, "veth0", false).unwrap();
            let mut appending = CsvSink::create(&path, "veth1", true).unwrap();

            truncating
                .write_batch(std::slice::from_ref(&event))
                .unwrap();
            truncating.flush().unwrap();
            appending.write_batch(std::slice::from_ref(&event)).unwrap();
            appending.flush().unwrap();
            // Without O_APPEND on the truncating sink this write went back to
            // its own offset and overwrote the veth1 row.
            truncating
                .write_batch(std::slice::from_ref(&event))
                .unwrap();
            truncating.flush().unwrap();
        }

        let rows = read_lines(&path);
        remove(&path);
        assert_eq!(rows.len(), 4, "header + 3 rows: {rows:?}");
        assert_eq!(
            rows.iter().filter(|r| r.starts_with("ts_unix_ms")).count(),
            1
        );
        assert_eq!(rows.iter().filter(|r| r.contains(",veth0,")).count(), 2);
        assert_eq!(rows.iter().filter(|r| r.contains(",veth1,")).count(), 1);
    }

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "{name}-{}-{}.csv",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn read_lines(path: &Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn remove(path: &Path) {
        let _ = std::fs::remove_file(path);
    }
}
