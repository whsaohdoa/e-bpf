//! The deterministic frame matrix from `probe/tests/fixtures/frames`.
//!
//! `index.tsv` is the single source of truth: every `*.bin` row carries the
//! expectation for the data path (`accept` = the parser must emit a sample and
//! an aggregate key for it). The sender and the e2e assertions both read it, so
//! adding a fixture is a one-line table change.

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

/// One row of `index.tsv`, with the payload already loaded.
pub struct FrameSpec {
    /// File name inside the fixture directory, e.g. `ipv4_tcp.bin`.
    pub file: String,
    /// `4`, `6`, or `None` for frames the parser must reject before L3.
    pub family: Option<u8>,
    /// Source address as spelled in `index.tsv`; `None` for rejected frames.
    pub src: Option<String>,
    /// Destination address as spelled in `index.tsv`; `None` for rejected frames.
    pub dst: Option<String>,
    pub sport: u16,
    pub dport: u16,
    pub protocol: u8,
    /// `true` when the data path is expected to accept (parse + count) the frame.
    pub accept: bool,
    /// Raw Ethernet frame bytes.
    pub bytes: Vec<u8>,
}

impl fmt::Debug for FrameSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameSpec")
            .field("file", &self.file)
            .field("family", &self.family)
            .field("accept", &self.accept)
            .field("len", &self.bytes.len())
            .finish()
    }
}

/// All fixture rows, in `index.tsv` order.
pub struct FrameTable {
    pub dir: PathBuf,
    pub specs: Vec<FrameSpec>,
}

impl FrameTable {
    /// Loads and validates `index.tsv` plus every referenced `*.bin`.
    pub fn load(dir: impl AsRef<Path>) -> io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        let table_path = dir.join("index.tsv");
        let text = fs::read_to_string(&table_path).map_err(|err| {
            io::Error::new(err.kind(), format!("read {}: {err}", table_path.display()))
        })?;
        let mut specs = Vec::new();
        for (lineno, line) in text.lines().enumerate() {
            let line = line.trim_end_matches('\r');
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fields: Vec<&str> = line.split('\t').collect();
            if fields.len() != 8 {
                return Err(invalid(
                    &table_path,
                    lineno,
                    "expected 8 tab separated columns",
                ));
            }
            let opt = |value: &str| -> Option<String> {
                if value == "-" {
                    None
                } else {
                    Some(value.to_owned())
                }
            };
            let family = match fields[1] {
                "-" => None,
                "4" => Some(4),
                "6" => Some(6),
                other => {
                    return Err(invalid(
                        &table_path,
                        lineno,
                        &format!("family must be 4, 6 or -, got `{other}`"),
                    ));
                }
            };
            let sport = number::<u16>(&table_path, lineno, fields[4], "sport")?;
            let dport = number::<u16>(&table_path, lineno, fields[5], "dport")?;
            let protocol = number::<u8>(&table_path, lineno, fields[6], "proto")?;
            let accept = match fields[7] {
                "0" => false,
                "1" => true,
                other => {
                    return Err(invalid(
                        &table_path,
                        lineno,
                        &format!("accept must be 0 or 1, got `{other}`"),
                    ));
                }
            };
            let path = dir.join(fields[0]);
            let bytes = fs::read(&path).map_err(|err| {
                io::Error::new(err.kind(), format!("read {}: {err}", path.display()))
            })?;
            specs.push(FrameSpec {
                file: fields[0].to_owned(),
                family,
                src: opt(fields[2]),
                dst: opt(fields[3]),
                sport,
                dport,
                protocol,
                accept,
                bytes,
            });
        }
        if specs.is_empty() {
            return Err(invalid(&table_path, 0, "table has no rows"));
        }
        Ok(Self { dir, specs })
    }

    /// Rows the data path must parse and count.
    pub fn accepted(&self) -> impl Iterator<Item = &FrameSpec> {
        self.specs.iter().filter(|spec| spec.accept)
    }

    /// Rows the data path must drop without a sample (`malformed`).
    pub fn rejected(&self) -> impl Iterator<Item = &FrameSpec> {
        self.specs.iter().filter(|spec| !spec.accept)
    }
}

fn number<T: std::str::FromStr>(
    path: &Path,
    lineno: usize,
    raw: &str,
    name: &str,
) -> io::Result<T> {
    raw.parse::<T>()
        .map_err(|_| invalid(path, lineno, &format!("`{name}` is not a number: `{raw}`")))
}

fn invalid(path: &Path, lineno: usize, reason: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{}: line {}: {reason}", path.display(), lineno + 1),
    )
}
