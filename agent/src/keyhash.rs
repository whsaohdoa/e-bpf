//! `key_hash`: the 32-byte opaque key the control plane stores per aggregate.
//!
//! Layout (contract §7.2, 56 bytes total):
//!
//! | offset    | bytes | field                                |
//! |-----------|-------|--------------------------------------|
//! | 0..13     | 13    | magic `probe.agg.v1\0`               |
//! | 13        | 1     | direction                            |
//! | 14        | 1     | family                               |
//! | 15        | 1     | action                               |
//! | 16        | 1     | reason                               |
//! | 17..21    | 4     | rule_index, big-endian, 1-based      |
//! | 21..37    | 16    | src_prefix                           |
//! | 37..53    | 16    | dst_addr                             |
//! | 53..55    | 2     | dst_port, big-endian                 |
//! | 55        | 1     | protocol                             |
//!
//! Preconditions, because the hash must agree with the control plane:
//!
//! - `k.src_prefix` has already been masked with the config's prefix length by
//!   the data path. `AggKey` carries no `prefix_len`, so this module must not
//!   mask again — doing so would silently disagree on host bits.
//! - IPv4 addresses stay left-aligned in their 16-byte field. They are *not*
//!   rewritten into v4-mapped-v6 form.
//! - `window_id` is not part of the hash: aggregation windows are reported, not
//!   identified.

use probe_common::AggKey;
use sha2::{Digest, Sha256};

/// Domain-separation prefix of every hash input (contract §7.2).
pub const MAGIC: &[u8; 13] = b"probe.agg.v1\0";

/// Serialises an aggregation key into the exact 56 bytes that get hashed.
pub fn key_hash_input(k: &AggKey) -> [u8; 56] {
    let mut out = [0u8; 56];
    out[..13].copy_from_slice(MAGIC);
    out[13] = k.direction;
    out[14] = k.family;
    out[15] = k.action;
    out[16] = k.reason;
    out[17..21].copy_from_slice(&k.rule_id.to_be_bytes());
    out[21..37].copy_from_slice(&k.src_prefix);
    out[37..53].copy_from_slice(&k.dst_addr);
    out[53..55].copy_from_slice(&k.dst_port.to_be_bytes());
    out[55] = k.protocol;
    out
}

/// SHA-256 of [`key_hash_input`].
pub fn key_hash(k: &AggKey) -> [u8; 32] {
    let digest = Sha256::digest(key_hash_input(k));
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// Lowercase hex form of [`key_hash`], as used in reports.
pub fn key_hash_hex(k: &AggKey) -> String {
    use std::fmt::Write;

    let mut out = String::with_capacity(64);
    for byte in key_hash(k) {
        write!(out, "{byte:02x}").expect("writing to a String cannot fail");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{bytes_to_hex, hex_to_bytes};
    use probe_common::{ACTION_BLOCK, ACTION_PASS, DIRECTION_INGRESS, FAMILY_IPV4, FAMILY_IPV6};

    // Contract §7.3 vectors. Do not edit a single hex digit.
    const INPUT_A: &str = "70726f62652e6167672e76310001040000000000000a0001000000000000000000000000000a0002050000000000000000000000001f9006";
    const HASH_A: &str = "2bd3476570235caf978971eac565dc1e63931786ae3c817bd31ea795c3e23c4a";
    const INPUT_B: &str = "70726f62652e6167672e76310001040101000000010a0001000000000000000000000000000a0002050000000000000000000000001f9006";
    const HASH_B: &str = "2da4fcf437006d16db67fb98e71cc0d6e5248715e11ef6d4811786a7b6f8f115";
    const INPUT_C: &str = "70726f62652e6167672e7631000106000000000000fd000099000000000000000000000000fd00009900000000000000000000000114e911";
    const HASH_C: &str = "d0e9cc4a039d43a800cfeb64950ec348e59b3089c84914a5d8cde15f5c775624";

    fn v4(octets: [u8; 4]) -> [u8; 16] {
        let mut addr = [0u8; 16];
        addr[..4].copy_from_slice(&octets);
        addr
    }

    fn key_a() -> AggKey {
        AggKey {
            // Deliberately arbitrary: window_id is not hashed.
            window_id: 123_456,
            rule_id: 0,
            direction: DIRECTION_INGRESS,
            family: FAMILY_IPV4,
            src_prefix: v4([10, 0, 1, 0]),
            dst_addr: v4([10, 0, 2, 5]),
            dst_port: 8080,
            protocol: 6,
            action: ACTION_PASS,
            reason: 0,
            pad: [0; 5],
        }
    }

    fn key_b() -> AggKey {
        AggKey {
            action: ACTION_BLOCK,
            reason: 1,
            rule_id: 1,
            ..key_a()
        }
    }

    fn key_c() -> AggKey {
        AggKey {
            window_id: 7,
            rule_id: 0,
            direction: DIRECTION_INGRESS,
            family: FAMILY_IPV6,
            src_prefix: [0xfd, 0x00, 0x00, 0x99, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            dst_addr: [0xfd, 0x00, 0x00, 0x99, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
            dst_port: 5353,
            protocol: 17,
            action: ACTION_PASS,
            reason: 0,
            pad: [0; 5],
        }
    }

    #[test]
    fn input_matches_contract_vector_a() {
        let input = key_hash_input(&key_a());
        assert_eq!(input.len(), 56);
        assert_eq!(bytes_to_hex(&input), INPUT_A);
    }

    #[test]
    fn input_layout_offsets() {
        let input = key_hash_input(&key_a());
        assert_eq!(&input[..13], MAGIC);
        assert_eq!(input[13], 1, "direction");
        assert_eq!(input[14], 4, "family");
        assert_eq!(input[15], 0, "action");
        assert_eq!(input[16], 0, "reason");
        assert_eq!(
            &input[17..21],
            &[0u8, 0, 0, 0],
            "rule_index 0 = no rule hit"
        );
        assert_eq!(
            &input[21..25],
            &[10u8, 0, 1, 0],
            "src_prefix is left-aligned"
        );
        assert_eq!(&input[37..41], &[10u8, 0, 2, 5], "dst_addr is left-aligned");
        assert_eq!(&input[53..55], &[0x1Fu8, 0x90], "dst_port big-endian");
        assert_eq!(input[55], 6, "protocol");
    }

    #[test]
    fn hash_matches_contract_vector_a() {
        assert_eq!(bytes_to_hex(&key_hash(&key_a())), HASH_A);
        assert_eq!(key_hash_hex(&key_a()), HASH_A);
    }

    #[test]
    fn hash_matches_contract_vector_b() {
        assert_eq!(bytes_to_hex(&key_hash_input(&key_b())), INPUT_B);
        assert_eq!(key_hash_hex(&key_b()), HASH_B);
    }

    #[test]
    fn hash_matches_contract_vector_c() {
        assert_eq!(bytes_to_hex(&key_hash_input(&key_c())), INPUT_C);
        assert_eq!(key_hash_hex(&key_c()), HASH_C);
    }

    #[test]
    fn a_and_b_differ_only_in_action_reason_and_rule_index() {
        let a = key_hash_input(&key_a());
        let b = key_hash_input(&key_b());
        let diff: Vec<usize> = a
            .iter()
            .zip(b.iter())
            .enumerate()
            .filter(|(_, (x, y))| x != y)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(diff, vec![15, 16, 20], "bytes 15/16/17..21 of the layout");
        assert_ne!(key_hash(&key_a()), key_hash(&key_b()));
    }

    #[test]
    fn window_id_is_not_hashed() {
        let mut other = key_a();
        other.window_id = 0;
        assert_eq!(key_hash_input(&other), key_hash_input(&key_a()));
        assert_eq!(key_hash_hex(&other), HASH_A);
    }

    #[test]
    fn hex_helpers_round_trip() {
        assert_eq!(bytes_to_hex(&hex_to_bytes(INPUT_C)), INPUT_C);
        assert_eq!(hex_to_bytes(INPUT_A).len(), 56);
    }
}
