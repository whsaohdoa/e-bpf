//! AF_PACKET frame sender.
//!
//! The sender is *armed* when [`FrameSender::open`] binds the raw socket and
//! disarmed when it is closed (explicitly or on drop), so the caller knows the
//! exact number of frames the kernel accepted: [`FrameSender::send`] only
//! returns once `sendto(2)` reported a full write.

use std::{
    ffi::{CString, c_char, c_int},
    io,
    os::fd::RawFd,
};

/// `AF_PACKET` from `linux/socket.h`.
const AF_PACKET: c_int = 17;
/// `SOCK_RAW`.
const SOCK_RAW: c_int = 3;
/// `SOCK_CLOEXEC`: the fd must not leak into the probe child.
const SOCK_CLOEXEC: c_int = 0o2_000_000;
/// `ETH_P_ALL` — receive/transmit every ethertype.
const ETH_P_ALL: u16 = 0x0003;

/// `struct sockaddr_ll` from `linux/if_packet.h` (20 bytes on Linux).
#[repr(C)]
#[derive(Clone, Copy)]
struct SockAddrLl {
    sll_family: u16,
    sll_protocol: u16,
    sll_ifindex: i32,
    sll_hatype: u16,
    sll_pkttype: u8,
    sll_halen: u8,
    sll_addr: [u8; 8],
}

unsafe extern "C" {
    fn socket(domain: c_int, ty: c_int, protocol: c_int) -> c_int;
    fn bind(fd: c_int, addr: *const SockAddrLl, addrlen: u32) -> c_int;
    fn sendto(
        fd: c_int,
        buf: *const u8,
        len: usize,
        flags: c_int,
        addr: *const SockAddrLl,
        addrlen: u32,
    ) -> isize;
    fn if_nametoindex(ifname: *const c_char) -> u32;
    fn close(fd: c_int) -> c_int;
}

/// Resolves an interface name to its kernel index.
pub fn ifindex(iface: &str) -> io::Result<i32> {
    let name = CString::new(iface)?;
    // SAFETY: `name` is a valid NUL terminated C string for the call.
    let index = unsafe { if_nametoindex(name.as_ptr()) };
    if index == 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("interface `{iface}` does not exist"),
        ));
    }
    Ok(index as i32)
}

/// An armed AF_PACKET socket bound to one interface.
pub struct FrameSender {
    fd: RawFd,
    ifindex: i32,
    iface: String,
}

impl FrameSender {
    /// Arms the sender: opens and binds a raw `ETH_P_ALL` socket on `iface`.
    pub fn open(iface: &str) -> io::Result<Self> {
        let ifindex = ifindex(iface)?;
        // SAFETY: plain libc call, no pointers involved yet.
        let fd = unsafe { socket(AF_PACKET, SOCK_RAW | SOCK_CLOEXEC, ETH_P_ALL as c_int) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let sender = Self {
            fd,
            ifindex,
            iface: iface.to_owned(),
        };
        let addr = SockAddrLl {
            sll_family: AF_PACKET as u16,
            sll_protocol: ETH_P_ALL.to_be(),
            sll_ifindex: ifindex,
            sll_hatype: 0,
            sll_pkttype: 0,
            sll_halen: 0,
            sll_addr: [0; 8],
        };
        // SAFETY: `addr` is a fully initialised `sockaddr_ll` and `sender.fd`
        // is a live AF_PACKET socket; the size is that of the struct.
        let rc = unsafe { bind(sender.fd, &addr, std::mem::size_of::<SockAddrLl>() as u32) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(sender)
    }

    pub fn iface(&self) -> &str {
        &self.iface
    }

    /// Transmits one pre-built Ethernet frame.
    pub fn send(&self, frame: &[u8]) -> io::Result<()> {
        if frame.len() < 14 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("frame too short for Ethernet: {} bytes", frame.len()),
            ));
        }
        let mut addr = SockAddrLl {
            sll_family: AF_PACKET as u16,
            sll_protocol: u16::from_be_bytes([frame[12], frame[13]]),
            sll_ifindex: self.ifindex,
            sll_hatype: 1, // ARPHRD_ETHER
            sll_pkttype: 0,
            sll_halen: 6,
            sll_addr: [0; 8],
        };
        addr.sll_addr[..6].copy_from_slice(&frame[..6]);
        // SAFETY: `frame` is a live slice and `addr` a fully initialised
        // `sockaddr_ll`; both outlive the call.
        let written = unsafe {
            sendto(
                self.fd,
                frame.as_ptr(),
                frame.len(),
                0,
                &addr,
                std::mem::size_of::<SockAddrLl>() as u32,
            )
        };
        if written < 0 {
            return Err(io::Error::last_os_error());
        }
        if written as usize != frame.len() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                format!("short write: {} of {} bytes", written, frame.len()),
            ));
        }
        Ok(())
    }

    /// Sends `frame` `count` times and returns the exact number of frames the
    /// kernel accepted.
    pub fn send_repeat(&self, frame: &[u8], count: usize) -> io::Result<usize> {
        for _ in 0..count {
            self.send(frame)?;
        }
        Ok(count)
    }

    /// Sends every frame in `frames` once per pass, `passes` times, and returns
    /// the exact number of frames the kernel accepted (`passes * frames.len()`).
    pub fn send_cycle(&self, frames: &[Vec<u8>], passes: usize) -> io::Result<usize> {
        if frames.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "no frames to send",
            ));
        }
        for _ in 0..passes {
            for frame in frames {
                self.send(frame)?;
            }
        }
        Ok(passes * frames.len())
    }

    /// Disarms the sender explicitly (drops the fd now instead of on scope exit).
    pub fn close(mut self) {
        self.release();
    }

    fn release(&mut self) {
        if self.fd >= 0 {
            // SAFETY: `self.fd` is an open descriptor owned by this struct and
            // is not used again after being reset below.
            unsafe { close(self.fd) };
            self.fd = -1;
        }
    }
}

impl Drop for FrameSender {
    fn drop(&mut self) {
        self.release();
    }
}
