//! Generic socket syscalls (M9 B1, #472; ARCHITECTURE section 8).
//!
//! Every handler here is generic over [`SockOps`]: Linux v6.12 argument/errno
//! ordering (`net/socket.c`), address decoding and encoding (`lumen-sockabi`),
//! `msghdr` and `iovec` validation, `CmsgIter` / `CmsgWriter` ancillary data
//! (`SCM_RIGHTS` via [`InFlightFile`], `SCM_CREDENTIALS`), option decoding
//! (`decode_opt` / `write_opt_for`), and descriptor slot reservation before
//! each call with installation after. Families (`AF_UNIX` in C1, UDP in C3,
//! TCP in C4, ICMP in C6) only implement [`SockOps`] and plug into
//! [`crate::net::sock::create_socket`] / [`crate::net::sock::create_socketpair`].

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::mem::offset_of;

use lumen_sockabi::abi::{Msghdr, SOCKADDR_UN_LEN};
use lumen_sockabi::{
    AF_INET, AF_INET6, AF_UNSPEC, Call, CmsgIter, CmsgWriter, Errno, Family, MAX_OPT_LEN, MSG_CMSG_CLOEXEC, MSG_CTRUNC,
    MSG_DONTWAIT, MSG_EOR, MSG_ERRQUEUE, MSG_OOB, MSGHDR_LEN, OptVal, SCM_CREDENTIALS, SCM_MAX_FD, SCM_RIGHTS,
    SOCKADDR_STORAGE_LEN, SOL_SOCKET, SockAddr, SockOpt, UCRED_LEN, check_optlen, clamp_msg_namelen, cmsg_space,
    decode_opt, peek_family, write_opt_for,
};

use super::at::{Fd, UserPtr};
use super::fd::vfs;
use super::{RawIovec, copy_iovecs, copy_pod_from_user, copy_pod_to_user, finish};
use crate::linux::process::{self, IoFile, KernelFile, ReservedFd};
use crate::linux::{errno, general, net};
use crate::net::sock::{
    InFlightFile, MAX_RW_COUNT, RecvDone, RecvIo, SendIo, ShutdownHow, SockKind, SockOps, UserIovec, UserSpan,
    create_socket, create_socketpair, new_socket_file,
};
use crate::user::{CopyError, copy_from_user, copy_to_user, user_range_valid};

/// Low 4 bits of `socket`/`socketpair`'s `type` argument (`SOCK_TYPE_MASK`).
const SOCK_TYPE_MASK: u32 = 0xf;

/// Obsolete Linux `SOCK_PACKET` socket type (Linux v6.12 `include/linux/net.h`).
const SOCK_PACKET: u32 = 10;

/// Upper bound of valid Linux `enum sock_type` values (`SOCK_PACKET + 1`,
/// Linux v6.12 `include/linux/net.h`).
const SOCK_MAX: u32 = SOCK_PACKET + 1;

/// Upper bound of valid Linux address families (`NPROTO` / `AF_MAX`, Linux
/// v6.12 `include/linux/socket.h:244`).
const AF_MAX: i32 = 46;

/// Upper bound of IP protocol numbers (`IPPROTO_MAX`, Linux v6.12
/// `include/uapi/linux/in.h:84-88`: `IPPROTO_SMC` = 256, `IPPROTO_MPTCP` = 262).
const IPPROTO_MAX: i32 = 263;

/// Internal 32-bit compat cmsg flag rejected by native 64-bit `sendmsg`/
/// `recvmsg` (`MSG_CMSG_COMPAT`, Linux v6.12 `net/socket.c:2683, 2891`).
const MSG_CMSG_COMPAT: u32 = 0x8000_0000;

/// Default `net.core.somaxconn` clamp for `listen(2)` (Linux v6.12
/// `include/linux/socket.h:298`, `net/socket.c:1887`).
const SOMAXCONN: i32 = 4096;

/// `SOCK_NONBLOCK` on Linux x86-64 (identical to `O_NONBLOCK`).
const SOCK_NONBLOCK: u32 = general::O_NONBLOCK;

/// `SOCK_CLOEXEC` on Linux x86-64 (identical to `O_CLOEXEC`).
const SOCK_CLOEXEC: u32 = general::O_CLOEXEC;

/// Maximum control buffer size (strictly `< OPTMEM_MAX`, matching Linux v6.12
/// `net/core/sock.c:2784` `sock_kmalloc` with `sysctl_optmem_max = 131072`).
const OPTMEM_MAX: usize = 131_072;

/// Look up an open, non-`O_PATH` descriptor (`fdget` in Linux v6.12, returning
/// `-EBADF` for closed or `O_PATH` descriptors).
fn open_non_path(fd: Fd) -> Result<IoFile, u32> {
    let file = fd.file()?;
    file.require_non_path().map_err(vfs)?;
    Ok(file)
}

/// Look up an open, non-`O_PATH` socket descriptor (`sockfd_lookup_light` in
/// Linux v6.12: `-EBADF` if closed or `O_PATH`, `-ENOTSOCK` if not a socket).
fn socket_file(fd: Fd) -> Result<(IoFile, Arc<dyn SockOps>), u32> {
    let file = open_non_path(fd)?;
    let sock = Arc::clone(file.sock().ok_or(errno::ENOTSOCK)?);
    Ok((file, sock))
}

/// Effective non-blocking flag for a socket call (`O_NONBLOCK` or `MSG_DONTWAIT`).
fn nonblock(file: &KernelFile, flags: u32) -> bool {
    file.is_nonblock() || (flags & MSG_DONTWAIT != 0)
}

/// Reject unsupported receive flags (`MSG_ERRQUEUE` -> `-EAGAIN`, `MSG_OOB` ->
/// `-EINVAL`; ARCHITECTURE section 8).
fn check_recv_flags(flags: u32) -> Result<(), u32> {
    if flags & MSG_ERRQUEUE != 0 {
        return Err(errno::EAGAIN);
    }
    if flags & MSG_OOB != 0 {
        return Err(errno::EINVAL);
    }
    Ok(())
}

/// Validate `type` creation flags (`SOCK_NONBLOCK | SOCK_CLOEXEC`), returning
/// `(base_type, nonblock, cloexec)` (Linux v6.12 `net/socket.c:1665, 1745`).
fn parse_type_flags(ty: i32) -> Result<(u32, bool, bool), u32> {
    let raw = ty as u32;
    let flags = raw & !SOCK_TYPE_MASK;
    if flags & !(SOCK_NONBLOCK | SOCK_CLOEXEC) != 0 {
        return Err(errno::EINVAL);
    }
    Ok((raw & SOCK_TYPE_MASK, flags & SOCK_NONBLOCK != 0, flags & SOCK_CLOEXEC != 0))
}

/// Validate `(domain, base_ty)` in Linux v6.12 `__sock_create` order
/// (`net/socket.c:1515-1564`): out-of-range family (`< 0` or `>= AF_MAX`) ->
/// `-EAFNOSUPPORT`, `base_ty >= SOCK_MAX` -> `-EINVAL` before an unknown
/// registered family's `-EAFNOSUPPORT`, and obsolete `(AF_INET, SOCK_PACKET)`
/// rerouted to `AF_PACKET` (`-EAFNOSUPPORT`).
fn parse_family(domain: i32, base_ty: u32) -> Result<Family, u32> {
    if !(0..AF_MAX).contains(&domain) {
        return Err(errno::EAFNOSUPPORT);
    }
    if base_ty >= SOCK_MAX {
        return Err(errno::EINVAL);
    }
    if domain == net::AF_INET as i32 && base_ty == SOCK_PACKET {
        return Err(errno::EAFNOSUPPORT);
    }
    Family::from_raw(domain as u16).ok_or(errno::EAFNOSUPPORT)
}

/// Validate `(family, base_ty, protocol)` against Linux v6.12 `unix_create`
/// (`net/unix/af_unix.c:1068-1092`) and `inet_create` (`net/ipv4/af_inet.c:263-289`).
fn validate_socket_spec(family: Family, base_ty: u32, protocol: i32) -> Result<SockKind, u32> {
    if family == Family::Unix {
        if protocol != 0 && protocol != net::AF_UNIX as i32 {
            return Err(errno::EPROTONOSUPPORT);
        }
        return match base_ty {
            net::SOCK_STREAM => Ok(SockKind::Stream),
            // `unix_create` maps `SOCK_RAW` to `SOCK_DGRAM` (af_unix.c:1081).
            net::SOCK_DGRAM | net::SOCK_RAW => Ok(SockKind::Dgram),
            net::SOCK_SEQPACKET => Ok(SockKind::Seqpacket),
            _ => Err(errno::ESOCKTNOSUPPORT),
        };
    }
    if !(0..IPPROTO_MAX).contains(&protocol) {
        return Err(errno::EINVAL);
    }
    match (family, base_ty) {
        (Family::Inet | Family::Inet6, net::SOCK_STREAM) => {
            if protocol != 0 && protocol != net::IPPROTO_TCP as i32 {
                return Err(errno::EPROTONOSUPPORT);
            }
            Ok(SockKind::Stream)
        }
        (Family::Inet, net::SOCK_DGRAM) => {
            if protocol != 0 && protocol != net::IPPROTO_UDP as i32 && protocol != net::IPPROTO_ICMP as i32 {
                return Err(errno::EPROTONOSUPPORT);
            }
            Ok(SockKind::Dgram)
        }
        (Family::Inet6, net::SOCK_DGRAM) => {
            if protocol != 0 && protocol != net::IPPROTO_UDP as i32 && protocol != net::IPPROTO_ICMPV6 as i32 {
                return Err(errno::EPROTONOSUPPORT);
            }
            Ok(SockKind::Dgram)
        }
        (Family::Inet, net::SOCK_RAW) => {
            if protocol != net::IPPROTO_ICMP as i32 {
                return Err(errno::EPROTONOSUPPORT);
            }
            Ok(SockKind::Raw)
        }
        (Family::Inet6, net::SOCK_RAW) => {
            if protocol != net::IPPROTO_ICMPV6 as i32 {
                return Err(errno::EPROTONOSUPPORT);
            }
            Ok(SockKind::Raw)
        }
        _ => Err(errno::ESOCKTNOSUPPORT),
    }
}

/// Copy a user `sockaddr` of `addrlen` bytes into kernel stack storage
/// (`move_addr_to_kernel`, Linux v6.12 `net/socket.c`).
fn copy_addr_from_user(addr: UserPtr, addrlen: i32) -> Result<([u8; SOCKADDR_STORAGE_LEN], usize), u32> {
    if addrlen < 0 || addrlen as usize > SOCKADDR_STORAGE_LEN {
        return Err(errno::EINVAL);
    }
    let len = addrlen as usize;
    let mut buf = [0u8; SOCKADDR_STORAGE_LEN];
    if len > 0 {
        copy_from_user(&mut buf[..len], addr.addr()).map_err(CopyError::to_errno)?;
    }
    Ok((buf, len))
}

/// Encode `sock_addr` (or empty if `None`), copy at most `cap` bytes to
/// `addr`, and return the full encoded length (`move_addr_to_user`, Linux
/// v6.12 `net/socket.c`).
fn write_name(addr: u64, cap: usize, sock_addr: Option<&SockAddr>) -> Result<i32, u32> {
    match sock_addr {
        Some(sa) => {
            let mut encoded = [0u8; SOCKADDR_UN_LEN + 1];
            let full_len = sa.encode(&mut encoded);
            let copy_len = full_len.min(cap);
            if copy_len > 0 {
                copy_to_user(addr, &encoded[..copy_len]).map_err(CopyError::to_errno)?;
            }
            Ok(full_len as i32)
        }
        None => Ok(0),
    }
}

/// Write an encoded [`SockAddr`] (or length 0 if `None`) back to
/// `(addr, addrlen_ptr)` (`move_addr_to_user`, Linux v6.12 `net/socket.c`).
fn put_name(addr: UserPtr, addrlen_ptr: UserPtr, sock_addr: Option<&SockAddr>) -> Result<(), u32> {
    // SAFETY: `i32` is 4 bytes with no padding; every bit pattern is valid.
    let ulen = unsafe { copy_pod_from_user::<i32>(addrlen_ptr.addr()) }.map_err(CopyError::to_errno)?;
    if ulen < 0 {
        return Err(errno::EINVAL);
    }
    let out_len = write_name(addr.addr(), ulen as usize, sock_addr)?;
    // SAFETY: `i32` is 4 bytes with no padding.
    unsafe { copy_pod_to_user(addrlen_ptr.addr(), &out_len) }.map_err(CopyError::to_errno)?;
    Ok(())
}

/// Decode a raw sockaddr slice for `sock` under syscall context `call`.
///
/// - For `SendTo`/`SendMsg`, non-datagram/non-raw sockets return `Ok(None)`.
/// - Plain `AF_INET` addresses on an `AF_INET6` socket are routed to
///   `SockAddr::Inet` only for UDP/datagram sockets on connect/send
///   (`udpv6_sendmsg`, `__ip6_datagram_connect`; C3 refuses routed v4 under
///   `IPV6_V6ONLY` with `-EAFNOSUPPORT` on connect and `-ENETUNREACH` on send).
/// - `AF_INET6` raw and ICMPv6 ping `connect` reject non-`AF_INET6` with
///   `-EAFNOSUPPORT` before any length check (`ip6_datagram_connect_v6_only`,
///   Linux v6.12 `net/ipv6/datagram.c:285-292`); their send side stays
///   length-first (`rawv6_sendmsg`).
/// - `AF_INET6` TCP `connect` keeps length-first `-EINVAL` for 16 bytes and
///   `-EAFNOSUPPORT` for >= 24 bytes (`tcp_v6_connect`, `net/ipv6/tcp_ipv6.c:152`).
fn decode_addr(sock: &dyn SockOps, call: Call, bytes: &[u8]) -> Result<Option<SockAddr>, u32> {
    if matches!(call, Call::SendTo | Call::SendMsg) && !matches!(sock.kind(), SockKind::Dgram | SockKind::Raw) {
        return Ok(None);
    }
    if sock.family() == Family::Inet6 {
        let is_icmpv6_ping = sock.kind() == SockKind::Dgram
            && sock.get_opt(SockOpt::Protocol) == Ok(OptVal::Int(net::IPPROTO_ICMPV6 as i32));
        let is_v6_udp = sock.kind() == SockKind::Dgram && !is_icmpv6_ping;
        if is_v6_udp
            && matches!(call, Call::Connect | Call::SendTo | Call::SendMsg)
            && bytes.len() >= 2
            && peek_family(bytes) == Ok(AF_INET)
        {
            return SockAddr::decode(Family::Inet, call, bytes).map(Some).map_err(Errno::to_errno);
        }
        if (sock.kind() == SockKind::Raw || is_icmpv6_ping)
            && call == Call::Connect
            && bytes.len() >= 2
            && peek_family(bytes) != Ok(AF_INET6)
        {
            return Err(errno::EAFNOSUPPORT);
        }
    }
    match call {
        Call::Bind | Call::Connect => SockAddr::decode(sock.family(), call, bytes).map(Some).map_err(Errno::to_errno),
        Call::SendTo | Call::SendMsg => SockAddr::decode_dest(sock.family(), call, bytes).map_err(Errno::to_errno),
    }
}

/// Validate `RawIovec` ranges into [`UserSpan`]s (`import_iovec`).
fn spans_from_raw(iovs: &[RawIovec]) -> Result<Vec<UserSpan>, u32> {
    let mut spans = Vec::new();
    spans.try_reserve_exact(iovs.len()).map_err(|_| errno::ENOMEM)?;
    for span in iovs {
        let len = span.len as usize;
        if len > 0 && !user_range_valid(span.base, len) {
            return Err(errno::EFAULT);
        }
        spans.push(UserSpan { base: span.base, len });
    }
    Ok(spans)
}

/// Copy and validate `iovlen` `struct iovec` entries for `sendmsg`/`recvmsg`
/// (`UIO_MAXIOV` cap returns `-EMSGSIZE`, Linux v6.12 `net/socket.c:2521`).
fn iovec_spans(user_iov: u64, iovlen: u64) -> Result<Vec<UserSpan>, u32> {
    if iovlen > u64::from(general::UIO_MAXIOV) {
        return Err(errno::EMSGSIZE);
    }
    let raw = copy_iovecs(user_iov, iovlen)?;
    spans_from_raw(&raw)
}

/// Validate a single-span user buffer (`import_ubuf`, Linux v6.12
/// `net/socket.c:2191, 2256`).
fn single_span(buf: UserPtr, len: u64) -> Result<UserSpan, u32> {
    let capped = len.min(MAX_RW_COUNT as u64) as usize;
    if capped > 0 && !user_range_valid(buf.addr(), capped) {
        return Err(errno::EFAULT);
    }
    Ok(UserSpan { base: buf.addr(), len: capped })
}

/// Validate `SCM_CREDENTIALS` pid: any existing process or thread passes
/// (every Lumen process has root capabilities), else `-ESRCH`.
fn check_cred_pid(pid: i32) -> Result<(), u32> {
    if pid <= 0 {
        return Err(errno::ESRCH);
    }
    let pid = pid as u32;
    if pid == process::current_process().pid()
        || process::lookup(pid).is_some()
        || process::lookup_thread(pid).is_some()
    {
        Ok(())
    } else {
        Err(errno::ESRCH)
    }
}

/// Copy and parse `sendmsg`'s control buffer into `(rights, creds)` outside
/// every socket lock.
///
/// Matches Linux v6.12 `sock_kmalloc` (`net/core/sock.c:2784`, strict
/// `< OPTMEM_MAX`), `__scm_send` (`AF_UNIX` resolves `SCM_RIGHTS` and
/// `SCM_CREDENTIALS`), and `__sock_cmsg_send` (`net/core/sock.c:2937-2939`,
/// inet skips `SCM_RIGHTS`/`SCM_CREDENTIALS`; unimplemented `SOL_SOCKET`
/// types `SO_MARK`, `SO_PRIORITY`, `SO_TIMESTAMPING_*`, `SCM_TXTIME` return
/// `-EINVAL`).
fn parse_send_cmsgs(
    family: Family,
    msg_control: u64,
    msg_controllen: u64,
) -> Result<(Vec<InFlightFile>, Option<lumen_sockabi::Ucred>), u32> {
    if msg_controllen == 0 {
        return Ok((Vec::new(), None));
    }
    if msg_controllen >= OPTMEM_MAX as u64 {
        return Err(errno::ENOBUFS);
    }
    let ctl_len = msg_controllen as usize;
    let mut ctl_buf = Vec::new();
    ctl_buf.try_reserve_exact(ctl_len).map_err(|_| errno::ENOMEM)?;
    ctl_buf.resize(ctl_len, 0);
    copy_from_user(&mut ctl_buf, msg_control).map_err(CopyError::to_errno)?;

    let mut rights = Vec::new();
    let mut creds = None;
    for item in CmsgIter::new(&ctl_buf) {
        let cmsg = item.map_err(Errno::to_errno)?;
        if cmsg.level != SOL_SOCKET {
            continue;
        }
        if family != Family::Unix {
            match cmsg.cmsg_type {
                SCM_RIGHTS | SCM_CREDENTIALS => continue,
                _ => return Err(errno::EINVAL),
            }
        }
        match cmsg.cmsg_type {
            SCM_RIGHTS => {
                let fds = cmsg.rights_fds().map_err(Errno::to_errno)?;
                if rights.len() + fds.len() > SCM_MAX_FD {
                    return Err(errno::EINVAL);
                }
                for raw_fd in fds {
                    rights.push(InFlightFile::from_fd(raw_fd)?);
                }
            }
            SCM_CREDENTIALS => {
                let ucred = cmsg.decode_creds().map_err(Errno::to_errno)?;
                check_cred_pid(ucred.pid)?;
                creds = Some(ucred);
            }
            _ => return Err(errno::EINVAL),
        }
    }
    Ok((rights, creds))
}

/// Shared receive path for `read`/`readv` on a socket (`sock_read_iter`,
/// Linux v6.12 `net/socket.c:1126`): zero total length returns 0 without
/// calling `SockOps::recv`.
fn recv_spans(sock: &dyn SockOps, spans: &[UserSpan], nonblock: bool) -> Result<u64, u32> {
    let iov = UserIovec::new(spans);
    if iov.is_empty() {
        return Ok(0);
    }
    let mut io = RecvIo { iov, flags: 0, nonblock };
    let done = sock.recv(&mut io)?;
    Ok(done.bytes as u64)
}

/// Shared send path for `write`/`writev` on a socket (`sock_write_iter`,
/// Linux v6.12 `net/socket.c:1148`): sets `MSG_EOR` on `SOCK_SEQPACKET`.
fn send_spans(sock: &dyn SockOps, spans: &[UserSpan], nonblock: bool) -> Result<u64, u32> {
    let flags = if sock.kind() == SockKind::Seqpacket { MSG_EOR } else { 0 };
    let mut io =
        SendIo { iov: UserIovec::new(spans), to: None, named: false, rights: Vec::new(), creds: None, flags, nonblock };
    sock.send(&mut io).map(|n| n as u64)
}

/// `read(2)` on a socket (`sock_read_iter`, Linux v6.12 `net/socket.c:1126`).
pub(super) fn read_sock(sock: &dyn SockOps, buf: UserPtr, count: u64, nonblock: bool) -> Result<u64, u32> {
    let span = [single_span(buf, count)?];
    recv_spans(sock, &span, nonblock)
}

/// `write(2)` on a socket (`sock_write_iter`, Linux v6.12 `net/socket.c:1148`).
pub(super) fn write_sock(sock: &dyn SockOps, buf: UserPtr, count: u64, nonblock: bool) -> Result<u64, u32> {
    let span = [single_span(buf, count)?];
    send_spans(sock, &span, nonblock)
}

/// `readv(2)` on a socket (`sock_read_iter`, Linux v6.12 `net/socket.c:1126`).
pub(super) fn readv_sock(sock: &dyn SockOps, iovs: &[RawIovec], nonblock: bool) -> Result<u64, u32> {
    let spans = spans_from_raw(iovs)?;
    recv_spans(sock, &spans, nonblock)
}

/// `writev(2)` on a socket (`sock_write_iter`, Linux v6.12 `net/socket.c:1148`).
pub(super) fn writev_sock(sock: &dyn SockOps, iovs: &[RawIovec], nonblock: bool) -> Result<u64, u32> {
    let spans = spans_from_raw(iovs)?;
    send_spans(sock, &spans, nonblock)
}

/// `socket(domain, type, protocol)` (41): dispatch by family (`AF_UNIX`,
/// `AF_INET`, `AF_INET6`), validate flags/family/type/protocol in Linux v6.12
/// order, reserve a descriptor slot first, and install the `sockfs` file.
pub(super) fn sys_socket(domain: i32, ty: i32, protocol: i32) -> u64 {
    finish(do_socket(domain, ty, protocol))
}

fn do_socket(domain: i32, ty: i32, protocol: i32) -> Result<u64, u32> {
    let (base_ty, nb, cloexec) = parse_type_flags(ty)?;
    let family = parse_family(domain, base_ty)?;
    let kind = validate_socket_spec(family, base_ty, protocol)?;
    let slot = process::reserve_current_fd().map_err(vfs)?;
    let ops = create_socket(family, kind, protocol)?;
    let file = new_socket_file(ops, nb)?;
    Ok(slot.install(file, cloexec) as u64)
}

/// `connect(fd, addr, addrlen)` (42): check fd (`-EBADF`), copy address
/// (`-EINVAL`/`-EFAULT`), verify socket (`-ENOTSOCK`), handle `AF_UNSPEC` via
/// `SockOps::disconnect`, else decode and dispatch to `SockOps::connect`.
pub(super) fn sys_connect(fd: Fd, addr: UserPtr, addrlen: i32) -> u64 {
    finish(do_connect(fd, addr, addrlen))
}

fn do_connect(fd: Fd, addr: UserPtr, addrlen: i32) -> Result<u64, u32> {
    // Linux v6.12 `net/socket.c:2082-2088`: `fdget`, then `move_addr_to_kernel`,
    // then `sock_from_file` (`-ENOTSOCK`).
    let file = open_non_path(fd)?;
    let (raw, len) = copy_addr_from_user(addr, addrlen)?;
    let sock = Arc::clone(file.sock().ok_or(errno::ENOTSOCK)?);
    let bytes = &raw[..len];
    if bytes.len() >= 2 && peek_family(bytes) == Ok(AF_UNSPEC) {
        sock.disconnect()?;
        return Ok(0);
    }
    let decoded = decode_addr(&*sock, Call::Connect, bytes)?.ok_or(errno::EINVAL)?;
    sock.connect(&decoded, file.is_nonblock())?;
    Ok(0)
}

/// `accept(fd, addr, addrlen)` (43): `accept4(fd, addr, addrlen, 0)`.
pub(super) fn sys_accept(fd: Fd, addr: UserPtr, addrlen: UserPtr) -> u64 {
    finish(do_accept4(fd, addr, addrlen, 0))
}

/// `accept4(fd, addr, addrlen, flags)` (288): Linux v6.12 order (`-EBADF`,
/// flags `-EINVAL`, slot reservation `-EMFILE`, `-ENOTSOCK`), then accept,
/// copy peer address out, and install with `SOCK_NONBLOCK`/`SOCK_CLOEXEC`.
pub(super) fn sys_accept4(fd: Fd, addr: UserPtr, addrlen: UserPtr, flags: i32) -> u64 {
    finish(do_accept4(fd, addr, addrlen, flags))
}

fn do_accept4(fd: Fd, addr: UserPtr, addrlen: UserPtr, flags: i32) -> Result<u64, u32> {
    // Linux v6.12 `net/socket.c:2020 -> 1982 -> 1988 -> 1925`:
    // `fdget` (`-EBADF`), flags (`-EINVAL`), `get_unused_fd_flags` (`-EMFILE`),
    // `sock_from_file` (`-ENOTSOCK`).
    let file = open_non_path(fd)?;
    let raw_flags = flags as u32;
    if raw_flags & !(SOCK_NONBLOCK | SOCK_CLOEXEC) != 0 {
        return Err(errno::EINVAL);
    }
    let slot = process::reserve_current_fd().map_err(vfs)?;
    let sock = Arc::clone(file.sock().ok_or(errno::ENOTSOCK)?);
    let (peer_ops, peer_addr) = sock.accept(file.is_nonblock())?;
    let peer_file = new_socket_file(peer_ops, raw_flags & SOCK_NONBLOCK != 0)?;
    if !addr.is_null() {
        put_name(addr, addrlen, peer_addr.as_ref())?;
    }
    Ok(slot.install(peer_file, raw_flags & SOCK_CLOEXEC != 0) as u64)
}

/// `sendto(fd, buf, len, flags, addr, addrlen)` (44): `import_ubuf` before fd
/// lookup (`net/socket.c:2191`), always copies and length-checks a non-null
/// destination name (`named = addrlen != 0`), decodes it for datagram/raw
/// sockets only, and dispatches to `SockOps::send`.
pub(super) fn sys_sendto(fd: Fd, buf: UserPtr, len: u64, flags: u32, addr: UserPtr, addrlen: i32) -> u64 {
    finish(do_sendto(fd, buf, len, flags, addr, addrlen))
}

fn do_sendto(fd: Fd, buf: UserPtr, len: u64, flags: u32, addr: UserPtr, addrlen: i32) -> Result<u64, u32> {
    let span = [single_span(buf, len)?];
    let (file, sock) = socket_file(fd)?;
    let raw_addr = if addr.is_null() { None } else { Some(copy_addr_from_user(addr, addrlen)?) };
    if flags & MSG_OOB != 0 {
        return Err(errno::EOPNOTSUPP);
    }
    let (named, to) = match raw_addr {
        Some((raw, raw_len)) if raw_len > 0 => (true, decode_addr(&*sock, Call::SendTo, &raw[..raw_len])?),
        _ => (false, None),
    };
    let mut io = SendIo {
        iov: UserIovec::new(&span),
        to,
        named,
        rights: Vec::new(),
        creds: None,
        flags,
        nonblock: nonblock(&file, flags),
    };
    sock.send(&mut io).map(|n| n as u64)
}

/// `recvfrom(fd, buf, len, flags, addr, addrlen)` (45): `import_ubuf` before
/// fd lookup (`net/socket.c:2256`), single-span receive with optional peer
/// address copy-out.
pub(super) fn sys_recvfrom(fd: Fd, buf: UserPtr, len: u64, flags: u32, addr: UserPtr, addrlen: UserPtr) -> u64 {
    finish(do_recvfrom(fd, buf, len, flags, addr, addrlen))
}

fn do_recvfrom(fd: Fd, buf: UserPtr, len: u64, flags: u32, addr: UserPtr, addrlen: UserPtr) -> Result<u64, u32> {
    let span = [single_span(buf, len)?];
    let (file, sock) = socket_file(fd)?;
    check_recv_flags(flags)?;
    let mut io = RecvIo { iov: UserIovec::new(&span), flags, nonblock: nonblock(&file, flags) };
    let done = sock.recv(&mut io)?;
    if !addr.is_null() {
        put_name(addr, addrlen, done.from.as_ref())?;
    }
    Ok(done.bytes as u64)
}

/// `sendmsg(fd, msg, flags)` (46): reject `MSG_CMSG_COMPAT` first
/// (`net/socket.c:2683`), copy in `msghdr`, destination name, `iovec`s
/// (`__copy_msghdr` order) and `cmsg` control buffer (`SCM_RIGHTS` resolved
/// to [`InFlightFile`]s for `AF_UNIX` before any socket lock), then dispatch
/// to `SockOps::send`.
pub(super) fn sys_sendmsg(fd: Fd, msg: UserPtr, flags: u32) -> u64 {
    finish(do_sendmsg(fd, msg, flags))
}

fn do_sendmsg(fd: Fd, msg: UserPtr, flags: u32) -> Result<u64, u32> {
    if flags & MSG_CMSG_COMPAT != 0 {
        return Err(errno::EINVAL);
    }
    let (file, sock) = socket_file(fd)?;
    let mut raw_hdr = [0u8; MSGHDR_LEN];
    copy_from_user(&mut raw_hdr, msg.addr()).map_err(CopyError::to_errno)?;
    let hdr = Msghdr::from_bytes(&raw_hdr);

    // `__copy_msghdr` order (Linux v6.12 `net/socket.c:2495-2546`):
    // validate/copy `msg_name` before `msg_iovlen` (`-EMSGSIZE`) and `iovec`s.
    let namelen = if hdr.msg_name == 0 { 0 } else { clamp_msg_namelen(hdr.msg_namelen).map_err(Errno::to_errno)? };
    let mut raw_name = [0u8; SOCKADDR_STORAGE_LEN];
    let named = if hdr.msg_name != 0 && namelen > 0 {
        copy_from_user(&mut raw_name[..namelen], hdr.msg_name).map_err(CopyError::to_errno)?;
        true
    } else {
        false
    };
    let spans = iovec_spans(hdr.msg_iov, hdr.msg_iovlen)?;
    let (rights, creds) = parse_send_cmsgs(sock.family(), hdr.msg_control, hdr.msg_controllen)?;
    if flags & MSG_OOB != 0 {
        return Err(errno::EOPNOTSUPP);
    }
    let to = if named { decode_addr(&*sock, Call::SendMsg, &raw_name[..namelen])? } else { None };
    let mut io =
        SendIo { iov: UserIovec::new(&spans), to, named, rights, creds, flags, nonblock: nonblock(&file, flags) };
    let sent = sock.send(&mut io)?;
    // Any unconsumed `io.rights` drop here, outside every socket lock.
    drop(io);
    Ok(sent as u64)
}

/// `recvmsg(fd, msg, flags)` (47): reject `MSG_CMSG_COMPAT` first
/// (`net/socket.c:2891`), copy in `msghdr` and `iovec`s (`__copy_msghdr`
/// order), receive via `SockOps::recv`, install `SCM_RIGHTS` outside every
/// socket lock (`-EMFILE` installs what fits and sets `MSG_CTRUNC`), encode
/// ancillary data with `CmsgWriter`, and write back `msghdr`.
pub(super) fn sys_recvmsg(fd: Fd, msg: UserPtr, flags: u32) -> u64 {
    finish(do_recvmsg(fd, msg, flags))
}

fn do_recvmsg(fd: Fd, msg: UserPtr, flags: u32) -> Result<u64, u32> {
    if flags & MSG_CMSG_COMPAT != 0 {
        return Err(errno::EINVAL);
    }
    let (file, sock) = socket_file(fd)?;
    let mut raw_hdr = [0u8; MSGHDR_LEN];
    copy_from_user(&mut raw_hdr, msg.addr()).map_err(CopyError::to_errno)?;
    let hdr = Msghdr::from_bytes(&raw_hdr);

    // `__copy_msghdr` order (Linux v6.12 `net/socket.c:2495-2546`):
    // validate `msg_namelen`, then `msg_iovlen`/`iovec`s, then receive flags.
    let name_cap = if hdr.msg_name == 0 { 0 } else { clamp_msg_namelen(hdr.msg_namelen).map_err(Errno::to_errno)? };
    let spans = iovec_spans(hdr.msg_iov, hdr.msg_iovlen)?;
    check_recv_flags(flags)?;
    let ctl_cap = if hdr.msg_control == 0 { 0 } else { usize::try_from(hdr.msg_controllen).unwrap_or(usize::MAX) };

    let mut io = RecvIo { iov: UserIovec::new(&spans), flags, nonblock: nonblock(&file, flags) };
    let mut done = sock.recv(&mut io)?;
    finish_recvmsg(msg, &hdr, name_cap, ctl_cap, flags, &mut done)?;
    Ok(done.bytes as u64)
}

/// Write a POD field at byte offset `off` inside user `struct msghdr` at `msg_ptr`.
///
/// # Safety
/// `T` must be a plain-old-data integer type with no padding bytes.
unsafe fn put_hdr_field<T: Copy>(msg_ptr: UserPtr, off: usize, val: &T) -> Result<(), u32> {
    let addr = msg_ptr.addr().checked_add(off as u64).ok_or(errno::EFAULT)?;
    // SAFETY: Caller guarantees `T` is POD with no padding bytes.
    unsafe { copy_pod_to_user(addr, val) }.map_err(CopyError::to_errno)
}

/// Install received rights, format ancillary data with [`CmsgWriter`], and
/// write back `msg_name`, `msg_control`, `msg_namelen`, `msg_controllen`, and
/// `msg_flags` (Linux v6.12 `net/socket.c:2800-2854`, `net/core/scm.c:231-369`).
/// Runs outside every socket lock.
fn finish_recvmsg(
    msg_ptr: UserPtr,
    hdr: &Msghdr,
    name_cap: usize,
    ctl_cap: usize,
    flags: u32,
    done: &mut RecvDone,
) -> Result<(), u32> {
    let creds_space = if done.creds.is_some() { cmsg_space(UCRED_LEN).unwrap_or(0) } else { 0 };
    let rights_space = if done.rights.is_empty() { 0 } else { cmsg_space(done.rights.len() * 4).unwrap_or(ctl_cap) };
    let needed = creds_space.saturating_add(rights_space);
    let buf_len = ctl_cap.min(needed);
    let mut ctl_buf = vec![0u8; buf_len];

    let mut writer = CmsgWriter::new(&mut ctl_buf);
    if let Some(creds) = done.creds {
        let _ = writer.push_creds(creds);
    }
    let total_rights = done.rights.len();
    let fdmax = writer.rights_capacity().min(total_rights);
    let mut reserved = Vec::with_capacity(fdmax);
    for _ in 0..fdmax {
        match process::reserve_current_fd() {
            Ok(slot) => reserved.push(slot),
            Err(_) => break,
        }
    }
    let fds: Vec<i32> = reserved.iter().map(ReservedFd::fd).collect();
    let written_fds = writer.push_rights(&fds, total_rights);
    debug_assert_eq!(written_fds, reserved.len());
    let (mut written_ctl_len, mut ctl_flags) = writer.finish();

    // Control-buffer copy-out faults do not fail `recvmsg`: `scm_detach_fds`
    // (Linux v6.12 `net/core/scm.c:361`) sets `MSG_CTRUNC` only when pending
    // rights could not be delivered, while `put_cmsg` for credentials alone
    // leaves `MSG_CTRUNC` clear (`net/core/scm.c:281`). Drop all reserved
    // slots and report 0 control bytes.
    if written_ctl_len > 0 && copy_to_user(hdr.msg_control, &ctl_buf[..written_ctl_len]).is_err() {
        if total_rights > 0 {
            ctl_flags |= MSG_CTRUNC;
        }
        written_ctl_len = 0;
        reserved.clear();
    }

    // A bad `msg_name` pointer still returns `-EFAULT` (`net/socket.c:2832-2838`).
    if hdr.msg_name != 0 {
        let namelen = write_name(hdr.msg_name, name_cap, done.from.as_ref())?;
        // SAFETY: `i32` is 4 bytes with no padding.
        unsafe { put_hdr_field(msg_ptr, offset_of!(Msghdr, msg_namelen), &namelen) }?;
    }
    let ctl_len_out = written_ctl_len as u64;
    // SAFETY: `u64` is 8 bytes with no padding.
    unsafe { put_hdr_field(msg_ptr, offset_of!(Msghdr, msg_controllen), &ctl_len_out) }?;

    // Echo `MSG_CMSG_CLOEXEC` into `msg_flags` (Linux v6.12 `net/socket.c:2815, 2839`).
    let out_flags = done.msg_flags | ctl_flags | (flags & MSG_CMSG_CLOEXEC);
    // SAFETY: `u32` is 4 bytes with no padding.
    unsafe { put_hdr_field(msg_ptr, offset_of!(Msghdr, msg_flags), &out_flags) }?;

    // Commit the reserved slots; any remaining uninstalled rights drop when
    // `done` drops, outside every lock.
    let cloexec = flags & MSG_CMSG_CLOEXEC != 0;
    let take = reserved.len();
    for (slot, inflight) in reserved.into_iter().zip(done.rights.drain(..take)) {
        let _ = slot.install_inflight(inflight, cloexec);
    }
    Ok(())
}

/// `shutdown(fd, how)` (48): validate `how` (`SHUT_RD`..=`SHUT_RDWR`) and
/// dispatch to `SockOps::shutdown`.
pub(super) fn sys_shutdown(fd: Fd, how: i32) -> u64 {
    finish(do_shutdown(fd, how))
}

fn do_shutdown(fd: Fd, how: i32) -> Result<u64, u32> {
    let (_file, sock) = socket_file(fd)?;
    let how = ShutdownHow::from_raw(how).ok_or(errno::EINVAL)?;
    sock.shutdown(how)?;
    Ok(0)
}

/// `bind(fd, addr, addrlen)` (49): decode address with `Call::Bind` and
/// dispatch to `SockOps::bind`.
pub(super) fn sys_bind(fd: Fd, addr: UserPtr, addrlen: i32) -> u64 {
    finish(do_bind(fd, addr, addrlen))
}

fn do_bind(fd: Fd, addr: UserPtr, addrlen: i32) -> Result<u64, u32> {
    let (_file, sock) = socket_file(fd)?;
    let (raw, len) = copy_addr_from_user(addr, addrlen)?;
    let decoded = decode_addr(&*sock, Call::Bind, &raw[..len])?.ok_or(errno::EINVAL)?;
    sock.bind(&decoded)?;
    Ok(0)
}

/// `listen(fd, backlog)` (50): stream and seqpacket sockets only
/// (`-EOPNOTSUPP` for datagram/raw), clamp backlog as `__sys_listen_socket`
/// (`SOMAXCONN` = 4096), and dispatch to `SockOps::listen`.
pub(super) fn sys_listen(fd: Fd, backlog: i32) -> u64 {
    finish(do_listen(fd, backlog))
}

fn do_listen(fd: Fd, backlog: i32) -> Result<u64, u32> {
    let (_file, sock) = socket_file(fd)?;
    if !matches!(sock.kind(), SockKind::Stream | SockKind::Seqpacket) {
        return Err(errno::EOPNOTSUPP);
    }
    let backlog = if (backlog as u32) > (SOMAXCONN as u32) { SOMAXCONN } else { backlog };
    sock.listen(backlog)?;
    Ok(0)
}

/// `getsockname(fd, addr, addrlen)` (51): encode `SockOps::local_name` and
/// copy out truncated to `*addrlen`.
pub(super) fn sys_getsockname(fd: Fd, addr: UserPtr, addrlen: UserPtr) -> u64 {
    finish(do_getsockname(fd, addr, addrlen))
}

fn do_getsockname(fd: Fd, addr: UserPtr, addrlen: UserPtr) -> Result<u64, u32> {
    let (_file, sock) = socket_file(fd)?;
    let name = sock.local_name()?;
    put_name(addr, addrlen, Some(&name))?;
    Ok(0)
}

/// `getpeername(fd, addr, addrlen)` (52): encode `SockOps::peer_name` and copy
/// out truncated to `*addrlen`.
pub(super) fn sys_getpeername(fd: Fd, addr: UserPtr, addrlen: UserPtr) -> u64 {
    finish(do_getpeername(fd, addr, addrlen))
}

fn do_getpeername(fd: Fd, addr: UserPtr, addrlen: UserPtr) -> Result<u64, u32> {
    let (_file, sock) = socket_file(fd)?;
    let name = sock.peer_name()?;
    put_name(addr, addrlen, Some(&name))?;
    Ok(0)
}

/// `socketpair(domain, type, protocol, sv)` (53): Linux v6.12 order
/// (`net/socket.c:1744-1801`): flags (`-EINVAL`), reserve both slots
/// (`-EMFILE`), copy `[sv0, sv1]` (`-EFAULT`), validate family/type/protocol
/// (`-ESOCKTNOSUPPORT`/`-EPROTONOSUPPORT` before inet's `-EOPNOTSUPP`), create
/// pair, and install both slots.
pub(super) fn sys_socketpair(domain: i32, ty: i32, protocol: i32, sv: UserPtr) -> u64 {
    finish(do_socketpair(domain, ty, protocol, sv))
}

fn do_socketpair(domain: i32, ty: i32, protocol: i32, sv: UserPtr) -> Result<u64, u32> {
    let (base_ty, nb, cloexec) = parse_type_flags(ty)?;
    let slot_a = process::reserve_current_fd().map_err(vfs)?;
    let slot_b = process::reserve_current_fd().map_err(vfs)?;
    let pair = [slot_a.fd(), slot_b.fd()];
    // SAFETY: `[i32; 2]` is 8 bytes with no padding.
    unsafe { copy_pod_to_user(sv.addr(), &pair) }.map_err(CopyError::to_errno)?;
    let family = parse_family(domain, base_ty)?;
    let kind = validate_socket_spec(family, base_ty, protocol)?;
    if family != Family::Unix {
        return Err(errno::EOPNOTSUPP);
    }
    let (ops_a, ops_b) = create_socketpair(family, kind, protocol)?;
    let file_a = match new_socket_file(ops_a, nb) {
        Ok(f) => f,
        Err(e) => {
            ops_b.release();
            return Err(e);
        }
    };
    let file_b = new_socket_file(ops_b, nb)?;
    let _ = slot_a.install(file_a, cloexec);
    let _ = slot_b.install(file_b, cloexec);
    Ok(0)
}

/// `setsockopt(fd, level, name, val, len)` (54): decode via `decode_opt` and
/// dispatch to `SockOps::set_opt`.
pub(super) fn sys_setsockopt(fd: Fd, level: i32, name: i32, val: UserPtr, len: i32) -> u64 {
    finish(do_setsockopt(fd, level, name, val, len))
}

fn do_setsockopt(fd: Fd, level: i32, name: i32, val: UserPtr, len: i32) -> Result<u64, u32> {
    let (_file, sock) = socket_file(fd)?;
    if len < 0 {
        return Err(errno::EINVAL);
    }
    if sock.family() == Family::Unix && level != SOL_SOCKET {
        return Err(errno::EOPNOTSUPP);
    }
    let len = len as usize;
    let mut buf = [0u8; MAX_OPT_LEN];
    let slice_len = match SockOpt::from_raw(level, name) {
        Ok(opt) if opt.settable() => {
            let need = opt.kind().size();
            let copy_len = len.min(need);
            // Copy from user only if the length meets the option's minimum
            // (or for `IntOrByte`, which accepts 1..4 bytes); shorter buffers
            // return `EINVAL` in `decode_opt` without touching `val`.
            let should_copy = match opt.kind() {
                lumen_sockabi::OptKind::IntOrByte => copy_len > 0,
                _ => len >= need,
            };
            if should_copy {
                copy_from_user(&mut buf[..copy_len], val.addr()).map_err(CopyError::to_errno)?;
            }
            copy_len
        }
        _ => len.min(MAX_OPT_LEN),
    };
    let (opt, optval) = decode_opt(level, name, &buf[..slice_len]).map_err(Errno::to_errno)?;
    sock.set_opt(opt, &optval)?;
    Ok(0)
}

/// `getsockopt(fd, level, name, val, len)` (55): query `SockOps::get_opt` and
/// copy out truncated via `write_opt_for`.
pub(super) fn sys_getsockopt(fd: Fd, level: i32, name: i32, val: UserPtr, len: UserPtr) -> u64 {
    finish(do_getsockopt(fd, level, name, val, len))
}

fn do_getsockopt(fd: Fd, level: i32, name: i32, val: UserPtr, len: UserPtr) -> Result<u64, u32> {
    let (_file, sock) = socket_file(fd)?;
    if sock.family() == Family::Unix && level != SOL_SOCKET {
        return Err(errno::EOPNOTSUPP);
    }
    // SAFETY: `i32` is 4 bytes with no padding; every bit pattern is valid.
    let raw_len = unsafe { copy_pod_from_user::<i32>(len.addr()) }.map_err(CopyError::to_errno)?;
    let optlen = check_optlen(raw_len).map_err(Errno::to_errno)?;
    let opt = SockOpt::from_raw(level, name).map_err(Errno::to_errno)?;
    let value = sock.get_opt(opt)?;
    let (buf, written) = write_opt_for(opt, value, optlen);
    if written > 0 {
        copy_to_user(val.addr(), &buf[..written]).map_err(CopyError::to_errno)?;
    }
    let out_len = written as i32;
    // SAFETY: `i32` is 4 bytes with no padding.
    unsafe { copy_pod_to_user(len.addr(), &out_len) }.map_err(CopyError::to_errno)?;
    Ok(0)
}
