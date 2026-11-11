#![no_std]
#![no_main]

use aya_ebpf::{
    bindings::xdp_action,
    helpers::bpf_ktime_get_ns,
    macros::{map, xdp},
    maps::{Array, PerCpuArray, PerCpuHashMap, RingBuf},
    programs::XdpContext,
};
use probe_common::{
    ACTION_PASS, AggKey, AggValue, CONFIG_VERSION, DEFAULT_WINDOW_NS, DIRECTION_INGRESS,
    FAMILY_IPV4, ProbeConfig, STATS_AGG_OVERFLOW, STATS_CONFIG_MISSING, STATS_PACKETS_TOTAL,
    STATS_SAMPLE_FAILED, SampleEvent, mask_src_prefix,
    parse::{Pkt, parse_ip},
};

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

/// Borrowed view of the XDP packet window.
///
/// All verifier-visible bounds checking lives in [`Pkt::bytes`]; the parsing
/// itself is `probe_common::parse`, shared with the L0 host tests.
struct XdpPkt {
    data: usize,
    data_end: usize,
}

impl Pkt for XdpPkt {
    #[inline(always)]
    fn bytes(&self, off: usize, len: usize) -> Option<&[u8]> {
        let from = self.data.checked_add(off)?;
        let to = from.checked_add(len)?;
        if to > self.data_end {
            return None;
        }
        // SAFETY: `[from, to)` was just proven to lie inside the packet window
        // `[data, data_end)`.  XDP packet bytes are read-only for the duration
        // of this invocation and the borrow cannot outlive `self`.
        Some(unsafe { core::slice::from_raw_parts(from as *const u8, len) })
    }
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
    let frame = XdpPkt {
        data: ctx.data(),
        data_end: ctx.data_end(),
    };

    if let Ok(pkt) = parse_ip(&frame) {
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
