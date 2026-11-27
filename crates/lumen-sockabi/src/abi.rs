//! Fixed Linux x86-64 socket and epoll wire layouts, independent of host
//! architecture.
//!
//! Sources: `include/uapi/linux/in.h`, `include/uapi/linux/in6.h`,
//! `include/uapi/linux/un.h`, `include/uapi/linux/socket.h`,
//! `include/linux/socket.h`, `include/uapi/linux/tcp.h`,
//! `include/uapi/linux/eventpoll.h`, and `include/net/scm.h`.
//!
//! Compile-time assertions pin every size and field offset against x86-64 UAPI
//! literals and cross-check against `linux-raw-sys` where it provides the
//! matching target layout. Serialization never reads Rust padding bytes.

use core::mem::{offset_of, size_of};

/// Address families (`include/linux/socket.h`).
pub const AF_UNSPEC: u16 = 0;
pub const AF_UNIX: u16 = 1;
pub const AF_INET: u16 = 2;
pub const AF_INET6: u16 = 10;

/// Socket option levels (`include/uapi/asm-generic/socket.h`, `include/uapi/linux/in.h`).
pub const SOL_IP: i32 = 0;
pub const SOL_SOCKET: i32 = 1;
pub const SOL_TCP: i32 = 6;
pub const SOL_IPV6: i32 = 41;

/// Protocol numbers (`include/uapi/linux/in.h`).
pub const IPPROTO_IP: i32 = 0;
pub const IPPROTO_TCP: i32 = 6;
pub const IPPROTO_UDP: i32 = 17;
pub const IPPROTO_IPV6: i32 = 41;

/// `SOL_SOCKET` option names (`include/uapi/asm-generic/socket.h`).
pub const SO_DEBUG: i32 = 1;
pub const SO_REUSEADDR: i32 = 2;
pub const SO_TYPE: i32 = 3;
pub const SO_ERROR: i32 = 4;
pub const SO_DONTROUTE: i32 = 5;
pub const SO_BROADCAST: i32 = 6;
pub const SO_SNDBUF: i32 = 7;
pub const SO_RCVBUF: i32 = 8;
pub const SO_KEEPALIVE: i32 = 9;
pub const SO_OOBINLINE: i32 = 10;
pub const SO_LINGER: i32 = 13;
pub const SO_REUSEPORT: i32 = 15;
pub const SO_PASSCRED: i32 = 16;
pub const SO_PEERCRED: i32 = 17;
pub const SO_RCVLOWAT: i32 = 18;
pub const SO_SNDLOWAT: i32 = 19;
pub const SO_RCVTIMEO_OLD: i32 = 20;
pub const SO_SNDTIMEO_OLD: i32 = 21;
pub const SO_BINDTODEVICE: i32 = 25;
pub const SO_ACCEPTCONN: i32 = 30;
pub const SO_SNDBUFFORCE: i32 = 32;
pub const SO_RCVBUFFORCE: i32 = 33;
pub const SO_PROTOCOL: i32 = 38;
pub const SO_DOMAIN: i32 = 39;
pub const SO_RCVTIMEO_NEW: i32 = 66;
pub const SO_SNDTIMEO_NEW: i32 = 67;

/// `IPPROTO_TCP` option names (`include/uapi/linux/tcp.h`).
pub const TCP_NODELAY: i32 = 1;
pub const TCP_MAXSEG: i32 = 2;
pub const TCP_CORK: i32 = 3;
pub const TCP_KEEPIDLE: i32 = 4;
pub const TCP_KEEPINTVL: i32 = 5;
pub const TCP_KEEPCNT: i32 = 6;
pub const TCP_QUICKACK: i32 = 12;
pub const TCP_CONGESTION: i32 = 13;
pub const TCP_USER_TIMEOUT: i32 = 18;
pub const TCP_ULP: i32 = 31;
pub const TCP_FASTOPEN_KEY: i32 = 33;

/// `IPPROTO_IP` option names (`include/uapi/linux/in.h`).
pub const IP_TOS: i32 = 1;
pub const IP_TTL: i32 = 2;
pub const IP_RECVERR: i32 = 11;

/// `IPPROTO_IPV6` option names (`include/uapi/linux/in6.h`).
pub const IPV6_UNICAST_HOPS: i32 = 16;
pub const IPV6_RECVERR: i32 = 25;
pub const IPV6_V6ONLY: i32 = 26;

/// `SOL_SOCKET` control-message types (`include/linux/socket.h`).
pub const SCM_RIGHTS: i32 = 1;
pub const SCM_CREDENTIALS: i32 = 2;

/// Maximum file descriptors in one `SCM_RIGHTS` control message (`include/net/scm.h`).
pub const SCM_MAX_FD: usize = 253;

/// `sendmsg`/`recvmsg` flags (`include/linux/socket.h`).
pub const MSG_OOB: u32 = 0x0001;
pub const MSG_PEEK: u32 = 0x0002;
pub const MSG_DONTROUTE: u32 = 0x0004;
pub const MSG_CTRUNC: u32 = 0x0008;
pub const MSG_TRUNC: u32 = 0x0020;
pub const MSG_DONTWAIT: u32 = 0x0040;
pub const MSG_EOR: u32 = 0x0080;
pub const MSG_WAITALL: u32 = 0x0100;
pub const MSG_ERRQUEUE: u32 = 0x2000;
pub const MSG_NOSIGNAL: u32 = 0x4000;
pub const MSG_MORE: u32 = 0x8000;
pub const MSG_CMSG_CLOEXEC: u32 = 0x4000_0000;

/// Pinned sizes on Linux x86-64.
pub const SOCKADDR_IN_LEN: usize = size_of::<SockaddrIn>();
pub const SOCKADDR_IN6_LEN: usize = size_of::<SockaddrIn6>();
/// Minimum valid `sockaddr_in6` length without `sin6_scope_id` (`include/uapi/linux/in6.h`).
pub const SIN6_LEN_RFC2133: usize = 24;
pub const UNIX_PATH_MAX: usize = 108;
pub const SOCKADDR_UN_LEN: usize = size_of::<SockaddrUn>();
pub const SOCKADDR_STORAGE_LEN: usize = 128;
pub const MSGHDR_LEN: usize = size_of::<Msghdr>();
pub const CMSG_HDRLEN: usize = size_of::<Cmsghdr>();
/// Control-message alignment on Linux x86-64 (`sizeof(long)`).
pub const CMSG_ALIGN_SIZE: usize = 8;
pub const UCRED_LEN: usize = size_of::<RawUcred>();
pub const LINGER_LEN: usize = size_of::<RawLinger>();
pub const TIMEVAL_LEN: usize = size_of::<RawTimeval>();
pub const EPOLL_EVENT_LEN: usize = size_of::<EpollEvent>();

/// Linux x86-64 `struct sockaddr_in` (16 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SockaddrIn {
    pub sin_family: u16,
    pub sin_port: u16,
    pub sin_addr: [u8; 4],
    pub pad: [u8; 8],
}

/// Linux x86-64 `struct sockaddr_in6` (28 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SockaddrIn6 {
    pub sin6_family: u16,
    pub sin6_port: u16,
    pub sin6_flowinfo: u32,
    pub sin6_addr: [u8; 16],
    pub sin6_scope_id: u32,
}

/// Linux x86-64 `struct sockaddr_un` (110 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SockaddrUn {
    pub sun_family: u16,
    pub sun_path: [u8; UNIX_PATH_MAX],
}

/// Linux x86-64 `struct msghdr` (56 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Msghdr {
    pub msg_name: u64,
    pub msg_namelen: i32,
    pub msg_iov: u64,
    pub msg_iovlen: u64,
    pub msg_control: u64,
    pub msg_controllen: u64,
    pub msg_flags: u32,
}

impl Msghdr {
    /// Decode a 56-byte x86-64 `struct msghdr` image, ignoring alignment padding.
    pub const fn from_bytes(bytes: &[u8; MSGHDR_LEN]) -> Self {
        Self {
            msg_name: u64::from_le_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ]),
            msg_namelen: i32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            msg_iov: u64::from_le_bytes([
                bytes[16], bytes[17], bytes[18], bytes[19], bytes[20], bytes[21], bytes[22], bytes[23],
            ]),
            msg_iovlen: u64::from_le_bytes([
                bytes[24], bytes[25], bytes[26], bytes[27], bytes[28], bytes[29], bytes[30], bytes[31],
            ]),
            msg_control: u64::from_le_bytes([
                bytes[32], bytes[33], bytes[34], bytes[35], bytes[36], bytes[37], bytes[38], bytes[39],
            ]),
            msg_controllen: u64::from_le_bytes([
                bytes[40], bytes[41], bytes[42], bytes[43], bytes[44], bytes[45], bytes[46], bytes[47],
            ]),
            msg_flags: u32::from_le_bytes([bytes[48], bytes[49], bytes[50], bytes[51]]),
        }
    }

    /// Encode into a 56-byte x86-64 `struct msghdr` image with all padding zeroed.
    pub fn to_bytes(&self) -> [u8; MSGHDR_LEN] {
        let mut out = [0u8; MSGHDR_LEN];
        out[0..8].copy_from_slice(&self.msg_name.to_le_bytes());
        out[8..12].copy_from_slice(&self.msg_namelen.to_le_bytes());
        out[16..24].copy_from_slice(&self.msg_iov.to_le_bytes());
        out[24..32].copy_from_slice(&self.msg_iovlen.to_le_bytes());
        out[32..40].copy_from_slice(&self.msg_control.to_le_bytes());
        out[40..48].copy_from_slice(&self.msg_controllen.to_le_bytes());
        out[48..52].copy_from_slice(&self.msg_flags.to_le_bytes());
        out
    }
}

/// Linux x86-64 `struct cmsghdr` (16 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cmsghdr {
    pub cmsg_len: u64,
    pub cmsg_level: i32,
    pub cmsg_type: i32,
}

/// Linux x86-64 `struct ucred` (12 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawUcred {
    pub pid: i32,
    pub uid: u32,
    pub gid: u32,
}

/// Linux x86-64 `struct linger` (8 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawLinger {
    pub l_onoff: i32,
    pub l_linger: i32,
}

/// Linux x86-64 `struct __kernel_sock_timeval` / `struct timeval` (16 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawTimeval {
    pub tv_sec: i64,
    pub tv_usec: i64,
}

/// Linux x86-64 `struct epoll_event` (packed 12 bytes; unpacked 16 on arm64).
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EpollEvent {
    pub events: u32,
    pub data: u64,
}

impl EpollEvent {
    /// Decode a 12-byte packed x86-64 `struct epoll_event`.
    pub const fn from_bytes(bytes: &[u8; EPOLL_EVENT_LEN]) -> Self {
        Self {
            events: u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            data: u64::from_le_bytes([
                bytes[4], bytes[5], bytes[6], bytes[7], bytes[8], bytes[9], bytes[10], bytes[11],
            ]),
        }
    }

    /// Encode into a 12-byte packed x86-64 `struct epoll_event`.
    pub fn to_bytes(&self) -> [u8; EPOLL_EVENT_LEN] {
        let events = self.events;
        let data = self.data;
        let mut out = [0u8; EPOLL_EVENT_LEN];
        out[0..4].copy_from_slice(&events.to_le_bytes());
        out[4..12].copy_from_slice(&data.to_le_bytes());
        out
    }
}

macro_rules! offsets {
    ($ty:ty, $size:expr; $($field:ident: $offset:expr),+ $(,)?) => {
        const _: () = {
            assert!(size_of::<$ty>() == $size);
            $(assert!(offset_of!($ty, $field) == $offset);)+
        };
    };
}

offsets!(SockaddrIn, 16; sin_family: 0, sin_port: 2, sin_addr: 4, pad: 8);
offsets!(SockaddrIn6, 28; sin6_family: 0, sin6_port: 2, sin6_flowinfo: 4, sin6_addr: 8, sin6_scope_id: 24);
offsets!(SockaddrUn, 110; sun_family: 0, sun_path: 2);
offsets!(Msghdr, 56;
    msg_name: 0, msg_namelen: 8, msg_iov: 16, msg_iovlen: 24,
    msg_control: 32, msg_controllen: 40, msg_flags: 48,
);
offsets!(Cmsghdr, 16; cmsg_len: 0, cmsg_level: 8, cmsg_type: 12);
offsets!(RawUcred, 12; pid: 0, uid: 4, gid: 8);
offsets!(RawLinger, 8; l_onoff: 0, l_linger: 4);
offsets!(RawTimeval, 16; tv_sec: 0, tv_usec: 8);
offsets!(EpollEvent, 12; events: 0, data: 4);

// Cross-check every 64-bit Linux UAPI struct and constant against `linux-raw-sys`.
#[cfg(target_pointer_width = "64")]
const _: () = {
    use linux_raw_sys::{general, net};

    assert!(size_of::<SockaddrIn>() == size_of::<net::sockaddr_in>());
    assert!(offset_of!(SockaddrIn, sin_family) == offset_of!(net::sockaddr_in, sin_family));
    assert!(offset_of!(SockaddrIn, sin_port) == offset_of!(net::sockaddr_in, sin_port));
    assert!(offset_of!(SockaddrIn, sin_addr) == offset_of!(net::sockaddr_in, sin_addr));
    assert!(offset_of!(SockaddrIn, pad) == offset_of!(net::sockaddr_in, __pad));

    assert!(size_of::<SockaddrIn6>() == size_of::<net::sockaddr_in6>());
    assert!(offset_of!(SockaddrIn6, sin6_family) == offset_of!(net::sockaddr_in6, sin6_family));
    assert!(offset_of!(SockaddrIn6, sin6_port) == offset_of!(net::sockaddr_in6, sin6_port));
    assert!(offset_of!(SockaddrIn6, sin6_flowinfo) == offset_of!(net::sockaddr_in6, sin6_flowinfo));
    assert!(offset_of!(SockaddrIn6, sin6_addr) == offset_of!(net::sockaddr_in6, sin6_addr));
    assert!(offset_of!(SockaddrIn6, sin6_scope_id) == offset_of!(net::sockaddr_in6, sin6_scope_id));

    assert!(size_of::<SockaddrUn>() == size_of::<net::sockaddr_un>());
    assert!(offset_of!(SockaddrUn, sun_family) == offset_of!(net::sockaddr_un, sun_family));
    assert!(offset_of!(SockaddrUn, sun_path) == offset_of!(net::sockaddr_un, sun_path));

    assert!(SOCKADDR_STORAGE_LEN == size_of::<net::__kernel_sockaddr_storage>());

    assert!(size_of::<Msghdr>() == size_of::<net::msghdr>());
    assert!(offset_of!(Msghdr, msg_name) == offset_of!(net::msghdr, msg_name));
    assert!(offset_of!(Msghdr, msg_namelen) == offset_of!(net::msghdr, msg_namelen));
    assert!(offset_of!(Msghdr, msg_iov) == offset_of!(net::msghdr, msg_iov));
    assert!(offset_of!(Msghdr, msg_iovlen) == offset_of!(net::msghdr, msg_iovlen));
    assert!(offset_of!(Msghdr, msg_control) == offset_of!(net::msghdr, msg_control));
    assert!(offset_of!(Msghdr, msg_controllen) == offset_of!(net::msghdr, msg_controllen));
    assert!(offset_of!(Msghdr, msg_flags) == offset_of!(net::msghdr, msg_flags));

    assert!(size_of::<Cmsghdr>() == size_of::<net::cmsghdr>());
    assert!(offset_of!(Cmsghdr, cmsg_len) == offset_of!(net::cmsghdr, cmsg_len));
    assert!(offset_of!(Cmsghdr, cmsg_level) == offset_of!(net::cmsghdr, cmsg_level));
    assert!(offset_of!(Cmsghdr, cmsg_type) == offset_of!(net::cmsghdr, cmsg_type));

    assert!(size_of::<RawUcred>() == size_of::<net::ucred>());
    assert!(offset_of!(RawUcred, pid) == offset_of!(net::ucred, pid));
    assert!(offset_of!(RawUcred, uid) == offset_of!(net::ucred, uid));
    assert!(offset_of!(RawUcred, gid) == offset_of!(net::ucred, gid));

    assert!(size_of::<RawLinger>() == size_of::<net::linger>());
    assert!(offset_of!(RawLinger, l_onoff) == offset_of!(net::linger, l_onoff));
    assert!(offset_of!(RawLinger, l_linger) == offset_of!(net::linger, l_linger));

    assert!(size_of::<RawTimeval>() == size_of::<general::timeval>());
    assert!(size_of::<RawTimeval>() == size_of::<general::__kernel_old_timeval>());
    assert!(offset_of!(RawTimeval, tv_sec) == offset_of!(general::timeval, tv_sec));
    assert!(offset_of!(RawTimeval, tv_usec) == offset_of!(general::timeval, tv_usec));

    // Constants
    assert!(AF_UNSPEC as u32 == net::AF_UNSPEC);
    assert!(AF_UNIX as u32 == net::AF_UNIX);
    assert!(AF_INET as u32 == net::AF_INET);
    assert!(AF_INET6 as u32 == net::AF_INET6);

    assert!(SOL_IP as u32 == net::SOL_IP);
    assert!(SOL_SOCKET as u32 == net::SOL_SOCKET);
    assert!(SOL_TCP as u32 == net::SOL_TCP);
    assert!(SOL_IPV6 as u32 == net::SOL_IPV6);

    assert!(IPPROTO_IP as u32 == net::IPPROTO_IP as u32);
    assert!(IPPROTO_TCP as u32 == net::IPPROTO_TCP as u32);
    assert!(IPPROTO_UDP as u32 == net::IPPROTO_UDP as u32);
    assert!(IPPROTO_IPV6 as u32 == net::IPPROTO_IPV6 as u32);

    assert!(SO_DEBUG as u32 == net::SO_DEBUG);
    assert!(SO_REUSEADDR as u32 == net::SO_REUSEADDR);
    assert!(SO_TYPE as u32 == net::SO_TYPE);
    assert!(SO_ERROR as u32 == net::SO_ERROR);
    assert!(SO_DONTROUTE as u32 == net::SO_DONTROUTE);
    assert!(SO_BROADCAST as u32 == net::SO_BROADCAST);
    assert!(SO_SNDBUF as u32 == net::SO_SNDBUF);
    assert!(SO_RCVBUF as u32 == net::SO_RCVBUF);
    assert!(SO_KEEPALIVE as u32 == net::SO_KEEPALIVE);
    assert!(SO_OOBINLINE as u32 == net::SO_OOBINLINE);
    assert!(SO_LINGER as u32 == net::SO_LINGER);
    assert!(SO_REUSEPORT as u32 == net::SO_REUSEPORT);
    assert!(SO_PASSCRED as u32 == net::SO_PASSCRED);
    assert!(SO_PEERCRED as u32 == net::SO_PEERCRED);
    assert!(SO_RCVLOWAT as u32 == net::SO_RCVLOWAT);
    assert!(SO_SNDLOWAT as u32 == net::SO_SNDLOWAT);
    assert!(SO_RCVTIMEO_OLD as u32 == net::SO_RCVTIMEO_OLD);
    assert!(SO_SNDTIMEO_OLD as u32 == net::SO_SNDTIMEO_OLD);
    assert!(SO_RCVTIMEO_NEW as u32 == net::SO_RCVTIMEO_NEW);
    assert!(SO_SNDTIMEO_NEW as u32 == net::SO_SNDTIMEO_NEW);
    assert!(SO_BINDTODEVICE as u32 == net::SO_BINDTODEVICE);
    assert!(SO_ACCEPTCONN as u32 == net::SO_ACCEPTCONN);
    assert!(SO_SNDBUFFORCE as u32 == net::SO_SNDBUFFORCE);
    assert!(SO_RCVBUFFORCE as u32 == net::SO_RCVBUFFORCE);
    assert!(SO_PROTOCOL as u32 == net::SO_PROTOCOL);
    assert!(SO_DOMAIN as u32 == net::SO_DOMAIN);

    assert!(TCP_NODELAY as u32 == net::TCP_NODELAY);
    assert!(TCP_MAXSEG as u32 == net::TCP_MAXSEG);
    assert!(TCP_CORK as u32 == net::TCP_CORK);
    assert!(TCP_KEEPIDLE as u32 == net::TCP_KEEPIDLE);
    assert!(TCP_KEEPINTVL as u32 == net::TCP_KEEPINTVL);
    assert!(TCP_KEEPCNT as u32 == net::TCP_KEEPCNT);
    assert!(TCP_QUICKACK as u32 == net::TCP_QUICKACK);
    assert!(TCP_CONGESTION as u32 == net::TCP_CONGESTION);
    assert!(TCP_USER_TIMEOUT as u32 == net::TCP_USER_TIMEOUT);
    assert!(TCP_ULP as u32 == net::TCP_ULP);
    assert!(TCP_FASTOPEN_KEY as u32 == net::TCP_FASTOPEN_KEY);

    assert!(IP_TOS as u32 == net::IP_TOS);
    assert!(IP_TTL as u32 == net::IP_TTL);
    assert!(IP_RECVERR as u32 == net::IP_RECVERR);

    assert!(IPV6_UNICAST_HOPS as u32 == net::IPV6_UNICAST_HOPS);
    assert!(IPV6_RECVERR as u32 == net::IPV6_RECVERR);
    assert!(IPV6_V6ONLY as u32 == net::IPV6_V6ONLY);

    assert!(SCM_RIGHTS as u32 == net::SCM_RIGHTS);
    assert!(SCM_CREDENTIALS as u32 == net::SCM_CREDENTIALS);

    assert!(MSG_OOB == net::MSG_OOB);
    assert!(MSG_PEEK == net::MSG_PEEK);
    assert!(MSG_DONTROUTE == net::MSG_DONTROUTE);
    assert!(MSG_CTRUNC == net::MSG_CTRUNC);
    assert!(MSG_TRUNC == net::MSG_TRUNC);
    assert!(MSG_DONTWAIT == net::MSG_DONTWAIT);
    assert!(MSG_EOR == net::MSG_EOR);
    assert!(MSG_WAITALL == net::MSG_WAITALL);
    assert!(MSG_ERRQUEUE == net::MSG_ERRQUEUE);
    assert!(MSG_NOSIGNAL == net::MSG_NOSIGNAL);
    assert!(MSG_MORE == net::MSG_MORE);
    assert!(MSG_CMSG_CLOEXEC == net::MSG_CMSG_CLOEXEC);
};

// `epoll_event` is packed (12 bytes) specifically on x86-64; on arm64
// `linux-raw-sys` exposes the 16-byte unpacked arm64 layout.
#[cfg(target_arch = "x86_64")]
const _: () = {
    use linux_raw_sys::general;
    assert!(size_of::<EpollEvent>() == size_of::<general::epoll_event>());
    assert!(offset_of!(EpollEvent, events) == offset_of!(general::epoll_event, events));
    assert!(offset_of!(EpollEvent, data) == offset_of!(general::epoll_event, data));
};
