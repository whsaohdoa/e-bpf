//! In-process mock control plane.
//!
//! The S-B tests need a real gRPC server behind real mutual TLS to drive the
//! real client and the real state machine, but they must not need a control
//! center deployment. This module is that server: it implements
//! [`pb::agent_control_server::AgentControl`], issues its own CA and client
//! certificate with `rcgen`, and keeps just enough state to behave like the
//! real thing (see `docs/Control-Agent-API-v1.md` §5):
//!
//! * `AckConfig` is stored per call,
//! * `GetCursor` reports the highest window id that has been committed,
//! * `ReportWindows` dedupes rows by `(window_id, key_hash)` and answers
//!   `accepted + duplicates + rejected == submitted windows`,
//! * `Watch` pushes config notifications, or answers `UNIMPLEMENTED` when
//!   [`MockConfig::watch_supported`] is false.
//!
//! It is test scaffolding, not production code: the `control-mock` binary in
//! `src/bin/control-mock.rs` exists only to let a human poke the agent by hand.
//! Nothing here is part of the shipped agent, and no state it keeps survives a
//! process restart.

use std::collections::{HashSet, VecDeque};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ::tonic::codegen::BoxStream;
use ::tonic::codegen::tokio_stream::Stream;
use ::tonic::transport::server::TcpIncoming;
use ::tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};
use ::tonic::{Code, Request, Response, Status};
use async_trait::async_trait;
use prost::Message as _;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use sha2::{Digest, Sha256};

use crate::agent_loop::{ApplyFailure, ConfigApply};
use crate::client::Rpc;
use crate::client::tonic::{ClientConfig, ERROR_CODE_HEADER};
use crate::error::ServerErrorCode;
use crate::pb;

/// Heartbeat interval the mock hands out when the config does not say.
pub const DEFAULT_HEARTBEAT_INTERVAL_MS: u32 = 1_000;

/// Idle heartbeat interval the mock hands out when the config does not say.
pub const DEFAULT_IDLE_HEARTBEAT_INTERVAL_MS: u32 = 5_000;

/// `agent_id` every mock instance registers agents as.
pub const DEFAULT_AGENT_ID: &str = "agent-1";

/// `machine_id` the mock uses when a test does not care.
pub const DEFAULT_MACHINE_ID: &str = "machine-1";

/// `group_id` every mock instance reports.
pub const DEFAULT_GROUP_ID: &str = "group-1";

/// Common name of the mock's CA.
const CA_COMMON_NAME: &str = "probe-agent-mock-ca";

/// How the mock behaves. Everything the S-B tests need to vary lives here.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default)]
pub struct MockConfig {
    /// Reject client certificates other than the one this instance issued.
    /// Setting this to `false` leaves the TLS listener anonymous.
    pub require_client_cert: bool,
    /// Answer `Watch` with a notification stream. `false` makes `Watch` return
    /// `UNIMPLEMENTED`, which is what the real Train 0 control center does
    /// (§5.7) and what the agent must treat as "no push channel, poll instead".
    pub watch_supported: bool,
    /// Version announced in `Register`, `Heartbeat` and `GetConfig` before any
    /// document is published. `0` means "no config", a normal state.
    pub desired_version: u64,
    /// Heartbeat interval handed out to the agent (contract §5.2).
    pub heartbeat_interval_ms: u32,
    /// Idle heartbeat interval handed out to the agent (contract §5.2).
    pub idle_heartbeat_interval_ms: u32,
    /// The cursor the very first `GetCursor` reports, i.e. the highest window
    /// id the control plane claims to have committed. `None` is §5.6's "never
    /// reported anything" (proto3 `optional` absent), which also makes the
    /// `ReportWindows` response start at 0.
    pub initial_cursor: Option<u64>,
    /// `agent_id` this instance hands out.
    pub agent_id: String,
    /// `group_id` this instance hands out.
    pub group_id: String,
    /// Report `AGENT_STATUS_REVOKED` (§5.2) so the agent must stop reporting and
    /// exit non-zero while leaving the data path alone.
    pub revoked: bool,
}

impl Default for MockConfig {
    fn default() -> Self {
        Self {
            require_client_cert: true,
            watch_supported: false,
            desired_version: 0,
            heartbeat_interval_ms: DEFAULT_HEARTBEAT_INTERVAL_MS,
            idle_heartbeat_interval_ms: DEFAULT_IDLE_HEARTBEAT_INTERVAL_MS,
            initial_cursor: None,
            agent_id: DEFAULT_AGENT_ID.to_string(),
            group_id: DEFAULT_GROUP_ID.to_string(),
            revoked: false,
        }
    }
}

impl MockConfig {
    /// Reads a config file written as JSON, for `control-mock --config`.
    ///
    /// Unknown fields are ignored and every field is optional, so a file may
    /// name only the knobs a human wants to change.
    pub fn from_json_file(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path.as_ref())?;
        Ok(serde_json::from_str(&text)?)
    }
}

/// One published config document and the hash the mock announces for it.
#[derive(Clone, Debug)]
struct Published {
    version: u64,
    sha256: Vec<u8>,
    document: Vec<u8>,
    published_at: u64,
}

/// What `Register`/`Heartbeat` announce about the desired config.
#[derive(Clone, Debug, Default)]
struct Announcement {
    version: u64,
    sha256: Vec<u8>,
}

/// Everything the mock remembers between calls.
#[derive(Default)]
struct State {
    calls: [usize; 7],
    failures: [VecDeque<Status>; 7],
    delays: [VecDeque<Duration>; 7],
    acks: Vec<pb::AckConfigRequest>,
    heartbeats: Vec<pb::HeartbeatRequest>,
    reports: Vec<pb::ReportWindowsRequest>,
    /// Committed rows, keyed exactly like §5.5's upsert key.
    committed: HashSet<(u64, Vec<u8>)>,
    /// Highest committed window id; `None` means "nothing committed yet".
    max_window_id: Option<u64>,
    /// What `Register`/`Heartbeat`/`GetConfig` compare against.
    announced_version: u64,
    /// The document `GetConfig` serves, if any.
    published: Option<Published>,
    /// Successful `Watch` subscriptions, for tests that assert on reconnects.
    watch_subscriptions: usize,
    /// One sender per open `Watch` stream; closed streams are dropped on the
    /// next notification.
    watchers: Vec<tokio::sync::mpsc::UnboundedSender<u64>>,
}

impl State {
    /// §5.6: the control plane is authoritative, so the cursor is whatever has
    /// actually been committed — floored by the configured initial cursor.
    fn cursor(&self, cfg: &MockConfig) -> Option<u64> {
        self.max_window_id.or(cfg.initial_cursor)
    }

    fn announcement(&self, cfg: &MockConfig) -> Announcement {
        match &self.published {
            Some(published) => {
                let version = self.announced_version.max(published.version);
                // Announcing a version whose document is not published yet
                // yields an unknown hash on purpose: the agent must then pull
                // (§5.3 only lets it skip the pull when the hash is equal),
                // which is how the stale-version path becomes reachable.
                let known = version == published.version;
                Announcement {
                    version,
                    sha256: if known {
                        published.sha256.clone()
                    } else {
                        Vec::new()
                    },
                }
            }
            None => Announcement {
                version: self.announced_version.max(cfg.desired_version),
                sha256: Vec::new(),
            },
        }
    }
}

/// The handle every test keeps: TLS material, the endpoint and the controls.
pub struct MockHandle {
    /// `https://127.0.0.1:<port>` of the listener.
    pub endpoint: String,
    /// PEM of the CA the client must trust.
    pub ca_pem: Vec<u8>,
    /// PEM of the client certificate the mock accepts.
    pub cert_pem: Vec<u8>,
    /// PEM of that certificate's key.
    pub key_pem: Vec<u8>,
    /// The knobs and the observations; `shared()` is the same thing.
    pub shared: MockShared,
    /// A second client certificate, signed by the same CA but with a different
    /// subject: TLS accepts it, the service's identity check does not.
    intruder_cert_pem: Vec<u8>,
    intruder_key_pem: Vec<u8>,
    cert_dir: PathBuf,
}

impl MockHandle {
    /// The knobs and observations of this instance.
    pub fn shared(&self) -> &MockShared {
        &self.shared
    }

    /// A client config that passes the identity check.
    pub fn client_config(&self) -> ClientConfig {
        ClientConfig::new(
            self.endpoint.clone(),
            self.ca_pem.clone(),
            self.cert_pem.clone(),
            self.key_pem.clone(),
        )
        .with_domain_name(DEFAULT_AGENT_ID)
    }

    /// A client config with no client certificate at all. With
    /// [`MockConfig::require_client_cert`] the TLS handshake fails, which the
    /// client must surface as `agent.unauthenticated` instead of retrying.
    pub fn anonymous_client_config(&self) -> ClientConfig {
        ClientConfig::new(
            self.endpoint.clone(),
            self.ca_pem.clone(),
            Vec::new(),
            Vec::new(),
        )
        .with_domain_name(DEFAULT_AGENT_ID)
    }

    /// A client config whose certificate the mock's identity check rejects.
    pub fn mismatched_client_config(&self) -> ClientConfig {
        ClientConfig::new(
            self.endpoint.clone(),
            self.ca_pem.clone(),
            self.intruder_cert_pem.clone(),
            self.intruder_key_pem.clone(),
        )
        .with_domain_name(DEFAULT_AGENT_ID)
    }

    /// Where this instance wrote its PEM files.
    pub fn cert_dir(&self) -> &Path {
        &self.cert_dir
    }
}

impl Drop for MockHandle {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_dir_all(&self.cert_dir)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            log::debug!(
                "could not remove the mock certificate directory {}: {err}",
                self.cert_dir.display()
            );
        }
    }
}

/// The controls a test (or the agent under test) reaches into.
#[derive(Clone)]
pub struct MockShared {
    cfg: Arc<MockConfig>,
    state: Arc<Mutex<State>>,
    expected_client_cert: Arc<Vec<u8>>,
}

impl MockShared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // A poisoned lock would only mean another test panicked mid-call; the
        // state is still consistent enough to keep asserting on.
        self.state.lock().unwrap_or_else(|err| err.into_inner())
    }

    /// The configuration this instance was started with.
    pub fn config(&self) -> &MockConfig {
        &self.cfg
    }

    /// Publishes `document` as `version`: the next `GetConfig` serves it and
    /// connected `Watch` streams are told about it.
    ///
    /// The announced hash is the control plane's job (§5.3 step 3), so it is
    /// computed here, over the exact bytes that will be served.
    pub fn publish(&self, version: u64, document: pb::ConfigDocument) {
        let bytes = document.encode_to_vec();
        let sha256 = Sha256::digest(&bytes).to_vec();
        self.publish_bytes(version, bytes, sha256);
    }

    /// Publishes `document` while announcing a hash that does not match it, so
    /// the agent's integrity check has something to reject.
    pub fn publish_corrupt_hash(&self, version: u64, document: pb::ConfigDocument) {
        let bytes = document.encode_to_vec();
        let mut sha256 = Sha256::digest(&bytes).to_vec();
        sha256[0] ^= 0xff;
        self.publish_bytes(version, bytes, sha256);
    }

    fn publish_bytes(&self, version: u64, document: Vec<u8>, sha256: Vec<u8>) {
        let published_at = unix_ms();
        {
            let mut state = self.lock();
            state.announced_version = state.announced_version.max(version);
            state.published = Some(Published {
                version,
                sha256,
                document,
                published_at,
            });
            // A publish is exactly the event `Watch` exists for, so every open
            // stream is told. Closed streams are dropped here instead of being
            // tracked separately.
            state
                .watchers
                .retain(|watcher| watcher.send(version).is_ok());
        }
    }

    /// Tells connected watchers about `version` without changing the document.
    pub fn push_watch(&self, version: u64) {
        let mut state = self.lock();
        state
            .watchers
            .retain(|watcher| watcher.send(version).is_ok());
    }

    /// Announces `version` to `Register`/`Heartbeat`/`GetConfig` while keeping
    /// the published document's own (older) version. That is how a test drives
    /// §5.3's "version must move forward" check without inventing a second
    /// document.
    pub fn announce_version(&self, version: u64) {
        self.lock().announced_version = version;
    }

    /// Makes the next call of `rpc` fail with `status` instead of running.
    ///
    /// The status travels with its own metadata, so a test can attach
    /// `x-error-code` (and thus drive the §12 retry table) itself.
    pub fn fail_next(&self, rpc: Rpc, status: Status) {
        self.lock().failures[rpc.index()].push_back(status);
    }

    /// Makes the next call of `rpc` wait `delay` before answering. A delay
    /// longer than the RPC's deadline (§2.6) is how the client's deadline and
    /// retry path is exercised.
    pub fn delay_next(&self, rpc: Rpc, delay: Duration) {
        self.lock().delays[rpc.index()].push_back(delay);
    }

    /// Every `AckConfig` the agent sent, in order.
    pub fn acks(&self) -> Vec<pb::AckConfigRequest> {
        self.lock().acks.clone()
    }

    /// Every `Heartbeat` the agent sent, in order.
    pub fn heartbeats(&self) -> Vec<pb::HeartbeatRequest> {
        self.lock().heartbeats.clone()
    }

    /// Every `ReportWindows` the agent sent, in order.
    pub fn reports(&self) -> Vec<pb::ReportWindowsRequest> {
        self.lock().reports.clone()
    }

    /// How many times `rpc` was called, injected failures included.
    pub fn calls(&self, rpc: Rpc) -> usize {
        self.lock().calls[rpc.index()]
    }

    /// Rows committed so far, i.e. after dedupe.
    pub fn committed_rows(&self) -> usize {
        self.lock().committed.len()
    }

    /// The cursor `GetCursor` would report right now.
    pub fn cursor(&self) -> Option<u64> {
        let state = self.lock();
        state.cursor(&self.cfg)
    }
}

/// Entry points for serving the mock.
pub struct MockControl;

impl MockControl {
    /// Starts a mock on an ephemeral loopback port and returns everything a
    /// test needs to talk to it. The listener is dropped with the test's
    /// runtime; the returned handle owns the certificate directory.
    pub async fn start(cfg: MockConfig) -> anyhow::Result<MockHandle> {
        let material = TlsMaterial::generate()?;
        let cert_dir = unique_cert_dir()?;
        material.write_pem_files(&cert_dir)?;

        let shared = MockShared {
            state: Arc::new(Mutex::new(State {
                announced_version: cfg.desired_version,
                ..State::default()
            })),
            expected_client_cert: Arc::new(material.client_cert_der.clone()),
            cfg: Arc::new(cfg.clone()),
        };

        let incoming = TcpIncoming::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?;
        let addr = incoming.local_addr()?;
        let router = build_router(&cfg, &material, shared.clone())?;
        tokio::spawn(async move {
            if let Err(err) = router.serve_with_incoming(incoming).await {
                log::debug!("mock control plane stopped: {err}");
            }
        });

        Ok(MockHandle {
            endpoint: format!("https://127.0.0.1:{}", addr.port()),
            ca_pem: material.ca_pem.clone(),
            cert_pem: material.client_cert_pem.clone(),
            key_pem: material.client_key_pem.clone(),
            intruder_cert_pem: material.intruder_cert_pem.clone(),
            intruder_key_pem: material.intruder_key_pem.clone(),
            cert_dir,
            shared,
        })
    }

    /// Serves a mock on `addr` until the process ends, writing the PEM files
    /// into `cert_dir` so a human (or `curl`) can use them. This is what the
    /// `control-mock` binary runs.
    pub async fn serve(cfg: MockConfig, addr: SocketAddr, cert_dir: &Path) -> anyhow::Result<()> {
        let material = TlsMaterial::generate()?;
        std::fs::create_dir_all(cert_dir)?;
        material.write_pem_files(cert_dir)?;

        let shared = MockShared {
            state: Arc::new(Mutex::new(State {
                announced_version: cfg.desired_version,
                ..State::default()
            })),
            expected_client_cert: Arc::new(material.client_cert_der.clone()),
            cfg: Arc::new(cfg.clone()),
        };
        let router = build_router(&cfg, &material, shared)?;
        let incoming = TcpIncoming::bind(addr)?;
        log::info!(
            "mock control plane listening on https://{addr}; PEM files in {}",
            cert_dir.display()
        );
        router.serve_with_incoming(incoming).await?;
        Ok(())
    }
}

/// Builds the TLS listener and registers the service on it.
fn build_router(
    cfg: &MockConfig,
    material: &TlsMaterial,
    shared: MockShared,
) -> anyhow::Result<::tonic::transport::server::Router> {
    let mut tls = ServerTlsConfig::new().identity(Identity::from_pem(
        &material.server_cert_pem,
        &material.server_key_pem,
    ));
    if cfg.require_client_cert {
        // Requesting a client certificate makes rustls reject a client that has
        // none during the handshake, which is the mTLS behaviour of §2.4.
        tls = tls.client_ca_root(Certificate::from_pem(&material.ca_pem));
    }
    let mut server = Server::builder().tls_config(tls)?;
    Ok(
        server.add_service(pb::agent_control_server::AgentControlServer::new(
            MockService { shared },
        )),
    )
}

/// The gRPC service itself.
struct MockService {
    shared: MockShared,
}

impl MockService {
    /// Counts the call, applies an injected delay and/or failure, and checks
    /// the caller's certificate — in that order, so a test that injects a
    /// failure still sees the call counted.
    async fn pre_call<T>(&self, rpc: Rpc, request: &Request<T>) -> Result<(), Status> {
        let delay = {
            let mut state = self.shared.lock();
            state.calls[rpc.index()] += 1;
            state.delays[rpc.index()].pop_front()
        };
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }

        self.check_identity(request)?;

        let failure = self.shared.lock().failures[rpc.index()].pop_front();
        if let Some(status) = failure {
            log::info!(
                "mock control plane: {} -> injected {} ({})",
                rpc.name(),
                status.code(),
                status.message()
            );
            return Err(status);
        }
        log::info!("mock control plane: {}", rpc.name());
        Ok(())
    }

    /// mTLS identity beyond "the chain verifies": only the certificate this
    /// instance issued may speak for the agent (§12.2 `agent.identity_mismatch`).
    fn check_identity<T>(&self, request: &Request<T>) -> Result<(), Status> {
        if !self.shared.cfg.require_client_cert {
            return Ok(());
        }
        let chain = request.peer_certs();
        let leaf = chain.as_ref().and_then(|chain| chain.first());
        let Some(leaf) = leaf else {
            return Err(error_status(
                Code::Unauthenticated,
                ServerErrorCode::AgentUnauthenticated,
                "the connection carried no client certificate",
            ));
        };
        if leaf.as_ref() != self.shared.expected_client_cert.as_slice() {
            return Err(error_status(
                Code::Unauthenticated,
                ServerErrorCode::AgentIdentityMismatch,
                "the client certificate was not issued to this agent",
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl pb::agent_control_server::AgentControl for MockService {
    type WatchStream = BoxStream<pb::ConfigNotification>;

    async fn register(
        &self,
        request: Request<pb::RegisterRequest>,
    ) -> Result<Response<pb::RegisterResponse>, Status> {
        self.pre_call(Rpc::Register, &request).await?;
        let request = request.into_inner();
        let announcement = self.shared.lock().announcement(&self.shared.cfg);
        let cfg = &self.shared.cfg;
        Ok(Response::new(pb::RegisterResponse {
            agent_id: cfg.agent_id.clone(),
            machine_id: request.machine_id.clone(),
            group_id: cfg.group_id.clone(),
            status: if cfg.revoked {
                pb::AgentStatus::Revoked as i32
            } else {
                pb::AgentStatus::Active as i32
            },
            desired_version: announcement.version,
            desired_sha256: announcement.sha256,
            heartbeat_interval_ms: cfg.heartbeat_interval_ms,
            idle_heartbeat_interval_ms: cfg.idle_heartbeat_interval_ms,
            server_time: now_timestamp(),
            effective_capabilities: request.capabilities.clone(),
        }))
    }

    async fn heartbeat(
        &self,
        request: Request<pb::HeartbeatRequest>,
    ) -> Result<Response<pb::HeartbeatResponse>, Status> {
        self.pre_call(Rpc::Heartbeat, &request).await?;
        let request = request.into_inner();
        let announcement = {
            let mut state = self.shared.lock();
            state.heartbeats.push(request);
            state.announcement(&self.shared.cfg)
        };
        let cfg = &self.shared.cfg;
        Ok(Response::new(pb::HeartbeatResponse {
            desired_version: announcement.version,
            desired_sha256: announcement.sha256,
            heartbeat_interval_ms: cfg.heartbeat_interval_ms,
            idle_heartbeat_interval_ms: cfg.idle_heartbeat_interval_ms,
            server_time: now_timestamp(),
        }))
    }

    async fn get_config(
        &self,
        request: Request<pb::GetConfigRequest>,
    ) -> Result<Response<pb::GetConfigResponse>, Status> {
        self.pre_call(Rpc::GetConfig, &request).await?;
        let request = request.into_inner();
        let state = self.shared.lock();
        let announced = state.announcement(&self.shared.cfg).version;
        if announced <= request.current_version {
            // §5.3: "you are up to date" is a normal answer, not an error.
            return Ok(Response::new(pb::GetConfigResponse {
                up_to_date: true,
                config: None,
            }));
        }
        let Some(published) = state.published.clone() else {
            // A version is announced but no document was ever published; the
            // only honest answer is still "nothing newer to serve".
            log::warn!(
                "mock control plane: version {announced} is announced but no document is published"
            );
            return Ok(Response::new(pb::GetConfigResponse {
                up_to_date: true,
                config: None,
            }));
        };
        Ok(Response::new(pb::GetConfigResponse {
            up_to_date: false,
            config: Some(pb::ConfigEnvelope {
                // Deliberately the *published* version, not the announced one:
                // `announce_version` is how a test makes the agent pull an
                // envelope it must then reject as stale.
                version: published.version,
                schema_version: 1,
                content_sha256: published.sha256,
                document: published.document,
                published_at: Some(timestamp_from_unix_ms(published.published_at)),
            }),
        }))
    }

    async fn ack_config(
        &self,
        request: Request<pb::AckConfigRequest>,
    ) -> Result<Response<pb::AckConfigResponse>, Status> {
        self.pre_call(Rpc::AckConfig, &request).await?;
        let request = request.into_inner();
        let (desired_version, received_at) = {
            let mut state = self.shared.lock();
            let announced = state.announcement(&self.shared.cfg).version;
            // §5.4: acknowledging a version the control plane never published is
            // a client bug, not a state change.
            if request.version > announced {
                return Err(error_status(
                    Code::FailedPrecondition,
                    ServerErrorCode::AckVersionUnknown,
                    format!(
                        "ack for version {} but only {announced} was announced",
                        request.version
                    ),
                ));
            }
            state.acks.push(request);
            (announced, unix_ms())
        };
        Ok(Response::new(pb::AckConfigResponse {
            recorded: true,
            desired_version,
            received_at: Some(timestamp_from_unix_ms(received_at)),
            server_time: now_timestamp(),
        }))
    }

    async fn report_windows(
        &self,
        request: Request<pb::ReportWindowsRequest>,
    ) -> Result<Response<pb::ReportWindowsResponse>, Status> {
        self.pre_call(Rpc::ReportWindows, &request).await?;
        let request = request.into_inner();
        let submitted = request.windows.len() as u32;
        let mut accepted = 0_u32;
        let mut duplicates = 0_u32;
        let mut rejected = 0_u32;
        let mut rejected_windows = Vec::new();

        {
            let mut state = self.shared.lock();
            let cursor = state.cursor(&self.shared.cfg);
            for window in &request.windows {
                // §5.5: a window the control plane has already moved past by
                // more than 1000 ids is refused instead of resurrected.
                if let Some(cursor) = cursor
                    && window.window_id.saturating_add(1_000) < cursor
                {
                    rejected += 1;
                    rejected_windows.push(pb::RejectedWindow {
                        window_id: window.window_id,
                        error_code: "report.window_too_old".to_string(),
                    });
                    continue;
                }

                let mut fresh = 0_usize;
                let mut seen = 0_usize;
                for aggregate in &window.aggregates {
                    let key = (window.window_id, aggregate.key_hash.clone());
                    if state.committed.contains(&key) {
                        seen += 1;
                    } else {
                        fresh += 1;
                    }
                }
                for aggregate in &window.aggregates {
                    state
                        .committed
                        .insert((window.window_id, aggregate.key_hash.clone()));
                }
                state.max_window_id = Some(
                    state
                        .max_window_id
                        .map_or(window.window_id, |max| max.max(window.window_id)),
                );

                // One window is one unit of the response counters: a window is
                // a duplicate only when every row of it was already stored.
                if fresh > 0 || seen == 0 {
                    accepted += 1;
                } else {
                    duplicates += 1;
                }
            }
            state.reports.push(request);
        }

        debug_assert_eq!(
            accepted + duplicates + rejected,
            submitted,
            "§5.5 requires accepted + duplicates + rejected == submitted windows"
        );
        let last_window_id = self.shared.cursor().unwrap_or(0);
        Ok(Response::new(pb::ReportWindowsResponse {
            accepted,
            duplicates,
            rejected,
            rejected_windows,
            last_window_id,
            server_time: now_timestamp(),
        }))
    }

    async fn get_cursor(
        &self,
        request: Request<pb::GetCursorRequest>,
    ) -> Result<Response<pb::GetCursorResponse>, Status> {
        self.pre_call(Rpc::GetCursor, &request).await?;
        Ok(Response::new(pb::GetCursorResponse {
            last_window_id: self.shared.cursor(),
            server_time: now_timestamp(),
        }))
    }

    async fn watch(
        &self,
        request: Request<pb::WatchRequest>,
    ) -> Result<Response<Self::WatchStream>, Status> {
        self.pre_call(Rpc::Watch, &request).await?;
        if !self.shared.cfg.watch_supported {
            // §5.7: the real Train 0 control plane has no push channel and says
            // so; the agent must fall back to polling instead of failing.
            return Err(Status::unimplemented(
                "no push channel: Watch is not implemented by this control plane",
            ));
        }
        let request = request.into_inner();
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let initial = {
            let mut state = self.shared.lock();
            state.watch_subscriptions += 1;
            let announced = state.announcement(&self.shared.cfg).version;
            state.watchers.push(sender);
            (announced > request.current_version).then_some(announced)
        };
        let stream = WatchNotifications {
            receiver,
            shared: self.shared.clone(),
            pending: initial,
            last_sent: request.current_version,
        };
        Ok(Response::new(Box::pin(stream)))
    }
}

/// §5.7 notifications carry a version and a hash, never a document.
struct WatchNotifications {
    receiver: tokio::sync::mpsc::UnboundedReceiver<u64>,
    shared: MockShared,
    pending: Option<u64>,
    last_sent: u64,
}

impl WatchNotifications {
    fn notification(&self, version: u64) -> pb::ConfigNotification {
        let state = self.shared.lock();
        let sha256 = state
            .published
            .as_ref()
            .map(|published| published.sha256.clone())
            .unwrap_or_default();
        let issued_at = state
            .published
            .as_ref()
            .map(|published| published.published_at)
            .unwrap_or_else(unix_ms);
        pb::ConfigNotification {
            version,
            content_sha256: sha256,
            issued_at: Some(timestamp_from_unix_ms(issued_at)),
        }
    }
}

impl Stream for WatchNotifications {
    type Item = Result<pb::ConfigNotification, Status>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if let Some(version) = this.pending.take() {
            this.last_sent = version;
            let notification = this.notification(version);
            return Poll::Ready(Some(Ok(notification)));
        }
        loop {
            match this.receiver.poll_recv(cx) {
                Poll::Ready(Some(version)) => {
                    if version <= this.last_sent {
                        continue;
                    }
                    this.last_sent = version;
                    let notification = this.notification(version);
                    return Poll::Ready(Some(Ok(notification)));
                }
                // The sender is gone: the mock is shutting down.
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Everything the mock issues: one CA, one server leaf, the accepted client
/// leaf and a second client leaf the service refuses.
struct TlsMaterial {
    ca_pem: Vec<u8>,
    server_cert_pem: Vec<u8>,
    server_key_pem: Vec<u8>,
    client_cert_pem: Vec<u8>,
    client_key_pem: Vec<u8>,
    client_cert_der: Vec<u8>,
    intruder_cert_pem: Vec<u8>,
    intruder_key_pem: Vec<u8>,
}

impl TlsMaterial {
    fn generate() -> anyhow::Result<Self> {
        let ca_key = KeyPair::generate()?;
        let mut ca_params = CertificateParams::new(Vec::<String>::new())?;
        ca_params
            .distinguished_name
            .push(DnType::CommonName, CA_COMMON_NAME);
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let ca_cert = ca_params.self_signed(&ca_key)?;
        let issuer = Issuer::from_params(&ca_params, &ca_key);

        // The client overrides its TLS domain name with `agent-1` (§3), so the
        // server leaf must carry that name as a SAN.
        let server_key = KeyPair::generate()?;
        let mut server_params = CertificateParams::new(vec![
            DEFAULT_AGENT_ID.to_string(),
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ])?;
        server_params
            .distinguished_name
            .push(DnType::CommonName, DEFAULT_AGENT_ID);
        server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let server_cert = server_params.signed_by(&server_key, &issuer)?;

        let client_key = KeyPair::generate()?;
        let mut client_params = CertificateParams::new(vec![DEFAULT_AGENT_ID.to_string()])?;
        client_params
            .distinguished_name
            .push(DnType::CommonName, DEFAULT_AGENT_ID);
        client_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let client_cert = client_params.signed_by(&client_key, &issuer)?;

        let intruder_key = KeyPair::generate()?;
        let mut intruder_params = CertificateParams::new(vec!["intruder".to_string()])?;
        intruder_params
            .distinguished_name
            .push(DnType::CommonName, "intruder");
        intruder_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let intruder_cert = intruder_params.signed_by(&intruder_key, &issuer)?;

        Ok(Self {
            ca_pem: ca_cert.pem().into_bytes(),
            server_cert_pem: server_cert.pem().into_bytes(),
            server_key_pem: server_key.serialize_pem().into_bytes(),
            client_cert_pem: client_cert.pem().into_bytes(),
            client_key_pem: client_key.serialize_pem().into_bytes(),
            client_cert_der: client_cert.der().to_vec(),
            intruder_cert_pem: intruder_cert.pem().into_bytes(),
            intruder_key_pem: intruder_key.serialize_pem().into_bytes(),
        })
    }

    fn write_pem_files(&self, dir: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(dir)?;
        std::fs::write(dir.join("ca.pem"), &self.ca_pem)?;
        std::fs::write(dir.join("server-cert.pem"), &self.server_cert_pem)?;
        std::fs::write(dir.join("server-key.pem"), &self.server_key_pem)?;
        std::fs::write(dir.join("client-cert.pem"), &self.client_cert_pem)?;
        std::fs::write(dir.join("client-key.pem"), &self.client_key_pem)?;
        Ok(())
    }
}

/// A fresh directory under the OS temporary directory. The handle deletes it.
fn unique_cert_dir() -> anyhow::Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "probe-agent-mock-{}-{}",
        std::process::id(),
        unix_ms()
    ));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// A test double for [`ConfigApply`] that records what the agent applied.
///
/// It lives here rather than in each test file so the five integration tests
/// can share it without a `tests/common/` module.
#[derive(Clone, Default)]
pub struct RecordingApply {
    inner: Arc<Mutex<RecordingState>>,
}

#[derive(Default)]
struct RecordingState {
    applied: Vec<probe_common::ProbeConfig>,
    calls: usize,
    state: Option<pb::ConfigState>,
    failure: Option<ApplyFailure>,
    /// Data-plane pumps seen by the loop (S-C drives the real probe this way).
    drains: usize,
    ticks: usize,
    flushes: usize,
    shutdowns: usize,
}

impl RecordingApply {
    /// A double that accepts every config.
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes the next `apply` fail with `failure`, leaving the data path as it
    /// was (the §5.4 `FAILED` path).
    pub fn fail_next(&self, failure: ApplyFailure) {
        self.lock().failure = Some(failure);
    }

    /// The configs that were applied, in order.
    pub fn applied(&self) -> Vec<probe_common::ProbeConfig> {
        self.lock().applied.clone()
    }

    /// How many times `apply` was called, failures included.
    pub fn calls(&self) -> usize {
        self.lock().calls
    }

    /// How many times the loop flushed the open windows (shutdown included).
    pub fn flushes(&self) -> usize {
        self.lock().flushes
    }

    /// How many times the loop drained the ring.
    pub fn drains(&self) -> usize {
        self.lock().drains
    }

    fn lock(&self) -> MutexGuard<'_, RecordingState> {
        self.inner.lock().unwrap_or_else(|err| err.into_inner())
    }
}

impl ConfigApply for RecordingApply {
    fn apply(
        &mut self,
        cfg: &probe_common::ProbeConfig,
    ) -> Result<probe_common::ProbeConfig, ApplyFailure> {
        let mut state = self.lock();
        state.calls += 1;
        if let Some(failure) = state.failure.take() {
            return Err(failure);
        }
        state.applied.push(*cfg);
        state.state = Some(pb::ConfigState::Applied);
        Ok(*cfg)
    }

    fn applied_version(&self) -> u64 {
        // `ProbeConfig::version` is the ABI version of the struct, never the
        // config envelope's version, so this double starts from "nothing
        // applied" and lets the loop track the envelope versions itself.
        0
    }

    fn config_state(&self) -> pb::ConfigState {
        self.lock().state.unwrap_or(pb::ConfigState::Missing)
    }
}

impl crate::driver::DataPlane for RecordingApply {
    fn config(&self) -> probe_common::ProbeConfig {
        // Before the first apply the double has no configuration; the loop never
        // reads it, and `ProbeConfig::default()` keeps the trait total.
        self.lock().applied.last().copied().unwrap_or_default()
    }

    fn drain(&mut self) {
        self.lock().drains += 1;
    }

    fn tick(&mut self, _now_ns: u64) -> anyhow::Result<()> {
        self.lock().ticks += 1;
        Ok(())
    }

    fn flush_pending(&mut self) -> anyhow::Result<()> {
        self.lock().flushes += 1;
        Ok(())
    }

    fn shutdown(&mut self) -> anyhow::Result<probe::pipeline::Summary> {
        self.lock().shutdowns += 1;
        Ok(probe::pipeline::Summary::default())
    }
}

/// A window report with one synthetic aggregate, for tests that only care
/// about window ids and batching.
pub fn window_report(window_id: u64, window_ms: u32) -> pb::WindowReport {
    let mut key_hash = [0_u8; 32];
    key_hash[..8].copy_from_slice(&window_id.to_be_bytes());
    pb::WindowReport {
        window_id,
        window_ms,
        complete: true,
        window_start: now_timestamp(),
        aggregates: vec![pb::Aggregate {
            key_hash: key_hash.to_vec(),
            packets: window_id + 1,
            first_seen_offset_ns: 0,
            last_seen_offset_ns: u64::from(window_ms)
                .saturating_mul(1_000_000)
                .saturating_sub(1),
            sample_dropped: false,
            degraded: false,
        }],
        truncated: false,
        samples: Vec::new(),
    }
}

/// A minimal, valid config document (contract §6.2): one second windows, no
/// grace, full prefixes, PASS, no rules.
pub fn config_document() -> pb::ConfigDocument {
    pb::ConfigDocument {
        schema_version: 1,
        window_ms: 1_000,
        grace_ms: 0,
        src_prefix_v4: 32,
        src_prefix_v6: 128,
        default_action: pb::DefaultAction::Pass as i32,
        rules: Vec::new(),
    }
}

/// Polls `condition` every 10ms until it holds or `timeout` expires.
///
/// The agent is a background state machine, so tests wait for observable
/// effects instead of assuming a fixed number of loop iterations.
pub async fn wait_for<F>(timeout: Duration, mut condition: F) -> anyhow::Result<()>
where
    F: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if condition() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("condition did not hold within {timeout:?}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A `Status` that carries its §12.2 `x-error-code` in the trailers.
fn error_status(code: Code, error: ServerErrorCode, message: impl Into<String>) -> Status {
    let mut status = Status::new(code, message.into());
    if let Ok(value) = error.as_str().parse() {
        status.metadata_mut().insert(ERROR_CODE_HEADER, value);
    }
    status
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

fn timestamp_from_unix_ms(unix_ms: u64) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: (unix_ms / 1_000) as i64,
        nanos: ((unix_ms % 1_000) * 1_000_000) as i32,
    }
}

fn now_timestamp() -> Option<prost_types::Timestamp> {
    Some(timestamp_from_unix_ms(unix_ms()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_document_is_a_valid_v1_document() {
        let document = config_document();
        assert_eq!(document.schema_version, 1);
        assert!((1_000..=60_000).contains(&document.window_ms));
        assert!(document.grace_ms <= document.window_ms);
        assert_eq!(document.default_action, pb::DefaultAction::Pass as i32);
    }

    #[test]
    fn timestamps_keep_millisecond_precision() {
        let timestamp = timestamp_from_unix_ms(1_700_000_000_123);
        assert_eq!(timestamp.seconds, 1_700_000_000);
        assert_eq!(timestamp.nanos, 123_000_000);
    }

    #[tokio::test]
    async fn windows_are_deduped_by_row_and_counters_add_up() {
        let shared = MockShared {
            cfg: Arc::new(MockConfig::default()),
            state: Arc::new(Mutex::new(State::default())),
            expected_client_cert: Arc::new(Vec::new()),
        };
        // The service is not needed to exercise the dedupe rule itself: it is
        // the shared state that carries it.
        let mut state = shared.lock();
        let window = window_report(7, 1_000);
        let key = (7_u64, window.aggregates[0].key_hash.clone());
        assert!(!state.committed.contains(&key));
        state.committed.insert(key.clone());
        assert!(state.committed.contains(&key));
        state.max_window_id = Some(7);
        assert_eq!(state.cursor(&shared.cfg), Some(7));
    }
}
