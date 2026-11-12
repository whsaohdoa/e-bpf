//! Minimal std-only UDP traffic generator / receiver for the A9 performance comparison.
//!
//! This file is intentionally NOT part of the cargo workspace. Compile it standalone:
//!
//!     rustc -O verification/traffic_gen.rs -o /tmp/traffic_gen
//!
//! Send mode:
//!     /tmp/traffic_gen send --dst 10.99.0.1:9999 --pps 100000 --secs 5 --payload 64
//!     -> prints one line:  sent=N elapsed_ms=T pps=X target_pps=P payload=B dports=D
//!
//! Send mode with many destination ports (creates many distinct aggregation keys):
//!     /tmp/traffic_gen send --dst 10.99.0.1:9999 --dports 50000 --pps 100000 --secs 5
//!     dest port of packet i = base_port + (i % dports), so with --dports 50000 and
//!     base port 9999 the flow spreads over ports 9999..59998.
//!
//! Recv mode (keeps the port bound so ICMP port-unreachable never disturbs the sender):
//!     /tmp/traffic_gen recv --bind 10.99.0.1:9999 --secs 8
//!     -> prints progress lines and a final line: received=N elapsed_ms=T pps=X
//!     recv binds ONLY the base port; the other --dports ports are unbound and simply
//!     generate ICMP port-unreachable, which does not affect send accounting.
//!
//! Pacing detail: send mode uses a 1 ms absolute-deadline tick loop.  Every tick it
//! releases the number of packets that *should* have been sent by now (fractional
//! remainder carried forward), then sleeps until the tick deadline.  If the socket
//! cannot sustain the requested rate the loop simply runs flat out; the reported
//! `pps=` is always measured (`sent / elapsed`), never the requested target.

use std::env;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::process;
use std::thread;
use std::time::{Duration, Instant};

const USAGE: &str = "\
traffic_gen (std-only UDP load generator)

Usage:
  traffic_gen send --dst IP:PORT [--pps N] [--secs S] [--payload BYTES]
  traffic_gen recv --bind IP:PORT [--secs S]

send options:
  --dst ADDR      destination socket address (required)
  --dports N      spread destination ports over base_port..=base_port+N-1 (default 1)
  --pps N         target packets per second (default 100000)
  --secs S        duration in seconds (default 5)
  --payload BYTES datagram size in bytes, 8..1472 (default 64)

recv options:
  --bind ADDR     local address to bind (required)
  --secs S        how long to keep receiving (default 10)
";

struct SendOpts {
    dst: SocketAddr,
    dports: u16,
    pps: u64,
    secs: f64,
    payload: usize,
}

struct RecvOpts {
    bind: SocketAddr,
    secs: f64,
}

fn die(msg: &str) -> ! {
    eprintln!("traffic_gen: {msg}");
    process::exit(2);
}

fn arg_value<'a>(args: &'a [String], i: &mut usize, name: &str) -> &'a str {
    *i += 1;
    match args.get(*i) {
        Some(v) => v.as_str(),
        None => die(&format!("missing value for {name}")),
    }
}

fn resolve(addr: &str) -> SocketAddr {
    match addr.to_socket_addrs() {
        Ok(mut it) => match it.next() {
            Some(a) => a,
            None => die(&format!("cannot resolve {addr}")),
        },
        Err(e) => die(&format!("cannot resolve {addr}: {e}")),
    }
}

fn parse_send(args: &[String]) -> SendOpts {
    let mut dst: Option<SocketAddr> = None;
    let mut dports: u16 = 1;
    let mut pps: u64 = 100_000;
    let mut secs: f64 = 5.0;
    let mut payload: usize = 64;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dst" => {
                let v = arg_value(args, &mut i, "--dst");
                dst = Some(resolve(v));
            }
            "--dports" => {
                let v = arg_value(args, &mut i, "--dports");
                let n: u32 = v.parse().unwrap_or_else(|e| die(&format!("bad --dports: {e}")));
                if n == 0 || n > 65_536 {
                    die("--dports must be in 1..=65536");
                }
                dports = n as u16;
            }
            "--pps" => {
                let v = arg_value(args, &mut i, "--pps");
                pps = v.parse().unwrap_or_else(|e| die(&format!("bad --pps: {e}")));
            }
            "--secs" => {
                let v = arg_value(args, &mut i, "--secs");
                secs = v.parse().unwrap_or_else(|e| die(&format!("bad --secs: {e}")));
            }
            "--payload" => {
                let v = arg_value(args, &mut i, "--payload");
                payload = v.parse().unwrap_or_else(|e| die(&format!("bad --payload: {e}")));
            }
            other => die(&format!("unknown send option {other}")),
        }
        i += 1;
    }
    let dst = match dst {
        Some(a) => a,
        None => die("send mode requires --dst"),
    };
    if pps == 0 {
        die("--pps must be > 0");
    }
    if !(secs.is_finite() && secs > 0.0) {
        die("--secs must be > 0");
    }
    if payload < 8 || payload > 1472 {
        die("--payload must be in 8..=1472");
    }
    let base_port = dst.port() as u32;
    if base_port + dports as u32 - 1 > 65_535 {
        die(&format!(
            "--dports {dports} overflows the port range from base port {base_port}"
        ));
    }
    SendOpts {
        dst,
        dports,
        pps,
        secs,
        payload,
    }
}

fn parse_recv(args: &[String]) -> RecvOpts {
    let mut bind: Option<SocketAddr> = None;
    let mut secs: f64 = 10.0;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--bind" => {
                let v = arg_value(args, &mut i, "--bind");
                bind = Some(resolve(v));
            }
            "--secs" => {
                let v = arg_value(args, &mut i, "--secs");
                secs = v.parse().unwrap_or_else(|e| die(&format!("bad --secs: {e}")));
            }
            other => die(&format!("unknown recv option {other}")),
        }
        i += 1;
    }
    let bind = match bind {
        Some(a) => a,
        None => die("recv mode requires --bind"),
    };
    if !(secs.is_finite() && secs > 0.0) {
        die("--secs must be > 0");
    }
    RecvOpts { bind, secs }
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        print!("{USAGE}");
        process::exit(2);
    }
    match args[0].as_str() {
        "send" => run_send(parse_send(&args[1..])),
        "recv" => run_recv(parse_recv(&args[1..])),
        "-h" | "--help" | "help" => print!("{USAGE}"),
        other => die(&format!("unknown mode {other} (expected send|recv)")),
    }
}

fn run_send(opts: SendOpts) {
    let socket = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(e) => die(&format!("bind failed: {e}")),
    };
    // Larger payloads benefit from a bigger socket send buffer, but 64B pilot
    // traffic does not need it and std has no portable SO_SNDBUF setter.
    let mut payload = vec![0u8; opts.payload];
    // Payload is irrelevant to the XDP parser, but make each datagram non-constant
    // so accidental dedup/checksum offload shortcuts cannot hide anything.
    for (i, b) in payload.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(31).wrapping_add(7);
    }
    payload[0..4].copy_from_slice(&0x4149_0909u32.to_ne_bytes());

    let total: u64 = (opts.pps as f64 * opts.secs).round() as u64;
    let tick = Duration::from_millis(1);
    let per_tick = opts.pps as f64 / 1_000.0; // packets per 1ms tick
    let base_port = opts.dst.port();
    let mut packet_dst = opts.dst;

    let start = Instant::now();
    let mut sent: u64 = 0;
    let mut carry: f64 = 0.0;
    let mut tick_idx: u64 = 0;
    let mut send_err: u64 = 0;

    while sent < total {
        tick_idx += 1;
        let deadline = start + tick * (tick_idx.min(u32::MAX as u64) as u32);
        carry += per_tick;
        let mut want = carry.floor() as u64;
        carry -= want as f64;
        if want > total - sent {
            want = total - sent;
        }
        if want > 0 {
            let mut n = 0u64;
            while n < want {
                // Vary the destination port when --dports > 1 so the probe sees
                // many distinct aggregation keys (port = base + seq % dports).
                let seq = sent + n;
                packet_dst.set_port(base_port + (seq % opts.dports as u64) as u16);
                match socket.send_to(&payload, packet_dst) {
                    Ok(_) => n += 1,
                    Err(e) => {
                        send_err += 1;
                        if send_err <= 3 {
                            eprintln!("traffic_gen: send error: {e}");
                        }
                        // Keep going: transient ENOBUFS under load should not abort a run.
                        if send_err > 10_000 {
                            die("too many send errors");
                        }
                    }
                }
            }
            sent += want;
        }
        if sent >= total {
            break;
        }
        // Sleep until the tick deadline, spinning the last ~150us for accuracy.
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let wait = deadline - now;
            if wait > Duration::from_micros(200) {
                thread::sleep(wait - Duration::from_micros(150));
            } else {
                std::hint::spin_loop();
            }
        }
    }

    let elapsed = start.elapsed();
    let ms = elapsed.as_secs_f64() * 1000.0;
    let pps = sent as f64 / elapsed.as_secs_f64();
    println!(
        "sent={} elapsed_ms={:.0} pps={:.0} target_pps={} payload={} dports={} base_port={} send_errors={}",
        sent, ms, pps, opts.pps, opts.payload, opts.dports, base_port, send_err
    );
}

fn run_recv(opts: RecvOpts) {
    let socket = match UdpSocket::bind(opts.bind) {
        Ok(s) => s,
        Err(e) => die(&format!("bind {} failed: {e}", opts.bind)),
    };
    if let Err(e) = socket.set_read_timeout(Some(Duration::from_millis(100))) {
        die(&format!("set_read_timeout failed: {e}"));
    }
    let mut buf = [0u8; 2048];
    let start = Instant::now();
    let limit = start + Duration::from_secs_f64(opts.secs);
    let mut received: u64 = 0;
    let mut last_report = start;
    let mut next_report = start + Duration::from_secs(1);

    loop {
        match socket.recv_from(&mut buf) {
            Ok((_n, _src)) => received += 1,
            Err(e) => match e.kind() {
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {}
                _ => die(&format!("recv failed: {e}")),
            },
        }
        let now = Instant::now();
        if now >= next_report {
            let el = now.duration_since(last_report).as_secs_f64();
            let pps = received as f64 / now.duration_since(start).as_secs_f64().max(1e-9);
            println!(
                "progress t={:.1}s received={} avg_pps={:.0}",
                now.duration_since(start).as_secs_f64(),
                received,
                pps
            );
            let _ = el;
            last_report = now;
            next_report += Duration::from_secs(1);
        }
        if now >= limit {
            break;
        }
    }

    let elapsed = start.elapsed();
    println!(
        "received={} elapsed_ms={:.0} pps={:.0}",
        received,
        elapsed.as_secs_f64() * 1000.0,
        received as f64 / elapsed.as_secs_f64().max(1e-9)
    );
}

// Silence dead-code warning for IpAddr import on some rustc versions.
#[allow(dead_code)]
fn _assert_ip(_ip: IpAddr) {}
