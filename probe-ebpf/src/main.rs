#![no_std]
#![no_main]

use aya_ebpf::{
    bindings::xdp_action,
    helpers::bpf_ktime_get_ns,
    macros::{map, xdp},
    maps::{Array, PerCpuArray, PerCpuHashMap, RingBuf},
    programs::XdpContext,
};
use core::mem;
use probe_common::{
    ACTION_PASS, AggKey, AggValue, CONFIG_VERSION, DEFAULT_WINDOW_NS, DIRECTION_INGRESS,
    FAMILY_IPV4, FAMILY_IPV6, ProbeConfig, STATS_AGG_OVERFLOW, STATS_CONFIG_MISSING,
    STATS_PACKETS_TOTAL, STATS_SAMPLE_FAILED, SampleEvent, mask_src_prefix,
};

const ETH_HDR_LEN: usize = 14;
const VLAN_HDR_LEN: usize = 4;

const ETH_P_IP: u16 = 0x0800;
const ETH_P_IPV6: u16 = 0x86dd;
const ETH_P_8021Q: u16 = 0x8100;
const ETH_P_8021AD: u16 = 0x88a8;

const IPV4_MIN_IHL: usize = 5;

/// IPv6 has a fixed 40-byte base header; there is no `ihl` equivalent.
const IPV6_HDR_LEN: usize = 40;

/// L4 protocol numbers whose first four bytes are the source/destination port.
const IPPROTO_TCP: u8 = 6;
const IPPROTO_UDP: u8 = 17;

/// First-packet samples.  Sized as a power-of-two multiple of the page size as
/// required by the kernel ring buffer.  Samples are tiny (one per key/window
/// per CPU), so 256 KiB is far more than enough for Train 0.
#[map]
static SAMPLES: RingBuf = RingBuf::with_byte_size(256 * 1024, 0);

/// Data-path config, written once by userspace before attach.
#[map]
static CONFIG: Array<ProbeConfig> = Array::with_max_entries(1, 0);

/// Per-CPU aggregation map.  131072 keys; key/value are 56/32 bytes.
#[map]
static AGG: PerCpuHashMap<AggKey, AggValue> = PerCpuHashMap::with_max_entries(131_072, 0);

/// Per-CPU counters.  Indexes are the `STATS_*` constants in `probe-common`.
#[map]
static STATS: PerCpuArray<u64> = PerCpuArray::with_max_entries(8, 0);

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

/// First four bytes of a TCP/UDP header, i.e. both ports.  Kept as `u16` bytes
/// so the data path only ever copies network-order values.
#[repr(C)]
struct L4Ports {
    src: u16,
    dst: u16,
}

const _: () = assert!(
    mem::size_of::<Ipv6Hdr>() == IPV6_HDR_LEN,
    "IPv6 base header must be exactly 40 bytes"
);

/// Addresses + protocol + L4 ports of one packet, in the shape of the ABI.
struct L3Info {
    src: [u8; 16],
    dst: [u8; 16],
    src_port: u16,
    dst_port: u16,
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

/// Parses TCP/UDP ports at `l4_offset`.  Missing/truncated L4 returns `(0, 0)`
/// instead of dropping the whole packet: the aggregate still counts it.
#[inline(always)]
fn parse_l4_ports(ctx: &XdpContext, l4_offset: usize, protocol: u8) -> (u16, u16) {
    if protocol != IPPROTO_TCP && protocol != IPPROTO_UDP {
        return (0, 0);
    }
    match unsafe { ptr_at::<L4Ports>(ctx, l4_offset) } {
        Ok(ports) => unsafe { ((*ports).src, (*ports).dst) },
        Err(()) => (0, 0),
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
    // (ihl > 5) are inside the packet before adding `ihl * 4` to the offset.
    if ctx.data() + l3_offset + ihl * 4 > ctx.data_end() {
        return Err(());
    }

    let protocol = unsafe { (*ip).protocol };
    let src4 = unsafe { (*ip).saddr };
    let dst4 = unsafe { (*ip).daddr };

    // A non-first fragment has no L4 header; leave the ports 0.  First
    // fragments (offset 0) still carry the L4 header and are parsed.
    let frag_off = u16::from_be(unsafe { (*ip).frag_off });
    let (src_port, dst_port) = if frag_off & 0x1fff != 0 {
        (0, 0)
    } else {
        parse_l4_ports(ctx, l3_offset + ihl * 4, protocol)
    };

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
/// protocol.  Ports are only parsed when that next-header value is TCP/UDP.
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

    let protocol = unsafe { (*ip).next_header };
    let (src_port, dst_port) = parse_l4_ports(ctx, l3_offset + IPV6_HDR_LEN, protocol);

    Ok(L3Info {
        src: unsafe { (*ip).saddr },
        dst: unsafe { (*ip).daddr },
        src_port,
        dst_port,
        // Base-header Next Header.  Do not walk extension headers.
        protocol,
        family: FAMILY_IPV6,
    })
}

/// Reads `CONFIG` and applies the safe fallbacks.  No packet is ever dropped
/// because of missing config, and `window_ns == 0` never divides by zero.
#[inline(always)]
fn load_config() -> ProbeConfig {
    let mut config = match CONFIG.get(0) {
        Some(config) => *config,
        None => ProbeConfig {
            version: 0,
            window_ns: 0,
            grace_ns: 0,
            ipv4_prefix_len: 24,
            ipv6_prefix_len: 64,
            default_action: ACTION_PASS,
            pad: [0; 5],
        },
    };

    let mut invalid = config.version != CONFIG_VERSION;
    if config.window_ns == 0 {
        config.window_ns = DEFAULT_WINDOW_NS;
        invalid = true;
    }
    if config.ipv4_prefix_len > 32 {
        config.ipv4_prefix_len = 32;
        invalid = true;
    }
    if config.ipv6_prefix_len > 128 {
        config.ipv6_prefix_len = 128;
        invalid = true;
    }
    if invalid {
        stat_inc(STATS_CONFIG_MISSING);
    }
    config
}

/// Increments the current CPU's slot of a `STATS` counter.
#[inline(always)]
fn stat_inc(index: u32) {
    if let Some(counter) = STATS.get_ptr_mut(index) {
        // SAFETY: `index` comes from the fixed `STATS_*` constants, within the
        // map's 8 slots, and `PerCpuArray` gives this CPU an exclusive slot.
        unsafe { *counter += 1 };
    }
}

/// Updates the per-CPU aggregate for one packet and returns `true` when this is
/// the first packet for `(key, this CPU, this window)`, i.e. when a sample must
/// be attempted.  A full `AGG` map increments `STATS_AGG_OVERFLOW` and skips
/// the sample, but never blocks the data path.
#[inline(always)]
fn aggregate(key: &AggKey, ts_ns: u64) -> bool {
    let mut value = AGG.get_ptr_mut(key);
    if value.is_none() {
        let initial = AggValue::default();
        if AGG.insert(key, &initial, 0).is_err() {
            stat_inc(STATS_AGG_OVERFLOW);
            return false;
        }
        value = AGG.get_ptr_mut(key);
    }

    let Some(value) = value else {
        // The key disappeared or the insert failed after reporting; counting it
        // as overflow keeps the failure visible without touching XDP's verdict.
        stat_inc(STATS_AGG_OVERFLOW);
        return false;
    };

    // SAFETY: per-CPU hash maps give this CPU exclusive ownership of its value
    // slot.  `value` was obtained from `get_ptr_mut` and is non-null.
    unsafe {
        if (*value).count == 0 {
            (*value).count = 1;
            (*value).first_seen = ts_ns;
            (*value).last_seen = ts_ns;
            true
        } else {
            (*value).count += 1;
            if ts_ns < (*value).first_seen {
                (*value).first_seen = ts_ns;
            }
            if ts_ns > (*value).last_seen {
                (*value).last_seen = ts_ns;
            }
            false
        }
    }
}

/// Tries to enqueue exactly one 64-byte sample.  A full ring buffer is counted
/// in `STATS[SAMPLE_FAILED]` and flagged on the aggregate; it never blocks and
/// never changes the XDP action.
#[inline(always)]
fn emit_sample(sample: SampleEvent, key: &AggKey) {
    match SAMPLES.reserve::<SampleEvent>(0) {
        // SAFETY: the reservation is exactly `size_of::<SampleEvent>()` bytes
        // and the verifier requires every reservation to be submitted or
        // discarded.
        Some(mut entry) => {
            entry.write(sample);
            entry.submit(0);
        }
        None => {
            stat_inc(STATS_SAMPLE_FAILED);
            if let Some(value) = AGG.get_ptr_mut(key) {
                // SAFETY: same per-CPU exclusive slot as in `aggregate`.
                unsafe { (*value).sample_dropped = 1 };
            }
        }
    }
}

fn try_probe(ctx: XdpContext) -> u32 {
    if let Ok(pkt) = parse_packet(&ctx) {
        // Exactly one ktime call per packet: the same timestamp is used for the
        // window id and for first/last_seen.
        let ts_ns = unsafe { bpf_ktime_get_ns() };
        stat_inc(STATS_PACKETS_TOTAL);

        let config = load_config();
        let window_id = ts_ns / config.window_ns;
        let prefix_len = match pkt.family {
            FAMILY_IPV4 => config.ipv4_prefix_len,
            _ => config.ipv6_prefix_len,
        };

        // E6 hook: rule matching uses `default_action`, `rule_id` and `reason`
        // here.  Train 0 only observes: action is always pass and ids are 0.
        let action = ACTION_PASS;
        let rule_id = 0u32;
        let reason = 0u8;

        let key = AggKey {
            window_id,
            rule_id,
            direction: DIRECTION_INGRESS,
            family: pkt.family,
            src_prefix: mask_src_prefix(&pkt.src, prefix_len),
            dst_addr: pkt.dst,
            dst_port: pkt.dst_port,
            protocol: pkt.protocol,
            action,
            reason,
            pad: [0; 5],
        };

        if aggregate(&key, ts_ns) {
            emit_sample(
                SampleEvent {
                    ts_ns,
                    window_id,
                    src_addr: pkt.src,
                    dst_addr: pkt.dst,
                    src_port: pkt.src_port,
                    dst_port: pkt.dst_port,
                    protocol: pkt.protocol,
                    family: pkt.family,
                    action,
                    reason,
                    rule_id,
                    reserved: 0,
                },
                &key,
            );
        }
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
