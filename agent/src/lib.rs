//! Pure logic and control-plane plumbing of the control agent (stages S-A/S-B).
//!
//! The S-A half has no transport and no kernel access: it decodes a
//! `ConfigEnvelope` document, checks it in the contract's six-step order,
//! converts it into the data path's `ProbeConfig`, and computes the `key_hash`
//! used to anonymise aggregation keys in reports.
//!
//! The S-B half talks to a control plane over gRPC + mutual TLS
//! ([`client`]), keeps the call schedule of contract §2.6 ([`backoff`]) and
//! runs the register → cursor → config → apply → report state machine of §8
//! ([`agent_loop`], [`report`]).
//!
//! The S-C half attaches the real data path ([`driver`], [`crate::driver::DataPlane`])
//! and drives it from the same loop; the agent never blocks the data path.
//!
//! Contract: `docs/Control-Agent-API-v1.md` (in this repository; see
//! `AGENTS.md` for the doc index).

pub mod agent_loop;
pub mod backoff;
pub mod cidr;
pub mod client;
pub mod clock;
pub mod config;
pub mod driver;
pub mod error;
pub mod keyhash;
pub mod report;

/// In-process mock control plane, used by this crate's integration tests and by
/// the `control-mock` binary. Test scaffolding only — never deployed.
#[cfg(feature = "mock")]
pub mod mock;

/// Types generated from `proto/control.proto` (contract §4).
///
/// `prost-build` names the output file after the proto package and does not
/// wrap it in a module, so it is included inside this one.
pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/control.agent.v1.rs"));
}

/// Small helpers shared by the unit tests of this crate.
#[cfg(test)]
pub(crate) mod test_util {
    /// Decodes lowercase or uppercase hex; panics on malformed input, which can
    /// only mean a typo in a test vector.
    pub fn hex_to_bytes(hex: &str) -> Vec<u8> {
        assert_eq!(hex.len() % 2, 0, "hex string must have an even length");
        hex.as_bytes()
            .chunks(2)
            .map(|pair| {
                let hi = (pair[0] as char).to_digit(16).expect("hex digit") as u8;
                let lo = (pair[1] as char).to_digit(16).expect("hex digit") as u8;
                (hi << 4) | lo
            })
            .collect()
    }

    /// Lowercase hex, the form used by the contract vectors.
    pub fn bytes_to_hex(bytes: &[u8]) -> String {
        use std::fmt::Write;

        let mut out = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            write!(out, "{byte:02x}").expect("writing to a String cannot fail");
        }
        out
    }
}
