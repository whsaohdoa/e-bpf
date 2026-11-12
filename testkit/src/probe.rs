//! Child process wrapper for the probe binary.
//!
//! Synchronisation is strictly event based: the probe announces readiness on
//! stderr (`attached XDP program to '<iface>'`), the sinks are flushed as soon
//! as a window closes (so a CSV row is the event for "window closed"), and the
//! `stats:` summary line on stdout is the event for "child finished". No test
//! needs a fixed sleep to guess where the child is.

use std::{
    ffi::c_int,
    io::{self, BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

use crate::{csv::Stats, workspace_root};

/// Signal numbers used by the e2e suite.
pub const SIGKILL: c_int = 9;
pub const SIGTERM: c_int = 15;

unsafe extern "C" {
    fn kill(pid: c_int, sig: c_int) -> c_int;
}

/// Everything the child printed, shared with two reader threads.
#[derive(Default)]
pub struct Logs {
    state: Mutex<LogState>,
    ready: Condvar,
}

#[derive(Default)]
struct LogState {
    lines: Vec<String>,
    /// Reader threads still running; 0 means both streams hit EOF.
    open_readers: usize,
}

impl Logs {
    fn push(&self, line: String) {
        let mut state = self.state.lock().expect("log mutex");
        state.lines.push(line);
        drop(state);
        self.ready.notify_all();
    }

    fn reader_done(&self) {
        let mut state = self.state.lock().expect("log mutex");
        state.open_readers = state.open_readers.saturating_sub(1);
        drop(state);
        self.ready.notify_all();
    }

    /// Snapshot of every line seen so far.
    pub fn lines(&self) -> Vec<String> {
        self.state.lock().expect("log mutex").lines.clone()
    }

    /// Snapshot joined with newlines, for assertion messages.
    pub fn text(&self) -> String {
        self.lines().join("\n")
    }

    /// Waits until `predicate` accepts the collected lines, or the timeout
    /// elapses. Returns the lines as they were when the predicate held.
    pub fn wait_until<F>(&self, timeout: Duration, predicate: F) -> Option<Vec<String>>
    where
        F: Fn(&[String]) -> bool,
    {
        let deadline = Instant::now() + timeout;
        let mut state = self.state.lock().expect("log mutex");
        loop {
            if predicate(&state.lines) {
                return Some(state.lines.clone());
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let (next, _) = self
                .ready
                .wait_timeout(state, deadline - now)
                .expect("log mutex");
            state = next;
        }
    }

    /// Waits for the attach announcement: `attached XDP program to '<iface>'`.
    pub fn wait_attached(&self, iface: &str, timeout: Duration) -> io::Result<()> {
        let marker = format!("attached XDP program to '{iface}'");
        let found = self.wait_until(timeout, |lines| {
            lines.iter().any(|line| line.contains(&marker))
        });
        if found.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "probe did not attach to `{iface}` within {timeout:?}; child log:\n{}",
                    self.text()
                ),
            ));
        }
        Ok(())
    }

    /// Waits for the final `stats:` line (only printed on a graceful exit).
    pub fn wait_stats(&self, timeout: Duration) -> Option<Stats> {
        self.wait_until(timeout, |lines| {
            lines.iter().any(|line| Stats::parse(line).is_some())
        })
        .and_then(|lines| {
            lines
                .iter()
                .filter_map(|line| Stats::parse(line))
                .next_back()
        })
    }
}

/// Fluent constructor for [`Probe`].
pub struct ProbeBuilder {
    bin: PathBuf,
    iface: String,
    out: PathBuf,
    agg_out: PathBuf,
    window_ms: u64,
    grace_ms: u64,
    scan_ms: u64,
    append: bool,
    extra: Vec<String>,
}

impl ProbeBuilder {
    /// Defaults mirror the CLI: 10 s window, 2 s grace, 1 s scan, truncating sink.
    pub fn new(
        bin: impl Into<PathBuf>,
        iface: &str,
        out: impl Into<PathBuf>,
        agg_out: impl Into<PathBuf>,
    ) -> Self {
        Self {
            bin: bin.into(),
            iface: iface.to_owned(),
            out: out.into(),
            agg_out: agg_out.into(),
            window_ms: 10_000,
            grace_ms: 2_000,
            scan_ms: 1_000,
            append: false,
            extra: Vec::new(),
        }
    }

    pub fn window_ms(mut self, window_ms: u64) -> Self {
        self.window_ms = window_ms;
        self
    }

    pub fn grace_ms(mut self, grace_ms: u64) -> Self {
        self.grace_ms = grace_ms;
        self
    }

    pub fn scan_ms(mut self, scan_ms: u64) -> Self {
        self.scan_ms = scan_ms;
        self
    }

    pub fn append(mut self, append: bool) -> Self {
        self.append = append;
        self
    }

    /// Extra raw CLI arguments (e.g. `--src-prefix-v4`).
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.extra.push(arg.into());
        self
    }

    /// Spawns the child and starts draining its stdout/stderr.
    pub fn spawn(self) -> io::Result<Probe> {
        let mut command = Command::new(&self.bin);
        command
            .arg("--iface")
            .arg(&self.iface)
            .arg("--out")
            .arg(&self.out)
            .arg("--agg-out")
            .arg(&self.agg_out)
            .arg("--window-ms")
            .arg(self.window_ms.to_string())
            .arg("--grace-ms")
            .arg(self.grace_ms.to_string())
            .arg("--scan-ms")
            .arg(self.scan_ms.to_string())
            .env(
                "RUST_LOG",
                std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_owned()),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if self.append {
            command.arg("--append");
        }
        for arg in &self.extra {
            command.arg(arg);
        }
        let mut child = command.spawn()?;
        let logs = Arc::new(Logs::default());
        logs.state.lock().expect("log mutex").open_readers = 2;
        if let Some(stdout) = child.stdout.take() {
            pump(stdout, Arc::clone(&logs));
        }
        if let Some(stderr) = child.stderr.take() {
            pump(stderr, Arc::clone(&logs));
        }
        Ok(Probe {
            child,
            logs,
            iface: self.iface,
            out: self.out,
            agg_out: self.agg_out,
            exited: None,
        })
    }
}

fn pump<R: io::Read + Send + 'static>(stream: R, logs: Arc<Logs>) {
    thread::spawn(move || {
        let reader = BufReader::new(stream);
        for line in reader.lines() {
            match line {
                Ok(line) => logs.push(line),
                Err(_) => break,
            }
        }
        logs.reader_done();
    });
}

/// A running probe child.
pub struct Probe {
    child: Child,
    logs: Arc<Logs>,
    iface: String,
    out: PathBuf,
    agg_out: PathBuf,
    exited: Option<ExitStatus>,
}

/// What the child left behind: exit status, parsed stats, full log.
pub struct Outcome {
    pub status: ExitStatus,
    pub stats: Stats,
    pub logs: Vec<String>,
}

impl Probe {
    pub fn logs(&self) -> &Arc<Logs> {
        &self.logs
    }

    pub fn out_path(&self) -> &Path {
        &self.out
    }

    pub fn agg_path(&self) -> &Path {
        &self.agg_out
    }

    /// Blocks until the attach marker for the configured interface shows up.
    pub fn wait_attached(&self, timeout: Duration) -> io::Result<()> {
        self.logs.wait_attached(&self.iface, timeout)
    }

    /// Sends `sig` to the child.
    pub fn signal(&self, sig: c_int) -> io::Result<()> {
        let pid = self.child.id() as c_int;
        // SAFETY: `pid` is the still running child owned by this struct.
        let rc = unsafe { kill(pid, sig) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn sigterm(&self) -> io::Result<()> {
        self.signal(SIGTERM)
    }

    /// Polls `waitpid` until the child exits or `timeout` elapses.
    pub fn wait_exit(&mut self, timeout: Duration) -> io::Result<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.exited {
                return Ok(status);
            }
            if let Some(status) = self.child.try_wait()? {
                self.exited = Some(status);
                return Ok(status);
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "probe did not exit within {timeout:?}; child log:\n{}",
                        self.logs.text()
                    ),
                ));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// SIGTERM + wait + stats, the graceful path used by every e2e case.
    pub fn finish(mut self, timeout: Duration) -> io::Result<Outcome> {
        if self.exited.is_none() {
            self.sigterm()?;
        }
        let status = self.wait_exit(timeout)?;
        let deadline = Instant::now() + timeout;
        let stats = self
            .logs
            .wait_until(
                deadline.saturating_duration_since(Instant::now()),
                |lines| lines.iter().any(|line| Stats::parse(line).is_some()),
            )
            .and_then(|lines| {
                lines
                    .iter()
                    .filter_map(|line| Stats::parse(line))
                    .next_back()
            })
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("probe printed no stats line:\n{}", self.logs.text()),
                )
            })?;
        Ok(Outcome {
            status,
            stats,
            logs: self.logs.lines(),
        })
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        if self.exited.is_none() {
            let _ = self.signal(SIGKILL);
            let _ = self.child.wait();
        }
    }
}

/// Locates the probe binary: `PROBE_BIN`, then `target/{debug,release}/probe`.
pub fn probe_bin() -> PathBuf {
    if let Some(path) = std::env::var_os("PROBE_BIN") {
        return PathBuf::from(path);
    }
    for profile in ["debug", "release"] {
        let candidate = workspace_root().join("target").join(profile).join("probe");
        if candidate.is_file() {
            return candidate;
        }
    }
    PathBuf::from("probe")
}
