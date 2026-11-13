//! Wire error codes from contract §12.2 and §12.3.
//!
//! The control plane is written in Java, so these strings cross a language
//! boundary: the exact spelling is part of the contract and is compared
//! character by character on the other side. `as_str` and `from_wire` are kept
//! adjacent, and a round-trip test covers every variant in both directions.

use std::error::Error;
use std::fmt;

/// Codes the agent reports in `AckConfigRequest.error_code` (contract §12.3).
///
/// A rejected config is acknowledged with the first code the six-step check
/// (§5.3) produced, so these variants mirror that order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AckErrorCode {
    StaleVersion,
    HashMismatch,
    SchemaUnsupported,
    InvalidDocument,
    UnsupportedDefaultAction,
    BpfWriteConfigFailed,
    BpfAttachFailed,
}

impl AckErrorCode {
    /// Every variant, in contract-table order (used by the round-trip test).
    pub const ALL: [Self; 7] = [
        Self::StaleVersion,
        Self::HashMismatch,
        Self::SchemaUnsupported,
        Self::InvalidDocument,
        Self::UnsupportedDefaultAction,
        Self::BpfWriteConfigFailed,
        Self::BpfAttachFailed,
    ];

    /// The `error_code` string of contract §12.3.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::StaleVersion => "config.stale_version",
            Self::HashMismatch => "config.hash_mismatch",
            Self::SchemaUnsupported => "config.schema_unsupported",
            Self::InvalidDocument => "config.invalid_document",
            Self::UnsupportedDefaultAction => "config.unsupported_default_action",
            Self::BpfWriteConfigFailed => "bpf.write_config_failed",
            Self::BpfAttachFailed => "bpf.attach_failed",
        }
    }

    /// Parses a wire string; `None` for codes this agent does not know.
    pub fn from_wire(s: &str) -> Option<Self> {
        Some(match s {
            "config.stale_version" => Self::StaleVersion,
            "config.hash_mismatch" => Self::HashMismatch,
            "config.schema_unsupported" => Self::SchemaUnsupported,
            "config.invalid_document" => Self::InvalidDocument,
            "config.unsupported_default_action" => Self::UnsupportedDefaultAction,
            "bpf.write_config_failed" => Self::BpfWriteConfigFailed,
            "bpf.attach_failed" => Self::BpfAttachFailed,
            _ => return None,
        })
    }
}

impl fmt::Display for AckErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for AckErrorCode {}

/// Codes the control plane reports in the `x-error-code` trailer (contract §12.2).
///
/// `report.window_too_old` is deliberately absent: it is a response field, not
/// an error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ServerErrorCode {
    RequestInvalid,
    RequestTooLarge,
    AgentUnauthenticated,
    AgentIdentityMismatch,
    AgentMachineConflict,
    AgentNotRegistered,
    AgentRevoked,
    AckVersionUnknown,
    ConfigTooLarge,
    ConfigInvalidDocument,
    ReportBatchTooLarge,
    ReportMalformed,
    ServerBusy,
    ServerError,
}

impl ServerErrorCode {
    /// Every variant, in contract-table order (used by the round-trip test).
    pub const ALL: [Self; 14] = [
        Self::RequestInvalid,
        Self::RequestTooLarge,
        Self::AgentUnauthenticated,
        Self::AgentIdentityMismatch,
        Self::AgentMachineConflict,
        Self::AgentNotRegistered,
        Self::AgentRevoked,
        Self::AckVersionUnknown,
        Self::ConfigTooLarge,
        Self::ConfigInvalidDocument,
        Self::ReportBatchTooLarge,
        Self::ReportMalformed,
        Self::ServerBusy,
        Self::ServerError,
    ];

    /// The `x-error-code` string of contract §12.2.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::RequestInvalid => "request.invalid",
            Self::RequestTooLarge => "request.too_large",
            Self::AgentUnauthenticated => "agent.unauthenticated",
            Self::AgentIdentityMismatch => "agent.identity_mismatch",
            Self::AgentMachineConflict => "agent.machine_conflict",
            Self::AgentNotRegistered => "agent.not_registered",
            Self::AgentRevoked => "agent.revoked",
            Self::AckVersionUnknown => "ack.version_unknown",
            Self::ConfigTooLarge => "config.too_large",
            Self::ConfigInvalidDocument => "config.invalid_document",
            Self::ReportBatchTooLarge => "report.batch_too_large",
            Self::ReportMalformed => "report.malformed",
            Self::ServerBusy => "server.busy",
            Self::ServerError => "server.error",
        }
    }

    /// Parses a wire string; `None` for codes this agent does not know.
    pub fn from_wire(s: &str) -> Option<Self> {
        Some(match s {
            "request.invalid" => Self::RequestInvalid,
            "request.too_large" => Self::RequestTooLarge,
            "agent.unauthenticated" => Self::AgentUnauthenticated,
            "agent.identity_mismatch" => Self::AgentIdentityMismatch,
            "agent.machine_conflict" => Self::AgentMachineConflict,
            "agent.not_registered" => Self::AgentNotRegistered,
            "agent.revoked" => Self::AgentRevoked,
            "ack.version_unknown" => Self::AckVersionUnknown,
            "config.too_large" => Self::ConfigTooLarge,
            "config.invalid_document" => Self::ConfigInvalidDocument,
            "report.batch_too_large" => Self::ReportBatchTooLarge,
            "report.malformed" => Self::ReportMalformed,
            "server.busy" => Self::ServerBusy,
            "server.error" => Self::ServerError,
            _ => return None,
        })
    }

    /// Whether the gRPC status behind this code is retryable (contract §12.1).
    ///
    /// This is transport-level advice, not the agent's whole policy: §12.2's
    /// "agent action" column is more specific and, for example, tells the agent
    /// not to retry `request.too_large` even though `RESOURCE_EXHAUSTED` as a
    /// status is retryable.
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::RequestTooLarge
                | Self::ReportBatchTooLarge
                | Self::ServerBusy
                | Self::ServerError
        )
    }
}

impl fmt::Display for ServerErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for ServerErrorCode {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ack_codes_round_trip() {
        for code in AckErrorCode::ALL {
            let wire = code.as_str();
            assert_eq!(AckErrorCode::from_wire(wire), Some(code), "wire={wire}");
            assert_eq!(code.to_string(), wire);
        }
    }

    #[test]
    fn server_codes_round_trip() {
        for code in ServerErrorCode::ALL {
            let wire = code.as_str();
            assert_eq!(ServerErrorCode::from_wire(wire), Some(code), "wire={wire}");
            assert_eq!(code.to_string(), wire);
        }
    }

    #[test]
    fn wire_strings_are_lowercase_snake_case() {
        // The contract spells every code in lowercase snake case with a dot
        // separator; a camelCase slip would only show up against the control
        // plane, so check the shape here too.
        let all = AckErrorCode::ALL
            .iter()
            .map(|c| c.as_str())
            .chain(ServerErrorCode::ALL.iter().map(|c| c.as_str()));
        for wire in all {
            let (prefix, suffix) = wire.split_once('.').expect("code has a prefix");
            assert!(
                !prefix.is_empty()
                    && !suffix.is_empty()
                    && wire.bytes().all(|b| b.is_ascii_lowercase()
                        || b.is_ascii_digit()
                        || b == b'.'
                        || b == b'_'),
                "unexpected wire spelling: {wire}"
            );
        }
    }

    #[test]
    fn unknown_wire_strings_are_rejected() {
        assert_eq!(AckErrorCode::from_wire("config.hashMismatch"), None);
        assert_eq!(AckErrorCode::from_wire(""), None);
        assert_eq!(ServerErrorCode::from_wire("report.window_too_old"), None);
        assert_eq!(ServerErrorCode::from_wire("server.busy "), None);
    }

    #[test]
    fn retryability_follows_status() {
        // §12.1: RESOURCE_EXHAUSTED and INTERNAL are retryable, the other four
        // statuses used by the table are not.
        for code in ServerErrorCode::ALL {
            let expected = matches!(
                code,
                ServerErrorCode::RequestTooLarge
                    | ServerErrorCode::ReportBatchTooLarge
                    | ServerErrorCode::ServerBusy
                    | ServerErrorCode::ServerError
            );
            assert_eq!(code.retryable(), expected, "code={code}");
        }
        assert!(!ServerErrorCode::RequestInvalid.retryable());
        assert!(!ServerErrorCode::AgentRevoked.retryable());
        assert!(ServerErrorCode::ServerBusy.retryable());
    }
}
