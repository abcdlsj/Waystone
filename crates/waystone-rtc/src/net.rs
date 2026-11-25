//! Destination address discovery for a wildcard UDP socket.
//!
//! A single socket bound to `0.0.0.0` serves every interface, but ICE needs to
//! know which of our own addresses a datagram arrived on: the connectivity check
//! is only accepted when the destination matches one of the local candidates.
//! `recv_from` does not report that, so the destination is read from the
//! ancillary data with `recvmsg`.
//!
//! On platforms without `recvmsg` (Windows) the destination is not reported and
//! the caller derives it from the bound address or the route to the sender.

use std::io;
use std::net::{IpAddr, SocketAddr};
#[cfg(unix)]
use std::os::fd::{AsRawFd, RawFd};

use socket2::SockRef;
use tokio::net::UdpSocket;
use tracing::debug;

/// Preferred UDP kernel buffer sizes, tried from largest to smallest so OS caps
/// (such as macOS/iOS `kern.ipc.maxsockbuf`) degrade gracefully without failing.
const UDP_BUFFER_CANDIDATES: [usize; 4] =
    [4 * 1024 * 1024, 2 * 1024 * 1024, 1024 * 1024, 512 * 1024];

/// Raises `SO_SNDBUF` and `SO_RCVBUF` on a media UDP socket so keyframe bursts
/// do not overflow the 64 KiB Windows default buffer.
pub fn configure_udp_buffers(socket: &UdpSocket) {
    let sock = SockRef::from(socket);
    for size in UDP_BUFFER_CANDIDATES {
        if sock.set_send_buffer_size(size).is_ok() {
            break;
        }
    }
    for size in UDP_BUFFER_CANDIDATES {
        if sock.set_recv_buffer_size(size).is_ok() {
            break;
        }
    }
    debug!(
        "udp socket buffers: send={} KiB, recv={} KiB",
        sock.send_buffer_size().unwrap_or(0) / 1024,
        sock.recv_buffer_size().unwrap_or(0) / 1024
    );
}

/// One datagram plus the local address it was delivered to.
pub struct Datagram {
    pub len: usize,
    pub source: SocketAddr,
    /// Local address the datagram arrived on, when the platform reports it.
    pub destination: Option<IpAddr>,
}

/// Enables destination address reporting; unsupported on this platform.
#[cfg(not(unix))]
pub fn enable_destination_info(_socket: &UdpSocket) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "destination addresses are not reported on this platform",
    ))
}

/// Receives one datagram; the destination is never known on this platform.
#[cfg(not(unix))]
pub async fn recv_with_destination(socket: &UdpSocket, buffer: &mut [u8]) -> io::Result<Datagram> {
    let (len, source) = socket.recv_from(buffer).await?;
    Ok(Datagram {
        len,
        source,
        destination: None,
    })
}

/// Enables destination address reporting on `socket`.
#[cfg(unix)]
///
/// Both families are attempted and failures are tolerated individually: a socket
/// bound to `0.0.0.0` rejects the IPv6 option with `EINVAL`, and vice versa.
pub fn enable_destination_info(socket: &UdpSocket) -> io::Result<()> {
    let fd = socket.as_raw_fd();

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    let options: &[(libc::c_int, libc::c_int)] = &[
        (libc::IPPROTO_IP, libc::IP_RECVDSTADDR),
        (libc::IPPROTO_IPV6, libc::IPV6_RECVPKTINFO),
    ];

    #[cfg(any(target_os = "linux", target_os = "android"))]
    let options: &[(libc::c_int, libc::c_int)] = &[
        (libc::IPPROTO_IP, libc::IP_PKTINFO),
        (libc::IPPROTO_IPV6, libc::IPV6_RECVPKTINFO),
    ];

    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "linux",
        target_os = "android"
    )))]
    let options: &[(libc::c_int, libc::c_int)] = &[];

    let mut enabled = false;
    let mut last_error = None;
    for (level, option) in options {
        let on: libc::c_int = 1;
        let result = unsafe {
            libc::setsockopt(
                fd,
                *level,
                *option,
                &on as *const _ as *const libc::c_void,
                std::mem::size_of_val(&on) as libc::socklen_t,
            )
        };
        if result == 0 {
            enabled = true;
        } else {
            last_error = Some(io::Error::last_os_error());
        }
    }

    if enabled {
        Ok(())
    } else {
        Err(last_error.unwrap_or_else(|| io::Error::other("no destination option available")))
    }
}

/// Receives one datagram together with its destination address.
///
/// `try_io` is used instead of `readable()` plus a raw `recvmsg`: it clears the
/// tokio readiness flag when the raw read returns `WouldBlock`, so a spurious
/// wake-up cannot turn into a busy loop that starves the runtime.
#[cfg(unix)]
pub async fn recv_with_destination(socket: &UdpSocket, buffer: &mut [u8]) -> io::Result<Datagram> {
    let fd = socket.as_raw_fd();
    loop {
        // `readable()` waits for the readiness flag; `try_io` is what clears it
        // again when the raw read returns `WouldBlock`, so the pair is required.
        // Using `try_io` alone spins, because it reports readiness instead of
        // waiting for it.
        socket.readable().await?;
        match socket.try_io(tokio::io::Interest::READABLE, || try_recv(fd, buffer)) {
            Ok(datagram) => return Ok(datagram),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
            Err(error) => return Err(error),
        }
    }
}

#[cfg(unix)]
fn try_recv(fd: RawFd, buffer: &mut [u8]) -> io::Result<Datagram> {
    let mut name: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    // Room for a few control messages; the kernel only ever sends one or two.
    let mut control = [0u8; 128];
    let mut iov = libc::iovec {
        iov_base: buffer.as_mut_ptr() as *mut libc::c_void,
        iov_len: buffer.len(),
    };
    let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
    header.msg_name = &mut name as *mut _ as *mut libc::c_void;
    header.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    header.msg_iov = &mut iov;
    header.msg_iovlen = 1;
    header.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    header.msg_controllen = control.len() as _;

    let len = unsafe { libc::recvmsg(fd, &mut header, 0) };
    if len < 0 {
        return Err(io::Error::last_os_error());
    }

    let source = sockaddr_to_socket_addr(&name)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unparsable source address"))?;
    let destination = destination_from_control(&header);

    Ok(Datagram {
        len: len as usize,
        source,
        destination,
    })
}

/// Extracts the destination address from the ancillary data.
#[cfg(any(target_os = "macos", target_os = "ios"))]
fn destination_from_control(header: &libc::msghdr) -> Option<IpAddr> {
    let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(header) };
    while !cmsg.is_null() {
        let message = unsafe { &*cmsg };
        let data = unsafe { libc::CMSG_DATA(cmsg) } as *const u8;
        match (message.cmsg_level, message.cmsg_type) {
            (libc::IPPROTO_IP, libc::IP_RECVDSTADDR) => {
                let mut octets = [0u8; 4];
                unsafe { std::ptr::copy_nonoverlapping(data, octets.as_mut_ptr(), 4) };
                return Some(IpAddr::from(octets));
            }
            (libc::IPPROTO_IPV6, libc::IPV6_PKTINFO) => {
                let mut octets = [0u8; 16];
                unsafe { std::ptr::copy_nonoverlapping(data, octets.as_mut_ptr(), 16) };
                return Some(IpAddr::from(octets));
            }
            _ => {}
        }
        cmsg = unsafe { libc::CMSG_NXTHDR(header, cmsg) };
    }
    None
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn destination_from_control(header: &libc::msghdr) -> Option<IpAddr> {
    let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(header) };
    while !cmsg.is_null() {
        let message = unsafe { &*cmsg };
        let data = unsafe { libc::CMSG_DATA(cmsg) } as *const u8;
        match (message.cmsg_level, message.cmsg_type) {
            (libc::IPPROTO_IP, libc::IP_PKTINFO) => {
                // struct in_pktinfo { int ifindex; struct in_addr spec_dst; struct in_addr addr; }
                let mut octets = [0u8; 4];
                unsafe { std::ptr::copy_nonoverlapping(data.add(8), octets.as_mut_ptr(), 4) };
                return Some(IpAddr::from(octets));
            }
            (libc::IPPROTO_IPV6, libc::IPV6_PKTINFO) => {
                let mut octets = [0u8; 16];
                unsafe { std::ptr::copy_nonoverlapping(data, octets.as_mut_ptr(), 16) };
                return Some(IpAddr::from(octets));
            }
            _ => {}
        }
        cmsg = unsafe { libc::CMSG_NXTHDR(header, cmsg) };
    }
    None
}

#[cfg(all(
    unix,
    not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "linux",
        target_os = "android"
    ))
))]
fn destination_from_control(_header: &libc::msghdr) -> Option<IpAddr> {
    None
}

#[cfg(unix)]
fn sockaddr_to_socket_addr(storage: &libc::sockaddr_storage) -> Option<SocketAddr> {
    match storage.ss_family as libc::c_int {
        libc::AF_INET => {
            let address = unsafe { &*(storage as *const _ as *const libc::sockaddr_in) };
            let octets = address.sin_addr.s_addr.to_ne_bytes();
            Some(SocketAddr::from((
                std::net::Ipv4Addr::from(octets),
                u16::from_be(address.sin_port),
            )))
        }
        libc::AF_INET6 => {
            let address = unsafe { &*(storage as *const _ as *const libc::sockaddr_in6) };
            Some(SocketAddr::from((
                std::net::Ipv6Addr::from(address.sin6_addr.s6_addr),
                u16::from_be(address.sin6_port),
            )))
        }
        _ => None,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reports_the_loopback_destination() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        enable_destination_info(&server).unwrap();
        let address = server.local_addr().unwrap();

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.send_to(b"ping", address).await.unwrap();

        let mut buffer = [0u8; 32];
        let datagram = recv_with_destination(&server, &mut buffer).await.unwrap();
        assert_eq!(&buffer[..datagram.len], b"ping");
        assert_eq!(datagram.source.ip(), IpAddr::from([127, 0, 0, 1]));
        assert_eq!(datagram.destination, Some(IpAddr::from([127, 0, 0, 1])));
    }

    #[tokio::test]
    async fn wildcard_socket_reports_the_target_address() {
        let server = UdpSocket::bind("0.0.0.0:0").await.unwrap();
        enable_destination_info(&server).unwrap();
        let port = server.local_addr().unwrap().port();

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client
            .send_to(b"ping", SocketAddr::from(([127, 0, 0, 1], port)))
            .await
            .unwrap();

        let mut buffer = [0u8; 32];
        let datagram = recv_with_destination(&server, &mut buffer).await.unwrap();
        assert_eq!(datagram.destination, Some(IpAddr::from([127, 0, 0, 1])));
    }
}
