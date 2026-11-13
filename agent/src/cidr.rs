//! Text parsing for the address fields of `ConfigDocument` (contract §6.2).
//!
//! The published config carries addresses as text so a human can read it, while
//! the data path works with the 16-byte left-aligned form used by
//! `probe_common::AggKey` (IPv4 occupies the first four bytes, the rest are
//! zero). Parsing is deliberately syntactic: host bits are *not* masked here —
//! the caller decides what a prefix means — and `"10.0.1.7/24"` is accepted.

use std::error::Error;
use std::fmt;
use std::net::IpAddr;

/// A parsed address plus its prefix length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    /// `4` or `6`, matching the `FAMILY_*` values of the ABI.
    pub family: u8,
    /// Network-order address, left-aligned; bytes 4..16 are zero for IPv4.
    pub addr: [u8; 16],
    /// `0..=32` for IPv4, `0..=128` for IPv6.
    pub prefix_len: u8,
}

/// Why a config address field could not be parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CidrError {
    /// The field is an empty string.
    Empty,
    /// More than one `/` in the text.
    TooManySlashes,
    /// A `%scope` suffix (e.g. `fe80::1%eth0`), which has no meaning here.
    ScopedAddress,
    /// The address part is not an IPv4/IPv6 address (or the brackets are
    /// unbalanced).
    InvalidAddress,
    /// The prefix part is not a decimal number within the family's range.
    InvalidPrefixLen,
}

impl fmt::Display for CidrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Empty => "empty address",
            Self::TooManySlashes => "more than one '/'",
            Self::ScopedAddress => "scoped address ('%') is not allowed",
            Self::InvalidAddress => "not a valid IPv4/IPv6 address",
            Self::InvalidPrefixLen => "invalid prefix length",
        };
        f.write_str(text)
    }
}

impl Error for CidrError {}

/// Parses `"10.0.1.0/24"`, `"10.0.2.5"`, `"fd00:99::/64"` or `"[fd00:99::1]"`.
///
/// A missing prefix is the host prefix (`/32` or `/128`). Brackets around the
/// address are optional and may be combined with a prefix, e.g.
/// `"[fd00:99::1]/128"`.
pub fn parse_cidr(s: &str) -> Result<Cidr, CidrError> {
    if s.is_empty() {
        return Err(CidrError::Empty);
    }
    if s.contains('%') {
        return Err(CidrError::ScopedAddress);
    }

    let mut parts = s.split('/');
    let mut addr_text = parts.next().unwrap_or_default();
    let prefix_text = parts.next();
    if parts.next().is_some() {
        return Err(CidrError::TooManySlashes);
    }

    if let Some(rest) = addr_text.strip_prefix('[') {
        addr_text = rest.strip_suffix(']').ok_or(CidrError::InvalidAddress)?;
    }

    let addr: IpAddr = addr_text.parse().map_err(|_| CidrError::InvalidAddress)?;
    let max_prefix: u32 = match addr {
        IpAddr::V4(_) => 32,
        IpAddr::V6(_) => 128,
    };

    let prefix_len = match prefix_text {
        None => max_prefix as u8,
        Some(text) => {
            if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
                return Err(CidrError::InvalidPrefixLen);
            }
            let value: u32 = text.parse().map_err(|_| CidrError::InvalidPrefixLen)?;
            if value > max_prefix {
                return Err(CidrError::InvalidPrefixLen);
            }
            value as u8
        }
    };

    let mut out = [0u8; 16];
    match addr {
        IpAddr::V4(v4) => out[..4].copy_from_slice(&v4.octets()),
        IpAddr::V6(v6) => out.copy_from_slice(&v6.octets()),
    }
    let family = match addr {
        IpAddr::V4(_) => 4,
        IpAddr::V6(_) => 6,
    };

    Ok(Cidr {
        family,
        addr: out,
        prefix_len,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ipv4_cidr() {
        let cidr = parse_cidr("10.0.1.0/24").expect("valid");
        assert_eq!(cidr.family, 4);
        assert_eq!(cidr.prefix_len, 24);
        assert_eq!(cidr.addr, [10, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn bare_ipv4_is_a_host_route() {
        let cidr = parse_cidr("10.0.2.5").expect("valid");
        assert_eq!(cidr.prefix_len, 32);
        assert_eq!(&cidr.addr[..4], &[10, 0, 2, 5]);
        assert_eq!(&cidr.addr[4..], &[0u8; 12]);
    }

    #[test]
    fn host_bits_may_be_non_zero() {
        // The data path masks with prefix_len; the parser must not reject this.
        let cidr = parse_cidr("10.0.1.7/24").expect("valid");
        assert_eq!(&cidr.addr[..4], &[10, 0, 1, 7]);
        assert_eq!(cidr.prefix_len, 24);
    }

    #[test]
    fn parses_ipv6_cidr() {
        let cidr = parse_cidr("fd00:99::/64").expect("valid");
        assert_eq!(cidr.family, 6);
        assert_eq!(cidr.prefix_len, 64);
        assert_eq!(&cidr.addr[..2], &[0xfd, 0x00]);
        assert_eq!(cidr.addr[15], 0);
    }

    #[test]
    fn brackets_are_optional() {
        let bare = parse_cidr("fd00:99::1").expect("valid");
        let bracketed = parse_cidr("[fd00:99::1]").expect("valid");
        assert_eq!(bare, bracketed);
        assert_eq!(bare.addr[15], 1);
        assert_eq!(bare.prefix_len, 128);

        let with_prefix = parse_cidr("[fd00:99::1]/96").expect("valid");
        assert_eq!(with_prefix.prefix_len, 96);
    }

    #[test]
    fn rejects_malformed_text() {
        for text in [
            "",
            "10.0.1.0/24/8",
            "/24",
            "10.0.1.0/",
            "10.0.1.0/-1",
            "10.0.1.0/+8",
            "10.0.1.0/24x",
            "10.0.1.0/33",
            "10.0.1/24",
            "10.0.1.256/24",
            "10.0.01.0/24x",
            "10.0.1.0 ",
            " 10.0.1.0",
            "10.0.1.0/0x18",
            "not-an-ip",
            "fe80::1%eth0",
            "[fd00::1",
            "fd00::1]",
            "[]",
            "fd00::1/129",
            "fd00::1/abc",
            "[fd00::1]/64 ",
        ] {
            assert!(
                parse_cidr(text).is_err(),
                "expected {text:?} to be rejected"
            );
        }
    }

    #[test]
    fn prefix_zero_is_valid() {
        assert_eq!(parse_cidr("0.0.0.0/0").expect("valid").prefix_len, 0);
        assert_eq!(parse_cidr("::/0").expect("valid").prefix_len, 0);
    }
}
