//! Validation of a `ConfigEnvelope` and conversion to `probe_common::ProbeConfig`
//! (contract §5.3, §6.2, §6.3).
//!
//! The order of the six checks is itself part of the contract: the control
//! plane and the agent run the same sequence, so on the same bad input both
//! sides must name the same error code. Every check therefore returns
//! immediately, and a document that violates two rules reports the earlier one.
//!
//! Train 0 can apply window and prefix configuration but not rules; §6.3 calls
//! that a *degraded accept*: the config is installed, `["rules"]` is reported
//! back, and the data path keeps running its current behaviour (fail-open, see
//! contract §10 L1).

use std::{collections::HashSet, error::Error, fmt};

use probe_common::{CONFIG_VERSION, ProbeConfig};
use prost::Message;
use sha2::{Digest, Sha256};

use crate::{cidr::parse_cidr, error::AckErrorCode, pb};

/// `AckConfigRequest.error_detail` is limited to 512 characters.
const DETAIL_MAX_CHARS: usize = 512;

/// At most 20 rules per document (contract §6.2).
const MAX_RULES: usize = 20;

/// `rule_id` is `^[a-z0-9][a-z0-9._-]{0,31}$` (contract §6.2).
const RULE_ID_PATTERN: &str = "^[a-z0-9][a-z0-9._-]{0,31}$";
const MAX_RULE_ID_BYTES: usize = 32;

/// A config the agent refuses to apply, with the code it will acknowledge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    /// Code the agent sends back in `AckConfigRequest.error_code` (§12.3).
    pub code: AckErrorCode,
    /// Human-readable reason, at most 512 characters (§12.3).
    pub detail: String,
}

impl Rejection {
    /// Builds a rejection, truncating `detail` to the contract's limit.
    pub fn new(code: AckErrorCode, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        let detail = if detail.chars().count() > DETAIL_MAX_CHARS {
            detail.chars().take(DETAIL_MAX_CHARS).collect()
        } else {
            detail
        };
        Self { code, detail }
    }
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}

impl Error for Rejection {}

/// A config that passed all six checks.
#[derive(Debug, Clone)]
pub struct ValidatedConfig {
    /// What the data path will install. `version` is the ABI version, not the
    /// envelope's config version.
    pub probe_config: ProbeConfig,
    /// The decoded document, kept for callers that need the original text
    /// fields (prefix lengths are already in `probe_config`).
    pub document: pb::ConfigDocument,
    /// Features accepted but not applicable in Train 0; `["rules"]` or empty.
    pub degraded: Vec<&'static str>,
    /// Number of rules in the document, for the ack's diagnostics.
    pub rule_count: usize,
}

/// Runs the six checks of contract §5.3 and converts the document.
///
/// `current_version` is the version the agent currently has applied (0 when
/// none), and `supported_schemas` lists the document schema versions this
/// build understands.
pub fn validate(
    env: &pb::ConfigEnvelope,
    current_version: u64,
    supported_schemas: &[u32],
) -> Result<ValidatedConfig, Rejection> {
    // 1. The version must move forward, otherwise the agent would re-apply an
    //    older config after a reconnect.
    if env.version <= current_version {
        return Err(Rejection::new(
            AckErrorCode::StaleVersion,
            format!(
                "envelope version {} is not newer than the applied version {current_version}",
                env.version
            ),
        ));
    }

    // 2. Schema negotiation.
    if !supported_schemas.contains(&env.schema_version) {
        return Err(Rejection::new(
            AckErrorCode::SchemaUnsupported,
            format!(
                "schema {} is not supported (supported: {supported_schemas:?})",
                env.schema_version
            ),
        ));
    }

    // 3. Integrity, over the bytes as received: re-serialising first would make
    //    the hash depend on our own encoder.
    let hash_len = env.content_sha256.len();
    if hash_len != 32 {
        return Err(Rejection::new(
            AckErrorCode::HashMismatch,
            format!("content_sha256 is {hash_len} bytes, expected 32"),
        ));
    }
    let actual = Sha256::digest(&env.document);
    if actual.as_slice() != env.content_sha256.as_slice() {
        return Err(Rejection::new(
            AckErrorCode::HashMismatch,
            format!(
                "content_sha256 {} does not match document digest {}",
                hex(&env.content_sha256),
                hex(&actual)
            ),
        ));
    }

    // 4. Protobuf decoding.
    let document = pb::ConfigDocument::decode(env.document.as_slice()).map_err(|err| {
        Rejection::new(
            AckErrorCode::InvalidDocument,
            format!("document does not decode as ConfigDocument: {err}"),
        )
    })?;

    // 5. Value ranges.
    check_document(&document, env.schema_version)?;

    // 6. Capability trimming.
    if document.default_action != pb::DefaultAction::Pass as i32 {
        return Err(Rejection::new(
            AckErrorCode::UnsupportedDefaultAction,
            format!(
                "default_action {} is not supported in Train 0; only PASS is accepted",
                document.default_action
            ),
        ));
    }

    let rule_count = document.rules.len();
    let degraded = if rule_count == 0 {
        Vec::new()
    } else {
        vec!["rules"]
    };

    Ok(ValidatedConfig {
        probe_config: to_probe_config(&document),
        document,
        degraded,
        rule_count,
    })
}

/// Step 5: every §6.2 range, in document order.
fn check_document(document: &pb::ConfigDocument, envelope_schema: u32) -> Result<(), Rejection> {
    if document.schema_version != envelope_schema {
        return Err(invalid(format!(
            "document schema_version {} does not match envelope schema_version {envelope_schema}",
            document.schema_version
        )));
    }
    if !(1_000..=60_000).contains(&document.window_ms) {
        return Err(invalid(format!(
            "window_ms {} is outside 1000..=60000",
            document.window_ms
        )));
    }
    if document.grace_ms > document.window_ms {
        return Err(invalid(format!(
            "grace_ms {} exceeds window_ms {}",
            document.grace_ms, document.window_ms
        )));
    }
    if document.src_prefix_v4 > 32 {
        return Err(invalid(format!(
            "src_prefix_v4 {} is outside 0..=32",
            document.src_prefix_v4
        )));
    }
    if document.src_prefix_v6 > 128 {
        return Err(invalid(format!(
            "src_prefix_v6 {} is outside 0..=128",
            document.src_prefix_v6
        )));
    }
    if document.rules.len() > MAX_RULES {
        return Err(invalid(format!(
            "{} rules exceed the limit of {MAX_RULES}",
            document.rules.len()
        )));
    }

    let mut rule_ids: HashSet<&str> = HashSet::with_capacity(document.rules.len());
    let mut priorities: HashSet<u32> = HashSet::with_capacity(document.rules.len());
    for (index, rule) in document.rules.iter().enumerate() {
        let expected_index = index as u32 + 1;
        if rule.rule_index != expected_index {
            return Err(invalid(format!(
                "rules[{index}] has rule_index {}, expected {expected_index}",
                rule.rule_index
            )));
        }
        check_rule_id(index, &rule.rule_id)?;
        if !rule_ids.insert(rule.rule_id.as_str()) {
            return Err(invalid(format!(
                "rules[{index}] repeats rule_id {:?}",
                rule.rule_id
            )));
        }
        if !(1..=1_000).contains(&rule.priority) {
            return Err(invalid(format!(
                "rules[{index}] has priority {} outside 1..=1000",
                rule.priority
            )));
        }
        if !priorities.insert(rule.priority) {
            return Err(invalid(format!(
                "rules[{index}] repeats priority {}",
                rule.priority
            )));
        }
        check_match(index, rule)?;
    }

    Ok(())
}

/// `rule_id` syntax (the uniqueness check lives in [`check_document`]).
fn check_rule_id(index: usize, rule_id: &str) -> Result<(), Rejection> {
    let mut bytes = rule_id.bytes();
    let leading_ok = bytes.next().is_some_and(is_rule_id_start);
    let valid =
        leading_ok && rule_id.len() <= MAX_RULE_ID_BYTES && bytes.all(is_rule_id_continuation);
    if !valid {
        return Err(invalid(format!(
            "rules[{index}] has rule_id {rule_id:?} that does not match {RULE_ID_PATTERN}"
        )));
    }
    Ok(())
}

fn is_rule_id_start(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit()
}

fn is_rule_id_continuation(byte: u8) -> bool {
    is_rule_id_start(byte) || matches!(byte, b'.' | b'_' | b'-')
}

/// Rule match fields: enum values must be known and addresses must parse.
///
/// An absent `match` means "any", so it carries nothing to check.
fn check_match(index: usize, rule: &pb::Rule) -> Result<(), Rejection> {
    let Some(rule_match) = rule.r#match.as_ref() else {
        return Ok(());
    };

    if pb::Family::try_from(rule_match.family).is_err() {
        return Err(invalid(format!(
            "rules[{index}] match.family {} is not a known Family value",
            rule_match.family
        )));
    }
    if pb::L4Protocol::try_from(rule_match.protocol).is_err() {
        return Err(invalid(format!(
            "rules[{index}] match.protocol {} is not a known L4Protocol value",
            rule_match.protocol
        )));
    }
    if let Some(text) = rule_match.src_prefix.as_deref() {
        parse_cidr(text).map_err(|err| {
            invalid(format!(
                "rules[{index}] src_prefix {text:?} is invalid: {err}"
            ))
        })?;
    }
    if let Some(text) = rule_match.dst_addr.as_deref() {
        parse_cidr(text).map_err(|err| {
            invalid(format!(
                "rules[{index}] dst_addr {text:?} is invalid: {err}"
            ))
        })?;
    }
    for port in &rule_match.dst_ports {
        if !(1..=65_535).contains(port) {
            return Err(invalid(format!(
                "rules[{index}] dst_ports contains {port} outside 1..=65535"
            )));
        }
    }

    Ok(())
}

/// Converts a validated document into the data path's config.
fn to_probe_config(document: &pb::ConfigDocument) -> ProbeConfig {
    ProbeConfig {
        // The ABI version of `ProbeConfig`, not the config's own version: these
        // two numbers share a name and never carry the same value.
        version: CONFIG_VERSION,
        window_ns: u64::from(document.window_ms) * 1_000_000,
        grace_ns: u64::from(document.grace_ms) * 1_000_000,
        // Ranges were checked above, so the narrowing cannot lose data.
        ipv4_prefix_len: document.src_prefix_v4 as u8,
        ipv6_prefix_len: document.src_prefix_v6 as u8,
        default_action: pb::DefaultAction::Pass as u8,
        pad: [0; 5],
    }
}

fn invalid(detail: String) -> Rejection {
    Rejection::new(AckErrorCode::InvalidDocument, detail)
}

/// Lowercase hex, used only in the hash-mismatch detail.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;

    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(out, "{byte:02x}").expect("writing to a String cannot fail");
    }
    out
}

#[cfg(test)]
mod tests {
    use probe_common::FAMILY_IPV6;

    use super::*;
    use crate::test_util::{bytes_to_hex, hex_to_bytes};

    // Contract §6.4 vectors. Do not edit a single hex digit.
    const DOC_A: &str = "080110904e18d00f20182840";
    const DOC_A_SHA: &str = "971d3944b510ab8ea6cbc469ccae565c9ad0a60f27801e9cad052adbe4253c05";
    const DOC_B: &str = "080110904e18d00f201828403a3108011206722d38303830180a20012a1f080410061a0b31302e302e312e302f3234220831302e302e322e352a02903f3001";
    const DOC_B_SHA: &str = "c3e4eea7f545643b966956fbea32fbbc3fb44acc019999dd5ed9552940b3d30f";

    const SUPPORTED: &[u32] = &[1];

    fn base_document() -> pb::ConfigDocument {
        pb::ConfigDocument {
            schema_version: 1,
            window_ms: 10_000,
            grace_ms: 2_000,
            src_prefix_v4: 24,
            src_prefix_v6: 64,
            default_action: pb::DefaultAction::Pass as i32,
            rules: Vec::new(),
        }
    }

    fn rule(index: u32) -> pb::Rule {
        pb::Rule {
            rule_index: index,
            rule_id: format!("r-{index}"),
            priority: index,
            enabled: true,
            r#match: Some(pb::RuleMatch {
                family: pb::Family::Ipv4 as i32,
                protocol: pb::L4Protocol::Tcp as i32,
                src_prefix: Some("10.0.1.0/24".to_owned()),
                dst_addr: Some("10.0.2.5".to_owned()),
                dst_ports: vec![8080],
            }),
            action: pb::Action::Block as i32,
        }
    }

    fn envelope(version: u64, schema_version: u32, document: Vec<u8>) -> pb::ConfigEnvelope {
        pb::ConfigEnvelope {
            version,
            schema_version,
            content_sha256: Sha256::digest(&document).to_vec(),
            document,
            published_at: None,
        }
    }

    fn envelope_for(document: &pb::ConfigDocument) -> pb::ConfigEnvelope {
        envelope(1, 1, document.encode_to_vec())
    }

    fn assert_invalid(document: &pb::ConfigDocument) -> Rejection {
        let rejection = validate(&envelope_for(document), 0, SUPPORTED)
            .err()
            .unwrap_or_else(|| panic!("expected rejection for {document:?}"));
        assert_eq!(
            rejection.code,
            AckErrorCode::InvalidDocument,
            "unexpected code for {document:?}: {rejection}"
        );
        assert!(
            rejection.detail.chars().count() <= DETAIL_MAX_CHARS,
            "detail longer than 512 characters"
        );
        rejection
    }

    fn assert_accepted(document: &pb::ConfigDocument) -> ValidatedConfig {
        validate(&envelope_for(document), 0, SUPPORTED)
            .unwrap_or_else(|rejection| panic!("expected acceptance, got {rejection}"))
    }

    #[test]
    fn doc_a_matches_contract_hash_and_validates() {
        let bytes = hex_to_bytes(DOC_A);
        assert_eq!(bytes.len(), 12, "doc A is a 12-byte document");
        assert_eq!(bytes_to_hex(&Sha256::digest(&bytes)), DOC_A_SHA);

        let validated = validate(&envelope(1, 1, bytes), 0, SUPPORTED).expect("doc A is valid");
        assert_eq!(validated.rule_count, 0);
        assert!(validated.degraded.is_empty());
        assert_eq!(validated.probe_config.version, CONFIG_VERSION);
        assert_eq!(
            validated.probe_config.window_ns,
            u64::from(validated.document.window_ms) * 1_000_000
        );
        assert_eq!(validated.probe_config.window_ns, 10_000_000_000);
        assert_eq!(validated.probe_config.grace_ns, 2_000_000_000);
        assert_eq!(validated.probe_config.ipv4_prefix_len, 24);
        assert_eq!(validated.probe_config.ipv6_prefix_len, 64);
        assert_eq!(validated.probe_config.default_action, 0);
        // The document carries no default_action field at all: proto3 omits 0.
        assert_eq!(
            validated.document.default_action,
            pb::DefaultAction::Pass as i32
        );
    }

    #[test]
    fn missing_default_action_field_means_pass() {
        let bytes = hex_to_bytes(DOC_A);
        let decoded = pb::ConfigDocument::decode(bytes.as_slice()).expect("decodes");
        assert_eq!(decoded.default_action, 0);
        assert_eq!(decoded.default_action, pb::DefaultAction::Pass as i32);
        // A 12-byte document cannot hold a field 6; this pins the "absent = PASS"
        // rule that the vector relies on.
        assert_eq!(bytes.len(), 12);
        assert!(validate(&envelope(1, 1, bytes), 0, SUPPORTED).is_ok());
    }

    #[test]
    fn doc_b_matches_contract_hash_and_is_degraded() {
        let bytes = hex_to_bytes(DOC_B);
        assert_eq!(bytes.len(), 63, "doc B is a 63-byte document");
        assert_eq!(bytes_to_hex(&Sha256::digest(&bytes)), DOC_B_SHA);

        let validated = validate(&envelope(1, 1, bytes), 0, SUPPORTED).expect("doc B is valid");
        assert_eq!(validated.rule_count, 1);
        assert_eq!(validated.degraded, vec!["rules"]);
        assert_eq!(validated.document.rules[0].rule_id, "r-8080");
        assert_eq!(validated.document.rules[0].priority, 10);
        assert_eq!(validated.probe_config.version, CONFIG_VERSION);
        assert_eq!(validated.probe_config.window_ns, 10_000_000_000);
        assert_eq!(validated.probe_config.default_action, 0);
        assert_eq!(validated.probe_config.ipv4_prefix_len, 24);
        assert_eq!(validated.probe_config.ipv6_prefix_len, 64);
    }

    #[test]
    fn stale_version_is_rejected_before_anything_else() {
        // Same (garbage) document: version wins over schema, hash and content.
        let env = envelope(3, 1, vec![0xff; 8]);
        let rejection = validate(&env, 3, SUPPORTED).expect_err("stale");
        assert_eq!(rejection.code, AckErrorCode::StaleVersion);

        let env = envelope(2, 1, vec![0xff; 8]);
        let rejection = validate(&env, 3, SUPPORTED).expect_err("stale");
        assert_eq!(rejection.code, AckErrorCode::StaleVersion);
    }

    #[test]
    fn unsupported_schema_is_rejected_before_the_hash() {
        let env = envelope(1, 2, vec![0xff; 8]);
        let rejection = validate(&env, 0, SUPPORTED).expect_err("schema");
        assert_eq!(rejection.code, AckErrorCode::SchemaUnsupported);
        assert_eq!(
            validate(&envelope(1, 1, hex_to_bytes(DOC_A)), 0, &[1, 2]).map(|_| ()),
            Ok(()),
            "a supported schema list may contain more than one version"
        );
    }

    #[test]
    fn hash_length_must_be_32_bytes() {
        let mut env = envelope_for(&base_document());
        env.content_sha256.truncate(31);
        assert_eq!(
            validate(&env, 0, SUPPORTED).expect_err("short hash").code,
            AckErrorCode::HashMismatch
        );

        let mut env = envelope_for(&base_document());
        env.content_sha256.push(0);
        assert_eq!(
            validate(&env, 0, SUPPORTED).expect_err("long hash").code,
            AckErrorCode::HashMismatch
        );
    }

    #[test]
    fn hash_mismatch_is_reported_before_range_errors() {
        // The document itself is bad (window_ms 999) *and* the digest is wrong;
        // the contract's order says the hash is checked first.
        let mut broken = base_document();
        broken.window_ms = 999;
        let env = pb::ConfigEnvelope {
            version: 1,
            schema_version: 1,
            content_sha256: hex_to_bytes(DOC_A_SHA),
            document: broken.encode_to_vec(),
            published_at: None,
        };
        assert_eq!(
            validate(&env, 0, SUPPORTED).expect_err("hash first").code,
            AckErrorCode::HashMismatch
        );

        // The same document with a correct digest reaches step 5.
        assert_invalid(&broken);
    }

    #[test]
    fn hash_covers_the_received_bytes_including_unknown_fields() {
        // Unknown field 15, varint 1: prost ignores it on decode, but the digest
        // must be taken over the raw bytes, so this config is accepted.
        let mut bytes = hex_to_bytes(DOC_A);
        bytes.extend_from_slice(&[0x78, 0x01]);
        let env = envelope(1, 1, bytes.clone());
        assert_eq!(
            bytes_to_hex(&Sha256::digest(&bytes)),
            bytes_to_hex(&env.content_sha256)
        );
        let validated = validate(&env, 0, SUPPORTED).expect("unknown fields are tolerated");
        assert_eq!(validated.rule_count, 0);
    }

    #[test]
    fn undecodable_document_is_invalid_document() {
        let env = envelope(1, 1, vec![0xff, 0xff, 0xff]);
        assert_eq!(
            validate(&env, 0, SUPPORTED).expect_err("bad protobuf").code,
            AckErrorCode::InvalidDocument
        );
    }

    #[test]
    fn document_schema_must_equal_envelope_schema() {
        let mut document = base_document();
        document.schema_version = 2;
        assert_invalid(&document);
    }

    #[test]
    fn window_ms_bounds() {
        for window_ms in [999, 60_001] {
            let mut document = base_document();
            document.window_ms = window_ms;
            assert_invalid(&document);
        }
        for window_ms in [1_000, 60_000] {
            let mut document = base_document();
            document.window_ms = window_ms;
            document.grace_ms = window_ms;
            let validated = assert_accepted(&document);
            assert_eq!(
                validated.probe_config.window_ns,
                u64::from(window_ms) * 1_000_000
            );
        }
    }

    #[test]
    fn grace_ms_must_not_exceed_window_ms() {
        let mut document = base_document();
        document.grace_ms = document.window_ms + 1;
        assert_invalid(&document);

        let mut document = base_document();
        document.grace_ms = document.window_ms;
        assert_eq!(
            assert_accepted(&document).probe_config.grace_ns,
            10_000_000_000
        );
    }

    #[test]
    fn prefix_lengths_must_stay_in_range() {
        let mut document = base_document();
        document.src_prefix_v4 = 33;
        assert_invalid(&document);

        let mut document = base_document();
        document.src_prefix_v6 = 129;
        assert_invalid(&document);

        let mut document = base_document();
        document.src_prefix_v4 = 32;
        document.src_prefix_v6 = 128;
        let validated = assert_accepted(&document);
        assert_eq!(validated.probe_config.ipv4_prefix_len, 32);
        assert_eq!(validated.probe_config.ipv6_prefix_len, 128);
    }

    #[test]
    fn at_most_twenty_rules() {
        let mut document = base_document();
        document.rules = (1..=MAX_RULES as u32).map(rule).collect();
        let validated = assert_accepted(&document);
        assert_eq!(validated.rule_count, MAX_RULES);
        assert_eq!(validated.degraded, vec!["rules"]);

        let mut document = base_document();
        document.rules = (1..=MAX_RULES as u32 + 1).map(rule).collect();
        let rejection = assert_invalid(&document);
        assert!(rejection.detail.contains("exceed"), "{rejection}");
    }

    #[test]
    fn rule_index_must_be_the_one_based_position() {
        let mut document = base_document();
        document.rules = vec![rule(1), rule(3)];
        assert_invalid(&document);

        let mut document = base_document();
        document.rules = vec![rule(0)];
        assert_invalid(&document);

        let mut document = base_document();
        document.rules = vec![rule(1), rule(2)];
        assert_eq!(assert_accepted(&document).rule_count, 2);
    }

    #[test]
    fn priority_must_be_in_range_and_unique() {
        let mut document = base_document();
        document.rules = vec![rule(1)];
        document.rules[0].priority = 0;
        assert_invalid(&document);

        let mut document = base_document();
        document.rules = vec![rule(1)];
        document.rules[0].priority = 1_001;
        assert_invalid(&document);

        let mut document = base_document();
        document.rules = vec![rule(1), rule(2)];
        document.rules[1].priority = document.rules[0].priority;
        assert_invalid(&document);
    }

    #[test]
    fn rule_id_syntax_and_uniqueness() {
        for rule_id in ["R-8080", "-8080", "r 8080", "r/8080", ""] {
            let mut document = base_document();
            document.rules = vec![rule(1)];
            document.rules[0].rule_id = rule_id.to_owned();
            assert_invalid(&document);
        }

        let mut document = base_document();
        document.rules = vec![rule(1)];
        document.rules[0].rule_id = "r".repeat(33);
        assert_invalid(&document);

        let mut document = base_document();
        document.rules = vec![rule(1), rule(2)];
        document.rules[1].rule_id = document.rules[0].rule_id.clone();
        assert_invalid(&document);

        // 32 characters is the inclusive limit, and dots/underscores/dashes are
        // all allowed after the first character.
        let mut document = base_document();
        document.rules = vec![rule(1)];
        document.rules[0].rule_id = format!("a.{}", "b-".repeat(15));
        assert_eq!(assert_accepted(&document).rule_count, 1);
    }

    #[test]
    fn long_rule_id_is_truncated_to_the_detail_limit() {
        let mut document = base_document();
        document.rules = vec![rule(1)];
        document.rules[0].rule_id = "x".repeat(4_000);
        let rejection = assert_invalid(&document);
        assert_eq!(rejection.detail.chars().count(), DETAIL_MAX_CHARS);
    }

    #[test]
    fn unknown_enum_values_are_rejected() {
        let mut document = base_document();
        document.rules = vec![rule(1)];
        document.rules[0].r#match.as_mut().expect("match").family = 5;
        assert_invalid(&document);

        let mut document = base_document();
        document.rules = vec![rule(1)];
        document.rules[0].r#match.as_mut().expect("match").protocol = 7;
        assert_invalid(&document);

        // Any/Icmpv6 are known values and must be accepted.
        let mut document = base_document();
        document.rules = vec![rule(1)];
        let rule_match = document.rules[0].r#match.as_mut().expect("match");
        rule_match.family = pb::Family::Any as i32;
        rule_match.protocol = pb::L4Protocol::Icmpv6 as i32;
        assert_eq!(assert_accepted(&document).rule_count, 1);
    }

    #[test]
    fn dst_ports_must_stay_in_range() {
        for port in [0, 65_536] {
            let mut document = base_document();
            document.rules = vec![rule(1)];
            document.rules[0].r#match.as_mut().expect("match").dst_ports = vec![port];
            assert_invalid(&document);
        }

        let mut document = base_document();
        document.rules = vec![rule(1)];
        document.rules[0].r#match.as_mut().expect("match").dst_ports = vec![1, 65_535];
        assert_eq!(assert_accepted(&document).rule_count, 1);
    }

    #[test]
    fn bad_addresses_are_rejected() {
        for (src_prefix, dst_addr) in [
            (Some("10.0.1.0/33"), Some("10.0.2.5")),
            (Some("10.0.1.0/24"), Some("not-an-ip")),
            (Some("fe80::1%eth0"), None),
            (None, Some("[fd00:99::1")),
        ] {
            let mut document = base_document();
            document.rules = vec![rule(1)];
            let rule_match = document.rules[0].r#match.as_mut().expect("match");
            rule_match.src_prefix = src_prefix.map(str::to_owned);
            rule_match.dst_addr = dst_addr.map(str::to_owned);
            assert_invalid(&document);
        }

        // IPv6 text and bracket form are both fine.
        let mut document = base_document();
        document.rules = vec![rule(1)];
        let rule_match = document.rules[0].r#match.as_mut().expect("match");
        rule_match.family = FAMILY_IPV6 as i32;
        rule_match.src_prefix = Some("fd00:99::/64".to_owned());
        rule_match.dst_addr = Some("[fd00:99::1]".to_owned());
        assert_eq!(assert_accepted(&document).rule_count, 1);
    }

    #[test]
    fn absent_match_means_any() {
        let mut document = base_document();
        document.rules = vec![rule(1)];
        document.rules[0].r#match = None;
        assert_eq!(assert_accepted(&document).rule_count, 1);
    }

    #[test]
    fn drop_default_action_is_a_hard_reject() {
        let mut document = base_document();
        document.default_action = pb::DefaultAction::Drop as i32;
        let rejection =
            validate(&envelope_for(&document), 0, SUPPORTED).expect_err("DROP is not supported");
        assert_eq!(rejection.code, AckErrorCode::UnsupportedDefaultAction);
        assert_ne!(rejection.code, AckErrorCode::InvalidDocument);
        assert!(rejection.detail.contains("default_action"));
    }

    #[test]
    fn rules_are_degraded_but_accepted() {
        let mut document = base_document();
        document.rules = vec![rule(1)];
        let validated = assert_accepted(&document);
        assert_eq!(validated.degraded, vec!["rules"]);
        assert_eq!(validated.rule_count, 1);
        assert_eq!(validated.probe_config.version, CONFIG_VERSION);
        // The document is returned intact: degradation is a reporting decision,
        // not a rewrite of what the control plane published.
        assert_eq!(validated.document.rules.len(), 1);

        document.rules.clear();
        assert!(assert_accepted(&document).degraded.is_empty());
    }

    #[test]
    fn probe_config_version_is_the_abi_version_not_the_envelope_version() {
        let env = envelope(7, 1, hex_to_bytes(DOC_A));
        let validated = validate(&env, 0, SUPPORTED).expect("version 7 is accepted");
        assert_eq!(validated.probe_config.version, CONFIG_VERSION);
        assert_ne!(validated.probe_config.version, 7_u32);
        assert_ne!(CONFIG_VERSION, 7_u32);
    }

    #[test]
    fn rejects_are_serialisable_as_ack_fields() {
        // The code is meant to go straight into `AckConfigRequest.error_code`,
        // and the detail into `error_detail`, so both must round-trip.
        let rejection = assert_invalid(&document_with_window(0));
        assert_eq!(
            AckErrorCode::from_wire(rejection.code.as_str()),
            Some(rejection.code)
        );
        assert!(!rejection.to_string().is_empty());
    }

    fn document_with_window(window_ms: u32) -> pb::ConfigDocument {
        let mut document = base_document();
        document.window_ms = window_ms;
        document
    }
}
