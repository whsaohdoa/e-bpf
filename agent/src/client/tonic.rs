//! Mutual-TLS gRPC transport for [`ControlClient`].
//!
//! Three contract rules shape this module:
//!
//! - §2.6: every RPC has its own deadline and at most one call in flight. Seven
//!   `tokio::sync::Mutex`-guarded clients (one per class) give both: the lock
//!   serialises calls of one class, and cloning a `Channel` keeps heartbeats
//!   from queueing behind a 10s report.
//! - §2.3: every request carries an `x-request-id`, logged with the failure so
//!   one id can be traced across both ends.
//! - §12: retryability is decided by the status and refined by `x-error-code`;
//!   the four "exit" codes become [`ClientError::Fatal`] here, so no caller can
//!   accidentally retry a revoked agent.
//!
//! `connect_lazy` is deliberate: a control plane that is down at startup must
//! not panic the probe, and the caller already has a backoff schedule.

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use ::tonic::{
    Code, Status,
    metadata::MetadataValue,
    transport::{Certificate, Channel, ClientTlsConfig, Endpoint},
};
use async_trait::async_trait;

use super::{
    ClientError, ControlClient, MAX_DECODING_MESSAGE_SIZE, Rpc, WatchSession, WatchStream,
};
use crate::{error::ServerErrorCode, pb};

/// Contract §2.3: request correlation id.
pub const REQUEST_ID_HEADER: &str = "x-request-id";
/// Contract §12.2: machine-readable error code, a trailer on failures.
pub const ERROR_CODE_HEADER: &str = "x-error-code";
/// Contract §2.6: the server's suggested retry delay.
pub const RETRY_AFTER_HEADER: &str = "x-retry-after-ms";

/// Everything the client needs to reach the control plane.
///
/// The PEM blobs are bytes, not paths: the caller (S-C's `main`) reads them from
/// disk with `std::fs::read`, which keeps file handling — and its error paths —
/// out of the transport.
#[derive(Clone, Debug)]
pub struct ClientConfig {
    /// e.g. `https://127.0.0.1:9090`.
    pub endpoint: String,
    /// CA bundle that signed the server certificate.
    pub ca_pem: Vec<u8>,
    /// Client certificate (CN = `agent_id`).
    pub cert_pem: Vec<u8>,
    /// Private key matching `cert_pem`.
    pub key_pem: Vec<u8>,
    /// Expected server name; defaults to the endpoint's host. Overridable
    /// because tests run with a CN like `agent-1`, not `localhost`.
    pub domain_name: Option<String>,
}

impl ClientConfig {
    /// Builds a config from PEM bytes; `domain_name` defaults to the endpoint
    /// host.
    pub fn new(
        endpoint: impl Into<String>,
        ca_pem: impl Into<Vec<u8>>,
        cert_pem: impl Into<Vec<u8>>,
        key_pem: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            endpoint: endpoint.into(),
            ca_pem: ca_pem.into(),
            cert_pem: cert_pem.into(),
            key_pem: key_pem.into(),
            domain_name: None,
        }
    }

    /// Overrides the TLS server name (the mock's certificates carry a CN, not a
    /// DNS name).
    pub fn with_domain_name(mut self, domain_name: impl Into<String>) -> Self {
        self.domain_name = Some(domain_name.into());
        self
    }
}

/// [`ControlClient`] over a mutual-TLS HTTP/2 channel.
pub struct TonicClient {
    /// One client per [`Rpc`] class, so calls of one class are single-flight.
    clients: [tokio::sync::Mutex<pb::agent_control_client::AgentControlClient<Channel>>; 7],
    /// Source of the numeric tail of `x-request-id`.
    request_ids: AtomicU64,
    endpoint: String,
}

impl TonicClient {
    /// Builds an mTLS client. No connection is attempted until the first call.
    pub fn connect(cfg: &ClientConfig) -> Result<Self, ClientError> {
        let domain = cfg
            .domain_name
            .clone()
            .or_else(|| host_of(&cfg.endpoint))
            .ok_or_else(|| local_tls_error("endpoint has no host to verify"))?;

        let mut tls = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(&cfg.ca_pem))
            .domain_name(domain);
        // An empty certificate *and* key means "connect anonymously", which is
        // what a host without a provisioned identity does: the control plane
        // then rejects the handshake and the agent exits as
        // `agent.unauthenticated` instead of silently impersonating somebody.
        if !cfg.cert_pem.is_empty() || !cfg.key_pem.is_empty() {
            tls = tls.identity(::tonic::transport::Identity::from_pem(
                &cfg.cert_pem,
                &cfg.key_pem,
            ));
        }

        let channel = Endpoint::from_shared(cfg.endpoint.clone())
            .map_err(|e| local_tls_error(format!("invalid endpoint {}: {e}", cfg.endpoint)))?
            .tls_config(tls)
            .map_err(|e| local_tls_error(format!("invalid TLS material: {e}")))?
            .connect_lazy();

        Ok(Self {
            clients: std::array::from_fn(|_| {
                tokio::sync::Mutex::new(
                    pb::agent_control_client::AgentControlClient::new(channel.clone())
                        // §2.7: set explicitly, never trust the library default.
                        .max_decoding_message_size(MAX_DECODING_MESSAGE_SIZE)
                        .max_encoding_message_size(MAX_DECODING_MESSAGE_SIZE),
                )
            }),
            request_ids: AtomicU64::new(0),
            endpoint: cfg.endpoint.clone(),
        })
    }

    /// The endpoint this client was built for, for logs.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// §2.3 request id: process id plus a counter — unique enough to grep and
    /// cheaper than a UUID dependency.
    fn next_request_id(&self) -> String {
        let n = self.request_ids.fetch_add(1, Ordering::Relaxed);
        format!("{:x}-{:x}", std::process::id(), n)
    }
}

/// Runs one RPC body: builds the request with a fresh id, applies the §2.6
/// deadline and maps the outcome.
macro_rules! unary {
    ($self:expr, $rpc:expr, $req:expr, $method:ident) => {{
        let rpc = $rpc;
        let request_id = $self.next_request_id();
        let result = {
            // The guard is the single-flight lock of this class (§2.6).
            let mut client = $self.clients[rpc.index()].lock().await;
            let request = with_request_id($req, &request_id);
            let call = client.$method(request);
            match rpc.deadline() {
                Some(deadline) => match tokio::time::timeout(deadline, call).await {
                    Ok(result) => result,
                    Err(_) => Err(Status::deadline_exceeded(format!(
                        "{} exceeded its {:?} deadline",
                        rpc.name(),
                        deadline
                    ))),
                },
                None => call.await,
            }
        };
        result
            .map(::tonic::Response::into_inner)
            .map_err(|status| map_status(status, &request_id, rpc))
    }};
}

#[async_trait]
impl ControlClient for TonicClient {
    async fn register(
        &self,
        req: pb::RegisterRequest,
    ) -> Result<pb::RegisterResponse, ClientError> {
        unary!(self, Rpc::Register, req, register)
    }

    async fn heartbeat(
        &self,
        req: pb::HeartbeatRequest,
    ) -> Result<pb::HeartbeatResponse, ClientError> {
        unary!(self, Rpc::Heartbeat, req, heartbeat)
    }

    async fn get_cursor(
        &self,
        req: pb::GetCursorRequest,
    ) -> Result<pb::GetCursorResponse, ClientError> {
        unary!(self, Rpc::GetCursor, req, get_cursor)
    }

    async fn get_config(
        &self,
        req: pb::GetConfigRequest,
    ) -> Result<pb::GetConfigResponse, ClientError> {
        unary!(self, Rpc::GetConfig, req, get_config)
    }

    async fn ack_config(
        &self,
        req: pb::AckConfigRequest,
    ) -> Result<pb::AckConfigResponse, ClientError> {
        unary!(self, Rpc::AckConfig, req, ack_config)
    }

    async fn report_windows(
        &self,
        req: pb::ReportWindowsRequest,
    ) -> Result<pb::ReportWindowsResponse, ClientError> {
        unary!(self, Rpc::ReportWindows, req, report_windows)
    }

    async fn watch(&self, req: pb::WatchRequest) -> Result<Option<WatchStream>, ClientError> {
        let request_id = self.next_request_id();
        // Watch has no deadline (§2.6): it is a long-lived stream.
        let result = {
            let mut client = self.clients[Rpc::Watch.index()].lock().await;
            let request = with_request_id(req, &request_id);
            client.watch(request).await
        };
        match result {
            Ok(response) => Ok(Some(Box::new(TonicWatch {
                inner: response.into_inner(),
                request_id,
            }))),
            // §5.7: "this control plane has no push channel" is a legal answer,
            // not a failure.
            Err(status) if status.code() == Code::Unimplemented => Ok(None),
            Err(status) => Err(map_status(status, &request_id, Rpc::Watch)),
        }
    }
}

/// Server-streaming session over one [`::tonic::codec::Streaming`].
struct TonicWatch {
    inner: ::tonic::codec::Streaming<pb::ConfigNotification>,
    request_id: String,
}

#[async_trait]
impl WatchSession for TonicWatch {
    async fn next(&mut self) -> Option<Result<pb::ConfigNotification, ClientError>> {
        match self.inner.message().await {
            Ok(Some(notification)) => Some(Ok(notification)),
            // The server closed the stream; the caller reconnects with backoff.
            Ok(None) => None,
            Err(status) => Some(Err(map_status(status, &self.request_id, Rpc::Watch))),
        }
    }
}

/// Attaches the §2.3 correlation id.
fn with_request_id<T>(message: T, request_id: &str) -> ::tonic::Request<T> {
    let mut request = ::tonic::Request::new(message);
    if let Ok(value) = MetadataValue::try_from(request_id) {
        request.metadata_mut().insert(REQUEST_ID_HEADER, value);
    }
    request
}

/// §12.1 + §12.2: classify a failed call.
fn map_status(status: Status, request_id: &str, rpc: Rpc) -> ClientError {
    let metadata = status.metadata();
    let code = metadata
        .get(ERROR_CODE_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(ServerErrorCode::from_wire);
    let retry_after = metadata
        .get(RETRY_AFTER_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_millis);
    let message = status.message().to_owned();

    // A TLS handshake the control plane refuses (client certificate missing or
    // not the one it expects) reaches this layer as `Unknown` +
    // "transport error": the lazy channel's connect error is opaque and rustls'
    // alert never makes it into the status. It is the one failure the contract
    // names as fatal (§12.2 `agent.unauthenticated`), and reconnecting cannot
    // fix a rejected certificate, so it is classified here rather than being
    // retried as a transient network fault.
    let looks_like_tls_rejection =
        status.code() == Code::Unknown && message.contains("transport error");

    let fatal = looks_like_tls_rejection
        || matches!(
            status.code(),
            Code::Unauthenticated | Code::PermissionDenied
        )
        || matches!(
            code,
            Some(
                ServerErrorCode::AgentUnauthenticated
                    | ServerErrorCode::AgentIdentityMismatch
                    | ServerErrorCode::AgentRevoked
                    | ServerErrorCode::AgentMachineConflict
            )
        );

    if fatal {
        log::error!(
            "{} failed (request_id {request_id}): {} {message} — stopping",
            rpc.name(),
            code.map_or_else(|| status.code().to_string(), |code| code.to_string())
        );
        ClientError::Fatal {
            // A status-only fatal (e.g. no client certificate at all) carries no
            // trailer; `agent.unauthenticated` is the contract's name for it.
            code: code.unwrap_or(ServerErrorCode::AgentUnauthenticated),
            message,
        }
    } else {
        log::warn!(
            "{} failed (request_id {request_id}): {:?} {}{message}",
            rpc.name(),
            status.code(),
            code.map(|code| format!("({code}) ")).unwrap_or_default(),
        );
        ClientError::Transport {
            status: status.code(),
            code,
            retry_after,
            message,
        }
    }
}

/// Local TLS setup failures use the contract's name for "the certificate is
/// wrong" (§12.2 `agent.unauthenticated` → exit; retrying cannot help).
fn local_tls_error(message: impl std::fmt::Display) -> ClientError {
    ClientError::Fatal {
        code: ServerErrorCode::AgentUnauthenticated,
        message: message.to_string(),
    }
}

/// Host part of an endpoint URL, the default TLS server name.
fn host_of(endpoint: &str) -> Option<String> {
    let rest = endpoint
        .split_once("://")
        .map_or(endpoint, |(_, rest)| rest);
    // `dns:///host:port` leaves an empty first segment, so the leading slashes
    // of the path are not part of the authority.
    let authority = rest.trim_start_matches('/').split('/').next()?;
    // Bracketed IPv6 literals are not used by this contract (TLS server names
    // are DNS names or IP SANs), so a plain split is enough.
    let host = authority
        .rsplit_once(':')
        .map_or(authority, |(host, _port)| host);
    (!host.is_empty()).then(|| host.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_is_taken_from_the_endpoint() {
        assert_eq!(
            host_of("https://127.0.0.1:9090").as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(
            host_of("https://control.example:9090/path").as_deref(),
            Some("control.example")
        );
        assert_eq!(host_of("dns:///control:9090").as_deref(), Some("control"));
        assert_eq!(host_of("https://"), None);
    }

    #[test]
    fn status_without_trailers_is_a_plain_transport_error() {
        let err = map_status(
            Status::unavailable("connection refused"),
            "1-2",
            Rpc::Register,
        );
        assert!(!err.is_fatal());
        assert!(err.retryable());
        assert_eq!(err.code(), None);
        assert_eq!(err.retry_after(), None);
    }

    #[test]
    fn error_code_and_retry_after_come_from_trailers() {
        let mut status = Status::resource_exhausted("slow down");
        status
            .metadata_mut()
            .insert(ERROR_CODE_HEADER, "server.busy".parse().expect("ascii"));
        status
            .metadata_mut()
            .insert(RETRY_AFTER_HEADER, "1500".parse().expect("ascii"));
        let err = map_status(status, "1-2", Rpc::ReportWindows);
        assert_eq!(err.code(), Some(ServerErrorCode::ServerBusy));
        assert_eq!(err.retry_after(), Some(Duration::from_millis(1_500)));
        assert!(err.retryable());
    }

    #[test]
    fn revoked_is_fatal_even_though_failed_precondition_is_retryable_elsewhere() {
        let mut status = Status::failed_precondition("revoked");
        status
            .metadata_mut()
            .insert(ERROR_CODE_HEADER, "agent.revoked".parse().expect("ascii"));
        let err = map_status(status, "1-2", Rpc::Heartbeat);
        assert!(err.is_fatal());
        assert_eq!(err.code(), Some(ServerErrorCode::AgentRevoked));
    }

    #[test]
    fn unauthenticated_status_needs_no_trailer() {
        let err = map_status(Status::unauthenticated("bad cert"), "1-2", Rpc::Watch);
        assert!(err.is_fatal());
        assert_eq!(err.code(), Some(ServerErrorCode::AgentUnauthenticated));
    }
}
