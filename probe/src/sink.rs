//! Storage backends for captured packets.
//!
//! The eBPF side stops at `probe_common::PacketEvent`; everything that decides
//! *where* events go lives here.  Adding a database backend means adding a new
//! [`Sink`] implementation and a new arm in [`create_sink`] — no change to the
//! eBPF program or to the ring buffer ABI.
//!
//! CSV columns (fixed order):
//!
//! ```text
//! ts_unix_ms,iface,src_ip,dst_ip,src_port,dst_port,protocol
//! ```
//!
//! `src_port`/`dst_port` are emitted as empty fields in this revision (L4 is
//! not parsed yet); `protocol` is already the real IPv4 protocol number or the
//! IPv6 base-header Next Header.  Addresses are formatted with `Display`, so
//! IPv6 rows use the standard compressed form (`fd00:99::2`).  The columns are
//! present from day one so enabling L4 later does not change the CSV schema.

use std::{
    fs::{File, OpenOptions},
    io::{BufWriter, Write},
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::Path,
};

use anyhow::{Context as _, Result};
use clap::ValueEnum;
use probe_common::{EVENT_ABI_VERSION, FAMILY_IPV4, FAMILY_IPV6, PacketEvent};

/// A decoded event, independent of the ring buffer ABI.
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
    /// Converts one ring buffer record into a userspace event.
    ///
    /// Returns `None` when the record is not a v2 record with a known address
    /// family; the caller counts those as malformed instead of interpreting
    /// them with the wrong layout.  Addresses are stored as raw network-order
    /// bytes by the eBPF side, so no byte swapping happens here.
    pub fn from_abi(raw: &PacketEvent, ts_unix_ms: u64) -> Option<Self> {
        if raw.version != EVENT_ABI_VERSION {
            return None;
        }

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

/// Pluggable persistence layer.  Implementations must be usable from the async
/// main task (`Send`) and are allowed to buffer internally; `flush` is called
/// periodically and once more on shutdown.
pub trait Sink: Send {
    fn write_batch(&mut self, events: &[Event]) -> Result<()>;
    fn flush(&mut self) -> Result<()>;
}

/// Available `--sink` values.  A future `Db` variant only needs a new arm in
/// [`create_sink`]; the CLI and the eBPF side stay untouched.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum SinkKind {
    Csv,
}

/// Builds the sink selected on the command line.
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

/// CSV backend: one header row, then `ts_unix_ms,iface,src_ip,dst_ip` per event
/// (plus the reserved port columns and the protocol number).
pub struct CsvSink {
    writer: BufWriter<File>,
    iface: String,
}

impl CsvSink {
    pub const HEADER: &'static str =
        "ts_unix_ms,iface,src_ip,dst_ip,src_port,dst_port,protocol";

    /// Opens `path`.  Without `append` the file is truncated and the header is
    /// written; with `append` the header is only added when the file is empty,
    /// so successive runs accumulate rows instead of overwriting them.
    pub fn create(path: &Path, iface: &str, append: bool) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create output directory {}", parent.display()))?;
        }

        // Always append: two probes may write the same CSV concurrently (one per
        // veth end, as in verification/run_e2e_ipv6.sh).  Without O_APPEND a
        // truncating writer keeps its own file offset and would silently
        // overwrite rows appended by the other process.  `OpenOptions` rejects
        // `truncate + append`, so the non-append case truncates explicitly.
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .append(true)
            .open(path)
            .with_context(|| format!("open output file {}", path.display()))?;
        if !append {
            // Fresh run: empty the file once; writes then append from row 0.
            file.set_len(0)
                .with_context(|| format!("truncate output file {}", path.display()))?;
        }
        let write_header = file
            .metadata()
            .with_context(|| format!("stat output file {}", path.display()))?
            .len()
            == 0;

        let mut writer = BufWriter::new(file);
        if write_header {
            writeln!(writer, "{}", Self::HEADER).context("write CSV header")?;
            // Make the header visible immediately, even if no packet arrives.
            writer.flush().context("flush CSV header")?;
        }

        Ok(Self {
            writer,
            iface: iface.to_owned(),
        })
    }
}

impl Sink for CsvSink {
    fn write_batch(&mut self, events: &[Event]) -> Result<()> {
        for event in events {
            // Ports are printed as empty fields while they are 0 (L4 is not
            // parsed yet); once the eBPF side fills them in, the same writer
            // emits the numbers without a schema change.
            write!(
                self.writer,
                "{},{},{},{},",
                event.ts_unix_ms, self.iface, event.src_ip, event.dst_ip
            )
            .context("write CSV row")?;
            if event.src_port != 0 {
                write!(self.writer, "{}", event.src_port).context("write CSV row")?;
            }
            write!(self.writer, ",").context("write CSV row")?;
            if event.dst_port != 0 {
                write!(self.writer, "{}", event.dst_port).context("write CSV row")?;
            }
            writeln!(self.writer, ",{}", event.protocol).context("write CSV row")?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        self.writer.flush().context("flush CSV file")
    }
}

impl Drop for CsvSink {
    fn drop(&mut self) {
        // Best effort: the explicit flush on the normal exit path has already
        // run, but Drop covers early returns and panics.
        let _ = self.writer.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_event(family: u8, src: [u8; 16], dst: [u8; 16], protocol: u8) -> PacketEvent {
        PacketEvent {
            ts_ns: 1,
            src_addr: src,
            dst_addr: dst,
            src_port: 0,
            dst_port: 0,
            protocol,
            family,
            version: EVENT_ABI_VERSION,
            reserved: 0,
        }
    }

    fn ipv4_bytes(a: u8, b: u8, c: u8, d: u8) -> [u8; 16] {
        let mut addr = [0u8; 16];
        addr[..4].copy_from_slice(&[a, b, c, d]);
        addr
    }

    #[test]
    fn decodes_ipv4_from_left_aligned_bytes() {
        let raw = raw_event(
            FAMILY_IPV4,
            ipv4_bytes(10, 99, 0, 2),
            ipv4_bytes(10, 99, 0, 1),
            1,
        );
        let event = Event::from_abi(&raw, 1234).expect("valid v2 IPv4 record");
        assert_eq!(event.src_ip, IpAddr::V4(Ipv4Addr::new(10, 99, 0, 2)));
        assert_eq!(event.dst_ip, IpAddr::V4(Ipv4Addr::new(10, 99, 0, 1)));
        assert_eq!(event.protocol, 1);
        assert_eq!(event.ts_unix_ms, 1234);
        // CSV uses Display: dotted quad for IPv4.
        assert_eq!(event.src_ip.to_string(), "10.99.0.2");
    }

    #[test]
    fn decodes_ipv6_from_full_16_bytes() {
        let src: Ipv6Addr = "fd00:99::2".parse().unwrap();
        let dst: Ipv6Addr = "fd00:99::1".parse().unwrap();
        let raw = raw_event(FAMILY_IPV6, src.octets(), dst.octets(), 58);
        let event = Event::from_abi(&raw, 1).expect("valid v2 IPv6 record");
        assert_eq!(event.src_ip, IpAddr::V6(src));
        assert_eq!(event.dst_ip, IpAddr::V6(dst));
        assert_eq!(event.protocol, 58);
        // CSV uses Display: RFC 5952 compressed form.
        assert_eq!(event.src_ip.to_string(), "fd00:99::2");
    }

    #[test]
    fn rejects_unknown_version_and_family() {
        let mut wrong_version = raw_event(FAMILY_IPV4, [0; 16], [0; 16], 1);
        wrong_version.version = EVENT_ABI_VERSION + 1;
        assert!(Event::from_abi(&wrong_version, 0).is_none());

        let wrong_family = raw_event(5, [0; 16], [0; 16], 1);
        assert!(Event::from_abi(&wrong_family, 0).is_none());
    }

    /// Regression test for two probes writing one CSV: one truncating instance
    /// plus one `--append` instance, as done by verification/run_e2e_ipv6.sh.
    /// The truncating writer must not clobber rows the appending writer already
    /// flushed (it used to seek back to its own offset).
    #[test]
    fn truncating_sink_does_not_clobber_appended_rows() {
        let path = std::env::temp_dir().join(format!(
            "probe-csv-append-{}-{}.csv",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);

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

            truncating.write_batch(std::slice::from_ref(&event)).unwrap();
            truncating.flush().unwrap();
            appending.write_batch(std::slice::from_ref(&event)).unwrap();
            appending.flush().unwrap();
            // Without O_APPEND on the truncating sink this write went back to
            // its own offset and overwrote the veth1 row.
            truncating.write_batch(std::slice::from_ref(&event)).unwrap();
            truncating.flush().unwrap();
        }

        let rows: Vec<String> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        let _ = std::fs::remove_file(&path);

        assert_eq!(rows.len(), 4, "header + 3 rows: {rows:?}");
        assert_eq!(
            rows.iter().filter(|r| r.starts_with("ts_unix_ms")).count(),
            1
        );
        assert_eq!(rows.iter().filter(|r| r.contains(",veth0,")).count(), 2);
        assert_eq!(rows.iter().filter(|r| r.contains(",veth1,")).count(), 1);
    }
}
