//! Linux x86-64 socket ABI byte formats as pure `no_std` logic, independent of
//! host architecture, kernel memory copies, and descriptor lifetimes.
//!
//! Callers copy user buffers in or out and pass plain byte slices here:
//! - [`abi`]: pinned Linux x86-64 layouts (`sockaddr_in`, `sockaddr_in6`,
//!   `sockaddr_un`, `msghdr`, `cmsghdr`, `ucred`, `linger`, `timeval`, packed
//!   `epoll_event`) and socket constants, cross-checked against `linux-raw-sys`.
//! - [`sockaddr`]: decoding and encoding `AF_INET`, `AF_INET6`, and `AF_UNIX`
//!   addresses following Linux's per-call (`Call`) length and family rules.
//! - [`cmsg`]: walking (`CmsgIter`, `count_rights_fds`) and building
//!   (`CmsgWriter`) `msghdr` control buffers with x86-64 8-byte alignment and
//!   `MSG_CTRUNC` reporting.
//! - [`opt`]: decoding `setsockopt` option values (incl. `IntOrByte`, `Timeout`,
//!   and get-only rejection) and encoding truncated `getsockopt` replies
//!   (`write_opt`, `write_opt_for`).

#![no_std]

pub mod abi;
pub mod cmsg;
pub mod opt;
pub mod sockaddr;

pub use abi::{
    AF_INET, AF_INET6, AF_UNIX, AF_UNSPEC, CMSG_ALIGN_SIZE, CMSG_HDRLEN, EPOLL_EVENT_LEN, IPPROTO_IP, IPPROTO_IPV6,
    IPPROTO_TCP, IPPROTO_UDP, LINGER_LEN, MSG_CMSG_CLOEXEC, MSG_CTRUNC, MSG_DONTROUTE, MSG_DONTWAIT, MSG_EOR,
    MSG_ERRQUEUE, MSG_MORE, MSG_NOSIGNAL, MSG_OOB, MSG_PEEK, MSG_TRUNC, MSG_WAITALL, MSGHDR_LEN, SCM_CREDENTIALS,
    SCM_MAX_FD, SCM_RIGHTS, SIN6_LEN_RFC2133, SOCKADDR_IN_LEN, SOCKADDR_IN6_LEN, SOCKADDR_STORAGE_LEN, SOCKADDR_UN_LEN,
    SOL_IP, SOL_IPV6, SOL_SOCKET, SOL_TCP, TIMEVAL_LEN, UCRED_LEN, UNIX_PATH_MAX,
};
pub use cmsg::{Cmsg, CmsgIter, CmsgWriter, RightsIter, Ucred, cmsg_align, cmsg_len, cmsg_space, count_rights_fds};
pub use opt::{
    Linger, MAX_OPT_LEN, MAX_TIMEOUT_SECS, OptKind, OptVal, SockOpt, TIMEOUT_HZ, Timeout, Timeval, check_optlen,
    decode_opt, write_opt, write_opt_for,
};
pub use sockaddr::{
    Call, Family, Ipv4Addr, Ipv6Addr, SockAddr, SocketAddrV4, UnixBytes, UnixName, clamp_msg_namelen, peek_family,
};

use core::fmt;

/// Positive Linux errno values returned by socket ABI decoding and validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum Errno {
    /// `EINVAL` (22): invalid length, malformed header, or invalid family/argument.
    Inval = linux_raw_sys::errno::EINVAL,
    /// `EDOM` (33): timeout `tv_usec` outside `0..1_000_000`.
    Dom = linux_raw_sys::errno::EDOM,
    /// `ENOPROTOOPT` (92): unknown socket option level/name, or get-only option on set.
    NoProtoOpt = linux_raw_sys::errno::ENOPROTOOPT,
    /// `EAFNOSUPPORT` (97): address family does not match the expected INET/INET6 socket.
    AfNoSupport = linux_raw_sys::errno::EAFNOSUPPORT,
}

impl Errno {
    pub const EINVAL: Self = Self::Inval;
    pub const EDOM: Self = Self::Dom;
    pub const ENOPROTOOPT: Self = Self::NoProtoOpt;
    pub const EAFNOSUPPORT: Self = Self::AfNoSupport;

    /// Positive Linux x86-64 errno number.
    pub const fn to_errno(self) -> u32 {
        self as u32
    }
}

impl fmt::Display for Errno {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Inval => "EINVAL",
            Self::Dom => "EDOM",
            Self::NoProtoOpt => "ENOPROTOOPT",
            Self::AfNoSupport => "EAFNOSUPPORT",
        })
    }
}

#[cfg(test)]
mod tests;
