#![no_std]
#![no_main]

use aya_ebpf::{
    bindings::xdp_action,
    helpers::bpf_ktime_get_ns,
    macros::{map, xdp},
    maps::{PerCpuArray, RingBuf},
    programs::XdpContext,
};
use core::mem;
use probe_common::{EVENT_ABI_VERSION, FAMILY_IPV4, FAMILY_IPV6, PacketEvent};

const ETH_HDR_LEN: usize = 14;
const VLAN_HDR_LEN: usize = 4;

const ETH_P_IP: u16 = 0x0800;
const ETH_P_IPV6: u16 = 0x86dd;
const ETH_P_8021Q: u16 = 0x8100;
const ETH_P_8021AD: u16 = 0x88a8;

const IPV4_MIN_IHL: usize = 5;

/// IPv6 has a fixed 40-byte base header; there is no `ihl` equivalent.
const IPV6_HDR_LEN: usize = 40;

/// Events waiting for userspace.  Sized as a power-of-two multiple of the page
/// size as required by the kernel ring buffer.
#[map]
static EVENTS: RingBuf = RingBuf::with_byte_size(256 * 1024, 0);

/// `DROPS[0]` counts events lost because the ring buffer was full.  A per-CPU
/// array keeps the increment contention-free; userspace sums all slots.
#[map]
static DROPS: PerCpuArray<u64> = PerCpuArray::with_max_entries(1, 0);

#[repr(C)]
struct EthHdr {
    dst: [u8; 6],
    src: [u8; 6],
    ether_type: u16,
}

#[repr(C)]
struct VlanHdr {
    tci: u16,
    encap_proto: u16,
}

/// Fixed 20-byte IPv4 header.  Addresses stay `[u8; 4]` so no byte swapping is
/// involved anywhere on the data path.
#[repr(C)]
struct Ipv4Hdr {
    version_ihl: u8,
    tos: u8,
    tot_len: u16,
    id: u16,
    frag_off: u16,
    ttl: u8,
    protocol: u8,
    check: u16,
    saddr: [u8; 4],
    daddr: [u8; 4],
}

/// Fixed 40-byte IPv6 base header.
///
/// The first 32 bits combine version/traffic class/flow label.  They are kept
/// as bytes so the version nibble can be read from the first byte directly
/// instead of decoding a `u32`.
#[repr(C)]
struct Ipv6Hdr {
    version_tc_flow: [u8; 4],
    payload_len: u16,
    next_header: u8,
    hop_limit: u8,
    saddr: [u8; 16],
    daddr: [u8; 16],
}

const _: () = assert!(
    mem::size_of::<Ipv6Hdr>() == IPV6_HDR_LEN,
    "IPv6 base header must be exactly 40 bytes"
);

/// Addresses + protocol of one packet, already in the shape of the ABI event.
struct L3Info {
    src: [u8; 16],
    dst: [u8; 16],
    protocol: u8,
    family: u8,
}

/// Returns a pointer to `T` at `offset` from the start of the packet, after
/// proving to the verifier that all `size_of::<T>()` bytes are within
/// `[data, data_end)`.
#[inline(always)]
unsafe fn ptr_at<T>(ctx: &XdpContext, offset: usize) -> Result<*const T, ()> {
    let start = ctx.data();
    let end = ctx.data_end();
    let len = mem::size_of::<T>();
    if start + offset + len > end {
        return Err(());
    }
    Ok((start + offset) as *const T)
}

/// Parses Ethernet (with up to two stacked 802.1Q/802.1ad tags) and dispatches
/// on the innermost ethertype: `0x0800` -> IPv4, `0x86DD` -> IPv6.
///
/// Returns addresses as raw network-order bytes, left-aligned in 16-byte fields.
/// Any malformed/unsupported packet is reported as `Err(())` and simply passed
/// to the stack.
#[inline(always)]
fn parse_packet(ctx: &XdpContext) -> Result<L3Info, ()> {
    let eth = unsafe { ptr_at::<EthHdr>(ctx, 0)? };
    let mut ether_type = u16::from_be(unsafe { (*eth).ether_type });
    let mut l3_offset = ETH_HDR_LEN;

    for _ in 0..2 {
        if ether_type != ETH_P_8021Q && ether_type != ETH_P_8021AD {
            break;
        }
        let vlan = unsafe { ptr_at::<VlanHdr>(ctx, l3_offset)? };
        ether_type = u16::from_be(unsafe { (*vlan).encap_proto });
        l3_offset += VLAN_HDR_LEN;
    }

    match ether_type {
        ETH_P_IP => parse_ipv4(ctx, l3_offset),
        ETH_P_IPV6 => parse_ipv6(ctx, l3_offset),
        _ => Err(()),
    }
}

/// Parses the fixed IPv4 header at `l3_offset`.
#[inline(always)]
fn parse_ipv4(ctx: &XdpContext, l3_offset: usize) -> Result<L3Info, ()> {
    let ip = unsafe { ptr_at::<Ipv4Hdr>(ctx, l3_offset)? };
    let version_ihl = unsafe { (*ip).version_ihl };
    if version_ihl >> 4 != 4 {
        return Err(());
    }

    let ihl = (version_ihl & 0x0f) as usize;
    if ihl < IPV4_MIN_IHL {
        return Err(());
    }

    // The fixed 20 bytes were checked by `ptr_at`; also make sure IPv4 options
    // (ihl > 5) are inside the packet so the future L4 parser can safely use
    // `l4_offset = l3_offset + ihl * 4` and re-check `data_end` after that.
    if ctx.data() + l3_offset + ihl * 4 > ctx.data_end() {
        return Err(());
    }

    let protocol = unsafe { (*ip).protocol };
    let src4 = unsafe { (*ip).saddr };
    let dst4 = unsafe { (*ip).daddr };

    // Left-align the 4 address bytes and zero the rest of the 16-byte field.
    // Fixed-length copies only: no slicing, so there is no bounds-check panic
    // path for the verifier to reason about.
    let mut src = [0u8; 16];
    let mut dst = [0u8; 16];
    // SAFETY: both source arrays are 4 bytes long and both destinations hold 16
    // bytes, so copying 4 bytes cannot go out of bounds.  The packet bytes are
    // inside `[data, data_end)` after the `ptr_at` check above.
    unsafe {
        core::ptr::copy_nonoverlapping(src4.as_ptr(), src.as_mut_ptr(), 4);
        core::ptr::copy_nonoverlapping(dst4.as_ptr(), dst.as_mut_ptr(), 4);
    }

    Ok(L3Info {
        src,
        dst,
        protocol,
        family: FAMILY_IPV4,
    })
}

/// Parses the fixed 40-byte IPv6 base header at `l3_offset`.
///
/// Extension headers are deliberately not walked: `protocol` is the Next Header
/// field of the base header, which may be 0/43/44/51/60 instead of the final L4
/// protocol.  No payload bytes are read.
#[inline(always)]
fn parse_ipv6(ctx: &XdpContext, l3_offset: usize) -> Result<L3Info, ()> {
    // Proves `[l3_offset, l3_offset + 40)` is inside `[data, data_end)`;
    // truncated packets fall through to `Err(())` and are passed up.
    let ip = unsafe { ptr_at::<Ipv6Hdr>(ctx, l3_offset)? };

    // The version is the high nibble of the first header byte; read it
    // directly instead of decoding the version/traffic-class/flow-label word.
    let first_byte = unsafe { (*ip).version_tc_flow[0] };
    if first_byte >> 4 != 6 {
        return Err(());
    }

    Ok(L3Info {
        src: unsafe { (*ip).saddr },
        dst: unsafe { (*ip).daddr },
        // Base-header Next Header.  Do not walk extension headers.
        protocol: unsafe { (*ip).next_header },
        family: FAMILY_IPV6,
    })
}

#[inline(always)]
fn emit(event: PacketEvent) {
    match EVENTS.reserve::<PacketEvent>(0) {
        // SAFETY: the reservation is exactly `size_of::<PacketEvent>()` bytes
        // and the verifier requires every reservation to be submitted or
        // discarded.
        Some(mut entry) => {
            entry.write(event);
            entry.submit(0);
        }
        // Never block the data path; account for the loss and move on.
        None => {
            if let Some(drops) = DROPS.get_ptr_mut(0) {
                unsafe { *drops += 1 };
            }
        }
    }
}

fn try_probe(ctx: XdpContext) -> u32 {
    if let Ok(pkt) = parse_packet(&ctx) {
        emit(PacketEvent {
            ts_ns: unsafe { bpf_ktime_get_ns() },
            src_addr: pkt.src,
            dst_addr: pkt.dst,
            // Reserved for the 5-tuple extension; L4 is intentionally not
            // parsed in this revision.
            src_port: 0,
            dst_port: 0,
            protocol: pkt.protocol,
            family: pkt.family,
            version: EVENT_ABI_VERSION,
            reserved: 0,
        });
    }

    // This observer must never alter forwarding behaviour.
    xdp_action::XDP_PASS
}

#[xdp]
pub fn probe(ctx: XdpContext) -> u32 {
    try_probe(ctx)
}

#[cfg(all(not(test), target_arch = "bpf"))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[unsafe(link_section = "license")]
#[unsafe(no_mangle)]
static LICENSE: [u8; 13] = *b"Dual MIT/GPL\0";
