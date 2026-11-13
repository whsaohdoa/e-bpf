//! The control-plane client port.
//!
//! The state machine depends on [`ControlClient`], never on `tonic`, so tests
//! can drive it with a scripted fake and the mock control plane can reuse the
//! same trait. This module also owns the two tables both sides must agree on:
//! the per-RPC deadline (§2.6) and how a failure maps to "retry" or "exit"
//! (§12.1, §12.2).
//!
//! Two implementations exist:
//! - [`tonic::TonicClient`] (mutual TLS over HTTP/2);
//! - the mock control plane in `crate::mock`, which drives the real client
//!   against a real gRPC server on loopback.

use std::time::Duration;

use async_trait::async_trait;

use crate::{error::ServerErrorCode, pb};

pub mod tonic;

/// §2.7: `ReportWindowsRequest` may reach 4 MiB, so every message is capped at
/// that. The gRPC default is smaller, hence "set it explicitly" in the contract.
pub const MAX_DECODING_MESSAGE_SIZE: usize = 4 * 1024 * 1024;

/// The seven RPC classes of `AgentControl`.
///
/// Used as the key of the single-flight table and of the mock's call counters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rpc {
    /// §5.1 identity handshake.
    Register,
    /// §5.4 liveness and desired-version hint.
    Heartbeat,
    /// §5.5 window report batch.
    ReportWindows,
    /// §5.6 resume cursor.
    GetCursor,
    /// §5.3 configuration pull.
    GetConfig,
    /// §5.3 apply result.
    AckConfig,
    /// §5.7 push channel.
    Watch,
}

impl Rpc {
    /// Every class, in declaration order.
    pub const ALL: [Self; 7] = [
        Self::Register,
        Self::Heartbeat,
        Self::ReportWindows,
        Self::GetCursor,
        Self::GetConfig,
        Self::AckConfig,
        Self::Watch,
    ];

    /// The proto method name, for logs and mock bookkeeping.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Register => "Register",
            Self::Heartbeat => "Heartbeat",
            Self::ReportWindows => "ReportWindows",
            Self::GetCursor => "GetCursor",
            Self::GetConfig => "GetConfig",
            Self::AckConfig => "AckConfig",
            Self::Watch => "Watch",
        }
    }

    /// Slot in the single-flight table of [`tonic::TonicClient`].
    pub(crate) fn index(self) -> usize {
        match self {
            Self::Register => 0,
            Self::Heartbeat => 1,
            Self::ReportWindows => 4,
            Self::GetCursor => 5,
            Self::GetConfig => 2,
            Self::AckConfig => 3,
            Self::Watch => 6,
        }
    }

    /// Client-side deadline from §2.6, or `None` for the long-lived Watch
    /// stream, which is bounded by its reconnect backoff instead.
    pub fn deadline(&self) -> Option<Duration> {
        Some(match self {
            Self::Register => Duration::from_secs(5),
            Self::Heartbeat => Duration::from_secs(2),
            Self::ReportWindows => Duration::from_secs(10),
            Self::GetCursor => Duration::from_secs(3),
            Self::GetConfig => Duration::from_secs(5),
            Self::AckConfig => Duration::from_secs(5),
            Self::Watch => return None,
        })
    }
}

/// One in-flight Watch session.
///
/// A trait instead of a `Stream` so the agent needs no `futures` dependency and
/// the state machine can `select!` over `next()` like any other future.
#[async_trait]
pub trait WatchSession: Send {
    /// The next notification, or `None` once the server ended the stream.
    ///
    /// A transport error also ends the session: the caller reconnects with the
    /// Watch backoff (§5.7).
    async fn next(&mut self) -> Option<Result<pb::ConfigNotification, ClientError>>;
}

/// Boxed [`WatchSession`], the return type of [`ControlClient::watch`].
pub type WatchStream = Box<dyn WatchSession>;

/// The control plane as the state machine sees it: seven async calls, each
/// returning the proto response or a classified [`ClientError`].
///
/// Implementations must be usable from several tasks at once (`Send + Sync`);
/// single-flight per class is the implementation's business (§2.6).
#[async_trait]
pub trait ControlClient: Send + Sync {
    /// §5.1: idempotent registration, returns the server's view of this agent.
    async fn register(&self, req: pb::RegisterRequest)
    -> Result<pb::RegisterResponse, ClientError>;

    /// §5.4: liveness plus the server's desired version.
    async fn heartbeat(
        &self,
        req: pb::HeartbeatRequest,
    ) -> Result<pb::HeartbeatResponse, ClientError>;

    /// §5.6: the authoritative resume cursor.
    async fn get_cursor(
        &self,
        req: pb::GetCursorRequest,
    ) -> Result<pb::GetCursorResponse, ClientError>;

    /// §5.3: the desired configuration, or `up_to_date`.
    async fn get_config(
        &self,
        req: pb::GetConfigRequest,
    ) -> Result<pb::GetConfigResponse, ClientError>;

    /// §5.3: the apply result.
    async fn ack_config(
        &self,
        req: pb::AckConfigRequest,
    ) -> Result<pb::AckConfigResponse, ClientError>;

    /// §5.5: one batch of closed windows.
    async fn report_windows(
        &self,
        req: pb::ReportWindowsRequest,
    ) -> Result<pb::ReportWindowsResponse, ClientError>;

    /// §5.7: opens the push channel.
    ///
    /// `Ok(None)` means the server does not implement Watch (`UNIMPLEMENTED`);
    /// that is a legal deployment, not a failure, and the caller falls back to
    /// polling via heartbeats.
    async fn watch(&self, req: pb::WatchRequest) -> Result<Option<WatchStream>, ClientError>;
}

/// A failed call, classified per §12.
///
/// Exactly two shapes on purpose: everything the agent may retry, and the four
/// conditions after which retrying is either pointless or forbidden.
#[derive(Clone, Debug)]
pub enum ClientError {
    /// A call that failed and may be retried per §2.6's backoff table.
    ///
    /// `code` is the `x-error-code` trailer when the server sent one; it is
    /// more precise than the gRPC status (e.g. `server.busy` under
    /// `RESOURCE_EXHAUSTED`), so it wins when classifying retryability.
    Transport {
        /// gRPC status.
        status: ::tonic::Code,
        /// Machine-readable code from `x-error-code`, if present.
        code: Option<ServerErrorCode>,
        /// `x-retry-after-ms`, if the server asked for a specific delay.
        retry_after: Option<Duration>,
        /// Human-readable message, also logged.
        message: String,
    },
    /// Retrying cannot help, or must not happen (§12.2): the agent stops.
    Fatal {
        /// Why the agent must exit.
        code: ServerErrorCode,
        /// Human-readable message, also logged.
        message: String,
    },
}

impl ClientError {
    /// §12.1: whether the caller may retry this call after a backoff.
    ///
    /// A known `x-error-code` overrides the coarse gRPC status: the contract
    /// makes `server.busy` retryable while `agent.revoked` is fatal even though
    /// both arrive with statuses that would otherwise say "retry".
    pub fn retryable(&self) -> bool {
        match self {
            Self::Fatal { .. } => false,
            Self::Transport { status, code, .. } => match code {
                Some(code) => code.retryable(),
                None => matches!(
                    status,
                    ::tonic::Code::Unavailable
                        | ::tonic::Code::DeadlineExceeded
                        | ::tonic::Code::ResourceExhausted
                        | ::tonic::Code::Internal
                ),
            },
        }
    }

    /// §12.2: whether the agent must stop (exit non-zero, data path untouched).
    pub fn is_fatal(&self) -> bool {
        matches!(self, Self::Fatal { .. })
    }

    /// The server's suggested delay, when it sent `x-retry-after-ms`.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Transport { retry_after, .. } => *retry_after,
            Self::Fatal { .. } => None,
        }
    }

    /// The machine-readable code, when one is known.
    pub fn code(&self) -> Option<ServerErrorCode> {
        match self {
            Self::Transport { code, .. } => *code,
            Self::Fatal { code, .. } => Some(*code),
        }
    }

    /// The gRPC status, when the failure came from a call.
    pub fn status(&self) -> Option<::tonic::Code> {
        match self {
            Self::Transport { status, .. } => Some(*status),
            Self::Fatal { .. } => None,
        }
    }

    /// A log-friendly rendering of the failure.
    pub fn message(&self) -> &str {
        match self {
            Self::Transport { message, .. } | Self::Fatal { message, .. } => message,
        }
    }
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport {
                status,
                code,
                retry_after,
                message,
            } => {
                write!(f, "transport error {status:?}")?;
                if let Some(code) = code {
                    write!(f, " ({code})")?;
                }
                if let Some(delay) = retry_after {
                    write!(f, " retry after {delay:?}")?;
                }
                write!(f, ": {message}")
            }
            Self::Fatal { code, message } => write!(f, "fatal {code}: {message}"),
        }
    }
}

impl std::error::Error for ClientError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn transport(status: ::tonic::Code, code: Option<ServerErrorCode>) -> ClientError {
        ClientError::Transport {
            status,
            code,
            retry_after: None,
            message: "test".to_string(),
        }
    }

    #[test]
    fn deadline_table_matches_the_contract() {
        // §2.6, transcribed: Register 5s, Heartbeat 2s, ReportWindows 10s,
        // GetCursor 3s, GetConfig 5s, AckConfig 5s, Watch unbounded.
        let table = [
            (Rpc::Register, Some(5_000)),
            (Rpc::Heartbeat, Some(2_000)),
            (Rpc::ReportWindows, Some(10_000)),
            (Rpc::GetCursor, Some(3_000)),
            (Rpc::GetConfig, Some(5_000)),
            (Rpc::AckConfig, Some(5_000)),
            (Rpc::Watch, None),
        ];
        assert_eq!(table.len(), Rpc::ALL.len());
        for (rpc, millis) in table {
            assert_eq!(
                rpc.deadline().map(|d| d.as_millis() as u64),
                millis,
                "{}",
                rpc.name()
            );
        }
        assert!(Rpc::ALL.contains(&Rpc::Watch));
    }

    #[test]
    fn retryability_follows_the_status_without_a_code() {
        assert!(transport(::tonic::Code::Unavailable, None).retryable());
        assert!(transport(::tonic::Code::DeadlineExceeded, None).retryable());
        assert!(transport(::tonic::Code::ResourceExhausted, None).retryable());
        assert!(transport(::tonic::Code::Internal, None).retryable());
        // §12.1: everything else is a bug or a version skew, not a retry.
        assert!(!transport(::tonic::Code::InvalidArgument, None).retryable());
        assert!(!transport(::tonic::Code::NotFound, None).retryable());
        assert!(!transport(::tonic::Code::Unimplemented, None).retryable());
    }

    #[test]
    fn a_code_overrides_the_status() {
        assert!(
            transport(
                ::tonic::Code::ResourceExhausted,
                Some(ServerErrorCode::ServerBusy)
            )
            .retryable()
        );
        // `agent.revoked` arrives with a status that is retryable elsewhere.
        let revoked = transport(
            ::tonic::Code::Unavailable,
            Some(ServerErrorCode::AgentRevoked),
        );
        assert!(!revoked.retryable());
    }

    #[test]
    fn fatal_is_never_retryable() {
        let fatal = ClientError::Fatal {
            code: ServerErrorCode::AgentRevoked,
            message: "revoked".to_string(),
        };
        assert!(!fatal.retryable());
        assert!(fatal.is_fatal());
        assert_eq!(fatal.retry_after(), None);
        assert_eq!(fatal.status(), None);
        assert_eq!(fatal.code(), Some(ServerErrorCode::AgentRevoked));
    }
}
