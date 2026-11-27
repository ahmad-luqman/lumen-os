//! Socket address decoding and encoding for `AF_INET`, `AF_INET6`, and `AF_UNIX`.
//!
//! Validation follows Linux v6.12 per-call rules via [`Call`]:
//! - **`msg_namelen` upper bound and "no address" ([`SockAddr::decode_dest`]):**
//!   - [`Call::Bind`], [`Call::Connect`], [`Call::SendTo`] mirror `net/socket.c`
//!     `move_addr_to_kernel`: `bytes.len() > 128` (`sizeof(struct sockaddr_storage)`)
//!     returns `EINVAL`.
//!   - [`Call::SendMsg`] mirrors `net/socket.c` `__copy_msghdr`: negative raw
//!     `msg_namelen` returns `EINVAL` ([`clamp_msg_namelen`]); lengths above 128
//!     are clamped to 128 before family decoding; `msg_namelen == 0` (or a NULL
//!     `msg_name`) means no address (`Ok(None)`) in every family.
//!   - [`Call::SendTo`] and [`Call::SendMsg`] express **datagram** destination
//!     address rules (`udp_sendmsg`, `udpv6_sendmsg`, `unix_dgram_sendmsg`).
//!     Stream sockets ignore `msg_name` and never call `decode`/`decode_dest`
//!     for send calls.
//! - **`AF_INET`:**
//!   - `net/ipv4/af_inet.c` `inet_bind_sk`, `net/ipv4/tcp_ipv4.c` `tcp_v4_connect`,
//!     and `net/ipv4/udp.c` `udp_sendmsg`: `len < 16` is `EINVAL` (checked before
//!     the family; `SendTo` with `len == 0` stays `EINVAL`).
//!   - [`Call::Bind`] accepts `AF_INET`, or `AF_UNSPEC` only when `sin_addr` is
//!     `0.0.0.0` (`INADDR_ANY`), decoding as `SockAddr::Inet`.
//!   - [`Call::SendTo`] and [`Call::SendMsg`] accept `AF_INET` or `AF_UNSPEC`
//!     (any IPv4 address, matching `udp_sendmsg`), decoding as `SockAddr::Inet`.
//!   - [`Call::Connect`] requires `AF_INET` (`EAFNOSUPPORT` otherwise; callers
//!     implementing datagram and stream `connect(AF_UNSPEC)` disconnect check
//!     [`peek_family`] first, since Linux only requires `len >= 2` for disconnect).
//! - **`AF_INET6`:**
//!   - `net/ipv6/af_inet6.c` `inet6_bind_sk` and `net/ipv6/tcp_ipv6.c`
//!     `tcp_v6_connect`: `len < SIN6_LEN_RFC2133` (24) is `EINVAL`; wrong
//!     family is `EAFNOSUPPORT`; `sin6_scope_id` is read when `len >= 28` and
//!     defaults to `0` for `24..28`.
//!   - `net/ipv6/udp.c` `udpv6_sendmsg`: `SendTo` with `len == 0` is `EINVAL`;
//!     `AF_UNSPEC` with `len >= 2` means no address (`Ok(None)` from
//!     [`SockAddr::decode_dest`]).
//!   - **Caller duties for IPv6:** `udpv6_sendmsg` returns `EINVAL` (not
//!     `EAFNOSUPPORT`) for an unknown family and routes `AF_INET` to IPv4 on
//!     dual-stack sockets; `ip6_datagram_connect` accepts `AF_INET`/v4-mapped
//!     addresses, and datagram and stream `connect(AF_UNSPEC)` disconnects.
//!     Callers use [`peek_family`] to handle those branches before calling
//!     [`SockAddr::decode`] / [`SockAddr::decode_dest`].
//! - **`AF_UNIX`:**
//!   - `net/unix/af_unix.c` `unix_bind` accepts `addr_len == 2` (`sizeof(short)`)
//!     with `sun_family == AF_UNIX` as [`UnixName::Unnamed`] (autobind); wrong
//!     family is `EINVAL`.
//!   - `net/unix/af_unix.c` `unix_validate_addr` rejects `addr_len <= 2` or
//!     `addr_len > 110` (`sizeof(struct sockaddr_un)`) and any
//!     `sun_family != AF_UNIX` with `EINVAL`; accordingly, [`Call::Connect`],
//!     [`Call::SendTo`], and [`Call::SendMsg`] reject `len == 2` with `EINVAL`
//!     (callers handling datagram and stream `connect(AF_UNSPEC)` disconnect
//!     check [`peek_family`] first).
//!   - `net/unix/af_unix.c` `unix_dgram_sendmsg` checks only `if (msg->msg_namelen)`:
//!     `SendTo` with `len == 0` means no address (`Ok(None)` from
//!     [`SockAddr::decode_dest`]).
//!   - `unix_mkname_bsd`: a leading NUL in `sun_path` selects the length-delimited
//!     abstract namespace (preserving embedded NULs, up to 107 bytes); a non-NUL
//!     `sun_path` stops at the first NUL or `addr_len - 2` (a missing NUL is
//!     accepted, up to all 108 bytes).

use core::fmt;
use core::hash::{Hash, Hasher};
use core::ops::Deref;

pub use core::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4};

use crate::Errno;
use crate::abi::{
    AF_INET, AF_INET6, AF_UNIX, AF_UNSPEC, SIN6_LEN_RFC2133, SOCKADDR_IN_LEN, SOCKADDR_IN6_LEN, SOCKADDR_STORAGE_LEN,
    SOCKADDR_UN_LEN, UNIX_PATH_MAX,
};

/// Socket address families supported by [`SockAddr`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Family {
    Unix,
    Inet,
    Inet6,
}

impl Family {
    /// Convert a raw Linux `sa_family_t` value into a supported [`Family`].
    pub const fn from_raw(raw: u16) -> Option<Self> {
        match raw {
            AF_UNIX => Some(Self::Unix),
            AF_INET => Some(Self::Inet),
            AF_INET6 => Some(Self::Inet6),
            _ => None,
        }
    }

    /// Raw Linux `sa_family_t` value.
    pub const fn to_raw(self) -> u16 {
        match self {
            Self::Unix => AF_UNIX,
            Self::Inet => AF_INET,
            Self::Inet6 => AF_INET6,
        }
    }
}

/// Syscall context selecting Linux's per-call `sockaddr` rules (see the
/// [module documentation](self) for the full table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Call {
    /// `bind(2)`.
    Bind,
    /// `connect(2)`.
    Connect,
    /// `sendto(2)` (datagram destination).
    SendTo,
    /// `sendmsg(2)` (datagram destination).
    SendMsg,
}

/// Validate and clamp a raw `msghdr.msg_namelen` (`i32`) to
/// [`SOCKADDR_STORAGE_LEN`] (128), matching Linux `net/socket.c` `__copy_msghdr`
/// for `sendmsg` and `recvmsg`.
///
/// Negative values return `EINVAL`.
pub const fn clamp_msg_namelen(namelen: i32) -> Result<usize, Errno> {
    if namelen < 0 {
        return Err(Errno::EINVAL);
    }
    let len = namelen as usize;
    Ok(if len > SOCKADDR_STORAGE_LEN { SOCKADDR_STORAGE_LEN } else { len })
}

/// Read the leading 2-byte `sa_family_t` from a raw socket address buffer.
///
/// Returns `EINVAL` if fewer than 2 bytes are present. Callers use this before
/// [`SockAddr::decode`] for datagram and stream `connect(AF_UNSPEC)` disconnect
/// and dual-stack IPv6 `AF_INET` dispatch.
pub fn peek_family(bytes: &[u8]) -> Result<u16, Errno> {
    let &raw = bytes.first_chunk::<2>().ok_or(Errno::EINVAL)?;
    Ok(u16::from_le_bytes(raw))
}

/// Inline byte storage for a `sockaddr_un` path or abstract name (up to 108 bytes).
///
/// Allocates no heap memory. Unused tail bytes are always zeroed.
#[derive(Clone, Copy)]
pub struct UnixBytes {
    buf: [u8; UNIX_PATH_MAX],
    len: u8,
}

impl UnixBytes {
    fn from_slice_unchecked(bytes: &[u8]) -> Self {
        debug_assert!(bytes.len() <= UNIX_PATH_MAX);
        let mut buf = [0u8; UNIX_PATH_MAX];
        buf[..bytes.len()].copy_from_slice(bytes);
        Self { buf, len: bytes.len() as u8 }
    }

    /// The active bytes of the path or abstract name.
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len as usize]
    }
}

impl Deref for UnixBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl AsRef<[u8]> for UnixBytes {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl PartialEq for UnixBytes {
    fn eq(&self, other: &Self) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}

impl Eq for UnixBytes {}

impl PartialEq<[u8]> for UnixBytes {
    fn eq(&self, other: &[u8]) -> bool {
        self.as_bytes() == other
    }
}

impl PartialEq<&[u8]> for UnixBytes {
    fn eq(&self, other: &&[u8]) -> bool {
        self.as_bytes() == *other
    }
}

impl Hash for UnixBytes {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_bytes().hash(state);
    }
}

impl fmt::Debug for UnixBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_bytes().fmt(f)
    }
}

/// A decoded `AF_UNIX` socket name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnixName {
    /// `addrlen == 2` (`sizeof(sa_family_t)`): unnamed socket (`getsockname`)
    /// or autobind request (`bind(2)` only).
    Unnamed,
    /// Filesystem path (`1..=108` non-NUL bytes, excluding any trailing NUL).
    Path(UnixBytes),
    /// Abstract namespace name (`0..=107` bytes following the leading NUL; may contain NULs).
    Abstract(UnixBytes),
}

impl UnixName {
    /// Construct a validated filesystem `AF_UNIX` path (`1..=108` bytes, no NUL bytes).
    pub fn path(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.is_empty() || bytes.len() > UNIX_PATH_MAX || bytes.contains(&0) {
            return Err(Errno::EINVAL);
        }
        Ok(Self::Path(UnixBytes::from_slice_unchecked(bytes)))
    }

    /// Construct a validated abstract `AF_UNIX` name (`0..=107` bytes, embedded NULs allowed).
    pub fn abstract_name(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() > UNIX_PATH_MAX - 1 {
            return Err(Errno::EINVAL);
        }
        Ok(Self::Abstract(UnixBytes::from_slice_unchecked(bytes)))
    }
}

/// A decoded Linux socket address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SockAddr {
    Inet(SocketAddrV4),
    Inet6 { addr: Ipv6Addr, port: u16, flowinfo: u32, scope_id: u32 },
    Unix(UnixName),
}

impl SockAddr {
    /// The address family of this address.
    pub const fn family(&self) -> Family {
        match self {
            Self::Inet(_) => Family::Inet,
            Self::Inet6 { .. } => Family::Inet6,
            Self::Unix(_) => Family::Unix,
        }
    }

    /// Decode a required user-supplied sockaddr byte slice for `family_expected`
    /// under syscall context `call`.
    ///
    /// Applies Linux's length bound (clamping at 128 for [`Call::SendMsg`] and
    /// rejecting `> 128` for other calls) before family-specific validation.
    /// For optional destination addresses (`sendmsg` with `msg_namelen == 0`,
    /// `AF_UNIX` `sendto` with length 0, and `udpv6_sendmsg` `AF_UNSPEC`), use
    /// [`Self::decode_dest`].
    pub fn decode(family_expected: Family, call: Call, bytes: &[u8]) -> Result<Self, Errno> {
        let bytes = bound_addr_slice(call, bytes)?;
        match family_expected {
            Family::Inet => decode_inet(call, bytes),
            Family::Inet6 => decode_inet6(bytes),
            Family::Unix => decode_unix(call, bytes),
        }
    }

    /// Decode an optional datagram destination address for `family_expected`
    /// under `call`, returning `Ok(None)` when Linux treats the buffer as
    /// "no address":
    /// - [`Call::SendMsg`] with `bytes.is_empty()` in every family (`__copy_msghdr`);
    /// - [`Call::SendTo`] on [`Family::Unix`] with `bytes.is_empty()` (`unix_dgram_sendmsg`);
    /// - [`Call::SendTo`] or [`Call::SendMsg`] on [`Family::Inet6`] when
    ///   `bytes.len() >= 2` (after the 128-byte length rule) and
    ///   `sin6_family == AF_UNSPEC` (`udpv6_sendmsg`).
    ///
    /// `Call::SendTo` with `bytes.is_empty()` stays `EINVAL` for `Inet` and `Inet6`.
    pub fn decode_dest(family_expected: Family, call: Call, bytes: &[u8]) -> Result<Option<Self>, Errno> {
        match (family_expected, call) {
            (_, Call::SendMsg) if bytes.is_empty() => return Ok(None),
            (Family::Unix, Call::SendTo) if bytes.is_empty() => return Ok(None),
            (Family::Inet6, Call::SendTo | Call::SendMsg) => {
                let bounded = bound_addr_slice(call, bytes)?;
                if peek_family(bounded)? == AF_UNSPEC {
                    return Ok(None);
                }
                return decode_inet6(bounded).map(Some);
            }
            _ => {}
        }
        Self::decode(family_expected, call, bytes).map(Some)
    }

    /// The canonical encoded byte length Linux reports in `addrlen`.
    ///
    /// For a 108-byte `AF_UNIX` filesystem path this is 111 (`2 + 108 + 1` for
    /// the trailing NUL), matching Linux `unix_mkname_bsd` / `unix_getname`.
    pub const fn encoded_len(&self) -> usize {
        match self {
            Self::Inet(_) => SOCKADDR_IN_LEN,
            Self::Inet6 { .. } => SOCKADDR_IN6_LEN,
            Self::Unix(UnixName::Unnamed) => 2,
            Self::Unix(UnixName::Abstract(name)) => 3 + name.len as usize,
            Self::Unix(UnixName::Path(path)) => 2 + path.len as usize + 1,
        }
    }

    /// Encode this address into `out`, writing `min(out.len(), true_len)` bytes
    /// and returning `true_len` (the `addrlen` Linux writes back to userspace).
    ///
    /// Padding bytes inside `sockaddr_in` are zeroed. Bytes in `out` beyond
    /// `true_len` are left untouched.
    pub fn encode(&self, out: &mut [u8]) -> usize {
        let full_len = self.encoded_len();
        // Max encoded size is 111 bytes (AF_UNIX 108-byte path + 2-byte family + NUL).
        let mut tmp = [0u8; SOCKADDR_UN_LEN + 1];
        match self {
            Self::Inet(addr) => {
                tmp[0..2].copy_from_slice(&AF_INET.to_le_bytes());
                tmp[2..4].copy_from_slice(&addr.port().to_be_bytes());
                tmp[4..8].copy_from_slice(&addr.ip().octets());
                // tmp[8..16] is already zeroed (__pad).
            }
            Self::Inet6 { addr, port, flowinfo, scope_id } => {
                tmp[0..2].copy_from_slice(&AF_INET6.to_le_bytes());
                tmp[2..4].copy_from_slice(&port.to_be_bytes());
                tmp[4..8].copy_from_slice(&flowinfo.to_be_bytes());
                tmp[8..24].copy_from_slice(&addr.octets());
                tmp[24..28].copy_from_slice(&scope_id.to_le_bytes());
            }
            Self::Unix(name) => {
                tmp[0..2].copy_from_slice(&AF_UNIX.to_le_bytes());
                match name {
                    UnixName::Unnamed => {}
                    UnixName::Abstract(bytes) => {
                        let slice = bytes.as_bytes();
                        tmp[2] = 0;
                        tmp[3..3 + slice.len()].copy_from_slice(slice);
                    }
                    UnixName::Path(bytes) => {
                        let slice = bytes.as_bytes();
                        tmp[2..2 + slice.len()].copy_from_slice(slice);
                        tmp[2 + slice.len()] = 0;
                    }
                }
            }
        }
        let copy_len = out.len().min(full_len);
        out[..copy_len].copy_from_slice(&tmp[..copy_len]);
        full_len
    }
}

fn bound_addr_slice(call: Call, bytes: &[u8]) -> Result<&[u8], Errno> {
    match call {
        Call::SendMsg => Ok(&bytes[..bytes.len().min(SOCKADDR_STORAGE_LEN)]),
        Call::Bind | Call::Connect | Call::SendTo => {
            if bytes.len() > SOCKADDR_STORAGE_LEN {
                return Err(Errno::EINVAL);
            }
            Ok(bytes)
        }
    }
}

fn decode_inet(call: Call, bytes: &[u8]) -> Result<SockAddr, Errno> {
    let hdr = bytes.first_chunk::<SOCKADDR_IN_LEN>().ok_or(Errno::EINVAL)?;
    let family = u16::from_le_bytes([hdr[0], hdr[1]]);
    let port = u16::from_be_bytes([hdr[2], hdr[3]]);
    let ip = Ipv4Addr::new(hdr[4], hdr[5], hdr[6], hdr[7]);
    let family_ok = match (call, family) {
        (_, AF_INET) => true,
        (Call::Bind, AF_UNSPEC) => ip.is_unspecified(),
        (Call::SendTo | Call::SendMsg, AF_UNSPEC) => true,
        _ => false,
    };
    if !family_ok {
        return Err(Errno::EAFNOSUPPORT);
    }
    Ok(SockAddr::Inet(SocketAddrV4::new(ip, port)))
}

fn decode_inet6(bytes: &[u8]) -> Result<SockAddr, Errno> {
    let head = bytes.first_chunk::<SIN6_LEN_RFC2133>().ok_or(Errno::EINVAL)?;
    let family = u16::from_le_bytes([head[0], head[1]]);
    if family != AF_INET6 {
        return Err(Errno::EAFNOSUPPORT);
    }
    let port = u16::from_be_bytes([head[2], head[3]]);
    let flowinfo = u32::from_be_bytes([head[4], head[5], head[6], head[7]]);
    let mut octets = [0u8; 16];
    octets.copy_from_slice(&head[8..24]);
    let scope_id = bytes[SIN6_LEN_RFC2133..].first_chunk::<4>().map_or(0, |&raw| u32::from_le_bytes(raw));
    Ok(SockAddr::Inet6 { addr: Ipv6Addr::from(octets), port, flowinfo, scope_id })
}

fn decode_unix(call: Call, bytes: &[u8]) -> Result<SockAddr, Errno> {
    if bytes.len() > SOCKADDR_UN_LEN {
        return Err(Errno::EINVAL);
    }
    let family = peek_family(bytes)?;
    if family != AF_UNIX {
        return Err(Errno::EINVAL);
    }
    if bytes.len() == 2 {
        return match call {
            Call::Bind => Ok(SockAddr::Unix(UnixName::Unnamed)),
            Call::Connect | Call::SendTo | Call::SendMsg => Err(Errno::EINVAL),
        };
    }
    let path = &bytes[2..];
    if path[0] == 0 {
        Ok(SockAddr::Unix(UnixName::Abstract(UnixBytes::from_slice_unchecked(&path[1..]))))
    } else {
        let end = path.iter().position(|&b| b == 0).unwrap_or(path.len());
        Ok(SockAddr::Unix(UnixName::Path(UnixBytes::from_slice_unchecked(&path[..end]))))
    }
}
