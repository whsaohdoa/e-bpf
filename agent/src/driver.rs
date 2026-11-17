//! The data plane as the agent's state machine sees it (S-C).
//!
//! [`DataPlane`] is the small surface [`crate::agent_loop::AgentLoop`] needs to
//! keep samples moving while it talks to the control plane: drain the ring,
//! advance the window watermark, flush the open windows on shutdown.  It exists
//! as a trait so the S-B state-machine tests can run without a kernel, and
//! [`AttachedProbe`] is its one production implementation: it owns a
//! [`probe::driver::Probe`] (the eBPF object, the XDP link and the session) and
//! therefore also implements [`ConfigApply`], because applying a configuration
//! *is* a data-path operation.
//!
//! Ownership is the point of this module: the agent loop owns the data plane, so
//! nothing else can call `apply` concurrently with a heartbeat or a report.  The
//! binary reads the probe back with [`AttachedProbe::into_probe`] once the loop
//! has stopped, and only then detaches.

use anyhow::Result;
use probe::{
    driver::{Probe, ProbeOptions},
    pipeline::Summary,
    sink::{AggSink, Sink},
};
use probe_common::ProbeConfig;

use crate::{
    agent_loop::{ApplyFailure, ConfigApply},
    pb,
};

/// The data path operations the agent loop drives.
///
/// Every method is synchronous and cheap: the loop calls [`DataPlane::drain`]
/// and [`DataPlane::tick`] on a fixed cadence, so a probe that had to block here
/// would stall heartbeats and reports.
pub trait DataPlane {
    /// The configuration currently installed in the data path.
    fn config(&self) -> ProbeConfig;

    /// Moves every record the kernel has already produced into the session.
    fn drain(&mut self);

    /// Reports the windows the watermark closed and buffers the samples.
    fn tick(&mut self, now_ns: u64) -> Result<()>;

    /// Writes every still-open window with `window_complete = 0`.
    ///
    /// Used on shutdown, before the final report, so a partial window is not
    /// lost.  It is idempotent: the rows it wrote are not written again.
    fn flush_pending(&mut self) -> Result<()>;

    /// Final drain + flush + counters, once the loop has stopped.
    fn shutdown(&mut self) -> Result<Summary>;
}

/// A [`Probe`] whose configuration is driven by the control plane.
///
/// `applied_version`/`applied_sha256` are the *envelope* values of the last
/// successful apply, mirroring what [`ConfigApply`] reports.  They start at 0/0
/// and stay there until the control plane sends a configuration: the CLI flags
/// are a bootstrap value, never an applied configuration.
pub struct AttachedProbe {
    probe: Probe,
    applied_version: u64,
    applied_sha256: [u8; 32],
    /// What the next heartbeat should report.  Set to `Applied` by the first
    /// successful [`ConfigApply::apply`], never by the bootstrap configuration.
    state: pb::ConfigState,
}

impl AttachedProbe {
    /// Wraps an already attached probe with no applied configuration.
    pub fn new(probe: Probe) -> Self {
        Self {
            probe,
            applied_version: 0,
            applied_sha256: [0u8; 32],
            state: pb::ConfigState::Missing,
        }
    }

    /// Attaches the data path, mapping every failure to `bpf.attach_failed`.
    ///
    /// Loading the object, writing `CONFIG`, attaching the program and taking
    /// the maps all happen before the first `Register`, so a failure here can
    /// only be reported by exiting non-zero; there is no envelope to ack yet.
    pub fn attach(
        opt: &ProbeOptions,
        requested: ProbeConfig,
        sink: Box<dyn Sink + Send>,
        agg_sink: Box<dyn AggSink + Send>,
    ) -> Result<Self, ApplyFailure> {
        Probe::attach(opt, requested, sink, agg_sink)
            .map(Self::new)
            .map_err(|error| ApplyFailure::attach_failed(format!("{error:#}")))
    }

    /// Gives the probe back so the binary can shut it down and detach it.
    pub fn into_probe(self) -> Probe {
        self.probe
    }

    /// Records the envelope version and digest of a successful apply.
    pub fn record_applied(&mut self, version: u64, sha256: [u8; 32]) {
        self.applied_version = version;
        self.applied_sha256 = sha256;
    }

    /// Digest of the last applied envelope (`[0; 32]` before the first one).
    pub fn applied_sha256(&self) -> [u8; 32] {
        self.applied_sha256
    }
}

impl ConfigApply for AttachedProbe {
    fn apply(&mut self, cfg: &ProbeConfig) -> Result<ProbeConfig, ApplyFailure> {
        // The probe writes CONFIG, reads it back, flushes the rows that were
        // bucketed with the old width and only then swaps the configuration.
        // A failure means the data path keeps the previous configuration: there
        // is no fallback to defaults (contract §5.3).
        let applied = self
            .probe
            .apply_config(*cfg)
            .map_err(|error| ApplyFailure::write_config_failed(format!("{error:#}")))?;
        self.state = pb::ConfigState::Applied;
        Ok(applied)
    }

    fn applied_version(&self) -> u64 {
        // Stays 0 for the whole run: the envelope version never reaches this
        // trait (`ProbeConfig::version` is the ABI version of the struct) and
        // the loop owns the authoritative value, so seeding the loop from here
        // must not claim a configuration this process has not applied in this
        // run.  Only the binary writes the field, after the loop has stopped,
        // to persist `applied.json`.
        self.applied_version
    }

    fn config_state(&self) -> pb::ConfigState {
        self.state
    }
}

impl DataPlane for AttachedProbe {
    fn config(&self) -> ProbeConfig {
        *self.probe.config()
    }

    fn drain(&mut self) {
        self.probe.drain();
    }

    fn tick(&mut self, now_ns: u64) -> Result<()> {
        self.probe.tick(now_ns)
    }

    fn flush_pending(&mut self) -> Result<()> {
        self.probe.flush_pending()
    }

    fn shutdown(&mut self) -> Result<Summary> {
        self.probe.shutdown()
    }
}
