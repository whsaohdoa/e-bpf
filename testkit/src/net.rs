//! Kernel side plumbing: throw-away veth pairs, `ip`/`bpftool` wrappers.
//!
//! No network namespaces are involved. Both veth ends stay in the root
//! namespace and no address is assigned: the probe only needs ingress frames on
//! one end and the sender only needs egress on the other. That makes the
//! environment per-process unique, parallel safe, and cheap to tear down.

use std::{
    collections::BTreeMap,
    fs, io,
    process::Command,
    sync::atomic::{AtomicU32, Ordering},
};

/// MAC of the probe side (`VethPair::left`).
pub const PROBE_SIDE_MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 0x01];
/// MAC of the sending side (`VethPair::right`).
pub const SENDER_SIDE_MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 0x02];

static PAIRS: AtomicU32 = AtomicU32::new(0);

/// Runs `program args...`, returning stdout; failures carry the stderr text.
pub fn run_cmd(program: &str, args: &[&str]) -> io::Result<String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|err| io::Error::new(err.kind(), format!("spawn `{program}`: {err}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::other(format!(
            "`{program} {}` failed with {}: {}",
            args.join(" "),
            output.status,
            stderr.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `ip` with the given arguments.
pub fn ip(args: &[&str]) -> io::Result<String> {
    run_cmd("ip", args)
}

/// `bpftool` with the given arguments.
pub fn bpftool(args: &[&str]) -> io::Result<String> {
    run_cmd("bpftool", args)
}

/// A veth pair whose `left` end is the probe's ingress interface and whose
/// `right` end is where the sender injects frames.
pub struct VethPair {
    pub left: String,
    pub right: String,
    alive: std::cell::Cell<bool>,
}

impl VethPair {
    /// Creates the pair, disables IPv6 (so the kernel does not inject its own
    /// link-local/DAD traffic into the counters) and brings both ends up.
    pub fn new(tag: &str) -> io::Result<Self> {
        let serial = PAIRS.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id() % 1_000_000;
        let base = format!("p2e{pid}{tag}{serial}");
        let left = format!("{base}a");
        let right = format!("{base}b");
        if left.len() > 15 || right.len() > 15 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("interface name too long: {left}/{right}"),
            ));
        }
        let pair = Self {
            left,
            right,
            alive: std::cell::Cell::new(false),
        };
        ip(&[
            "link",
            "add",
            &pair.left,
            "address",
            "02:00:00:00:00:01",
            "type",
            "veth",
            "peer",
            "name",
            &pair.right,
            "address",
            "02:00:00:00:00:02",
        ])?;
        pair.alive.set(true);
        // IPv6 must be off before `up`, otherwise the kernel advertises a
        // link-local address and those frames land in the counters.
        pair.disable_ipv6(&pair.left)?;
        pair.disable_ipv6(&pair.right)?;
        pair.settle()?;
        Ok(pair)
    }

    fn settle(&self) -> io::Result<()> {
        ip(&["link", "set", &self.left, "up"])?;
        ip(&["link", "set", &self.right, "up"])?;
        Ok(())
    }

    fn disable_ipv6(&self, iface: &str) -> io::Result<()> {
        let path = format!("/proc/sys/net/ipv6/conf/{iface}/disable_ipv6");
        match fs::write(&path, b"1\n") {
            Ok(()) => Ok(()),
            // Kernels built without IPv6 have no sysctl at all.
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(io::Error::new(err.kind(), format!("write {path}: {err}"))),
        }
    }

    /// The interface the probe attaches to.
    pub fn probe_iface(&self) -> &str {
        &self.left
    }

    /// The interface the sender transmits from.
    pub fn sender_iface(&self) -> &str {
        &self.right
    }

    /// Deletes the pair explicitly; `Drop` does the same for the panic path.
    pub fn close(&self) {
        self.release();
    }

    fn release(&self) {
        if self.alive.get() {
            self.alive.set(false);
            let _ = ip(&["link", "del", &self.left]);
        }
    }
}

impl Drop for VethPair {
    fn drop(&mut self) {
        self.release();
    }
}

/// `true` when an XDP program is attached to `iface` according to `bpftool net`.
pub fn xdp_attached(iface: &str) -> io::Result<bool> {
    Ok(xdp_prog_id(iface)?.is_some())
}

/// Program id of the XDP program attached to `iface`, if any.
///
/// `bpftool net` prints the attachments unindented under a section header, e.g.
///
/// ```text
/// xdp:
/// veth0(15) driver id 761
///
/// tc:
/// ```
///
/// The id identifies *this* attachment, so a case can read the `run_cnt` of its
/// own program while other cases run the same binary; matching by program name
/// would sum the counters of every concurrent case.
pub fn xdp_prog_id(iface: &str) -> io::Result<Option<u32>> {
    let text = bpftool(&["net"])?;
    let prefix = format!("{iface}(");
    let mut in_xdp = false;
    for line in text.lines() {
        let entry = line.trim();
        if entry == "xdp:" {
            in_xdp = true;
            continue;
        }
        if !in_xdp {
            continue;
        }
        // The section ends at a blank line or at the next `something:` header.
        if entry.is_empty() || (entry.ends_with(':') && !entry.contains('(')) {
            in_xdp = false;
            continue;
        }
        if !entry.starts_with(&prefix) {
            continue;
        }
        let mut tokens = entry.split_whitespace();
        while let Some(token) = tokens.next() {
            if token == "id" {
                return Ok(tokens.next().and_then(|id| id.parse().ok()));
            }
        }
        return Ok(None);
    }
    Ok(None)
}

/// `run_cnt` of one program id.
///
/// `None` when the kernel does not maintain the counter, which is the case
/// while `kernel.bpf_stats_enabled` is 0.
pub fn prog_run_cnt(id: u32) -> io::Result<Option<u64>> {
    let text = bpftool(&["prog", "show", "id", &id.to_string()])?;
    Ok(text.lines().find_map(|line| {
        let rest = line.split_once("run_cnt ")?.1;
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        digits.parse().ok()
    }))
}

/// `CLOCK_MONOTONIC` in nanoseconds.
///
/// This is the clock `bpf_ktime_get_ns()` returns, so a case can line a burst
/// up with the kernel's aggregation-window boundaries instead of sleeping.
pub fn monotonic_ns() -> u64 {
    #[repr(C)]
    struct Timespec {
        tv_sec: i64,
        tv_nsec: i64,
    }

    unsafe extern "C" {
        fn clock_gettime(clk_id: i32, tp: *mut Timespec) -> i32;
    }

    const CLOCK_MONOTONIC: i32 = 1;

    let mut ts = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable `struct timespec` for the duration of
    // the call and `CLOCK_MONOTONIC` is a valid clock id.
    let rc = unsafe { clock_gettime(CLOCK_MONOTONIC, &mut ts) };
    assert_eq!(
        rc,
        0,
        "clock_gettime(CLOCK_MONOTONIC) failed: {}",
        io::Error::last_os_error()
    );
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// Pins the calling thread to the CPU it is running on and returns that CPU.
///
/// XDP runs in the `NET_RX` softirq of the CPU that queued the frame, so pinning
/// the sending thread keeps every map insert in one per-CPU hash.  Without it the
/// small-map overflow case would depend on where the scheduler put the thread.
pub fn pin_current_cpu() -> io::Result<i32> {
    unsafe extern "C" {
        fn sched_getcpu() -> i32;
        fn sched_setaffinity(pid: i32, cpusetsize: usize, mask: *const u8) -> i32;
    }

    /// `cpu_set_t` is 128 bytes on every Linux ABI glibc supports.
    const CPU_SET_BYTES: usize = 128;

    // SAFETY: both calls only read/write kernel-side per-thread state.
    let cpu = unsafe { sched_getcpu() };
    if cpu < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut mask = [0u8; CPU_SET_BYTES];
    mask[cpu as usize / 8] = 1u8 << ((cpu % 8) as u32);
    // SAFETY: `mask` outlives the call and holds `CPU_SET_BYTES` bytes, which is
    // the kernel's `cpusetsize` for the current CPU set.
    if unsafe { sched_setaffinity(0, mask.len(), mask.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cpu)
}

/// Reads `kernel.bpf_stats_enabled`.
pub fn bpf_stats_enabled() -> bool {
    fs::read_to_string("/proc/sys/kernel/bpf_stats_enabled")
        .map(|text| text.trim() == "1")
        .unwrap_or(false)
}

/// Enables `kernel.bpf_stats_enabled` so `run_cnt` is maintained.
pub fn enable_bpf_stats() -> io::Result<()> {
    fs::write("/proc/sys/kernel/bpf_stats_enabled", b"1\n")
        .map_err(|err| io::Error::new(err.kind(), format!("enable bpf_stats: {err}")))
}

/// Names and ids of the maps one loaded program holds.
///
/// Every case writes the same map names (`CONFIG`, `STATS`, `AGG`), so looking a
/// map up by name would read whichever probe happens to be first when cases run
/// in parallel.  `bpftool prog show id N` lists `map_ids 12,13,14,15` for *this*
/// program, and `bpftool map show id M` gives each one back its name.
pub fn prog_map_ids(prog_id: u32) -> io::Result<BTreeMap<String, u32>> {
    let text = bpftool(&["prog", "show", "id", &prog_id.to_string()])?;
    let ids: Vec<u32> = text
        .lines()
        .find_map(|line| line.split_once("map_ids ").map(|(_, rest)| rest))
        .and_then(|rest| rest.split_whitespace().next())
        .map(|list| {
            list.split(',')
                .filter_map(|id| id.parse::<u32>().ok())
                .collect()
        })
        .unwrap_or_default();

    let mut maps = BTreeMap::new();
    for id in ids {
        let text = bpftool(&["map", "show", "id", &id.to_string()])?;
        // `12: percpu_array  name STATS  flags 0x0`
        let name = text
            .split_whitespace()
            .skip_while(|token| *token != "name")
            .nth(1)
            .map(str::to_owned);
        if let Some(name) = name {
            maps.insert(name, id);
        }
    }
    Ok(maps)
}

/// Sum of one counter of a `STATS` per-CPU array over all CPUs.
///
/// `bpftool map dump` prints the array as `key:` followed by one
/// `value (CPU NN): <hex bytes>` line per CPU, so the slot is decoded
/// little-endian and summed.  The counters are the probe's contract with the
/// kernel, which is why the cases read them instead of the exit-only stats line.
pub fn stat_counter(map_id: u32, index: u32) -> io::Result<u64> {
    let text = bpftool(&["map", "dump", "id", &map_id.to_string()])?;
    let mut pending_key = false;
    let mut current: Option<u32> = None;
    let mut total = 0u64;
    for line in text.lines().map(str::trim) {
        if line == "key:" {
            pending_key = true;
            continue;
        }
        if pending_key {
            let bytes = hex_bytes(line)?;
            current = Some(bytes.iter().enumerate().fold(0u32, |acc, (shift, byte)| {
                acc | (u32::from(*byte) << (8 * shift))
            }));
            pending_key = false;
            continue;
        }
        let Some(rest) = line.strip_prefix("value (CPU ") else {
            continue;
        };
        if current != Some(index) {
            continue;
        }
        let value = rest.split_once("):").map(|(_, value)| value).unwrap_or("");
        let bytes = hex_bytes(value)?;
        total += bytes.iter().enumerate().fold(0u64, |acc, (shift, byte)| {
            acc | (u64::from(*byte) << (8 * shift))
        });
    }
    Ok(total)
}

/// Replaces the `CONFIG` entry of map `map_id` with `value` (already laid out
/// like `ProbeConfig`).  Used by the cases that need the data path to see a
/// config it must reject; a loaded array map cannot be deleted from user space.
pub fn update_config(map_id: u32, value: &[u8]) -> io::Result<()> {
    // bpftool parses each byte as a number, so the bytes need a `0x` prefix;
    // bare `c2` fails with "error parsing byte: c2".
    let hex: Vec<String> = value.iter().map(|byte| format!("0x{byte:02x}")).collect();
    let id = map_id.to_string();
    let mut args: Vec<&str> = vec![
        "map", "update", "id", &id, "key", "0", "0", "0", "0", "value",
    ];
    args.extend(hex.iter().map(String::as_str));
    bpftool(&args).map(|_| ())
}

/// Decodes whitespace-separated hex bytes (`00 c2 eb 0b`).
fn hex_bytes(text: &str) -> io::Result<Vec<u8>> {
    text.split_whitespace()
        .map(|byte| {
            u8::from_str_radix(byte.trim_start_matches("0x"), 16).map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("expected a hex byte, got {byte:?}: {err}"),
                )
            })
        })
        .collect()
}
