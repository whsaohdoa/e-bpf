//! Zero-allocation packet parser shared by the XDP data path and host tests.
//!
//! The data path must never allocate, never log per packet and never fault on a
//! malformed frame.  Every read therefore goes through [`Pkt::bytes`], which
//! performs the same bounds check the verifier needs:
//!
//! * on the kernel side the implementation is a thin wrapper around
//!   `data`/`data_end` and is inlined into the XDP program,
//! * on hosts `&[u8]` implements [`Pkt`], so the L0 fixtures and the future L3
//!   replay use exactly the same parser code.
//!
//! The parser is deliberately conservative about what it rejects:
//!
//! * up to two stacked 802.1Q/802.1ad tags are accepted (double VLAN),
//! * a truncated L4 header is *not* a reject: ports become `0` and the packet
//!   is still aggregated (parity with the pre-split `try_probe`),
//! * non-first IPv4 fragments carry no L4 header, so their ports are `0`,
//! * IPv6 extension headers are not walked; the base-header Next Header value
//!   is reported as the protocol.
//!
//! Rejects carry a [`ParseError`] naming the first check that failed.  The
//! caller only needs `is_ok()` to decide `XDP_PASS`, but the reason keeps the
//! test assertions precise.

use crate::{FAMILY_IPV4, FAMILY_IPV6};

/// Ethernet II header length: destination MAC, source MAC, ethertype.
const ETH_HDR_LEN: usize = 14;

/// One 802.1Q/802.1ad tag: TCI plus the inner ethertype.
const VLAN_HDR_LEN: usize = 4;

/// Maximum number of stacked VLAN tags the parser walks.
const MAX_VLAN_TAGS: usize = 2;

const ETH_P_IP: u16 = 0x0800;
const ETH_P_IPV6: u16 = 0x86dd;
const ETH_P_8021Q: u16 = 0x8100;
const ETH_P_8021AD: u16 = 0x88a8;

/// Fixed IPv4 header length without options.
const IPV4_HDR_LEN: usize = 20;
/// Smallest legal `ihl` value (five 32-bit words).
const IPV4_MIN_IHL: usize = 5;

/// IPv6 has a fixed 40-byte base header; there is no `ihl` equivalent.
const IPV6_HDR_LEN: usize = 40;

/// L4 protocol numbers whose first four bytes are the source/destination port.
const IPPROTO_TCP: u8 = 6;
const IPPROTO_UDP: u8 = 17;

/// A packet buffer the parser can read from.
///
/// `bytes` returns `None` when `[off, off + len)` is not entirely inside the
/// packet.  Implementations must not read outside that range.
pub trait Pkt {
    /// Borrows `len` bytes starting at `off`, or `None` when out of bounds.
    fn bytes(&self, off: usize, len: usize) -> Option<&[u8]>;
}

impl Pkt for [u8] {
    #[inline(always)]
    fn bytes(&self, off: usize, len: usize) -> Option<&[u8]> {
        self.get(off..off.checked_add(len)?)
    }
}

/// Addresses + protocol + L4 ports of one packet, in the shape of the ABI.
///
/// IPv4 addresses stay left-aligned in the 16-byte fields with the remaining
/// bytes zero, exactly like [`crate::AggKey::src_prefix`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct L3Info {
    /// Source address, raw network-order bytes; zero-padded for IPv4.
    pub src: [u8; 16],
    /// Destination address, raw network-order bytes; zero-padded for IPv4.
    pub dst: [u8; 16],
    /// L4 source port, network byte order; `0` when unavailable.
    pub src_port: u16,
    /// L4 destination port, network byte order; `0` when unavailable.
    pub dst_port: u16,
    /// IPv4 protocol / IPv6 base-header Next Header.
    pub protocol: u8,
    /// [`FAMILY_IPV4`] or [`FAMILY_IPV6`].
    pub family: u8,
}

/// First failed check while parsing a frame.
///
/// The variant is diagnostic only; the data path treats every error the same
/// way (`XDP_PASS`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// The frame is shorter than the 14-byte Ethernet header.
    ShortEthernet,
    /// A VLAN tag was announced but its four bytes are not in the frame.
    ShortVlan,
    /// The innermost ethertype is neither IPv4 nor IPv6.
    UnsupportedEtherType,
    /// The fixed IPv4/IPv6 header is not fully present.
    ShortIpHeader,
    /// The IP version nibble does not match the ethertype.
    UnsupportedIpVersion,
    /// IPv4 `ihl` is below the legal minimum of 5.
    BadIpv4Ihl,
    /// IPv4 options extend past the end of the frame.
    ShortIpv4Options,
}

/// Copies `N` bytes at `off` onto the stack.
///
/// Fixed-size copies keep the verifier's bounds reasoning simple: the packet is
/// read once into a local array and every later field access is stack-only.
#[inline(always)]
fn take<const N: usize>(p: &(impl Pkt + ?Sized), off: usize) -> Option<[u8; N]> {
    let src = p.bytes(off, N)?;
    if src.len() != N {
        return None;
    }
    let mut out = [0u8; N];
    // SAFETY: `src.len() == N` was checked above and `out` is exactly `N`
    // bytes, so the copy stays inside both buffers.
    unsafe {
        core::ptr::copy_nonoverlapping(src.as_ptr(), out.as_mut_ptr(), N);
    }
    Some(out)
}

/// Parses Ethernet (with up to two stacked VLAN tags) and the IP header.
///
/// Returns addresses as raw network-order bytes, left-aligned in 16-byte fields.
/// Any malformed or unsupported frame is reported as `Err`; the caller passes
/// it to the stack unchanged.
#[inline(always)]
pub fn parse_ip(p: &(impl Pkt + ?Sized)) -> Result<L3Info, ParseError> {
    let eth = take::<ETH_HDR_LEN>(p, 0).ok_or(ParseError::ShortEthernet)?;
    let mut ether_type = u16::from_be_bytes([eth[12], eth[13]]);
    let mut l3_offset = ETH_HDR_LEN;
    let mut tags = 0;

    while tags < MAX_VLAN_TAGS && (ether_type == ETH_P_8021Q || ether_type == ETH_P_8021AD) {
        let vlan = take::<VLAN_HDR_LEN>(p, l3_offset).ok_or(ParseError::ShortVlan)?;
        ether_type = u16::from_be_bytes([vlan[2], vlan[3]]);
        l3_offset += VLAN_HDR_LEN;
        tags += 1;
    }

    match ether_type {
        ETH_P_IP => parse_ipv4(p, l3_offset),
        ETH_P_IPV6 => parse_ipv6(p, l3_offset),
        _ => Err(ParseError::UnsupportedEtherType),
    }
}

/// Parses TCP/UDP ports at `l4_offset`.
///
/// A missing/truncated L4 header yields `(0, 0)` instead of rejecting the
/// packet: the aggregate still counts it.
#[inline(always)]
fn parse_ports(p: &(impl Pkt + ?Sized), l4_offset: usize, protocol: u8) -> (u16, u16) {
    if protocol != IPPROTO_TCP && protocol != IPPROTO_UDP {
        return (0, 0);
    }
    match take::<4>(p, l4_offset) {
        Some(ports) => (
            u16::from_be_bytes([ports[0], ports[1]]),
            u16::from_be_bytes([ports[2], ports[3]]),
        ),
        None => (0, 0),
    }
}

/// Parses the fixed IPv4 header at `l3_offset`.
#[inline(always)]
fn parse_ipv4(p: &(impl Pkt + ?Sized), l3_offset: usize) -> Result<L3Info, ParseError> {
    let ip = take::<IPV4_HDR_LEN>(p, l3_offset).ok_or(ParseError::ShortIpHeader)?;
    let version_ihl = ip[0];
    if version_ihl >> 4 != 4 {
        return Err(ParseError::UnsupportedIpVersion);
    }

    let ihl = (version_ihl & 0x0f) as usize;
    if ihl < IPV4_MIN_IHL {
        return Err(ParseError::BadIpv4Ihl);
    }

    // The fixed 20 bytes were checked above; also make sure IPv4 options
    // (ihl > 5) are inside the frame before deriving the L4 offset from them.
    let header_len = ihl * 4;
    if p.bytes(l3_offset, header_len).is_none() {
        return Err(ParseError::ShortIpv4Options);
    }

    let protocol = ip[9];
    // A non-first fragment has no L4 header; leave the ports 0.  First
    // fragments (offset 0) still carry the L4 header and are parsed.
    let frag_off = u16::from_be_bytes([ip[6], ip[7]]);
    let (src_port, dst_port) = if frag_off & 0x1fff != 0 {
        (0, 0)
    } else {
        parse_ports(p, l3_offset + header_len, protocol)
    };

    let mut src = [0u8; 16];
    let mut dst = [0u8; 16];
    src[..4].copy_from_slice(&ip[12..16]);
    dst[..4].copy_from_slice(&ip[16..20]);

    Ok(L3Info {
        src,
        dst,
        src_port,
        dst_port,
        protocol,
        family: FAMILY_IPV4,
    })
}

/// Parses the fixed 40-byte IPv6 base header at `l3_offset`.
///
/// Extension headers are deliberately not walked: `protocol` is the Next Header
/// field of the base header, which may be 0/43/44/51/60 instead of the final L4
/// protocol.  Ports are only parsed when that Next Header value is TCP/UDP.
#[inline(always)]
fn parse_ipv6(p: &(impl Pkt + ?Sized), l3_offset: usize) -> Result<L3Info, ParseError> {
    let ip = take::<IPV6_HDR_LEN>(p, l3_offset).ok_or(ParseError::ShortIpHeader)?;

    // The version is the high nibble of the first header byte.
    if ip[0] >> 4 != 6 {
        return Err(ParseError::UnsupportedIpVersion);
    }

    let protocol = ip[6];
    let (src_port, dst_port) = parse_ports(p, l3_offset + IPV6_HDR_LEN, protocol);

    let mut src = [0u8; 16];
    let mut dst = [0u8; 16];
    src.copy_from_slice(&ip[8..24]);
    dst.copy_from_slice(&ip[24..40]);

    Ok(L3Info {
        src,
        dst,
        src_port,
        dst_port,
        // Base-header Next Header.  Do not walk extension headers.
        protocol,
        family: FAMILY_IPV6,
    })
}

#[cfg(test)]
mod tests {
    use std::vec::Vec;

    use super::*;

    const MAC_DST: [u8; 6] = [0x02, 0, 0, 0, 0, 0x02];
    const MAC_SRC: [u8; 6] = [0x02, 0, 0, 0, 0, 0x01];

    fn eth(ether_type: u16) -> Vec<u8> {
        let mut frame = Vec::new();
        frame.extend_from_slice(&MAC_DST);
        frame.extend_from_slice(&MAC_SRC);
        frame.extend_from_slice(&ether_type.to_be_bytes());
        frame
    }

    fn vlan(tci: u16, inner: u16) -> Vec<u8> {
        let mut tag = Vec::new();
        tag.extend_from_slice(&tci.to_be_bytes());
        tag.extend_from_slice(&inner.to_be_bytes());
        tag
    }

    fn ipv4(protocol: u8, frag_off: u16, src: [u8; 4], dst: [u8; 4], l4: &[u8]) -> Vec<u8> {
        let mut ip = Vec::new();
        ip.push(0x45); // version 4, ihl 5
        ip.push(0); // tos
        ip.extend_from_slice(&(20u16 + l4.len() as u16).to_be_bytes());
        ip.extend_from_slice(&1u16.to_be_bytes()); // id
        ip.extend_from_slice(&frag_off.to_be_bytes());
        ip.push(64); // ttl
        ip.push(protocol);
        ip.extend_from_slice(&0u16.to_be_bytes()); // checksum (unchecked)
        ip.extend_from_slice(&src);
        ip.extend_from_slice(&dst);
        ip.extend_from_slice(l4);
        ip
    }

    fn ipv6(next_header: u8, src: [u8; 16], dst: [u8; 16], l4: &[u8]) -> Vec<u8> {
        let mut ip = Vec::new();
        ip.extend_from_slice(&[0x60, 0, 0, 0]);
        ip.extend_from_slice(&(l4.len() as u16).to_be_bytes());
        ip.push(next_header);
        ip.push(64); // hop limit
        ip.extend_from_slice(&src);
        ip.extend_from_slice(&dst);
        ip.extend_from_slice(l4);
        ip
    }

    fn ports(src: u16, dst: u16) -> Vec<u8> {
        let mut l4 = Vec::new();
        l4.extend_from_slice(&src.to_be_bytes());
        l4.extend_from_slice(&dst.to_be_bytes());
        l4
    }

    #[test]
    fn u_parse_single_vlan_ipv4_tcp() {
        let mut frame = eth(ETH_P_8021Q);
        frame.extend_from_slice(&vlan(100, ETH_P_IP));
        frame.extend_from_slice(&ipv4(
            IPPROTO_TCP,
            0x4000,
            [10, 0, 0, 1],
            [10, 0, 0, 2],
            &ports(12345, 80),
        ));

        let info = parse_ip(&frame[..]).expect("single VLAN IPv4 TCP");
        assert_eq!(info.family, FAMILY_IPV4);
        assert_eq!(info.src[..4], [10, 0, 0, 1]);
        assert_eq!(info.dst[..4], [10, 0, 0, 2]);
        assert_eq!(info.src[4..], [0u8; 12]);
        assert_eq!(info.src_port, 12345);
        assert_eq!(info.dst_port, 80);
        assert_eq!(info.protocol, IPPROTO_TCP);
    }

    #[test]
    fn u_parse_double_vlan_ipv6_udp() {
        let src = [0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
        let dst = [0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2];
        let mut frame = eth(ETH_P_8021AD);
        frame.extend_from_slice(&vlan(100, ETH_P_8021Q));
        frame.extend_from_slice(&vlan(200, ETH_P_IPV6));
        frame.extend_from_slice(&ipv6(IPPROTO_UDP, src, dst, &ports(5000, 53)));

        let info = parse_ip(&frame[..]).expect("QinQ IPv6 UDP");
        assert_eq!(info.family, FAMILY_IPV6);
        assert_eq!(info.src, src);
        assert_eq!(info.dst, dst);
        assert_eq!(info.src_port, 5000);
        assert_eq!(info.dst_port, 53);
        assert_eq!(info.protocol, IPPROTO_UDP);
    }

    #[test]
    fn u_parse_three_vlan_tags_are_rejected() {
        let mut frame = eth(ETH_P_8021Q);
        frame.extend_from_slice(&vlan(1, ETH_P_8021Q));
        frame.extend_from_slice(&vlan(2, ETH_P_8021Q));
        frame.extend_from_slice(&vlan(3, ETH_P_IP));
        frame.extend_from_slice(&ipv4(
            IPPROTO_UDP,
            0,
            [10, 0, 0, 1],
            [10, 0, 0, 2],
            &ports(1, 2),
        ));
        assert_eq!(
            parse_ip(&frame[..]).unwrap_err(),
            ParseError::UnsupportedEtherType
        );
    }

    #[test]
    fn u_parse_non_first_fragment_has_no_ports() {
        // Offset field 185 -> the fragment starts in the middle of the payload.
        let frame = {
            let mut frame = eth(ETH_P_IP);
            frame.extend_from_slice(&ipv4(
                IPPROTO_UDP,
                185,
                [10, 0, 0, 1],
                [10, 0, 0, 2],
                &ports(4000, 5000),
            ));
            frame
        };
        let info = parse_ip(&frame[..]).expect("non-first fragment is accepted");
        assert_eq!(info.src_port, 0);
        assert_eq!(info.dst_port, 0);
        assert_eq!(info.protocol, IPPROTO_UDP);
    }

    #[test]
    fn u_parse_first_fragment_parses_ports() {
        let mut frame = eth(ETH_P_IP);
        frame.extend_from_slice(&ipv4(
            IPPROTO_UDP,
            0x2000, // MF set, offset 0
            [10, 0, 0, 1],
            [10, 0, 0, 2],
            &ports(4000, 5000),
        ));
        let info = parse_ip(&frame[..]).expect("first fragment is accepted");
        assert_eq!(info.src_port, 4000);
        assert_eq!(info.dst_port, 5000);
    }

    #[test]
    fn u_parse_truncated_frames_are_rejected() {
        assert_eq!(
            parse_ip(&[0u8; 10][..]).unwrap_err(),
            ParseError::ShortEthernet
        );

        let mut vlan_frame = eth(ETH_P_8021Q);
        vlan_frame.extend_from_slice(&[0, 1]);
        assert_eq!(
            parse_ip(&vlan_frame[..]).unwrap_err(),
            ParseError::ShortVlan
        );

        let mut short_ipv4 = eth(ETH_P_IP);
        short_ipv4.extend_from_slice(&[0u8; 19]);
        assert_eq!(
            parse_ip(&short_ipv4[..]).unwrap_err(),
            ParseError::ShortIpHeader
        );

        let mut short_ipv6 = eth(ETH_P_IPV6);
        short_ipv6.extend_from_slice(&[0u8; 39]);
        assert_eq!(
            parse_ip(&short_ipv6[..]).unwrap_err(),
            ParseError::ShortIpHeader
        );

        // ihl says six words but only five are present.
        let mut options_truncated = eth(ETH_P_IP);
        options_truncated.extend_from_slice(&[0x46; 20]);
        assert_eq!(
            parse_ip(&options_truncated[..]).unwrap_err(),
            ParseError::ShortIpv4Options
        );
    }

    #[test]
    fn u_parse_truncated_l4_keeps_the_packet() {
        let mut frame = eth(ETH_P_IP);
        frame.extend_from_slice(&ipv4(IPPROTO_TCP, 0, [10, 0, 0, 1], [10, 0, 0, 2], &[]));

        let info = parse_ip(&frame[..]).expect("IP header alone is enough");
        assert_eq!(info.src_port, 0);
        assert_eq!(info.dst_port, 0);
        assert_eq!(info.protocol, IPPROTO_TCP);
    }

    #[test]
    fn u_parse_rejects_bad_versions_and_ihl() {
        let mut wrong_version = eth(ETH_P_IP);
        wrong_version.extend_from_slice(&[0x65; 20]);
        assert_eq!(
            parse_ip(&wrong_version[..]).unwrap_err(),
            ParseError::UnsupportedIpVersion
        );

        let mut bad_ihl = eth(ETH_P_IP);
        bad_ihl.extend_from_slice(&[0x44; 20]);
        assert_eq!(parse_ip(&bad_ihl[..]).unwrap_err(), ParseError::BadIpv4Ihl);

        let mut wrong_v6 = eth(ETH_P_IPV6);
        wrong_v6.extend_from_slice(&[0x40; 40]);
        assert_eq!(
            parse_ip(&wrong_v6[..]).unwrap_err(),
            ParseError::UnsupportedIpVersion
        );
    }

    #[test]
    fn u_parse_rejects_unknown_ethertype() {
        let mut arp = eth(0x0806);
        arp.extend_from_slice(&[0u8; 28]);
        assert_eq!(
            parse_ip(&arp[..]).unwrap_err(),
            ParseError::UnsupportedEtherType
        );
    }

    #[test]
    fn u_parse_ipv6_extension_header_reports_base_protocol() {
        let mut frame = eth(ETH_P_IPV6);
        frame.extend_from_slice(&ipv6(
            0, // hop-by-hop extension header
            [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
            [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2],
            &[0u8; 8],
        ));
        let info = parse_ip(&frame[..]).expect("extension header is not walked");
        assert_eq!(info.protocol, 0);
        assert_eq!(info.src_port, 0);
        assert_eq!(info.dst_port, 0);
    }

}
