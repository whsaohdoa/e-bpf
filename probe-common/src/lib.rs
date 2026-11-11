//! ABI shared by the XDP program and the userspace loader.
//!
//! The types in this crate are the *single* definition used by both sides:
//! `probe-ebpf` writes them into maps/ring buffers and `probe` reads them back.
//! Keep them `#[repr(C)]`, `Copy` and free of pointers.
//!
//! # ABI compatibility policy
//!
//! This stage intentionally has **no event version field and no version
//! constant**.  The eBPF object is embedded into `probe` at build time
//! (`include_bytes_aligned!`), so the object and the loader always come from the
//! same build.  Any layout change is therefore a breaking change: rebuild the
//! whole workspace, and never run an old `probe` binary against a newly built
//! object.  A version field is only worth adding once the object has to be
//! distributed independently of its loader; at that point start the field at 0.
//!
//! Userspace defences that remain: ring-buffer records must be *exactly*
//! `size_of::<SampleEvent>()` long, and `family` must be 4 or 6.  Window
//! ownership is re-checked in userspace with the same `window_ns` that was
//! written to `CONFIG`.
#![no_std]

// `no_std` and the data path never sees this import.
#[cfg(test)]
extern crate std;

pub mod parse;

/// [`SampleEvent::family`] value for IPv4 records.
pub const FAMILY_IPV4: u8 = 4;

/// [`SampleEvent::family`] value for IPv6 records.
pub const FAMILY_IPV6: u8 = 6;

/// `CONFIG.version`; identifies the `ProbeConfig` layout to the data path.
pub const CONFIG_VERSION: u32 = 1;

/// Direction value used by the aggregation key; Train 0 only observes ingress.
pub const DIRECTION_INGRESS: u8 = 1;

/// `action` value for a passed packet.
pub const ACTION_PASS: u8 = 0;

/// `action` value for a blocked packet (reserved for E6; unused in Train 0).
pub const ACTION_BLOCK: u8 = 1;

/// `STATS[STATS_PACKETS_TOTAL]`: parsed packets seen (all CPUs summed).
pub const STATS_PACKETS_TOTAL: u32 = 0;
/// `STATS[STATS_AGG_OVERFLOW]`: `AGG` inserts that failed (map full/OOM).
pub const STATS_AGG_OVERFLOW: u32 = 1;
/// `STATS[STATS_SAMPLE_FAILED]`: sample ring reservations that failed.
pub const STATS_SAMPLE_FAILED: u32 = 2;
/// `STATS[STATS_CONFIG_MISSING]`: packets using the fallback config.
pub const STATS_CONFIG_MISSING: u32 = 3;
/// Number of slots in the `STATS` per-CPU array (4..7 reserved).
pub const STATS_LEN: u32 = 8;

/// Kernel fallback window when `CONFIG` is missing or `window_ns == 0`.
pub const DEFAULT_WINDOW_NS: u64 = 10_000_000_000;

/// One first-packet sample, written by eBPF on the first packet of a key in a
/// window on each CPU.  Userspace deduplicates by `(window_id, AggKey)` and
/// keeps the record with the smallest `ts_ns`.
///
/// The record is exactly 64 bytes.  Type ABI: no version field, see the crate
/// docs.  `ts_ns` is a single `bpf_ktime_get_ns()` value used both to derive
/// `window_id` and as the source of `first_seen`/`last_seen`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SampleEvent {
    /// `bpf_ktime_get_ns()` timestamp (CLOCK_MONOTONIC, nanoseconds).
    pub ts_ns: u64,
    /// `ts_ns / window_ns`, computed in the data path with `CONFIG.window_ns`.
    pub window_id: u64,
    /// Source address, raw network-order bytes; zero-padded for IPv4.
    pub src_addr: [u8; 16],
    /// Destination address, raw network-order bytes; zero-padded for IPv4.
    pub dst_addr: [u8; 16],
    /// L4 source port, network byte order; `0` for ICMP/ICMPv6.
    pub src_port: u16,
    /// L4 destination port, network byte order; `0` for ICMP/ICMPv6.
    pub dst_port: u16,
    /// IPv4 protocol / IPv6 base-header Next Header.
    pub protocol: u8,
    /// [`FAMILY_IPV4`] or [`FAMILY_IPV6`].
    pub family: u8,
    /// [`ACTION_PASS`] in Train 0 (E6 hook reserved).
    pub action: u8,
    /// Reason code (0 in Train 0; E6 hook reserved).
    pub reason: u8,
    /// Rule id (0 in Train 0; E6 hook reserved).
    pub rule_id: u32,
    /// Always 0.
    pub reserved: u32,
}

/// Per-CPU aggregation key.  Exactly 56 bytes.
///
/// `rule_id` is placed directly after `window_id` so that the following byte
/// fields and the 16-byte arrays stay naturally aligned; the explicit
/// `pad: [u8; 5]` then brings the record to exactly 56 bytes.  (The same field
/// set in "logical" order would insert 2 bytes before `rule_id` and round to
/// 64 bytes.)  Every field, including the padding, is initialised explicitly:
/// uninitialised padding would split one logical key into several map entries.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
#[cfg_attr(feature = "user", derive(PartialEq, Eq, Hash))]
pub struct AggKey {
    /// Window index (`ts_ns / window_ns`).
    pub window_id: u64,
    /// Rule id (0 in Train 0; E6 hook reserved).
    pub rule_id: u32,
    /// [`DIRECTION_INGRESS`].
    pub direction: u8,
    /// [`FAMILY_IPV4`] or [`FAMILY_IPV6`].
    pub family: u8,
    /// Masked source prefix; raw network-order bytes, left-aligned for IPv4.
    pub src_prefix: [u8; 16],
    /// Destination address; raw network-order bytes, left-aligned for IPv4.
    pub dst_addr: [u8; 16],
    /// L4 destination port; 0 for ICMP/ICMPv6.
    pub dst_port: u16,
    /// IPv4 protocol / IPv6 base-header Next Header.
    pub protocol: u8,
    /// [`ACTION_PASS`] in Train 0.
    pub action: u8,
    /// Reason code (0 in Train 0).
    pub reason: u8,
    /// Explicit padding, always 0.
    pub pad: [u8; 5],
}

/// Per-CPU aggregation value.  Exactly 32 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct AggValue {
    /// Packets for this key/window on this CPU.
    pub count: u64,
    /// Smallest `ts_ns` among those packets.
    pub first_seen: u64,
    /// Largest `ts_ns` among those packets.
    pub last_seen: u64,
    /// A sample reservation failed for this key on this CPU.
    pub sample_dropped: u8,
    /// Reserved for the pass-ring backpressure path (unused in Train 0).
    pub degraded: u8,
    /// Explicit padding, always 0.
    pub pad: [u8; 6],
}

/// Data-path configuration written by userspace *before* attach.  Exactly 32
/// bytes.  Userspace must use the same `window_ns` it wrote here, never a
/// second local constant.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ProbeConfig {
    /// [`CONFIG_VERSION`].
    pub version: u32,
    /// Window width in nanoseconds; 0 makes the kernel fall back to
    /// [`DEFAULT_WINDOW_NS`] and increment `STATS_CONFIG_MISSING`.
    pub window_ns: u64,
    /// Grace period after window end in nanoseconds (userspace scan only).
    pub grace_ns: u64,
    /// Source prefix length for IPv4, 0..=32.
    pub ipv4_prefix_len: u8,
    /// Source prefix length for IPv6, 0..=128.
    pub ipv6_prefix_len: u8,
    /// Action for traffic not matching a rule (0 = pass in Train 0).
    pub default_action: u8,
    /// Explicit padding, always 0.
    pub pad: [u8; 5],
}

/// Masks a raw network-order address down to `prefix_len` bits.
///
/// IPv4 addresses are left-aligned with the remaining bytes zero, so the same
/// routine works for both families: a /24 zeroes bytes 3..16, a /0 zeroes
/// everything, and a /128 keeps all 16 bytes.  Out-of-range lengths are clamped
/// instead of panicking; the data path must never fault.
#[inline(always)]
pub fn mask_src_prefix(addr: &[u8; 16], prefix_len: u8) -> [u8; 16] {
    let bits = if prefix_len > 128 {
        128
    } else {
        prefix_len as usize
    };
    let mut out = [0u8; 16];
    let mut i = 0;
    while i < 16 {
        let base = i * 8;
        if base >= bits {
            break;
        }
        if base + 8 <= bits {
            out[i] = addr[i];
        } else {
            let keep = bits - base; // 1..=7
            let mask: u8 = 0xffu8 << (8 - keep);
            out[i] = addr[i] & mask;
        }
        i += 1;
    }
    out
}

impl AggKey {
    /// Rebuilds the aggregation key of a sample so userspace can deduplicate
    /// records by exactly the same key the kernel used.
    pub fn from_sample(sample: &SampleEvent, config: &ProbeConfig) -> Self {
        let prefix_len = match sample.family {
            FAMILY_IPV4 => config.ipv4_prefix_len,
            FAMILY_IPV6 => config.ipv6_prefix_len,
            _ => 128,
        };
        Self {
            window_id: sample.window_id,
            rule_id: sample.rule_id,
            direction: DIRECTION_INGRESS,
            family: sample.family,
            src_prefix: mask_src_prefix(&sample.src_addr, prefix_len),
            dst_addr: sample.dst_addr,
            dst_port: sample.dst_port,
            protocol: sample.protocol,
            action: sample.action,
            reason: sample.reason,
            pad: [0; 5],
        }
    }
}

/// `aya::Pod` promises that the type is plain old data and can be copied from
/// map memory as-is.  Only implemented for the userspace build where `aya` is
/// available; the eBPF side uses the same definitions by construction.
// SAFETY: the four types are `#[repr(C)]`, `Copy`, pointer-free and contain
// only integer/byte-array fields, so any bit pattern is a valid value and a
// byte copy from map memory is a valid `T`.  Layout is locked by the tests
// below; the same definitions are used by the eBPF side.
#[cfg(feature = "user")]
unsafe impl aya::Pod for SampleEvent {}
#[cfg(feature = "user")]
unsafe impl aya::Pod for AggKey {}
#[cfg(feature = "user")]
unsafe impl aya::Pod for AggValue {}
#[cfg(feature = "user")]
unsafe impl aya::Pod for ProbeConfig {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_event_layout_is_pinned() {
        assert_eq!(core::mem::size_of::<SampleEvent>(), 64);
        assert_eq!(core::mem::align_of::<SampleEvent>(), 8);

        assert_eq!(core::mem::offset_of!(SampleEvent, ts_ns), 0);
        assert_eq!(core::mem::offset_of!(SampleEvent, window_id), 8);
        assert_eq!(core::mem::offset_of!(SampleEvent, src_addr), 16);
        assert_eq!(core::mem::offset_of!(SampleEvent, dst_addr), 32);
        assert_eq!(core::mem::offset_of!(SampleEvent, src_port), 48);
        assert_eq!(core::mem::offset_of!(SampleEvent, dst_port), 50);
        assert_eq!(core::mem::offset_of!(SampleEvent, protocol), 52);
        assert_eq!(core::mem::offset_of!(SampleEvent, family), 53);
        assert_eq!(core::mem::offset_of!(SampleEvent, action), 54);
        assert_eq!(core::mem::offset_of!(SampleEvent, reason), 55);
        assert_eq!(core::mem::offset_of!(SampleEvent, rule_id), 56);
        assert_eq!(core::mem::offset_of!(SampleEvent, reserved), 60);
    }

    #[test]
    fn agg_key_layout_is_pinned() {
        assert_eq!(core::mem::size_of::<AggKey>(), 56);
        assert_eq!(core::mem::align_of::<AggKey>(), 8);

        assert_eq!(core::mem::offset_of!(AggKey, window_id), 0);
        assert_eq!(core::mem::offset_of!(AggKey, rule_id), 8);
        assert_eq!(core::mem::offset_of!(AggKey, direction), 12);
        assert_eq!(core::mem::offset_of!(AggKey, family), 13);
        assert_eq!(core::mem::offset_of!(AggKey, src_prefix), 14);
        assert_eq!(core::mem::offset_of!(AggKey, dst_addr), 30);
        assert_eq!(core::mem::offset_of!(AggKey, dst_port), 46);
        assert_eq!(core::mem::offset_of!(AggKey, protocol), 48);
        assert_eq!(core::mem::offset_of!(AggKey, action), 49);
        assert_eq!(core::mem::offset_of!(AggKey, reason), 50);
        assert_eq!(core::mem::offset_of!(AggKey, pad), 51);
    }

    #[test]
    fn agg_value_and_config_layouts_are_pinned() {
        assert_eq!(core::mem::size_of::<AggValue>(), 32);
        assert_eq!(core::mem::offset_of!(AggValue, count), 0);
        assert_eq!(core::mem::offset_of!(AggValue, first_seen), 8);
        assert_eq!(core::mem::offset_of!(AggValue, last_seen), 16);
        assert_eq!(core::mem::offset_of!(AggValue, sample_dropped), 24);
        assert_eq!(core::mem::offset_of!(AggValue, degraded), 25);
        assert_eq!(core::mem::offset_of!(AggValue, pad), 26);

        assert_eq!(core::mem::size_of::<ProbeConfig>(), 32);
        assert_eq!(core::mem::offset_of!(ProbeConfig, version), 0);
        assert_eq!(core::mem::offset_of!(ProbeConfig, window_ns), 8);
        assert_eq!(core::mem::offset_of!(ProbeConfig, grace_ns), 16);
        assert_eq!(core::mem::offset_of!(ProbeConfig, ipv4_prefix_len), 24);
        assert_eq!(core::mem::offset_of!(ProbeConfig, ipv6_prefix_len), 25);
        assert_eq!(core::mem::offset_of!(ProbeConfig, default_action), 26);
        assert_eq!(core::mem::offset_of!(ProbeConfig, pad), 27);
    }

    #[test]
    fn stats_indices_are_pinned() {
        assert_eq!(STATS_PACKETS_TOTAL, 0);
        assert_eq!(STATS_AGG_OVERFLOW, 1);
        assert_eq!(STATS_SAMPLE_FAILED, 2);
        assert_eq!(STATS_CONFIG_MISSING, 3);
        assert_eq!(STATS_LEN, 8);
    }

    #[test]
    fn prefix_mask_handles_boundaries_without_panic() {
        let mut addr = [0u8; 16];
        addr[..4].copy_from_slice(&[10, 99, 0, 2]);

        assert_eq!(mask_src_prefix(&addr, 0), [0u8; 16]);
        let mut expected = [0u8; 16];
        expected[..3].copy_from_slice(&[10, 99, 0]);
        assert_eq!(mask_src_prefix(&addr, 24), expected);
        assert_eq!(mask_src_prefix(&addr, 32)[..4], [10, 99, 0, 2]);
        assert_eq!(mask_src_prefix(&addr, 33), {
            let mut all = [0u8; 16];
            all[..4].copy_from_slice(&[10, 99, 0, 2]);
            all
        });

        let mut mask25 = [0u8; 16];
        mask25[..3].copy_from_slice(&[10, 99, 0]);
        assert_eq!(mask_src_prefix(&addr, 25), mask25);

        let v6: [u8; 16] = [0xfd, 0x00, 0x00, 0x99, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2];
        let mut expected64 = [0u8; 16];
        expected64[..8].copy_from_slice(&v6[..8]);
        assert_eq!(mask_src_prefix(&v6, 64), expected64);
        assert_eq!(mask_src_prefix(&v6, 128), v6);
        assert_eq!(mask_src_prefix(&v6, 0), [0u8; 16]);
        assert_eq!(mask_src_prefix(&v6, 255), v6);
    }

    #[test]
    fn sample_key_uses_prefix_and_keeps_ports() {
        let mut sample = SampleEvent {
            ts_ns: 42,
            window_id: 7,
            src_addr: [0; 16],
            dst_addr: [0; 16],
            src_port: 0x1234,
            dst_port: 0x0050,
            protocol: 6,
            family: FAMILY_IPV4,
            action: ACTION_PASS,
            reason: 0,
            rule_id: 0,
            reserved: 0,
        };
        sample.src_addr[..4].copy_from_slice(&[10, 99, 0, 2]);
        sample.dst_addr[..4].copy_from_slice(&[10, 99, 0, 1]);

        let config = ProbeConfig {
            version: CONFIG_VERSION,
            window_ns: 1_000,
            grace_ns: 100,
            ipv4_prefix_len: 24,
            ipv6_prefix_len: 64,
            default_action: ACTION_PASS,
            pad: [0; 5],
        };
        let key = AggKey::from_sample(&sample, &config);
        assert_eq!(key.window_id, 7);
        assert_eq!(key.family, FAMILY_IPV4);
        assert_eq!(key.src_prefix[..4], [10, 99, 0, 0]);
        assert_eq!(key.dst_addr[..4], [10, 99, 0, 1]);
        assert_eq!(key.dst_port, 0x0050);
        assert_eq!(key.protocol, 6);
        assert_eq!(key.pad, [0; 5]);
    }
}
