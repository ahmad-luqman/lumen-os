//! Socket option value decoding (`setsockopt`) and reply formatting (`getsockopt`).
//!
//! Follows Linux v6.12:
//! - `net/core/sock.c` (`sk_setsockopt`, `sk_getsockopt`, `sock_set_timeout`,
//!   `sock_get_timeout`):
//!   - `optlen < 4` returns `EINVAL` before get-only or unknown options return
//!     `ENOPROTOOPT`, except for string options (`SO_BINDTODEVICE`,
//!     `TCP_CONGESTION`, `TCP_ULP`, `TCP_FASTOPEN_KEY`), which return
//!     `ENOPROTOOPT` at any length.
//!   - Get-only options (`SO_TYPE`, `SO_ERROR`, `SO_PEERCRED`, `SO_SNDLOWAT`,
//!     `SO_ACCEPTCONN`, `SO_PROTOCOL`, `SO_DOMAIN`) return `ENOPROTOOPT` from
//!     [`decode_opt`].
//!   - `sock_set_timeout`: `tv_usec` outside `0..1_000_000` is `EDOM`;
//!     `tv_sec < 0` is a 0 timeout ([`Timeout::Immediate`], causing blocking
//!     calls to return `EAGAIN` at once); `{0, 0}` and `tv_sec >= MAX_TIMEOUT_SECS`
//!     are [`Timeout::Infinite`]. Microseconds round up (`DIV_ROUND_UP`) when
//!     converting to timer ticks or back to `Timeval`.
//!   - `sock_get_timeout`: both [`Timeout::Infinite`] and [`Timeout::Immediate`]
//!     encode back as `{0, 0}`.
//! - `net/ipv4/tcp.c` (`do_tcp_setsockopt`, `do_tcp_getsockopt`):
//!   - `optlen < 4` returns `EINVAL` on set before unknown options return
//!     `ENOPROTOOPT` (string options skipped as above).
//!   - In Linux v6.12, `do_tcp_getsockopt` rejects negative `optlen` with
//!     `EINVAL` before `min_t`, so [`check_optlen`] applies to `SOL_SOCKET`,
//!     `IPPROTO_IP`, and `IPPROTO_TCP`.
//! - `net/ipv4/ip_sockglue.c` (`do_ip_setsockopt`, `do_ip_getsockopt`):
//!   - `IP_TOS`, `IP_TTL`, and `IP_RECVERR` ([`OptKind::IntOrByte`]) accept
//!     `optlen` 1..3 as an unsigned byte and `>= 4` as a little-endian `i32` on
//!     set; `optlen == 0` decodes as `0` for `IP_TOS`/`IP_RECVERR` and returns
//!     `EINVAL` for `IP_TTL` (`if (optlen < 1)`).
//!   - On get, [`write_opt`] (when given [`OptVal::IntOrByte`]) and
//!     [`write_opt_for`] write 1 byte (`written_len = 1`) when `0 < optlen < 4`
//!     and `0 <= val <= 255`.
//! - **Caller duties:** the socket type decides which level handler runs before
//!   decoding here (for example, `IPPROTO_TCP` on a UDP socket is `ENOPROTOOPT`,
//!   any non-`SOL_SOCKET` level on an `AF_UNIX` socket is `EOPNOTSUPP`, and
//!   `SOL_UDP` options apply their own length check).

use core::time::Duration;

use crate::Errno;
use crate::abi::{
    IP_RECVERR, IP_TOS, IP_TTL, IPPROTO_IP, IPPROTO_IPV6, IPPROTO_TCP, IPV6_RECVERR, IPV6_UNICAST_HOPS, IPV6_V6ONLY,
    LINGER_LEN, SO_ACCEPTCONN, SO_BINDTODEVICE, SO_BROADCAST, SO_DEBUG, SO_DOMAIN, SO_DONTROUTE, SO_ERROR,
    SO_KEEPALIVE, SO_LINGER, SO_OOBINLINE, SO_PASSCRED, SO_PEERCRED, SO_PROTOCOL, SO_RCVBUF, SO_RCVBUFFORCE,
    SO_RCVLOWAT, SO_RCVTIMEO_NEW, SO_RCVTIMEO_OLD, SO_REUSEADDR, SO_REUSEPORT, SO_SNDBUF, SO_SNDBUFFORCE, SO_SNDLOWAT,
    SO_SNDTIMEO_NEW, SO_SNDTIMEO_OLD, SO_TYPE, SOL_SOCKET, TCP_CONGESTION, TCP_CORK, TCP_FASTOPEN_KEY, TCP_KEEPCNT,
    TCP_KEEPIDLE, TCP_KEEPINTVL, TCP_MAXSEG, TCP_NODELAY, TCP_QUICKACK, TCP_ULP, TCP_USER_TIMEOUT, TIMEVAL_LEN,
    UCRED_LEN,
};
use crate::cmsg::Ucred;

/// Largest encoded option value produced by [`write_opt`] (`Timeval`: 16 bytes).
pub const MAX_OPT_LEN: usize = TIMEVAL_LEN;

/// Microseconds per second for `Timeval` validation (`USEC_PER_SEC`).
const USEC_PER_SEC: i64 = 1_000_000;

/// Tick frequency (Hz) assumed for [`MAX_TIMEOUT_SECS`], matching Lumen's 1 ms
/// scheduler tick (`HZ = 1000`).
pub const TIMEOUT_HZ: i64 = 1_000;

/// Upper bound on finite `tv_sec` (`i64::MAX / TIMEOUT_HZ - 1`, matching Linux
/// `net/core/sock.c` `sock_set_timeout`'s `MAX_SCHEDULE_TIMEOUT / HZ - 1` at
/// `HZ = 1000`).
///
/// Any `tv_sec >= MAX_TIMEOUT_SECS` saturates to [`Timeout::Infinite`] so
/// callers converting `Duration` to ticks at [`TIMEOUT_HZ`] (rounding
/// microseconds up with `DIV_ROUND_UP`) cannot overflow `i64`. Callers using a
/// higher `HZ` must cap further before multiplying.
pub const MAX_TIMEOUT_SECS: i64 = i64::MAX / TIMEOUT_HZ - 1;

/// `SO_LINGER` value (`struct linger`, 8 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Linger {
    pub onoff: i32,
    pub linger: i32,
}

impl Linger {
    /// Decode an 8-byte little-endian `struct linger`.
    pub const fn from_bytes(bytes: &[u8; LINGER_LEN]) -> Self {
        Self {
            onoff: i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            linger: i32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        }
    }

    /// Encode into an 8-byte little-endian `struct linger`.
    pub fn to_bytes(&self) -> [u8; LINGER_LEN] {
        let mut out = [0u8; LINGER_LEN];
        out[0..4].copy_from_slice(&self.onoff.to_le_bytes());
        out[4..8].copy_from_slice(&self.linger.to_le_bytes());
        out
    }
}

/// Socket timeout semantics decoded from a [`Timeval`]
/// (`net/core/sock.c` `sock_set_timeout` / `sock_get_timeout`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timeout {
    /// `{ tv_sec: 0, tv_usec: 0 }` or `tv_sec >= MAX_TIMEOUT_SECS`: infinite
    /// timeout (`MAX_SCHEDULE_TIMEOUT`).
    Infinite,
    /// `tv_sec < 0` (with valid `tv_usec`): zero timeout (`*timeo_p = 0`;
    /// blocking operations return `EAGAIN` immediately).
    Immediate,
    /// Positive finite timeout duration (`0 < tv_sec < MAX_TIMEOUT_SECS` or
    /// `tv_sec == 0 && tv_usec > 0`). Callers converting to ticks round
    /// microseconds up (`DIV_ROUND_UP`).
    Duration(Duration),
}

impl Timeout {
    /// Encode back to a `struct timeval` for `getsockopt` (`net/core/sock.c`
    /// `sock_get_timeout`).
    ///
    /// Both [`Self::Infinite`] and [`Self::Immediate`] produce `{0, 0}`.
    /// Sub-microsecond nanoseconds in [`Self::Duration`] round up
    /// (`DIV_ROUND_UP`), carrying into seconds if needed.
    pub const fn to_timeval(self) -> Timeval {
        match self {
            Self::Infinite | Self::Immediate => Timeval { tv_sec: 0, tv_usec: 0 },
            Self::Duration(d) => {
                let mut secs = d.as_secs();
                let mut usec = (d.subsec_nanos() as u64).div_ceil(1_000);
                if usec >= USEC_PER_SEC as u64 {
                    secs = secs.saturating_add(1);
                    usec = 0;
                }
                let tv_sec = if secs > i64::MAX as u64 { i64::MAX } else { secs as i64 };
                Timeval { tv_sec, tv_usec: usec as i64 }
            }
        }
    }
}

/// `SO_RCVTIMEO` / `SO_SNDTIMEO` value (`struct timeval`, 16 bytes on x86-64).
///
/// On x86-64, `SO_RCVTIMEO_OLD`/`SO_RCVTIMEO_NEW` and
/// `SO_SNDTIMEO_OLD`/`SO_SNDTIMEO_NEW` share this identical 16-byte layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Timeval {
    pub tv_sec: i64,
    pub tv_usec: i64,
}

impl Timeval {
    /// Decode a 16-byte little-endian `struct timeval` without range checks.
    pub const fn from_bytes(bytes: &[u8; TIMEVAL_LEN]) -> Self {
        Self {
            tv_sec: i64::from_le_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ]),
            tv_usec: i64::from_le_bytes([
                bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
            ]),
        }
    }

    /// Encode into a 16-byte little-endian `struct timeval`.
    pub fn to_bytes(&self) -> [u8; TIMEVAL_LEN] {
        let mut out = [0u8; TIMEVAL_LEN];
        out[0..8].copy_from_slice(&self.tv_sec.to_le_bytes());
        out[8..16].copy_from_slice(&self.tv_usec.to_le_bytes());
        out
    }

    /// Interpret this `timeval` per Linux `net/core/sock.c` `sock_set_timeout`.
    ///
    /// - `tv_usec < 0 || tv_usec >= 1_000_000` -> `Err(Errno::EDOM)`;
    /// - `tv_sec < 0` -> `Ok(Timeout::Immediate)` (timeout 0, `EAGAIN` at once);
    /// - `(tv_sec == 0 && tv_usec == 0) || tv_sec >= MAX_TIMEOUT_SECS` ->
    ///   `Ok(Timeout::Infinite)`;
    /// - otherwise -> `Ok(Timeout::Duration(..))`.
    pub fn to_timeout(self) -> Result<Timeout, Errno> {
        if self.tv_usec < 0 || self.tv_usec >= USEC_PER_SEC {
            return Err(Errno::EDOM);
        }
        if self.tv_sec < 0 {
            Ok(Timeout::Immediate)
        } else if (self.tv_sec == 0 && self.tv_usec == 0) || self.tv_sec >= MAX_TIMEOUT_SECS {
            Ok(Timeout::Infinite)
        } else {
            Ok(Timeout::Duration(Duration::new(self.tv_sec as u64, (self.tv_usec as u32) * 1_000)))
        }
    }
}

/// Wire value shape expected by a socket option.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OptKind {
    /// 4-byte little-endian `i32` (`optlen < 4` -> `EINVAL`).
    Int,
    /// `IPPROTO_IP` int-or-byte option (`do_ip_setsockopt`/`do_ip_getsockopt`):
    /// `optlen >= 4` is an `i32`, `1..=3` is an unsigned byte, `0` is `0`
    /// (`IP_TTL` additionally rejects `optlen == 0` with `EINVAL` in [`decode_opt`]).
    IntOrByte,
    Linger,
    Timeval,
    Ucred,
}

impl OptKind {
    /// Canonical full output byte length.
    pub const fn size(self) -> usize {
        match self {
            Self::Int | Self::IntOrByte => 4,
            Self::Linger => LINGER_LEN,
            Self::Timeval => TIMEVAL_LEN,
            Self::Ucred => UCRED_LEN,
        }
    }
}

/// Decoded socket option value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptVal {
    Int(i32),
    /// `IPPROTO_IP` int-or-byte value: retains the 1-byte `do_ip_getsockopt`
    /// rule when passed to [`write_opt`].
    IntOrByte(i32),
    Linger(Linger),
    Timeout(Timeout),
    Ucred(Ucred),
}

impl OptVal {
    /// Decode a 32-bit signed integer option (`optlen < 4` -> `EINVAL`).
    ///
    /// Matches Linux `net/core/sock.c` `sk_setsockopt` and `net/ipv4/tcp.c`
    /// `do_tcp_setsockopt`: trailing bytes beyond 4 are accepted and ignored.
    pub fn int(bytes: &[u8]) -> Result<i32, Errno> {
        let &arr = bytes.first_chunk::<4>().ok_or(Errno::EINVAL)?;
        Ok(i32::from_le_bytes(arr))
    }

    /// Decode an `IPPROTO_IP` int-or-byte option (`net/ipv4/ip_sockglue.c`
    /// `do_ip_setsockopt`): `>= 4` bytes as little-endian `i32`, `1..=3` bytes
    /// as an unsigned byte (`bytes[0] as i32`), and `0` bytes as `0`.
    pub fn int_or_byte(bytes: &[u8]) -> i32 {
        if let Some(&arr) = bytes.first_chunk::<4>() {
            i32::from_le_bytes(arr)
        } else if let Some(&b) = bytes.first() {
            i32::from(b)
        } else {
            0
        }
    }

    /// Decode a `struct linger` option (`optlen < 8` -> `EINVAL`).
    pub fn linger(bytes: &[u8]) -> Result<Linger, Errno> {
        let arr = bytes.first_chunk::<LINGER_LEN>().ok_or(Errno::EINVAL)?;
        Ok(Linger::from_bytes(arr))
    }

    /// Decode and validate a raw `struct timeval` option (`net/core/sock.c`
    /// `sock_set_timeout`): `optlen < 16` -> `EINVAL`, `tv_usec` outside
    /// `0..1_000_000` -> `EDOM`.
    pub fn timeval(bytes: &[u8]) -> Result<Timeval, Errno> {
        let arr = bytes.first_chunk::<TIMEVAL_LEN>().ok_or(Errno::EINVAL)?;
        let tv = Timeval::from_bytes(arr);
        tv.to_timeout()?;
        Ok(tv)
    }

    /// Decode and interpret a `struct timeval` option into [`Timeout`].
    pub fn timeout(bytes: &[u8]) -> Result<Timeout, Errno> {
        let arr = bytes.first_chunk::<TIMEVAL_LEN>().ok_or(Errno::EINVAL)?;
        Timeval::from_bytes(arr).to_timeout()
    }

    /// Decode a `struct ucred` option (`optlen < 12` -> `EINVAL`).
    pub fn ucred(bytes: &[u8]) -> Result<Ucred, Errno> {
        let arr = bytes.first_chunk::<UCRED_LEN>().ok_or(Errno::EINVAL)?;
        Ok(Ucred::from_bytes(arr))
    }

    /// Decode a value of `kind` from `bytes`.
    pub fn decode(kind: OptKind, bytes: &[u8]) -> Result<Self, Errno> {
        match kind {
            OptKind::Int => Self::int(bytes).map(Self::Int),
            OptKind::IntOrByte => Ok(Self::IntOrByte(Self::int_or_byte(bytes))),
            OptKind::Linger => Self::linger(bytes).map(Self::Linger),
            OptKind::Timeval => Self::timeout(bytes).map(Self::Timeout),
            OptKind::Ucred => Self::ucred(bytes).map(Self::Ucred),
        }
    }

    /// Wire shape of this value.
    pub const fn kind(&self) -> OptKind {
        match self {
            Self::Int(_) => OptKind::Int,
            Self::IntOrByte(_) => OptKind::IntOrByte,
            Self::Linger(_) => OptKind::Linger,
            Self::Timeout(_) => OptKind::Timeval,
            Self::Ucred(_) => OptKind::Ucred,
        }
    }

    /// Canonical wire byte length of this option value.
    pub const fn size(&self) -> usize {
        self.kind().size()
    }
}

/// Validate a user `socklen_t` (`i32`) passed to `getsockopt` (`< 0` -> `EINVAL`).
///
/// Applies across `SOL_SOCKET` (`net/core/sock.c` `sk_getsockopt`),
/// `IPPROTO_IP` (`net/ipv4/ip_sockglue.c` `do_ip_getsockopt`), and
/// `IPPROTO_TCP` (Linux v6.12 `net/ipv4/tcp.c` `do_tcp_getsockopt` checks
/// `len < 0` before `min_t`).
pub const fn check_optlen(optlen: i32) -> Result<usize, Errno> {
    if optlen < 0 { Err(Errno::EINVAL) } else { Ok(optlen as usize) }
}

/// Format a `getsockopt` option value truncated to `min(optlen, value.size())`,
/// applying the 1-byte `do_ip_getsockopt` rule whenever `value` is
/// [`OptVal::IntOrByte`] (`0 < optlen < 4` and `0 <= val <= 255` writes 1 byte
/// and returns `written_len = 1`).
///
/// Unused tail bytes beyond `written_len` are zeroed.
pub fn write_opt(value: OptVal, optlen: usize) -> ([u8; MAX_OPT_LEN], usize) {
    if let OptVal::IntOrByte(v) = value
        && (1..4).contains(&optlen)
        && (0..=255).contains(&v)
    {
        let mut out = [0u8; MAX_OPT_LEN];
        out[0] = v as u8;
        return (out, 1);
    }
    let mut full = [0u8; MAX_OPT_LEN];
    let size = value.size();
    match value {
        OptVal::Int(v) | OptVal::IntOrByte(v) => full[0..4].copy_from_slice(&v.to_le_bytes()),
        OptVal::Linger(v) => full[0..LINGER_LEN].copy_from_slice(&v.to_bytes()),
        OptVal::Timeout(v) => full[0..TIMEVAL_LEN].copy_from_slice(&v.to_timeval().to_bytes()),
        OptVal::Ucred(v) => full[0..UCRED_LEN].copy_from_slice(&v.to_bytes()),
    }
    let written = optlen.min(size);
    full[written..].fill(0);
    (full, written)
}

/// Format a `getsockopt` reply for `opt`, ensuring `IPPROTO_IP`
/// [`OptKind::IntOrByte`] options always apply the 1-byte `do_ip_getsockopt`
/// rule even if the caller passed [`OptVal::Int`].
pub fn write_opt_for(opt: SockOpt, value: OptVal, optlen: usize) -> ([u8; MAX_OPT_LEN], usize) {
    let value = match (opt.kind(), value) {
        (OptKind::IntOrByte, OptVal::Int(v)) => OptVal::IntOrByte(v),
        _ => value,
    };
    write_opt(value, optlen)
}

/// Known socket option identifiers across `SOL_SOCKET`, `IPPROTO_TCP`,
/// `IPPROTO_IP`, and `IPPROTO_IPV6`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SockOpt {
    // SOL_SOCKET
    Debug,
    ReuseAddr,
    Type,
    Error,
    DontRoute,
    Broadcast,
    SndBuf,
    RcvBuf,
    KeepAlive,
    OobInline,
    Linger,
    ReusePort,
    PassCred,
    PeerCred,
    RcvLowat,
    SndLowat,
    RcvTimeo,
    SndTimeo,
    AcceptConn,
    SndBufForce,
    RcvBufForce,
    Protocol,
    Domain,
    // IPPROTO_TCP
    TcpNoDelay,
    TcpMaxSeg,
    TcpCork,
    TcpKeepIdle,
    TcpKeepIntvl,
    TcpKeepCnt,
    TcpQuickAck,
    TcpUserTimeout,
    // IPPROTO_IP
    IpTos,
    IpTtl,
    IpRecvErr,
    // IPPROTO_IPV6
    Ipv6UnicastHops,
    Ipv6RecvErr,
    Ipv6V6Only,
}

impl SockOpt {
    /// Identify a `(level, optname)` pair, returning `ENOPROTOOPT` if unknown.
    ///
    /// Both `SO_RCVTIMEO_OLD` (20) and `SO_RCVTIMEO_NEW` (66) map to
    /// [`SockOpt::RcvTimeo`], and both `SO_SNDTIMEO_OLD` (21) and
    /// `SO_SNDTIMEO_NEW` (67) map to [`SockOpt::SndTimeo`].
    pub const fn from_raw(level: i32, optname: i32) -> Result<Self, Errno> {
        match (level, optname) {
            (SOL_SOCKET, SO_DEBUG) => Ok(Self::Debug),
            (SOL_SOCKET, SO_REUSEADDR) => Ok(Self::ReuseAddr),
            (SOL_SOCKET, SO_TYPE) => Ok(Self::Type),
            (SOL_SOCKET, SO_ERROR) => Ok(Self::Error),
            (SOL_SOCKET, SO_DONTROUTE) => Ok(Self::DontRoute),
            (SOL_SOCKET, SO_BROADCAST) => Ok(Self::Broadcast),
            (SOL_SOCKET, SO_SNDBUF) => Ok(Self::SndBuf),
            (SOL_SOCKET, SO_RCVBUF) => Ok(Self::RcvBuf),
            (SOL_SOCKET, SO_KEEPALIVE) => Ok(Self::KeepAlive),
            (SOL_SOCKET, SO_OOBINLINE) => Ok(Self::OobInline),
            (SOL_SOCKET, SO_LINGER) => Ok(Self::Linger),
            (SOL_SOCKET, SO_REUSEPORT) => Ok(Self::ReusePort),
            (SOL_SOCKET, SO_PASSCRED) => Ok(Self::PassCred),
            (SOL_SOCKET, SO_PEERCRED) => Ok(Self::PeerCred),
            (SOL_SOCKET, SO_RCVLOWAT) => Ok(Self::RcvLowat),
            (SOL_SOCKET, SO_SNDLOWAT) => Ok(Self::SndLowat),
            (SOL_SOCKET, SO_RCVTIMEO_OLD | SO_RCVTIMEO_NEW) => Ok(Self::RcvTimeo),
            (SOL_SOCKET, SO_SNDTIMEO_OLD | SO_SNDTIMEO_NEW) => Ok(Self::SndTimeo),
            (SOL_SOCKET, SO_ACCEPTCONN) => Ok(Self::AcceptConn),
            (SOL_SOCKET, SO_SNDBUFFORCE) => Ok(Self::SndBufForce),
            (SOL_SOCKET, SO_RCVBUFFORCE) => Ok(Self::RcvBufForce),
            (SOL_SOCKET, SO_PROTOCOL) => Ok(Self::Protocol),
            (SOL_SOCKET, SO_DOMAIN) => Ok(Self::Domain),

            (IPPROTO_TCP, TCP_NODELAY) => Ok(Self::TcpNoDelay),
            (IPPROTO_TCP, TCP_MAXSEG) => Ok(Self::TcpMaxSeg),
            (IPPROTO_TCP, TCP_CORK) => Ok(Self::TcpCork),
            (IPPROTO_TCP, TCP_KEEPIDLE) => Ok(Self::TcpKeepIdle),
            (IPPROTO_TCP, TCP_KEEPINTVL) => Ok(Self::TcpKeepIntvl),
            (IPPROTO_TCP, TCP_KEEPCNT) => Ok(Self::TcpKeepCnt),
            (IPPROTO_TCP, TCP_QUICKACK) => Ok(Self::TcpQuickAck),
            (IPPROTO_TCP, TCP_USER_TIMEOUT) => Ok(Self::TcpUserTimeout),

            (IPPROTO_IP, IP_TOS) => Ok(Self::IpTos),
            (IPPROTO_IP, IP_TTL) => Ok(Self::IpTtl),
            (IPPROTO_IP, IP_RECVERR) => Ok(Self::IpRecvErr),

            (IPPROTO_IPV6, IPV6_UNICAST_HOPS) => Ok(Self::Ipv6UnicastHops),
            (IPPROTO_IPV6, IPV6_RECVERR) => Ok(Self::Ipv6RecvErr),
            (IPPROTO_IPV6, IPV6_V6ONLY) => Ok(Self::Ipv6V6Only),

            _ => Err(Errno::ENOPROTOOPT),
        }
    }

    /// Whether this option may be set with `setsockopt(2)`.
    ///
    /// Returns `false` for Linux's get-only `SOL_SOCKET` options
    /// (`SO_TYPE`, `SO_ERROR`, `SO_PEERCRED`, `SO_SNDLOWAT`, `SO_ACCEPTCONN`,
    /// `SO_PROTOCOL`, `SO_DOMAIN`; `net/core/sock.c` `sk_setsockopt`).
    pub const fn settable(self) -> bool {
        !matches!(
            self,
            Self::Type
                | Self::Error
                | Self::PeerCred
                | Self::SndLowat
                | Self::AcceptConn
                | Self::Protocol
                | Self::Domain
        )
    }

    /// The wire value shape expected by this option.
    pub const fn kind(self) -> OptKind {
        match self {
            Self::IpTos | Self::IpTtl | Self::IpRecvErr => OptKind::IntOrByte,
            Self::Linger => OptKind::Linger,
            Self::RcvTimeo | Self::SndTimeo => OptKind::Timeval,
            Self::PeerCred => OptKind::Ucred,
            _ => OptKind::Int,
        }
    }
}

const fn is_string_opt(level: i32, optname: i32) -> bool {
    matches!(
        (level, optname),
        (SOL_SOCKET, SO_BINDTODEVICE) | (IPPROTO_TCP, TCP_CONGESTION | TCP_ULP | TCP_FASTOPEN_KEY)
    )
}

/// Identify `(level, optname)` and decode its `setsockopt` wire value.
///
/// Mirrors Linux v6.12 `net/core/sock.c` `sk_setsockopt`, `net/ipv4/tcp.c`
/// `do_tcp_setsockopt`, and `net/ipv4/ip_sockglue.c` `do_ip_setsockopt`:
/// - String options (`SO_BINDTODEVICE`, `TCP_CONGESTION`, `TCP_ULP`,
///   `TCP_FASTOPEN_KEY`) skip `optlen < 4` and return `ENOPROTOOPT` at any length.
/// - On `SOL_SOCKET` and `IPPROTO_TCP`, `optlen < 4` returns `EINVAL` before
///   get-only (`!opt.settable()`) or unknown options return `ENOPROTOOPT`.
/// - `IP_TTL` rejects `optlen == 0` with `EINVAL` (`if (optlen < 1)`), while
///   `IP_TOS` and `IP_RECVERR` decode `optlen == 0` as `0`.
pub fn decode_opt(level: i32, optname: i32, bytes: &[u8]) -> Result<(SockOpt, OptVal), Errno> {
    if is_string_opt(level, optname) {
        return Err(Errno::ENOPROTOOPT);
    }
    match SockOpt::from_raw(level, optname) {
        Ok(opt) if opt.settable() => {
            if opt == SockOpt::IpTtl && bytes.is_empty() {
                return Err(Errno::EINVAL);
            }
            Ok((opt, OptVal::decode(opt.kind(), bytes)?))
        }
        _ => {
            if matches!(level, SOL_SOCKET | IPPROTO_TCP) && bytes.len() < 4 {
                Err(Errno::EINVAL)
            } else {
                Err(Errno::ENOPROTOOPT)
            }
        }
    }
}
