//! `probe-agent`: the production wiring of the control agent (S-C).
//!
//! One process, two halves that must not block each other: the data path
//! ([`probe::driver::Probe`] behind [`agent::driver::AttachedProbe`]) and the
//! control loop ([`agent::agent_loop::AgentLoop`]).  The loop owns the data
//! path, so attaching, applying and draining happen on one task; the only thing
//! the loop awaits is the network.
//!
//! Startup order matters and is the contract's (§8):
//!
//! 1. parse and validate the command line, read the PEM files;
//! 2. attach the data path with the command line's *bootstrap* configuration —
//!    it is not a control-plane configuration, so no ack is sent for it and
//!    `applied_version` stays 0;
//! 3. build the report sink (CSV or the control-plane queue) and the loop;
//! 4. run until SIGINT/SIGTERM: stop pulling configuration, flush the open
//!    windows, send the final bounded report;
//! 5. flush again (idempotent), print the summary, detach, exit 0.
//!
//! A control plane that is unreachable is *not* a reason to stop the data path:
//! the probe keeps forwarding and counting, and the loop retries with backoff
//! (fail-open, see `probe/README.md`'s temporary rule).

use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
    process::ExitCode,
    time::{SystemTime, UNIX_EPOCH},
};

use agent::{
    agent_loop::{AgentConfig, AgentLoop},
    client::tonic::{ClientConfig, TonicClient},
    clock::ClockOffset,
    driver::{AttachedProbe, DataPlane},
    report::{ReportQueue, ReportSink},
};
use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use probe::{
    aggregate::{Clock, NANOS_PER_MS},
    driver::{ProbeOptions, wall_clock},
    sink::{AggSink, NullSink, Sink, SinkKind},
};
use probe_common::{ACTION_PASS, CONFIG_VERSION, ProbeConfig};
use sha2::{Digest, Sha256};

/// Accepted `--window-ms` range, the same one [`agent::config`] enforces for a
/// control-plane document: a bootstrap configuration outside it would be
/// replaced by the first document anyway, so rejecting it early is honest.
const MIN_WINDOW_MS: u64 = 1_000;
/// Upper bound of `--window-ms`, shared with [`agent::config`].
const MAX_WINDOW_MS: u64 = 60_000;

/// Source prefix the kernel masks with when the control plane has not spoken.
/// Train 0 keeps the CLI's /24 and /64 (see `probe --src-prefix-v4/-v6`).
const BOOTSTRAP_IPV4_PREFIX_LEN: u8 = 24;
/// IPv6 counterpart of [`BOOTSTRAP_IPV4_PREFIX_LEN`].
const BOOTSTRAP_IPV6_PREFIX_LEN: u8 = 64;

/// File the binary writes after a graceful stop, next to `--state-dir`.
const APPLIED_FILE: &str = "applied.json";

/// Command line.  Every flag also reads `PROBE_AGENT_<FLAG>` so a systemd unit
/// can keep secrets out of `ps`.
#[derive(Debug, Parser)]
#[command(
    name = "probe-agent",
    version,
    about = "Attaches the XDP probe and keeps it configured from a control plane"
)]
struct Opt {
    /// Network interface to attach the XDP program to.
    #[arg(long, env = "PROBE_AGENT_IFACE", value_name = "IFACE")]
    iface: String,

    /// Control-plane gRPC endpoint.
    #[arg(
        long,
        env = "PROBE_AGENT_CONTROL_ENDPOINT",
        default_value = "https://127.0.0.1:9090",
        value_name = "URL"
    )]
    control_endpoint: String,

    /// CA bundle (PEM) that signed the control plane's certificate.
    #[arg(long, env = "PROBE_AGENT_CA", value_name = "FILE")]
    ca: PathBuf,

    /// Client certificate (PEM); its CN is the agent id the control plane sees.
    #[arg(long, env = "PROBE_AGENT_CERT", value_name = "FILE")]
    cert: PathBuf,

    /// Private key (PEM) matching `--cert`.
    #[arg(long, env = "PROBE_AGENT_KEY", value_name = "FILE")]
    key: PathBuf,

    /// Stable machine identity; defaults to `/etc/machine-id`, then a hash of
    /// the hostname.
    #[arg(long, env = "PROBE_AGENT_MACHINE_ID", value_name = "ID")]
    machine_id: Option<String>,

    /// Directory holding `applied.json` (created on the first successful stop).
    #[arg(
        long,
        env = "PROBE_AGENT_STATE_DIR",
        default_value = "/var/lib/probe-agent",
        value_name = "DIR"
    )]
    state_dir: PathBuf,

    /// Write samples and aggregates as CSV into this directory instead of
    /// discarding the samples and reporting only aggregate windows.
    #[arg(long, env = "PROBE_AGENT_CSV_DIR", value_name = "DIR")]
    csv_dir: Option<PathBuf>,

    /// Bootstrap window width in milliseconds; the first control-plane document
    /// replaces it.
    #[arg(
        long,
        env = "PROBE_AGENT_WINDOW_MS",
        default_value_t = 10_000,
        value_name = "MS"
    )]
    window_ms: u64,

    /// Bootstrap grace period in milliseconds.
    #[arg(
        long,
        env = "PROBE_AGENT_GRACE_MS",
        default_value_t = 2_000,
        value_name = "MS"
    )]
    grace_ms: u64,

    /// How often the ring buffer is drained, in milliseconds.
    #[arg(
        long,
        env = "PROBE_AGENT_FLUSH_MS",
        default_value_t = 500,
        value_name = "MS"
    )]
    flush_ms: u64,

    /// How often the aggregate map is scanned, in milliseconds.
    #[arg(
        long,
        env = "PROBE_AGENT_SCAN_MS",
        default_value_t = 1_000,
        value_name = "MS"
    )]
    scan_ms: u64,
}

/// The validated command line: paths turned into bytes, defaults resolved.
struct Settings {
    iface: String,
    endpoint: String,
    ca_pem: Vec<u8>,
    cert_pem: Vec<u8>,
    key_pem: Vec<u8>,
    machine_id: String,
    state_dir: PathBuf,
    csv_dir: Option<PathBuf>,
    window_ns: u64,
    grace_ns: u64,
    flush_ms: u64,
    scan_ms: u64,
}

impl Opt {
    /// Turns flags into [`Settings`], naming every rejected value.
    ///
    /// All checks run *before* the first privileged operation, so a typo never
    /// leaves an XDP program attached.
    fn validate(self) -> Result<Settings> {
        if !(MIN_WINDOW_MS..=MAX_WINDOW_MS).contains(&self.window_ms) {
            bail!(
                "--window-ms must be between {MIN_WINDOW_MS} and {MAX_WINDOW_MS}, got {}",
                self.window_ms
            );
        }
        if self.grace_ms > self.window_ms {
            bail!(
                "--grace-ms ({}) must not exceed --window-ms ({})",
                self.grace_ms,
                self.window_ms
            );
        }
        if self.flush_ms == 0 {
            bail!("--flush-ms must be greater than zero");
        }
        if self.scan_ms == 0 {
            bail!("--scan-ms must be greater than zero");
        }
        if self.iface.trim().is_empty() {
            bail!("--iface must not be empty");
        }

        // Reading the PEM files here keeps transport errors out of the client:
        // a missing certificate must fail with its path, before Register.
        let ca_pem = read_pem(&self.ca, "CA bundle")?;
        let cert_pem = read_pem(&self.cert, "client certificate")?;
        let key_pem = read_pem(&self.key, "client private key")?;

        Ok(Settings {
            iface: self.iface,
            endpoint: self.control_endpoint,
            ca_pem,
            cert_pem,
            key_pem,
            machine_id: resolve_machine_id(self.machine_id)?,
            state_dir: self.state_dir,
            csv_dir: self.csv_dir,
            window_ns: self.window_ms * NANOS_PER_MS,
            grace_ns: self.grace_ms * NANOS_PER_MS,
            flush_ms: self.flush_ms,
            scan_ms: self.scan_ms,
        })
    }
}

impl Settings {
    /// Options for [`probe::driver::Probe`]; the CSV paths are only dereferenced
    /// when the CSV sinks are actually built.
    fn probe_options(&self, clock: Clock) -> ProbeOptions {
        let (out, agg_out) = match &self.csv_dir {
            Some(dir) => (dir.join("packets.csv"), dir.join("aggregates.csv")),
            // Unused by `NullSink`/`ReportSink`; left empty on purpose so a
            // future bug cannot silently write outside `--csv-dir`.
            None => (PathBuf::new(), PathBuf::new()),
        };
        ProbeOptions {
            iface: self.iface.clone(),
            sink: SinkKind::Csv,
            out,
            agg_out,
            append: true,
            flush_ms: self.flush_ms,
            scan_ms: self.scan_ms,
            clock: Some(clock),
        }
    }

    /// The bootstrap configuration for the kernel.
    ///
    /// This is *not* a control-plane document: nothing acks it and
    /// `applied_version` stays 0.  It exists so the probe forwards traffic while
    /// the loop waits for its first `GetConfig` — dropping packets because the
    /// control plane is down would be a fail-closed behaviour Train 0 does not
    /// have.
    fn bootstrap_config(&self) -> ProbeConfig {
        ProbeConfig {
            version: CONFIG_VERSION,
            window_ns: self.window_ns,
            grace_ns: self.grace_ns,
            ipv4_prefix_len: BOOTSTRAP_IPV4_PREFIX_LEN,
            ipv6_prefix_len: BOOTSTRAP_IPV6_PREFIX_LEN,
            default_action: ACTION_PASS,
            pad: [0; 5],
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    // Parse first so `--help` and `--version` exit 0 (and a bad flag exits 2)
    // without touching the logger or any privileged subsystem.
    let opt = Opt::parse();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    match run(opt).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("probe-agent: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(opt: Opt) -> Result<()> {
    let settings = opt.validate()?;
    let config = settings.bootstrap_config();

    // One wall clock for both the CSV sinks and the session: the sample and
    // aggregate files must agree on what "now" was.
    let clock = wall_clock()?;
    // The report sink speaks the control plane's timestamps (unix nanos).
    let report_clock = ClockOffset::sample().context("sample the clock offset for reports")?;

    let options = settings.probe_options(clock);
    let (sink, agg_sink, reports) = build_sinks(&settings, &options, &report_clock, clock)?;

    log::info!(
        "attaching the XDP program to '{}' with the bootstrap configuration \
         (window_ns={} grace_ns={}); the control plane replaces it on the first GetConfig",
        settings.iface,
        settings.window_ns,
        settings.grace_ns
    );
    let attached = AttachedProbe::attach(&options, config, sink, agg_sink).map_err(|failure| {
        anyhow!(
            "attach the data path failed ({}): {}",
            failure.code.as_str(),
            failure.detail
        )
    })?;

    // The previous run's record is informative only: seeding the version from
    // it would let a stale configuration skip the first `GetConfig` (and leave
    // the bootstrap configuration installed while claiming otherwise).
    log_previous_applied(&settings.state_dir);

    let client = TonicClient::connect(&ClientConfig::new(
        settings.endpoint.clone(),
        settings.ca_pem.clone(),
        settings.cert_pem.clone(),
        settings.key_pem.clone(),
    ))?;
    let agent_config = AgentConfig::new(settings.machine_id.clone(), env!("CARGO_PKG_VERSION"));
    let mut agent = AgentLoop::new(client, attached, agent_config)
        .with_reports(reports)
        .with_clock(report_clock);

    let result = agent.run(shutdown_signal()).await;
    let applied_version = agent.applied_version();
    let applied_sha256 = agent.applied_sha256().to_vec();
    let mut attached = agent.into_apply();

    // Persist what the control plane last accepted, but only after a graceful
    // stop: a crashing process has no business claiming a version, and a
    // rejected document never reaches this point.
    if result.is_ok() && applied_version > 0 {
        match <[u8; 32]>::try_from(applied_sha256.as_slice()) {
            Ok(sha256) => {
                attached.record_applied(applied_version, sha256);
                if let Err(error) = write_applied(
                    &settings.state_dir,
                    applied_version,
                    attached.applied_sha256(),
                ) {
                    // A read-only state directory must not fail the agent: the
                    // data path is already flushed and reported.
                    log::warn!("cannot record the applied configuration: {error:#}");
                }
            }
            Err(_) => log::warn!(
                "the loop reported a {}-byte config digest; not writing {APPLIED_FILE}",
                applied_sha256.len()
            ),
        }
    } else if result.is_ok() {
        log::info!("no control-plane configuration was applied; {APPLIED_FILE} left untouched");
    }

    // `run` already flushed the open windows before its final report; this is
    // the idempotent second half, so the summary counts every row once.
    match attached.shutdown() {
        Ok(summary) => log::info!("{}", summary.stats_line()),
        Err(error) => log::warn!("cannot flush the data path on shutdown: {error:#}"),
    }
    attached.into_probe().detach();

    result.map_err(|error| anyhow!("agent loop stopped: {error}"))
}

/// The sample sink, the aggregate sink and the queue the loop drains.
type Sinks = (Box<dyn Sink + Send>, Box<dyn AggSink + Send>, ReportQueue);

/// Builds the sample and aggregate sinks plus the report queue the loop drains.
///
/// `--csv-dir` keeps the probe's CSV behaviour (7/16 columns, append mode) and
/// reports nothing; without it the samples are discarded and aggregate windows
/// become `ReportWindows` entries.
fn build_sinks(
    settings: &Settings,
    options: &ProbeOptions,
    report_clock: &ClockOffset,
    clock: Clock,
) -> Result<Sinks> {
    match &settings.csv_dir {
        Some(dir) => {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("create the CSV directory {}", dir.display()))?;
            let sink = options.create_sink()?;
            let agg_sink = options.create_agg_sink(settings.bootstrap_config(), clock)?;
            log::info!(
                "writing samples to {} and aggregates to {} (append=true)",
                options.out.display(),
                options.agg_out.display()
            );
            Ok((sink, agg_sink, ReportQueue::default()))
        }
        None => {
            let report = ReportSink::new(*report_clock, settings.window_ns);
            let queue = report.queue();
            log::info!(
                "discarding samples and reporting aggregate windows to '{}'",
                settings.endpoint
            );
            Ok((Box::new(NullSink), Box::new(report), queue))
        }
    }
}

/// Waits for SIGINT or SIGTERM and logs which one arrived.
///
/// A failed handler installation must not stop the agent: falling back to
/// SIGINT keeps the graceful path available, and the failure is logged.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => log::info!("received SIGINT, stopping"),
                    _ = sigterm.recv() => log::info!("received SIGTERM, stopping"),
                }
            }
            Err(error) => {
                log::warn!(
                    "cannot install the SIGTERM handler ({error}); only SIGINT stops this agent"
                );
                let _ = tokio::signal::ctrl_c().await;
                log::info!("received SIGINT, stopping");
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        log::info!("received a shutdown signal, stopping");
    }
}

/// Reads a PEM file, naming the file and its role when that fails.
fn read_pem(path: &Path, role: &str) -> Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("read the {role} from {}", path.display()))
}

/// `--machine-id`, else `/etc/machine-id`, else a hash of the hostname.
fn resolve_machine_id(explicit: Option<String>) -> Result<String> {
    if let Some(id) = explicit {
        let id = id.trim().to_owned();
        if id.is_empty() {
            bail!("--machine-id must not be empty");
        }
        if id.len() > 128 {
            bail!("--machine-id must be at most 128 characters");
        }
        return Ok(id);
    }

    if let Ok(text) = std::fs::read_to_string("/etc/machine-id") {
        let id = text.trim();
        if !id.is_empty() {
            return Ok(id.to_owned());
        }
    }

    // No machine-id (containers, mostly): hash the hostname so two hosts sharing
    // a hostname collide knowingly instead of the agent id being empty.
    let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|text| text.trim().to_owned())
        .ok()
        .filter(|name| !name.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "unknown-host".to_owned());
    let digest = Sha256::digest(hostname.as_bytes());
    Ok(format!("host-{}", hex(&digest[..8])))
}

/// Logs the last recorded applied configuration, if any.  Informative only.
fn log_previous_applied(state_dir: &Path) {
    let path = state_dir.join(APPLIED_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => log::info!("previous {}: {}", path.display(), text.trim()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            log::info!(
                "no {} yet; starting without an applied configuration",
                path.display()
            );
        }
        Err(error) => log::warn!("cannot read {}: {error}", path.display()),
    }
}

/// Atomically replaces `applied.json` with the version and digest to remember.
fn write_applied(state_dir: &Path, version: u64, sha256: [u8; 32]) -> Result<()> {
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("create the state directory {}", state_dir.display()))?;
    let path = state_dir.join(APPLIED_FILE);
    let temporary = state_dir.join(format!("{APPLIED_FILE}.tmp"));
    let document = serde_json::json!({
        "version": version,
        "sha256": hex(&sha256),
        "agent_version": env!("CARGO_PKG_VERSION"),
        "written_at_unix_s": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    });
    let bytes = serde_json::to_vec_pretty(&document)?;
    std::fs::write(&temporary, bytes).with_context(|| format!("write {}", temporary.display()))?;
    std::fs::rename(&temporary, &path)
        .with_context(|| format!("replace {} with the new state", path.display()))?;
    log::info!("recorded the applied configuration in {}", path.display());
    Ok(())
}

/// Lowercase hex, for digests in log lines and `applied.json`.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}
