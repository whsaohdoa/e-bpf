//! ABI shared by the XDP program and the userspace loader.
//!
//! `PacketEvent` is written verbatim into the ring buffer by the eBPF side and
//! read by userspace with [`core::ptr::read_unaligned`].  Keep the type
//! `#[repr(C)]`, `Copy` and free of pointers.
//!
//! # Versioning
//!
//! [`EVENT_ABI_VERSION`] tags every record.  Version 2 (this revision) merges
//! IPv4 and IPv6 into one 48-byte record and one ring buffer; it replaces the
//! IPv4-only 24-byte version 1 (`src_ip`/`dst_ip: u32`), which is intentionally
//! gone.  This is a **breaking ABI change**: a userspace binary built against
//! v1 must not read v2 records, so both the eBPF object and the loader have to
//! be rebuilt together.  Userspace checks [`PacketEvent::version`] and skips
//! records with an unknown version/family instead of guessing the layout.
//!
//! The CSV schema is unchanged (still 7 columns), so an existing IPv4 CSV can
//! still be extended with `--append`.
#![no_std]

/// Version of the [`PacketEvent`] layout written by the eBPF side.
pub const EVENT_ABI_VERSION: u8 = 2;

/// [`PacketEvent::family`] value for IPv4 records.
pub const FAMILY_IPV4: u8 = 4;

/// [`PacketEvent::family`] value for IPv6 records.
pub const FAMILY_IPV6: u8 = 6;

/// One packet seen on the XDP ingress path, IPv4 or IPv6.
///
/// # Address encoding
///
/// Addresses are copied verbatim from the IP header and therefore keep the
/// **raw network-order bytes** in a 16-byte field:
///
/// - IPv4 (`family == 4`): bytes 0..4 are the IPv4 address, bytes 4..16 are
///   zero.  Userspace decodes with
///   `Ipv4Addr::new(addr[0], addr[1], addr[2], addr[3])`.  IPv4 addresses are
///   never stored IPv4-mapped (`::ffff:`).
/// - IPv6 (`family == 6`): all 16 bytes are meaningful.  Userspace decodes with
///   `Ipv6Addr::from(addr)`.
///
/// No `from_be`/`to_be` is applied on either side; the bytes are exactly what
/// was on the wire, and userspace converts exactly once.
///
/// `src_port`/`dst_port` follow the same convention.  They are reserved for the
/// upcoming 5-tuple extension and stay `0` in this revision.
///
/// `protocol` is the IPv4 protocol number or the IPv6 **base-header** Next
/// Header.  IPv6 extension headers are not walked in this revision, so for
/// packets carrying Hop-by-Hop (0), Routing (43), Fragment (44), Destination
/// Options (60) or AH (51) the value may not be the final L4 protocol.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PacketEvent {
    /// `bpf_ktime_get_ns()` timestamp (CLOCK_MONOTONIC, nanoseconds).
    pub ts_ns: u64,
    /// Source address, raw network-order bytes; zero-padded for IPv4.
    pub src_addr: [u8; 16],
    /// Destination address, raw network-order bytes; zero-padded for IPv4.
    pub dst_addr: [u8; 16],
    /// L4 source port, network byte order; `0` until L4 parsing is enabled.
    pub src_port: u16,
    /// L4 destination port, network byte order; `0` until L4 parsing is enabled.
    pub dst_port: u16,
    /// IPv4 protocol / IPv6 Next Header, e.g. 1 = ICMP, 6 = TCP, 17 = UDP,
    /// 58 = ICMPv6.
    pub protocol: u8,
    /// [`FAMILY_IPV4`] or [`FAMILY_IPV6`].
    pub family: u8,
    /// [`EVENT_ABI_VERSION`]; userspace must reject anything else.
    pub version: u8,
    /// Explicit padding, always 0; keeps the record 48 bytes.
    pub reserved: u8,
}

/// `aya::Pod` promises that the type is plain old data and can be copied from
/// the map memory as-is.  Only implemented for the userspace build where `aya`
/// is available; the eBPF side uses the same layout by construction.
#[cfg(feature = "user")]
unsafe impl aya::Pod for PacketEvent {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards the hand-checked offset table in the README.  A drift here would
    /// make userspace decode garbage, so it is asserted field by field.
    #[test]
    fn event_layout_is_stable() {
        assert_eq!(core::mem::size_of::<PacketEvent>(), 48);
        assert_eq!(core::mem::align_of::<PacketEvent>(), 8);

        assert_eq!(core::mem::offset_of!(PacketEvent, ts_ns), 0);
        assert_eq!(core::mem::offset_of!(PacketEvent, src_addr), 8);
        assert_eq!(core::mem::offset_of!(PacketEvent, dst_addr), 24);
        assert_eq!(core::mem::offset_of!(PacketEvent, src_port), 40);
        assert_eq!(core::mem::offset_of!(PacketEvent, dst_port), 42);
        assert_eq!(core::mem::offset_of!(PacketEvent, protocol), 44);
        assert_eq!(core::mem::offset_of!(PacketEvent, family), 45);
        assert_eq!(core::mem::offset_of!(PacketEvent, version), 46);
        assert_eq!(core::mem::offset_of!(PacketEvent, reserved), 47);
    }

    #[test]
    fn abi_constants_are_pinned() {
        assert_eq!(EVENT_ABI_VERSION, 2);
        assert_eq!(FAMILY_IPV4, 4);
        assert_eq!(FAMILY_IPV6, 6);
    }
}
