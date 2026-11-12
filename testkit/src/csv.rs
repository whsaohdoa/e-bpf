//! Reader for the two CSV artifacts plus typed rows.
//!
//! The probe flushes both sinks after every closed window, so a reader that
//! tolerates a half-written trailing line can be used while the child is still
//! running (that is how E-02 observes a closed window without sleeping).

use std::{
    collections::BTreeMap,
    fmt, fs, io,
    path::{Path, PathBuf},
};

/// Column oriented view over a CSV file.
pub struct CsvTable {
    pub path: PathBuf,
    pub header: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

impl CsvTable {
    /// Reads `path`; a missing file yields the `io::Error` from `fs::read`.
    pub fn read(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let text = fs::read_to_string(&path)?;
        Ok(Self::parse(path, &text))
    }

    /// Reads `path`, treating a missing file as an empty table.
    pub fn read_or_empty(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        match fs::read_to_string(&path) {
            Ok(text) => Ok(Self::parse(path, &text)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Self {
                path,
                header: Vec::new(),
                rows: Vec::new(),
            }),
            Err(err) => Err(err),
        }
    }

    fn parse(path: PathBuf, text: &str) -> Self {
        let mut lines = text.lines().filter(|line| !line.trim().is_empty());
        let header: Vec<String> = match lines.next() {
            Some(line) => split(line),
            None => {
                return Self {
                    path,
                    header: Vec::new(),
                    rows: Vec::new(),
                };
            }
        };
        let mut rows = Vec::new();
        for line in lines {
            let record = split(line);
            // A concurrently growing file may end in a partial row: only the
            // last row can be short, and a full row is always complete.
            if record.len() != header.len() {
                if rows.is_empty() && record.len() < header.len() {
                    break;
                }
                continue;
            }
            rows.push(record);
        }
        Self { path, header, rows }
    }

    /// Column index by header name.
    pub fn column(&self, name: &str) -> Option<usize> {
        self.header.iter().position(|column| column == name)
    }

    /// All aggregate rows, parsed into typed records.
    pub fn agg_rows(&self) -> io::Result<Vec<AggRow>> {
        self.rows
            .iter()
            .map(|record| AggRow::from_record(&self.header, record))
            .collect()
    }

    /// All sample rows, parsed into typed records.
    pub fn sample_rows(&self) -> io::Result<Vec<SampleRow>> {
        self.rows
            .iter()
            .map(|record| SampleRow::from_record(&self.header, record))
            .collect()
    }
}

fn split(line: &str) -> Vec<String> {
    line.trim_end_matches('\r')
        .split(',')
        .map(|field| field.to_owned())
        .collect()
}

fn field<'a>(header: &'a [String], record: &'a [String], name: &str) -> io::Result<&'a str> {
    let index = header
        .iter()
        .position(|column| column == name)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("CSV has no `{name}` column (header: {header:?})"),
            )
        })?;
    record.get(index).map(String::as_str).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("CSV row is missing column `{name}`"),
        )
    })
}

fn parse<T: std::str::FromStr>(header: &[String], record: &[String], name: &str) -> io::Result<T> {
    let raw = field(header, record, name)?;
    raw.parse::<T>().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("CSV column `{name}` is not a number: `{raw}`"),
        )
    })
}

fn parse_flag(header: &[String], record: &[String], name: &str) -> io::Result<bool> {
    match parse::<u8>(header, record, name)? {
        0 => Ok(false),
        1 => Ok(true),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("CSV column `{name}` must be 0 or 1, got {other}"),
        )),
    }
}

/// One row of the aggregate CSV (16 columns, see README「CSV 契约」).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AggRow {
    pub window_start_ms: u64,
    pub window_end_ms: u64,
    pub iface: String,
    pub direction: String,
    pub src_prefix: String,
    pub dst_addr: String,
    pub dst_port: u16,
    pub protocol: u8,
    pub action: String,
    pub rule_id: u32,
    pub count: u64,
    pub first_seen_ms: u64,
    pub last_seen_ms: u64,
    pub sample_dropped: bool,
    pub degraded: bool,
    pub window_complete: bool,
}

impl AggRow {
    pub fn from_record(header: &[String], record: &[String]) -> io::Result<Self> {
        Ok(Self {
            window_start_ms: parse(header, record, "window_start_ms")?,
            window_end_ms: parse(header, record, "window_end_ms")?,
            iface: field(header, record, "iface")?.to_owned(),
            direction: field(header, record, "direction")?.to_owned(),
            src_prefix: field(header, record, "src_prefix")?.to_owned(),
            dst_addr: field(header, record, "dst_addr")?.to_owned(),
            dst_port: parse(header, record, "dst_port")?,
            protocol: parse(header, record, "protocol")?,
            action: field(header, record, "action")?.to_owned(),
            rule_id: parse(header, record, "rule_id")?,
            count: parse(header, record, "count")?,
            first_seen_ms: parse(header, record, "first_seen_ms")?,
            last_seen_ms: parse(header, record, "last_seen_ms")?,
            sample_dropped: parse_flag(header, record, "sample_dropped")?,
            degraded: parse_flag(header, record, "degraded")?,
            window_complete: parse_flag(header, record, "window_complete")?,
        })
    }
}

/// One row of the sample CSV (7 columns).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SampleRow {
    pub ts_unix_ms: u64,
    pub iface: String,
    pub src_ip: String,
    pub dst_ip: String,
    pub src_port: u16,
    pub dst_port: u16,
    pub protocol: u8,
}

impl SampleRow {
    pub fn from_record(header: &[String], record: &[String]) -> io::Result<Self> {
        Ok(Self {
            ts_unix_ms: parse(header, record, "ts_unix_ms")?,
            iface: field(header, record, "iface")?.to_owned(),
            src_ip: field(header, record, "src_ip")?.to_owned(),
            dst_ip: field(header, record, "dst_ip")?.to_owned(),
            src_port: parse(header, record, "src_port")?,
            dst_port: parse(header, record, "dst_port")?,
            protocol: parse(header, record, "protocol")?,
        })
    }
}

/// The `stats: key=value ...` summary the probe prints on stdout.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub fields: BTreeMap<String, u64>,
}

impl Stats {
    /// Parses the summary line; `None` when the line is not a stats line.
    pub fn parse(line: &str) -> Option<Self> {
        let rest = line.trim().strip_prefix("stats:")?;
        let mut fields = BTreeMap::new();
        for chunk in rest.split_whitespace() {
            let (key, value) = chunk.split_once('=')?;
            fields.insert(key.to_owned(), value.parse::<u64>().ok()?);
        }
        Some(Self { fields })
    }

    /// Field value; panics with the available keys when the name is unknown, so
    /// a renamed counter shows up as a readable test failure.
    pub fn get(&self, key: &str) -> u64 {
        *self.fields.get(key).unwrap_or_else(|| {
            panic!(
                "stats field `{key}` is missing (present: {:?})",
                self.keys()
            )
        })
    }

    pub fn keys(&self) -> Vec<&str> {
        self.fields.keys().map(String::as_str).collect()
    }
}

impl fmt::Display for Stats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, (key, value)) in self.fields.iter().enumerate() {
            if index > 0 {
                f.write_str(" ")?;
            }
            write!(f, "{key}={value}")?;
        }
        Ok(())
    }
}
