//! The agent state machine: register, pull configuration, apply it, report
//! windows — in the contract's order (§8) and with the contract's retry rules
//! (§2.6).
//!
//! The loop owns no I/O of its own. It talks to a [`ControlClient`] and a
//! [`ConfigApply`], and gets its clock and its sleep from [`Sleeper`], so every
//! transition is testable without a network, a kernel or real time.
//!
//! Two deliberate properties:
//!
//! - **Fail-open.** When the agent is unconfigured or the control plane is
//!   unreachable, the data path keeps its current (or default) configuration and
//!   keeps forwarding; nothing in this loop can stop the probe from passing
//!   packets (§8.1, §10 L1 of the contract).
//! - **Single source of truth for validation.** Only [`crate::config::validate`]
//!   decides whether an envelope is usable; this module never re-implements a
//!   check. A second copy would drift from the control plane's expectations.

use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{
    backoff::{self, Backoff},
    client::{ClientError, ControlClient, Rpc},
    clock::{ClockOffset, Sleeper, TokioSleeper},
    config,
    driver::DataPlane,
    error::{AckErrorCode, ServerErrorCode},
    pb,
    report::ReportQueue,
};

/// Contract §2.6: the whole graceful shutdown has a 5s budget, of which the
/// final report may take 2s.
pub const SHUTDOWN_REPORT_DEADLINE: Duration = Duration::from_secs(2);

/// How often the loop drains the ring and advances the window watermark.
///
/// Independent of the heartbeat cadence: a 5s heartbeat with a 10ms window would
/// let the ring overflow, so the data path is pumped five times per second even
/// while the loop is waiting for the control plane.
const DATA_PLANE_INTERVAL: Duration = Duration::from_millis(200);

/// Last-resort heartbeat cadence, used only until the first `Register` response
/// arrives; §5.2 says the server always supplies both values.
const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
/// Last-resort idle cadence, see above.
const DEFAULT_IDLE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// §6.2: the only document schema this agent understands.
pub const SUPPORTED_SCHEMA: u32 = 1;

/// The data path as the state machine sees it.
///
/// Implemented by `probe`'s BPF owner in production (S-C) and by a recording
/// double in tests.
pub trait ConfigApply: Send {
    /// Writes the configuration and reads it back (§5.3: "写 CONFIG → 回读 →
    /// 只用回读值"), returning the value actually in effect.
    fn apply(
        &mut self,
        cfg: &probe_common::ProbeConfig,
    ) -> Result<probe_common::ProbeConfig, ApplyFailure>;

    /// Version currently in effect; seeds the first heartbeat.
    fn applied_version(&self) -> u64;

    /// `MISSING` / `APPLYING` / `APPLIED` / `REJECTED`, reported verbatim.
    fn config_state(&self) -> pb::ConfigState;
}

/// Why applying an otherwise valid configuration failed (§5.4 `FAILED`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyFailure {
    /// `bpf.write_config_failed` or `bpf.attach_failed`.
    pub code: AckErrorCode,
    /// Human-readable detail, ≤512 chars (§12.3).
    pub detail: String,
}

impl ApplyFailure {
    /// Truncates `detail` to the contract's 512-character limit.
    pub fn new(code: AckErrorCode, detail: impl Into<String>) -> Self {
        let mut detail: String = detail.into();
        if detail.chars().count() > 512 {
            detail = detail.chars().take(512).collect();
        }
        Self { code, detail }
    }

    /// The CONFIG map rejected the write.
    pub fn write_config_failed(detail: impl Into<String>) -> Self {
        Self::new(AckErrorCode::BpfWriteConfigFailed, detail)
    }

    /// The programs are attached but the new configuration did not take.
    pub fn attach_failed(detail: impl Into<String>) -> Self {
        Self::new(AckErrorCode::BpfAttachFailed, detail)
    }
}

/// Why [`AgentLoop::run`] stopped early.
///
/// Every variant means "exit non-zero"; a graceful shutdown returns `Ok(())`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentError {
    /// §12.2: the control plane says this agent must not continue (revoked,
    /// conflicting identity, bad credentials).
    Fatal {
        /// Contract error code.
        code: ServerErrorCode,
        /// Human-readable message.
        message: String,
    },
    /// A call failed in a way §12.1 forbids retrying (a bug or version skew).
    Rejected {
        /// The failing RPC.
        rpc: Rpc,
        /// Human-readable message.
        message: String,
    },
    /// A retryable call used up its attempts — only `AckConfig` has a limit.
    RetriesExhausted {
        /// The failing RPC.
        rpc: Rpc,
        /// How many attempts were made.
        attempts: u32,
        /// Human-readable message.
        message: String,
    },
    /// Internal control-flow signal: shutdown arrived while the loop was
    /// waiting for a retry. [`AgentLoop::run`] turns it into `Ok(())`.
    Shutdown,
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fatal { code, message } => write!(f, "fatal {code}: {message}"),
            Self::Rejected { rpc, message } => {
                write!(
                    f,
                    "{} failed and must not be retried: {message}",
                    rpc.name()
                )
            }
            Self::RetriesExhausted {
                rpc,
                attempts,
                message,
            } => write!(
                f,
                "{} gave up after {attempts} attempt(s): {message}",
                rpc.name()
            ),
            Self::Shutdown => write!(f, "shutdown requested"),
        }
    }
}

impl std::error::Error for AgentError {}

/// Everything the loop needs that is not a trait object.
#[derive(Clone, Debug)]
pub struct AgentConfig {
    /// Stable host identity (§5.1, 1..128 chars).
    pub machine_id: String,
    /// Semver of this binary.
    pub agent_version: String,
    /// Random per process, for log correlation only (§5.1).
    pub boot_id: String,
    /// What this agent can do (§6.3 trimming input).
    pub capabilities: pb::Capabilities,
    /// Document schemas this build accepts (§5.3 step 2).
    pub supported_schemas: Vec<u32>,
    /// Graceful-shutdown budget for the final partial report (§2.6).
    pub shutdown_report_deadline: Duration,
}

impl AgentConfig {
    /// A Train 0 configuration: schema 1, default action PASS, no rule
    /// execution, a random `boot_id`.
    pub fn new(machine_id: impl Into<String>, agent_version: impl Into<String>) -> Self {
        Self {
            machine_id: machine_id.into(),
            agent_version: agent_version.into(),
            boot_id: random_boot_id(),
            capabilities: pb::Capabilities {
                config_schema: vec![SUPPORTED_SCHEMA],
                default_action: vec![pb::DefaultAction::Pass as i32],
                rules_execution: false,
            },
            supported_schemas: vec![SUPPORTED_SCHEMA],
            shutdown_report_deadline: SHUTDOWN_REPORT_DEADLINE,
        }
    }
}

/// §5.1: `boot_id` is random per process and never part of an id.
fn random_boot_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let stack = 0u8;
    format!(
        "{:x}-{:x}-{:x}",
        std::process::id(),
        nanos,
        &stack as *const u8 as usize
    )
}

/// The state machine.
///
/// Generic over the client and the applier so tests can inject either without a
/// trait object; the reporter side comes in as a [`ReportQueue`] shared with the
/// [`crate::report::ReportSink`] the probe writes to.
///
/// The applier is also the data plane ([`DataPlane`]): it owns the eBPF object,
/// so the loop is the only component that can drain the ring or apply a
/// configuration.  That is deliberate — a second owner would race with `apply`.
pub struct AgentLoop<C: ControlClient, A: ConfigApply + DataPlane> {
    client: C,
    apply: A,
    cfg: AgentConfig,
    reports: ReportQueue,
    sleeper: Arc<dyn Sleeper>,
    /// Whether retry delays get the §2.6 jitter. Production keeps it on;
    /// tests turn it off so they can assert the table's milliseconds.
    backoff_jitter: bool,
    clock: Option<ClockOffset>,
    heartbeat_interval: Duration,
    idle_heartbeat_interval: Duration,
    agent_id: String,
    applied_version: u64,
    applied_sha256: Vec<u8>,
    desired_version: u64,
    desired_sha256: Vec<u8>,
    /// Last window the control plane confirmed (§5.6); windows at or below it
    /// are never resent.
    last_window_id: u64,
    watch: Option<crate::client::WatchStream>,
    /// Cleared when the server answers `UNIMPLEMENTED` (§5.7).
    watch_supported: bool,
    watch_backoff: Backoff,
    watch_retry_at: tokio::time::Instant,
}

impl<C: ControlClient, A: ConfigApply + DataPlane> AgentLoop<C, A> {
    /// Builds a loop; it does nothing until [`Self::run`] is awaited.
    pub fn new(client: C, apply: A, cfg: AgentConfig) -> Self {
        let applied_version = apply.applied_version();
        Self {
            client,
            apply,
            cfg,
            reports: ReportQueue::default(),
            sleeper: Arc::new(TokioSleeper),
            backoff_jitter: true,
            clock: None,
            heartbeat_interval: DEFAULT_HEARTBEAT_INTERVAL,
            idle_heartbeat_interval: DEFAULT_IDLE_HEARTBEAT_INTERVAL,
            agent_id: String::new(),
            applied_version,
            applied_sha256: Vec::new(),
            desired_version: applied_version,
            desired_sha256: Vec::new(),
            last_window_id: 0,
            watch: None,
            watch_supported: true,
            watch_backoff: Backoff::new(
                backoff::Schedule::for_rpc(Rpc::Watch).expect("Watch has a schedule"),
            ),
            watch_retry_at: tokio::time::Instant::now(),
        }
    }

    /// Shares the queue written by [`crate::report::ReportSink`].
    ///
    /// Without this the loop would queue nothing and never report.
    pub fn with_reports(mut self, reports: ReportQueue) -> Self {
        self.reports = reports;
        self
    }

    /// Replaces the sleep implementation (tests use [`crate::clock::ManualSleeper`]).
    pub fn with_sleeper(mut self, sleeper: Arc<dyn Sleeper>) -> Self {
        self.sleeper = sleeper;
        self
    }

    /// Drops the retry jitter so the delays are exactly §2.6's table.
    /// Only for tests; production keeps the jitter on so a fleet that loses
    /// the control plane does not retry in lockstep.
    pub fn with_deterministic_backoff(mut self) -> Self {
        self.backoff_jitter = false;
        self
    }

    /// Supplies the monotonic→wall-clock mapping used for timestamps.
    pub fn with_clock(mut self, clock: ClockOffset) -> Self {
        self.clock = Some(clock);
        self
    }

    /// Agent id assigned by `Register` (empty before the first one succeeds).
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// Highest window the control plane has confirmed.
    pub fn last_window_id(&self) -> u64 {
        self.last_window_id
    }

    /// Runs until `shutdown` resolves.
    ///
    /// `Err` means "exit non-zero"; on a graceful shutdown the loop sends one
    /// final partial report (bounded by [`Self::shutdown_report_deadline`]) and
    /// returns `Ok(())`.
    pub async fn run(&mut self, shutdown: impl Future<Output = ()>) -> Result<(), AgentError> {
        let mut shutdown = std::pin::pin!(shutdown);
        let result = self.steps(&mut shutdown).await;
        match result {
            Ok(()) | Err(AgentError::Shutdown) => {
                // A partial window is still useful: write it before the final
                // report so the control plane sees the packets counted so far.
                // ReportSink only queues in memory, so this cannot hang, and a
                // failure here must not turn a clean exit into a non-zero one.
                if let Err(error) = self.apply.flush_pending() {
                    log::warn!("cannot flush the open windows on shutdown: {error:#}");
                }
                self.final_report().await;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// Drains the ring and advances the window watermark.
    ///
    /// Called on every loop iteration and on [`DATA_PLANE_INTERVAL`].  A failure
    /// is logged and otherwise ignored: the data path is fail-open, and a full
    /// ring only means samples are dropped by the kernel, never that the agent
    /// stops reporting.
    fn pump(&mut self) {
        self.apply.drain();
        match probe::ports::mono_now_ns() {
            Ok(now_ns) => {
                if let Err(error) = self.apply.tick(now_ns) {
                    log::warn!("data plane tick failed: {error:#}");
                }
            }
            Err(error) => log::warn!("cannot read the monotonic clock: {error:#}"),
        }
    }

    /// The data plane, returned once the loop has stopped.
    ///
    /// The binary needs it back to shut the probe down (final drain, counters)
    /// and to detach the XDP program; the loop itself never detaches because
    /// that would let packets bypass the program while reports are still in
    /// flight.
    pub fn into_apply(self) -> A {
        self.apply
    }

    /// Envelope version of the last configuration the control plane applied.
    pub fn applied_version(&self) -> u64 {
        self.applied_version
    }

    /// Digest of the last applied envelope (empty before the first one).
    pub fn applied_sha256(&self) -> &[u8] {
        &self.applied_sha256
    }

    /// §8: `Register` → `GetCursor` → `GetConfig` → validate → apply →
    /// `AckConfig` → `Watch` → the heartbeat/report loop.
    async fn steps<S: Future<Output = ()>>(
        &mut self,
        shutdown: &mut Pin<&mut S>,
    ) -> Result<(), AgentError> {
        self.register(shutdown).await?;
        self.get_cursor(shutdown).await?;
        self.sync_config(shutdown).await?;
        self.connect_watch().await?;
        self.main_loop(shutdown).await
    }

    /// §5.1: idempotent registration; the response carries the cadence and the
    /// first desired version.
    async fn register<S: Future<Output = ()>>(
        &mut self,
        shutdown: &mut Pin<&mut S>,
    ) -> Result<(), AgentError> {
        let request = pb::RegisterRequest {
            machine_id: self.cfg.machine_id.clone(),
            agent_version: self.cfg.agent_version.clone(),
            boot_id: self.cfg.boot_id.clone(),
            capabilities: Some(self.cfg.capabilities.clone()),
        };
        let response = call_with_retry(
            &*self.sleeper,
            Rpc::Register,
            self.backoff_jitter,
            || self.client.register(request.clone()),
            shutdown,
        )
        .await?;

        if response.status == pb::AgentStatus::Revoked as i32 {
            // §5.2: a revoked agent stops reporting and exits non-zero.
            return Err(AgentError::Fatal {
                code: ServerErrorCode::AgentRevoked,
                message: format!("agent {} is revoked", response.agent_id),
            });
        }
        log::info!(
            "registered as {} (machine {}, group {})",
            response.agent_id,
            response.machine_id,
            response.group_id
        );
        self.agent_id = response.agent_id;
        self.desired_version = response.desired_version;
        self.desired_sha256 = response.desired_sha256;
        self.update_intervals(
            response.heartbeat_interval_ms,
            response.idle_heartbeat_interval_ms,
        );
        Ok(())
    }

    /// §5.6: the control plane owns the cursor; the agent resumes at +1.
    async fn get_cursor<S: Future<Output = ()>>(
        &mut self,
        shutdown: &mut Pin<&mut S>,
    ) -> Result<(), AgentError> {
        let request = pb::GetCursorRequest {};
        let response = call_with_retry(
            &*self.sleeper,
            Rpc::GetCursor,
            self.backoff_jitter,
            || self.client.get_cursor(request),
            shutdown,
        )
        .await?;
        // Absent means "never reported"; a window 0 is a real window.
        self.last_window_id = response.last_window_id.unwrap_or(0);
        log::info!("resuming after window {}", self.last_window_id);
        Ok(())
    }

    /// §5.3: pulls, validates, applies and — always — acknowledges.
    async fn sync_config<S: Future<Output = ()>>(
        &mut self,
        shutdown: &mut Pin<&mut S>,
    ) -> Result<(), AgentError> {
        // §5.2: same content under a new version needs no download. Identical
        // bytes also mean an identical `schema_version` (it lives in the
        // envelope that the hash covers), so nothing is left to compare.
        if self.desired_version > self.applied_version
            && !self.desired_sha256.is_empty()
            && self.desired_sha256 == self.applied_sha256
        {
            log::debug!(
                "version {} has the applied content hash; acknowledging without a download",
                self.desired_version
            );
            let version = self.desired_version;
            self.ack(
                version,
                self.applied_sha256.clone(),
                pb::AckResult::Applied,
                0,
                Vec::new(),
                None,
                shutdown,
            )
            .await?;
            self.applied_version = version;
            return Ok(());
        }

        let request = pb::GetConfigRequest {
            current_version: self.applied_version,
        };
        let response = call_with_retry(
            &*self.sleeper,
            Rpc::GetConfig,
            self.backoff_jitter,
            || self.client.get_config(request),
            shutdown,
        )
        .await?;

        if response.up_to_date {
            // §5.3: "nothing newer" is a normal answer, not an error.
            self.desired_version = self.applied_version;
            return Ok(());
        }
        let Some(envelope) = response.config else {
            log::warn!("GetConfig answered neither up_to_date nor a config; ignoring");
            self.desired_version = self.applied_version;
            return Ok(());
        };
        self.apply_envelope(envelope, shutdown).await
    }

    /// Validates one envelope and acknowledges the outcome — including refusals
    /// (§5.3: otherwise the control plane shows APPLYING forever).
    async fn apply_envelope<S: Future<Output = ()>>(
        &mut self,
        envelope: pb::ConfigEnvelope,
        shutdown: &mut Pin<&mut S>,
    ) -> Result<(), AgentError> {
        let version = envelope.version;
        let sha = envelope.content_sha256.clone();
        match config::validate(&envelope, self.applied_version, &self.cfg.supported_schemas) {
            Ok(validated) => {
                let started = std::time::Instant::now();
                match self.apply.apply(&validated.probe_config) {
                    Ok(readback) => {
                        let apply_ms = elapsed_ms(started);
                        if readback.version != validated.probe_config.version
                            || readback.window_ns != validated.probe_config.window_ns
                        {
                            log::warn!(
                                "readback of version {version} differs from what was written; \
                                 keeping the readback as the applied state"
                            );
                        }
                        // The applied *config* version is the envelope's.
                        // `ProbeConfig::version` is the ABI version of the
                        // struct in `probe-common` and never carries it.
                        self.applied_version = version;
                        self.applied_sha256 = sha.clone();
                        log::info!(
                            "applied config version {version} ({} rules, {} degraded)",
                            validated.rule_count,
                            validated.degraded.len()
                        );
                        self.ack(
                            version,
                            sha,
                            pb::AckResult::Applied,
                            apply_ms,
                            validated
                                .degraded
                                .iter()
                                .map(|feature| (*feature).to_string())
                                .collect(),
                            None,
                            shutdown,
                        )
                        .await?;
                    }
                    Err(failure) => {
                        let apply_ms = elapsed_ms(started);
                        log::error!(
                            "config version {version} failed to apply: {} {}",
                            failure.code,
                            failure.detail
                        );
                        self.ack(
                            version,
                            sha,
                            pb::AckResult::Failed,
                            apply_ms,
                            Vec::new(),
                            Some(failure),
                            shutdown,
                        )
                        .await?;
                        // The data path is unchanged; do not spin on the same
                        // envelope — the next heartbeat decides when to retry.
                        self.desired_version = self.applied_version;
                    }
                }
            }
            Err(rejection) => {
                // §5.3: reject without touching the data path, and say why.
                log::warn!(
                    "config version {version} rejected: {} ({})",
                    rejection.code,
                    rejection.detail
                );
                self.ack(
                    version,
                    sha,
                    pb::AckResult::Rejected,
                    0,
                    Vec::new(),
                    Some(ApplyFailure::new(rejection.code, rejection.detail)),
                    shutdown,
                )
                .await?;
                // Do not pull the same rejected envelope in a tight loop.
                self.desired_version = self.applied_version;
            }
        }
        Ok(())
    }

    /// Sends one ack. Bound to `AckConfig`'s five attempts (§2.6); a failure
    /// here is reported but never blocks the report queue for long.
    #[allow(clippy::too_many_arguments)]
    async fn ack<S: Future<Output = ()>>(
        &mut self,
        version: u64,
        content_sha256: Vec<u8>,
        result: pb::AckResult,
        apply_ms: u32,
        degraded_features: Vec<String>,
        failure: Option<ApplyFailure>,
        shutdown: &mut Pin<&mut S>,
    ) -> Result<(), AgentError> {
        let request = pb::AckConfigRequest {
            version,
            content_sha256,
            result: result as i32,
            applied_at: self.timestamp(),
            apply_ms,
            degraded_features,
            error_code: failure
                .as_ref()
                .map(|failure| failure.code.to_string())
                .unwrap_or_default(),
            error_detail: failure
                .as_ref()
                .map(|failure| failure.detail.clone())
                .unwrap_or_default(),
        };
        let response = call_with_retry(
            &*self.sleeper,
            Rpc::AckConfig,
            self.backoff_jitter,
            || self.client.ack_config(request.clone()),
            shutdown,
        )
        .await?;
        log::info!(
            "acked config version {version} as {:?} (recorded: {})",
            result,
            response.recorded
        );
        Ok(())
    }

    /// §5.7: subscribes to push notifications.
    ///
    /// `UNIMPLEMENTED` is a legal deployment (Train 0's real control plane), so
    /// it turns the channel off for good instead of retrying. Any other failure
    /// is retried with the Watch backoff, never blocking the heartbeat loop.
    async fn connect_watch(&mut self) -> Result<(), AgentError> {
        if !self.watch_supported || self.watch.is_some() {
            return Ok(());
        }
        let request = pb::WatchRequest {
            current_version: self.applied_version,
        };
        match self.client.watch(request).await {
            Ok(Some(stream)) => {
                log::info!("watch: subscribed");
                self.watch = Some(stream);
                self.watch_backoff.reset();
            }
            Ok(None) => {
                self.watch_supported = false;
                log::info!(
                    "watch: the control plane does not implement Watch; falling back to heartbeat polling"
                );
            }
            Err(error) => return self.schedule_watch_retry(error),
        }
        Ok(())
    }

    /// Applies the Watch reconnect rule (§5.7: 1/2/4/8… capped at 60s).
    fn schedule_watch_retry(&mut self, error: ClientError) -> Result<(), AgentError> {
        if error.is_fatal() {
            return Err(AgentError::Fatal {
                code: error
                    .code()
                    .unwrap_or(ServerErrorCode::AgentUnauthenticated),
                message: error.to_string(),
            });
        }
        match self
            .watch_backoff
            .next_delay_with_retry_after(error.retry_after())
        {
            Some(delay) => {
                log::warn!("watch: {error}; reconnecting in {delay:?}");
                self.watch_retry_at = tokio::time::Instant::now() + delay;
            }
            None => {
                // Unreachable: the Watch schedule has no attempt limit, but if
                // it ever did, giving up means polling.
                log::warn!("watch: giving up after {error}; polling instead");
                self.watch_retry_at = tokio::time::Instant::now() + Duration::from_secs(60);
            }
        }
        Ok(())
    }

    /// The steady state (§8): report what is queued, catch up on configuration,
    /// otherwise heartbeat at the cadence the server asked for.
    async fn main_loop<S: Future<Output = ()>>(
        &mut self,
        shutdown: &mut Pin<&mut S>,
    ) -> Result<(), AgentError> {
        let mut next_heartbeat = tokio::time::Instant::now();
        let mut next_pump = tokio::time::Instant::now() + DATA_PLANE_INTERVAL;
        loop {
            // 0. Data path first.  The ring buffer is small and a heartbeat or a
            //    report may take seconds, so the loop drains on every iteration
            //    and on its own cadence while it waits.
            self.pump();

            // 1. Reports first: every window that waits is a window the control
            //    plane cannot see, and the queue grows at the window rate.
            if !self.reports.is_empty() {
                // §5.6: anything at or below the confirmed cursor is already
                // committed and must not be resubmitted.
                let dropped = self.reports.drop_through(self.last_window_id);
                if dropped > 0 {
                    log::debug!(
                        "dropped {dropped} queued window(s) at or below cursor {}",
                        self.last_window_id
                    );
                }
                if let Some(batch) = self.reports.pop_batch() {
                    self.send_batch(batch, shutdown).await?;
                    continue;
                }
            }

            // 2. Configuration catch-up: `desired > current` (§5.2) or a Watch
            //    notification that only names a version (§5.7).
            if self.desired_version > self.applied_version {
                self.sync_config(shutdown).await?;
                continue;
            }

            // 3. Watch reconnection, when the backoff timer has expired.
            if self.watch.is_none()
                && self.watch_supported
                && tokio::time::Instant::now() >= self.watch_retry_at
            {
                self.connect_watch().await?;
                continue;
            }

            // 4. Nothing to do: wait for the next heartbeat, the next data-plane
            //    pump, a notification, or shutdown.
            let sleep = tokio::time::sleep_until(next_heartbeat);
            tokio::pin!(sleep);
            let pump_due = tokio::time::sleep_until(next_pump);
            tokio::pin!(pump_due);
            match self.watch.take() {
                Some(mut watch) => {
                    let outcome = tokio::select! {
                        () = &mut *shutdown => Wait::Shutdown,
                        () = &mut sleep => Wait::Heartbeat,
                        () = &mut pump_due => Wait::Pump,
                        notification = watch.next() => Wait::Notification(notification),
                    };
                    match outcome {
                        Wait::Shutdown => return Ok(()),
                        Wait::Heartbeat => {
                            self.watch = Some(watch);
                            self.heartbeat(shutdown).await?;
                            next_heartbeat = tokio::time::Instant::now() + self.current_interval();
                        }
                        Wait::Pump => {
                            self.watch = Some(watch);
                            next_pump = tokio::time::Instant::now() + DATA_PLANE_INTERVAL;
                        }
                        Wait::Notification(Some(Ok(notification))) => {
                            self.watch = Some(watch);
                            log::info!("watch: version {} announced", notification.version);
                            // §5.7: the notification carries no body; it only
                            // tells us something newer exists.
                            self.desired_version = self.desired_version.max(notification.version);
                            // The hash we know comes from an older announcement.
                            // Keeping it would let `sync_config`'s "same content,
                            // new version" shortcut acknowledge this version
                            // without downloading it — with a stale hash.
                            self.desired_sha256.clear();
                        }
                        Wait::Notification(Some(Err(error))) => {
                            log::warn!("watch: stream failed: {error}");
                            self.schedule_watch_retry(error)?;
                        }
                        Wait::Notification(None) => {
                            log::info!("watch: server closed the stream; reconnecting");
                            self.watch_retry_at =
                                tokio::time::Instant::now() + self.current_interval();
                            self.watch_backoff.reset();
                        }
                    }
                }
                None => {
                    tokio::select! {
                        () = &mut *shutdown => return Ok(()),
                        () = &mut sleep => {
                            self.heartbeat(shutdown).await?;
                            next_heartbeat =
                                tokio::time::Instant::now() + self.current_interval();
                        }
                        () = &mut pump_due => {
                            next_pump = tokio::time::Instant::now() + DATA_PLANE_INTERVAL;
                        }
                    }
                }
            }
        }
    }

    /// §5.5: one batch out, cursor advanced from the response.
    async fn send_batch<S: Future<Output = ()>>(
        &mut self,
        request: pb::ReportWindowsRequest,
        shutdown: &mut Pin<&mut S>,
    ) -> Result<(), AgentError> {
        let submitted = request.windows.len();
        let response = call_with_retry(
            &*self.sleeper,
            Rpc::ReportWindows,
            self.backoff_jitter,
            || self.client.report_windows(request.clone()),
            shutdown,
        )
        .await?;

        // §5.5: every submitted window must be accounted for; a mismatch means
        // the control plane dropped rows and is worth an alert.
        let accounted = u64::from(response.accepted)
            + u64::from(response.duplicates)
            + u64::from(response.rejected);
        if accounted != submitted as u64 {
            log::error!(
                "ReportWindows: submitted {submitted} windows, control plane accounted for {accounted} \
                 (accepted {}, duplicates {}, rejected {})",
                response.accepted,
                response.duplicates,
                response.rejected
            );
        }
        for rejected in &response.rejected_windows {
            log::warn!(
                "window {} rejected: {}",
                rejected.window_id,
                rejected.error_code
            );
        }
        self.last_window_id = self.last_window_id.max(response.last_window_id);
        log::debug!(
            "reported {submitted} windows ({} accepted, {} duplicates, {} rejected); cursor now {}",
            response.accepted,
            response.duplicates,
            response.rejected,
            self.last_window_id
        );
        Ok(())
    }

    /// §5.4: liveness, the desired-version hint and the capability heartbeat.
    async fn heartbeat<S: Future<Output = ()>>(
        &mut self,
        shutdown: &mut Pin<&mut S>,
    ) -> Result<(), AgentError> {
        let request = pb::HeartbeatRequest {
            current_version: self.applied_version,
            config_state: self.apply.config_state() as i32,
            last_reported_window_id: self.last_window_id,
            pending_reports: u32::try_from(self.reports.len()).unwrap_or(u32::MAX),
            watch_connected: self.watch.is_some(),
            agent_time: self.timestamp(),
        };
        let response = call_with_retry(
            &*self.sleeper,
            Rpc::Heartbeat,
            self.backoff_jitter,
            || self.client.heartbeat(request),
            shutdown,
        )
        .await?;
        self.desired_version = response.desired_version;
        self.desired_sha256 = response.desired_sha256;
        self.update_intervals(
            response.heartbeat_interval_ms,
            response.idle_heartbeat_interval_ms,
        );
        Ok(())
    }

    /// §5.2: cadence comes from the server, never from a constant here.
    fn update_intervals(&mut self, heartbeat_ms: u32, idle_ms: u32) {
        if heartbeat_ms > 0 {
            self.heartbeat_interval = Duration::from_millis(u64::from(heartbeat_ms));
        }
        if idle_ms > 0 {
            self.idle_heartbeat_interval = Duration::from_millis(u64::from(idle_ms));
        }
    }

    /// Behind or with work queued: fast cadence. Caught up: idle cadence.
    fn current_interval(&self) -> Duration {
        if self.desired_version > self.applied_version || !self.reports.is_empty() {
            self.heartbeat_interval
        } else {
            self.idle_heartbeat_interval
        }
    }

    /// Monotonic → wall clock, when a clock was supplied.
    fn timestamp(&self) -> Option<prost_types::Timestamp> {
        let clock = self.clock.as_ref()?;
        let mono_ns = probe::ports::mono_now_ns().ok()?;
        Some(clock.timestamp(mono_ns))
    }

    /// §2.6: a graceful shutdown ends with one best-effort partial report,
    /// bounded by [`AgentConfig::shutdown_report_deadline`].
    async fn final_report(&mut self) {
        if self.reports.is_empty() {
            return;
        }
        let deadline = tokio::time::Instant::now() + self.cfg.shutdown_report_deadline;
        loop {
            let Some(batch) = self.reports.pop_batch() else {
                return;
            };
            let submitted = batch.windows.len();
            match tokio::time::timeout_at(deadline, self.client.report_windows(batch)).await {
                Ok(Ok(response)) => {
                    self.last_window_id = self.last_window_id.max(response.last_window_id);
                    log::info!(
                        "final report: {submitted} windows, cursor now {}",
                        self.last_window_id
                    );
                }
                Ok(Err(error)) => {
                    // No retries: the process is leaving.
                    log::warn!("final report failed ({error}); unsent windows are dropped");
                    return;
                }
                Err(_) => {
                    log::warn!(
                        "final report exceeded its {:?} budget; unsent windows are dropped",
                        self.cfg.shutdown_report_deadline
                    );
                    return;
                }
            }
            if tokio::time::Instant::now() >= deadline {
                log::warn!("final report budget exhausted");
                return;
            }
        }
    }
}

/// What the idle wait produced.
enum Wait {
    Shutdown,
    Heartbeat,
    /// The data-plane pump timer fired; nothing else has to happen.
    Pump,
    Notification(Option<Result<pb::ConfigNotification, ClientError>>),
}

/// Runs `op` until it succeeds, mapping failures per §12 and sleeping per §2.6.
///
/// `x-retry-after-ms` wins over the local schedule when the server sends one
/// (§2.6's table is a default, the server's hint is knowledge).
async fn call_with_retry<S, F, Fut, T>(
    sleeper: &dyn Sleeper,
    rpc: Rpc,
    jitter: bool,
    mut op: F,
    shutdown: &mut Pin<&mut S>,
) -> Result<T, AgentError>
where
    S: Future<Output = ()>,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ClientError>>,
{
    let schedule = backoff::Schedule::for_rpc(rpc)
        .unwrap_or_else(|| backoff::Schedule::for_rpc(Rpc::Register).expect("Register has one"));
    let mut backoff = if jitter {
        Backoff::new(schedule)
    } else {
        Backoff::deterministic(schedule)
    };
    loop {
        match op().await {
            Ok(value) => return Ok(value),
            Err(error) if error.is_fatal() => {
                return Err(AgentError::Fatal {
                    code: error
                        .code()
                        .unwrap_or(ServerErrorCode::AgentUnauthenticated),
                    message: error.to_string(),
                });
            }
            Err(error) if !error.retryable() => {
                return Err(AgentError::Rejected {
                    rpc,
                    message: error.to_string(),
                });
            }
            Err(error) => {
                let attempts = backoff.attempt() + 1;
                let Some(delay) = backoff.next_delay_with_retry_after(error.retry_after()) else {
                    return Err(AgentError::RetriesExhausted {
                        rpc,
                        attempts,
                        message: error.to_string(),
                    });
                };
                log::warn!("{} failed ({error}); retrying in {delay:?}", rpc.name());
                tokio::select! {
                    () = &mut *shutdown => return Err(AgentError::Shutdown),
                    () = sleeper.sleep(delay) => {}
                }
            }
        }
    }
}

/// Milliseconds elapsed since `started`, saturating at `u32::MAX`.
fn elapsed_ms(started: std::time::Instant) -> u32 {
    u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX)
}
