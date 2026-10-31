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
use probe_common::PacketEvent;

const ETH_HDR_LEN: usize = 14;
const VLAN_HDR_LEN: usize = 4;

const ETH_P_IP: u16 = 0x0800;
const ETH_P_8021Q: u16 = 0x8100;
const ETH_P_8021AD: u16 = 0x88a8;

const IPV4_MIN_IHL: usize = 5;

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

#[repr(C)]
struct IpHdr {
    version_ihl: u8,
    tos: u8,
    tot_len: u16,
    id: u16,
    frag_off: u16,
    ttl: u8,
    protocol: u8,
    check: u16,
    saddr: u32,
    daddr: u32,
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

/// Parses Ethernet (with up to two stacked 802.1Q/802.1ad tags) -> IPv4.
///
/// Returns `(saddr, daddr, protocol)` with addresses in network byte order as
/// they appear in the IPv4 header.  Any malformed/unsupported packet is
/// reported as `Err(())` and simply passed to the stack.
#[inline(always)]
fn parse_ipv4(ctx: &XdpContext) -> Result<(u32, u32, u8), ()> {
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

    if ether_type != ETH_P_IP {
        return Err(());
    }

    let ip = unsafe { ptr_at::<IpHdr>(ctx, l3_offset)? };
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

    let src = unsafe { (*ip).saddr };
    let dst = unsafe { (*ip).daddr };
    let protocol = unsafe { (*ip).protocol };
    Ok((src, dst, protocol))
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
    if let Ok((src_ip, dst_ip, protocol)) = parse_ipv4(&ctx) {
        emit(PacketEvent {
            ts_ns: unsafe { bpf_ktime_get_ns() },
            src_ip,
            dst_ip,
            // Reserved for the 5-tuple extension; L4 is intentionally not
            // parsed in this revision.
            src_port: 0,
            dst_port: 0,
            protocol,
            reserved: [0; 3],
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
