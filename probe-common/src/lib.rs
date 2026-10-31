//! ABI shared by the XDP program and the userspace loader.
//!
//! `PacketEvent` is written verbatim into the ring buffer by the eBPF side and
//! read by userspace with [`core::ptr::read_unaligned`].  Keep the type
//! `#[repr(C)]`, `Copy` and free of pointers, and only ever append fields (never
//! reorder existing ones) so that a userspace binary built against an older
//! revision keeps understanding events from a newer eBPF object.
#![no_std]

/// One IPv4 packet seen on the XDP ingress path.
///
/// # Byte order
///
/// `src_ip`/`dst_ip` are copied verbatim from the IPv4 header (`saddr`/`daddr`)
/// and therefore keep **network byte order in memory**.  Userspace must convert
/// exactly once: `Ipv4Addr::from(u32::from_be(event.src_ip))`.  Do not byte-swap
/// on the eBPF side as well, otherwise the printed addresses end up reversed.
///
/// `src_port`/`dst_port` follow the same convention.  They are reserved for the
/// upcoming 5-tuple extension and stay `0` in this revision.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PacketEvent {
    /// `bpf_ktime_get_ns()` timestamp (CLOCK_MONOTONIC, nanoseconds).
    pub ts_ns: u64,
    /// IPv4 source address, network byte order.
    pub src_ip: u32,
    /// IPv4 destination address, network byte order.
    pub dst_ip: u32,
    /// L4 source port, network byte order; `0` until L4 parsing is enabled.
    pub src_port: u16,
    /// L4 destination port, network byte order; `0` until L4 parsing is enabled.
    pub dst_port: u16,
    /// IP protocol number (IANA), e.g. 1 = ICMP, 6 = TCP, 17 = UDP.
    pub protocol: u8,
    /// Explicit padding: keeps the record 8-byte aligned and leaves room for a
    /// future flag/version byte without changing the size.
    pub reserved: [u8; 3],
}

/// `aya::Pod` promises that the type is plain old data and can be copied from
/// the map memory as-is.  Only implemented for the userspace build where `aya`
/// is available; the eBPF side uses the same layout by construction.
#[cfg(feature = "user")]
unsafe impl aya::Pod for PacketEvent {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_layout_is_stable() {
        // If this ever changes, older userspace binaries would misinterpret
        // ring buffer records and the CSV columns would silently shift.
        assert_eq!(core::mem::size_of::<PacketEvent>(), 24);
        assert_eq!(core::mem::align_of::<PacketEvent>(), 8);
        assert_eq!(core::mem::offset_of!(PacketEvent, src_ip), 8);
        assert_eq!(core::mem::offset_of!(PacketEvent, dst_ip), 12);
        assert_eq!(core::mem::offset_of!(PacketEvent, src_port), 16);
        assert_eq!(core::mem::offset_of!(PacketEvent, dst_port), 18);
        assert_eq!(core::mem::offset_of!(PacketEvent, protocol), 20);
    }
}
