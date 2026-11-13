//! Pure logic of the control agent (stage S-A).
//!
//! This crate has no transport and no kernel access: it decodes a
//! `ConfigEnvelope` document, checks it in the contract's six-step order,
//! converts it into the data path's `ProbeConfig`, and computes the `key_hash`
//! used to anonymise aggregation keys in reports. Everything here is
//! deterministic and testable without a network, a clock or a kernel.
//!
//! Contract: `docs/Control-Agent-API-v1.md` (kept outside this repository, see
//! `AGENTS.md`).

pub mod cidr;
pub mod config;
pub mod error;
pub mod keyhash;

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
