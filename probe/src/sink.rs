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
//! not parsed yet); `protocol` is already the real IPv4 protocol number.  The
//! columns are present from day one so enabling L4 later does not change the
//! CSV schema.

use std::{
    fs::{File, OpenOptions},
    io::{BufWriter, Write},
    net::Ipv4Addr,
    path::Path,
};

use anyhow::{Context as _, Result};
use clap::ValueEnum;

/// A decoded event, independent of the ring buffer ABI.
#[derive(Clone, Copy, Debug)]
pub struct Event {
    pub ts_unix_ms: u64,
    pub src_ip: Ipv4Addr,
    pub dst_ip: Ipv4Addr,
    pub src_port: u16,
    pub dst_port: u16,
    pub protocol: u8,
}

impl Event {
    /// Converts one ring buffer record into a userspace event.
    ///
    /// Addresses/ports are stored in network byte order by the eBPF side, so
    /// they are converted exactly once here (`from_be`).  Doing it in the
    /// kernel as well would reverse every address.
    pub fn from_abi(raw: &probe_common::PacketEvent, ts_unix_ms: u64) -> Self {
        Self {
            ts_unix_ms,
            src_ip: Ipv4Addr::from(u32::from_be(raw.src_ip)),
            dst_ip: Ipv4Addr::from(u32::from_be(raw.dst_ip)),
            src_port: u16::from_be(raw.src_port),
            dst_port: u16::from_be(raw.dst_port),
            protocol: raw.protocol,
        }
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

        let mut options = OpenOptions::new();
        options.create(true).write(true);
        if append {
            options.append(true);
        } else {
            options.truncate(true);
        }
        let file = options
            .open(path)
            .with_context(|| format!("open output file {}", path.display()))?;
        let write_header = if append {
            file.metadata()
                .with_context(|| format!("stat output file {}", path.display()))?
                .len()
                == 0
        } else {
            true
        };

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
