//! Generic socket syscalls (M9 B1, #472; ARCHITECTURE section 8).
//!
//! Every handler here is generic over [`SockOps`]: address decoding and
//! encoding (`lumen-sockabi`), `msghdr` and `iovec` validation, `CmsgIter` /
//! `CmsgWriter` ancillary data (`SCM_RIGHTS` via [`InFlightFile`],
//! `SCM_CREDENTIALS`), option decoding (`decode_opt` / `write_opt_for`), and
//! descriptor slot reservation before each call with installation after.
//! Families (`AF_UNIX` in C1, UDP in C3, TCP in C4, ICMP in C6) only implement
//! [`SockOps`] and plug into [`crate::net::sock::create_socket`] /
//! [`crate::net::sock::create_socketpair`].

use alloc::vec;
use alloc::vec::Vec;
use core::mem::{offset_of, size_of};

use lumen_sockabi::abi::{Msghdr, SOCKADDR_UN_LEN};
use lumen_sockabi::{
    AF_INET, AF_UNSPEC, CMSG_HDRLEN, Call, CmsgIter, CmsgWriter, Errno, Family, MAX_OPT_LEN, MSG_CMSG_CLOEXEC,
    MSG_DONTWAIT, MSG_ERRQUEUE, MSG_OOB, MSGHDR_LEN, SCM_CREDENTIALS, SCM_MAX_FD, SCM_RIGHTS, SOCKADDR_STORAGE_LEN,
    SOL_SOCKET, SockAddr, SockOpt, UCRED_LEN, check_optlen, clamp_msg_namelen, cmsg_space, decode_opt, peek_family,
    write_opt_for,
};

use super::at::{Fd, UserPtr};
use super::fd::vfs;
use super::{RawIovec, copy_pod_from_user, copy_pod_to_user, finish};
use crate::linux::process::{self, IoFile, ReservedFd};
use crate::linux::{errno, general, net};
use crate::net::sock::{
    InFlightFile, RecvDone, RecvIo, SendIo, ShutdownHow, SockKind, SockOps, UserIovec, UserSpan, create_socket,
    create_socketpair, new_socket_file,
};
use crate::user::{CopyError, copy_from_user, copy_to_user, user_range_valid};

/// Low 4 bits of `socket`/`socketpair`'s `type` argument (`SOCK_TYPE_MASK`).
const SOCK_TYPE_MASK: u32 = 0xf;

/// Upper bound of valid Linux `enum sock_type` values (`SOCK_MAX`).
const SOCK_MAX: u32 = 11;

/// `SOCK_NONBLOCK` on Linux x86-64 (identical to `O_NONBLOCK`).
const SOCK_NONBLOCK: u32 = general::O_NONBLOCK;

/// `SOCK_CLOEXEC` on Linux x86-64 (identical to `O_CLOEXEC`).
const SOCK_CLOEXEC: u32 = general::O_CLOEXEC;

/// Maximum control buffer size accepted by `sendmsg` (Linux's `optmem_max`
/// since 6.8; ARCHITECTURE section 8).
const OPTMEM_MAX: usize = 131_072;

/// Maximum single read/write transfer size (`MAX_RW_COUNT`, `0x7fff_f000`).
const MAX_RW_COUNT: usize = 0x7fff_f000;

/// Look up an open, non-`O_PATH` socket descriptor (`-EBADF` if closed or
/// `O_PATH`, `-ENOTSOCK` if not a socket).
fn socket_file(fd: Fd) -> Result<IoFile, u32> {
    let file = fd.file()?;
    file.require_non_path().map_err(vfs)?;
    if file.sock().is_none() {
        return Err(errno::ENOTSOCK);
    }
    Ok(file)
}

/// Validate `type` creation flags (`SOCK_NONBLOCK | SOCK_CLOEXEC`), returning
/// `(base_type, nonblock, cloexec)`.
fn parse_type_flags(ty: i32) -> Result<(u32, bool, bool), u32> {
    let raw = ty as u32;
    let flags = raw & !SOCK_TYPE_MASK;
    if flags & !(SOCK_NONBLOCK | SOCK_CLOEXEC) != 0 {
        return Err(errno::EINVAL);
    }
    Ok((raw & SOCK_TYPE_MASK, flags & SOCK_NONBLOCK != 0, flags & SOCK_CLOEXEC != 0))
}

/// Validate `(family, base_type, protocol)` against ARCHITECTURE section 8's
/// `socket(2)` rules, returning the [`SockKind`] to create.
fn validate_socket_spec(family: Family, base_ty: u32, protocol: i32) -> Result<SockKind, u32> {
    if base_ty >= SOCK_MAX {
        return Err(errno::EINVAL);
    }
    match (family, base_ty) {
        (Family::Unix, net::SOCK_STREAM) => {
            if protocol != 0 {
                return Err(errno::EPROTONOSUPPORT);
            }
            Ok(SockKind::Stream)
        }
        // `unix_create` maps `SOCK_RAW` to `SOCK_DGRAM` (ARCHITECTURE section 8).
        (Family::Unix, net::SOCK_DGRAM | net::SOCK_RAW) => {
            if protocol != 0 {
                return Err(errno::EPROTONOSUPPORT);
            }
            Ok(SockKind::Dgram)
        }
        (Family::Unix, net::SOCK_SEQPACKET) => {
            if protocol != 0 {
                return Err(errno::EPROTONOSUPPORT);
            }
            Ok(SockKind::Seqpacket)
        }
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
/// (`move_addr_to_kernel`).
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

/// Write an optional encoded [`SockAddr`] back to `(addr, addrlen_ptr)`
/// (`move_addr_to_user`). Does nothing if `addr` is NULL.
fn copy_opt_addr_to_user(addr: UserPtr, addrlen_ptr: UserPtr, sock_addr: Option<&SockAddr>) -> Result<(), u32> {
    if addr.is_null() {
        return Ok(());
    }
    // SAFETY: `i32` is 4 bytes with no padding; every bit pattern is valid.
    let ulen = unsafe { copy_pod_from_user::<i32>(addrlen_ptr.addr()) }.map_err(CopyError::to_errno)?;
    if ulen < 0 {
        return Err(errno::EINVAL);
    }
    let out_len = match sock_addr {
        Some(sa) => {
            let mut encoded = [0u8; SOCKADDR_UN_LEN + 1];
            let full_len = sa.encode(&mut encoded);
            let copy_len = full_len.min(ulen as usize);
            if copy_len > 0 {
                copy_to_user(addr.addr(), &encoded[..copy_len]).map_err(CopyError::to_errno)?;
            }
            full_len as i32
        }
        None => 0i32,
    };
    // SAFETY: `i32` is 4 bytes with no padding.
    unsafe { copy_pod_to_user(addrlen_ptr.addr(), &out_len) }.map_err(CopyError::to_errno)?;
    Ok(())
}

/// Write a required encoded [`SockAddr`] back to `(addr, addrlen_ptr)`
/// (`getsockname` / `getpeername`).
fn copy_addr_to_user(addr: UserPtr, addrlen_ptr: UserPtr, sock_addr: &SockAddr) -> Result<(), u32> {
    // SAFETY: `i32` is 4 bytes with no padding; every bit pattern is valid.
    let ulen = unsafe { copy_pod_from_user::<i32>(addrlen_ptr.addr()) }.map_err(CopyError::to_errno)?;
    if ulen < 0 {
        return Err(errno::EINVAL);
    }
    let mut encoded = [0u8; SOCKADDR_UN_LEN + 1];
    let full_len = sock_addr.encode(&mut encoded);
    let copy_len = full_len.min(ulen as usize);
    if copy_len > 0 {
        copy_to_user(addr.addr(), &encoded[..copy_len]).map_err(CopyError::to_errno)?;
    }
    let out_len = full_len as i32;
    // SAFETY: `i32` is 4 bytes with no padding.
    unsafe { copy_pod_to_user(addrlen_ptr.addr(), &out_len) }.map_err(CopyError::to_errno)?;
    Ok(())
}

/// Decode an optional datagram destination address, routing `AF_INET` on a
/// dual-stack `AF_INET6` socket to `SockAddr::Inet`.
fn decode_dest_addr(family: Family, call: Call, bytes: &[u8]) -> Result<Option<SockAddr>, u32> {
    if family == Family::Inet6 && bytes.len() >= 2 && peek_family(bytes) == Ok(AF_INET) {
        return SockAddr::decode(Family::Inet, call, bytes).map(Some).map_err(Errno::to_errno);
    }
    SockAddr::decode_dest(family, call, bytes).map_err(Errno::to_errno)
}

/// Copy and validate `iovlen` `struct iovec` entries for `sendmsg`/`recvmsg`
/// (`UIO_MAXIOV` cap returns `-EMSGSIZE`).
fn copy_msg_iovecs(user_iov: u64, iovlen: u64) -> Result<Vec<UserSpan>, u32> {
    if iovlen > u64::from(general::UIO_MAXIOV) {
        return Err(errno::EMSGSIZE);
    }
    let cnt = iovlen as usize;
    if cnt == 0 {
        return Ok(Vec::new());
    }
    let byte_len = cnt * size_of::<RawIovec>();
    let mut raw = Vec::new();
    raw.try_reserve_exact(byte_len).map_err(|_| errno::ENOMEM)?;
    raw.resize(byte_len, 0);
    copy_from_user(&mut raw, user_iov).map_err(CopyError::to_errno)?;

    let mut spans = Vec::new();
    spans.try_reserve_exact(cnt).map_err(|_| errno::ENOMEM)?;
    let mut total_len = 0usize;
    for chunk in raw.as_chunks::<16>().0 {
        let base = u64::from_le_bytes(chunk[0..8].try_into().expect("8 bytes"));
        let len = u64::from_le_bytes(chunk[8..16].try_into().expect("8 bytes"));
        if (len as isize) < 0 {
            return Err(errno::EINVAL);
        }
        let len = len as usize;
        total_len = total_len.checked_add(len).filter(|&t| t <= isize::MAX as usize).ok_or(errno::EINVAL)?;
        if len > 0 && !user_range_valid(base, len) {
            return Err(errno::EFAULT);
        }
        spans.push(UserSpan { base, len });
    }
    Ok(spans)
}

/// Validate a single-span user buffer (`read`/`write`/`sendto`/`recvfrom`).
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
fn parse_send_cmsgs(
    msg_control: u64,
    msg_controllen: u64,
) -> Result<(Vec<InFlightFile>, Option<lumen_sockabi::Ucred>), u32> {
    if msg_controllen == 0 {
        return Ok((Vec::new(), None));
    }
    if msg_controllen > OPTMEM_MAX as u64 {
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

/// `read(2)` on a socket: receive with flags 0 into one span.
pub(super) fn read_sock(sock: &dyn SockOps, buf: UserPtr, count: u64, nonblock: bool) -> Result<u64, u32> {
    let span = [single_span(buf, count)?];
    let mut io = RecvIo { iov: UserIovec::new(&span), max_fds: 0, flags: 0, nonblock };
    let done = sock.recv(&mut io)?;
    Ok(done.bytes as u64)
}

/// `write(2)` on a socket: send with flags 0 from one span.
pub(super) fn write_sock(sock: &dyn SockOps, buf: UserPtr, count: u64, nonblock: bool) -> Result<u64, u32> {
    let span = [single_span(buf, count)?];
    let mut io = SendIo { iov: UserIovec::new(&span), to: None, rights: Vec::new(), creds: None, flags: 0, nonblock };
    sock.send(&mut io).map(|n| n as u64)
}

/// `readv(2)` on a socket: receive with flags 0 across validated spans.
pub(super) fn readv_sock(sock: &dyn SockOps, iovs: &[RawIovec], nonblock: bool) -> Result<u64, u32> {
    let mut spans = Vec::new();
    spans.try_reserve_exact(iovs.len()).map_err(|_| errno::ENOMEM)?;
    for span in iovs {
        let len = span.len as usize;
        if len > 0 && !user_range_valid(span.base, len) {
            return Err(errno::EFAULT);
        }
        spans.push(UserSpan { base: span.base, len });
    }
    let mut io = RecvIo { iov: UserIovec::new(&spans), max_fds: 0, flags: 0, nonblock };
    let done = sock.recv(&mut io)?;
    Ok(done.bytes as u64)
}

/// `writev(2)` on a socket: send with flags 0 across validated spans.
pub(super) fn writev_sock(sock: &dyn SockOps, iovs: &[RawIovec], nonblock: bool) -> Result<u64, u32> {
    let mut spans = Vec::new();
    spans.try_reserve_exact(iovs.len()).map_err(|_| errno::ENOMEM)?;
    for span in iovs {
        let len = span.len as usize;
        if len > 0 && !user_range_valid(span.base, len) {
            return Err(errno::EFAULT);
        }
        spans.push(UserSpan { base: span.base, len });
    }
    let mut io = SendIo { iov: UserIovec::new(&spans), to: None, rights: Vec::new(), creds: None, flags: 0, nonblock };
    sock.send(&mut io).map(|n| n as u64)
}

/// `socket(domain, type, protocol)` (41): dispatch by family (`AF_UNIX`,
/// `AF_INET`, `AF_INET6`), validate type/flags/protocol, reserve a descriptor
/// slot first, and install the `sockfs` file.
pub(super) fn sys_socket(domain: i32, ty: i32, protocol: i32) -> u64 {
    finish(do_socket(domain, ty, protocol))
}

fn do_socket(domain: i32, ty: i32, protocol: i32) -> Result<u64, u32> {
    let (base_ty, nonblock, cloexec) = parse_type_flags(ty)?;
    let family = u16::try_from(domain).ok().and_then(Family::from_raw).ok_or(errno::EAFNOSUPPORT)?;
    let kind = validate_socket_spec(family, base_ty, protocol)?;
    let slot = process::reserve_current_fd().map_err(vfs)?;
    let ops = create_socket(family, kind, protocol)?;
    let file = new_socket_file(ops, nonblock)?;
    Ok(slot.install(file, cloexec) as u64)
}

/// `connect(fd, addr, addrlen)` (42): decode address (`AF_UNSPEC` disconnects,
/// dual-stack `AF_INET` on `AF_INET6` decoded as `Inet`), dispatch to `SockOps`.
pub(super) fn sys_connect(fd: Fd, addr: UserPtr, addrlen: i32) -> u64 {
    finish(do_connect(fd, addr, addrlen))
}

fn do_connect(fd: Fd, addr: UserPtr, addrlen: i32) -> Result<u64, u32> {
    let file = socket_file(fd)?;
    let sock = file.sock().unwrap();
    let (raw, len) = copy_addr_from_user(addr, addrlen)?;
    let bytes = &raw[..len];
    if bytes.len() >= 2 && peek_family(bytes) == Ok(AF_UNSPEC) {
        sock.disconnect()?;
        return Ok(0);
    }
    let decoded = if sock.family() == Family::Inet6 && bytes.len() >= 2 && peek_family(bytes) == Ok(AF_INET) {
        SockAddr::decode(Family::Inet, Call::Connect, bytes).map_err(Errno::to_errno)?
    } else {
        SockAddr::decode(sock.family(), Call::Connect, bytes).map_err(Errno::to_errno)?
    };
    sock.connect(&decoded, file.is_nonblock())?;
    Ok(0)
}

/// `accept(fd, addr, addrlen)` (43): `accept4(fd, addr, addrlen, 0)`.
pub(super) fn sys_accept(fd: Fd, addr: UserPtr, addrlen: UserPtr) -> u64 {
    finish(do_accept4(fd, addr, addrlen, 0))
}

/// `accept4(fd, addr, addrlen, flags)` (288): reserve a slot before accepting,
/// install the peer socket with `SOCK_NONBLOCK`/`SOCK_CLOEXEC`, and copy out
/// the peer address.
pub(super) fn sys_accept4(fd: Fd, addr: UserPtr, addrlen: UserPtr, flags: i32) -> u64 {
    finish(do_accept4(fd, addr, addrlen, flags))
}

fn do_accept4(fd: Fd, addr: UserPtr, addrlen: UserPtr, flags: i32) -> Result<u64, u32> {
    let raw_flags = flags as u32;
    if raw_flags & !(SOCK_NONBLOCK | SOCK_CLOEXEC) != 0 {
        return Err(errno::EINVAL);
    }
    let file = socket_file(fd)?;
    let sock = file.sock().unwrap();
    let slot = process::reserve_current_fd().map_err(vfs)?;
    let (peer_ops, peer_addr) = sock.accept(file.is_nonblock())?;
    let peer_file = new_socket_file(peer_ops, raw_flags & SOCK_NONBLOCK != 0)?;
    // Copy peer address out before installing the slot so a faulting pointer
    // drops the reservation and releases the accepted socket cleanly.
    copy_opt_addr_to_user(addr, addrlen, peer_addr.as_ref())?;
    Ok(slot.install(peer_file, raw_flags & SOCK_CLOEXEC != 0) as u64)
}

/// `sendto(fd, buf, len, flags, addr, addrlen)` (44): single-span send with
/// optional destination address (ignored on stream sockets).
pub(super) fn sys_sendto(fd: Fd, buf: UserPtr, len: u64, flags: u32, addr: UserPtr, addrlen: i32) -> u64 {
    finish(do_sendto(fd, buf, len, flags, addr, addrlen))
}

fn do_sendto(fd: Fd, buf: UserPtr, len: u64, flags: u32, addr: UserPtr, addrlen: i32) -> Result<u64, u32> {
    let file = socket_file(fd)?;
    let sock = file.sock().unwrap();
    if flags & MSG_OOB != 0 {
        return Err(errno::EOPNOTSUPP);
    }
    let span = [single_span(buf, len)?];
    let to = if sock.kind() == SockKind::Stream || addr.is_null() {
        None
    } else {
        let (raw, raw_len) = copy_addr_from_user(addr, addrlen)?;
        decode_dest_addr(sock.family(), Call::SendTo, &raw[..raw_len])?
    };
    let nonblock = file.is_nonblock() || (flags & MSG_DONTWAIT != 0);
    let mut io = SendIo { iov: UserIovec::new(&span), to, rights: Vec::new(), creds: None, flags, nonblock };
    sock.send(&mut io).map(|n| n as u64)
}

/// `recvfrom(fd, buf, len, flags, addr, addrlen)` (45): single-span receive
/// with optional peer address copy-out.
pub(super) fn sys_recvfrom(fd: Fd, buf: UserPtr, len: u64, flags: u32, addr: UserPtr, addrlen: UserPtr) -> u64 {
    finish(do_recvfrom(fd, buf, len, flags, addr, addrlen))
}

fn do_recvfrom(fd: Fd, buf: UserPtr, len: u64, flags: u32, addr: UserPtr, addrlen: UserPtr) -> Result<u64, u32> {
    let file = socket_file(fd)?;
    let sock = file.sock().unwrap();
    if flags & MSG_ERRQUEUE != 0 {
        return Err(errno::EAGAIN);
    }
    if flags & MSG_OOB != 0 {
        return Err(errno::EINVAL);
    }
    let span = [single_span(buf, len)?];
    let nonblock = file.is_nonblock() || (flags & MSG_DONTWAIT != 0);
    let mut io = RecvIo { iov: UserIovec::new(&span), max_fds: 0, flags, nonblock };
    let done = sock.recv(&mut io)?;
    copy_opt_addr_to_user(addr, addrlen, done.from.as_ref())?;
    Ok(done.bytes as u64)
}

/// `sendmsg(fd, msg, flags)` (46): copy in `msghdr`, `iovec`s, optional
/// destination and `cmsg` control buffer (`SCM_RIGHTS` resolved to
/// [`InFlightFile`]s before any socket lock), then dispatch to `SockOps::send`.
pub(super) fn sys_sendmsg(fd: Fd, msg: UserPtr, flags: u32) -> u64 {
    finish(do_sendmsg(fd, msg, flags))
}

fn do_sendmsg(fd: Fd, msg: UserPtr, flags: u32) -> Result<u64, u32> {
    let file = socket_file(fd)?;
    let sock = file.sock().unwrap();
    let mut raw_hdr = [0u8; MSGHDR_LEN];
    copy_from_user(&mut raw_hdr, msg.addr()).map_err(CopyError::to_errno)?;
    let hdr = Msghdr::from_bytes(&raw_hdr);
    if flags & MSG_OOB != 0 {
        return Err(errno::EOPNOTSUPP);
    }
    let spans = copy_msg_iovecs(hdr.msg_iov, hdr.msg_iovlen)?;
    let namelen = if hdr.msg_name == 0 { 0 } else { clamp_msg_namelen(hdr.msg_namelen).map_err(Errno::to_errno)? };
    let mut raw_name = [0u8; SOCKADDR_STORAGE_LEN];
    if hdr.msg_name != 0 && namelen > 0 {
        copy_from_user(&mut raw_name[..namelen], hdr.msg_name).map_err(CopyError::to_errno)?;
    }
    let to = if sock.kind() == SockKind::Stream || hdr.msg_name == 0 || namelen == 0 {
        None
    } else {
        decode_dest_addr(sock.family(), Call::SendMsg, &raw_name[..namelen])?
    };
    let (rights, creds) = parse_send_cmsgs(hdr.msg_control, hdr.msg_controllen)?;
    let nonblock = file.is_nonblock() || (flags & MSG_DONTWAIT != 0);
    let mut io = SendIo { iov: UserIovec::new(&spans), to, rights, creds, flags, nonblock };
    let sent = sock.send(&mut io)?;
    // Any unconsumed `io.rights` drop here, outside every socket lock.
    drop(io);
    Ok(sent as u64)
}

/// `recvmsg(fd, msg, flags)` (47): copy in `msghdr` and `iovec`s, receive via
/// `SockOps::recv`, reserve and install `SCM_RIGHTS` outside every socket lock
/// (`-EMFILE` installs what fits and sets `MSG_CTRUNC`), encode ancillary data
/// with `CmsgWriter`, and write back `msghdr` fields.
pub(super) fn sys_recvmsg(fd: Fd, msg: UserPtr, flags: u32) -> u64 {
    finish(do_recvmsg(fd, msg, flags))
}

fn do_recvmsg(fd: Fd, msg: UserPtr, flags: u32) -> Result<u64, u32> {
    let file = socket_file(fd)?;
    let sock = file.sock().unwrap();
    let mut raw_hdr = [0u8; MSGHDR_LEN];
    copy_from_user(&mut raw_hdr, msg.addr()).map_err(CopyError::to_errno)?;
    let hdr = Msghdr::from_bytes(&raw_hdr);
    if flags & MSG_ERRQUEUE != 0 {
        return Err(errno::EAGAIN);
    }
    if flags & MSG_OOB != 0 {
        return Err(errno::EINVAL);
    }
    let name_cap = if hdr.msg_name == 0 { 0 } else { clamp_msg_namelen(hdr.msg_namelen).map_err(Errno::to_errno)? };
    let spans = copy_msg_iovecs(hdr.msg_iov, hdr.msg_iovlen)?;
    let ctl_cap = if hdr.msg_control == 0 { 0 } else { usize::try_from(hdr.msg_controllen).unwrap_or(usize::MAX) };
    let max_fds = (ctl_cap.saturating_sub(CMSG_HDRLEN) / 4).min(SCM_MAX_FD);
    let nonblock = file.is_nonblock() || (flags & MSG_DONTWAIT != 0);

    let mut io = RecvIo { iov: UserIovec::new(&spans), max_fds, flags, nonblock };
    let mut done = sock.recv(&mut io)?;
    finish_recvmsg(msg, &hdr, name_cap, ctl_cap, flags, &mut done)?;
    Ok(done.bytes as u64)
}

/// Install received rights, format ancillary data with [`CmsgWriter`], and
/// write back `msg_name`, `msg_control`, `msg_namelen`, `msg_controllen`, and
/// `msg_flags`. Runs outside every socket lock.
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
    let (written_ctl_len, ctl_flags) = writer.finish();

    if written_ctl_len > 0 {
        copy_to_user(hdr.msg_control, &ctl_buf[..written_ctl_len]).map_err(CopyError::to_errno)?;
    }

    let out_namelen = if hdr.msg_name != 0 {
        let len = match done.from.as_ref() {
            Some(from) => {
                let mut encoded = [0u8; SOCKADDR_UN_LEN + 1];
                let full_len = from.encode(&mut encoded);
                let copy_len = full_len.min(name_cap);
                if copy_len > 0 {
                    copy_to_user(hdr.msg_name, &encoded[..copy_len]).map_err(CopyError::to_errno)?;
                }
                full_len as i32
            }
            None => 0i32,
        };
        Some(len)
    } else {
        None
    };

    if let Some(namelen) = out_namelen {
        let off = msg_ptr.addr().checked_add(offset_of!(Msghdr, msg_namelen) as u64).ok_or(errno::EFAULT)?;
        // SAFETY: `i32` is 4 bytes with no padding.
        unsafe { copy_pod_to_user(off, &namelen) }.map_err(CopyError::to_errno)?;
    }
    let ctl_len_out = written_ctl_len as u64;
    let ctl_off = msg_ptr.addr().checked_add(offset_of!(Msghdr, msg_controllen) as u64).ok_or(errno::EFAULT)?;
    // SAFETY: `u64` is 8 bytes with no padding.
    unsafe { copy_pod_to_user(ctl_off, &ctl_len_out) }.map_err(CopyError::to_errno)?;

    let out_flags = done.msg_flags | ctl_flags;
    let flags_off = msg_ptr.addr().checked_add(offset_of!(Msghdr, msg_flags) as u64).ok_or(errno::EFAULT)?;
    // SAFETY: `u32` is 4 bytes with no padding.
    unsafe { copy_pod_to_user(flags_off, &out_flags) }.map_err(CopyError::to_errno)?;

    // All user copies succeeded: commit the reserved slots. Any remaining
    // uninstalled rights drop when `done` drops, outside every lock.
    let cloexec = flags & MSG_CMSG_CLOEXEC != 0;
    for (slot, inflight) in reserved.into_iter().zip(done.rights.drain(..written_fds)) {
        let _ = slot.install_ref(inflight.into_file_ref(), cloexec);
    }
    Ok(())
}

/// `shutdown(fd, how)` (48): validate `how` (`SHUT_RD`..=`SHUT_RDWR`) and
/// dispatch to `SockOps::shutdown`.
pub(super) fn sys_shutdown(fd: Fd, how: i32) -> u64 {
    finish(do_shutdown(fd, how))
}

fn do_shutdown(fd: Fd, how: i32) -> Result<u64, u32> {
    let file = socket_file(fd)?;
    let how = ShutdownHow::from_raw(how).ok_or(errno::EINVAL)?;
    file.sock().unwrap().shutdown(how)?;
    Ok(0)
}

/// `bind(fd, addr, addrlen)` (49): decode address with `Call::Bind` and
/// dispatch to `SockOps::bind`.
pub(super) fn sys_bind(fd: Fd, addr: UserPtr, addrlen: i32) -> u64 {
    finish(do_bind(fd, addr, addrlen))
}

fn do_bind(fd: Fd, addr: UserPtr, addrlen: i32) -> Result<u64, u32> {
    let file = socket_file(fd)?;
    let sock = file.sock().unwrap();
    let (raw, len) = copy_addr_from_user(addr, addrlen)?;
    let decoded = SockAddr::decode(sock.family(), Call::Bind, &raw[..len]).map_err(Errno::to_errno)?;
    sock.bind(&decoded)?;
    Ok(0)
}

/// `listen(fd, backlog)` (50): stream and seqpacket sockets only
/// (`-EOPNOTSUPP` for datagram/raw), dispatch to `SockOps::listen`.
pub(super) fn sys_listen(fd: Fd, backlog: i32) -> u64 {
    finish(do_listen(fd, backlog))
}

fn do_listen(fd: Fd, backlog: i32) -> Result<u64, u32> {
    let file = socket_file(fd)?;
    let sock = file.sock().unwrap();
    if !matches!(sock.kind(), SockKind::Stream | SockKind::Seqpacket) {
        return Err(errno::EOPNOTSUPP);
    }
    sock.listen(backlog)?;
    Ok(0)
}

/// `getsockname(fd, addr, addrlen)` (51): encode `SockOps::local_name` and
/// copy out truncated to `*addrlen`.
pub(super) fn sys_getsockname(fd: Fd, addr: UserPtr, addrlen: UserPtr) -> u64 {
    finish(do_getsockname(fd, addr, addrlen))
}

fn do_getsockname(fd: Fd, addr: UserPtr, addrlen: UserPtr) -> Result<u64, u32> {
    let file = socket_file(fd)?;
    let name = file.sock().unwrap().local_name()?;
    copy_addr_to_user(addr, addrlen, &name)?;
    Ok(0)
}

/// `getpeername(fd, addr, addrlen)` (52): encode `SockOps::peer_name` and copy
/// out truncated to `*addrlen`.
pub(super) fn sys_getpeername(fd: Fd, addr: UserPtr, addrlen: UserPtr) -> u64 {
    finish(do_getpeername(fd, addr, addrlen))
}

fn do_getpeername(fd: Fd, addr: UserPtr, addrlen: UserPtr) -> Result<u64, u32> {
    let file = socket_file(fd)?;
    let name = file.sock().unwrap().peer_name()?;
    copy_addr_to_user(addr, addrlen, &name)?;
    Ok(0)
}

/// `socketpair(domain, type, protocol, sv)` (53): `AF_UNIX` only (inet
/// `-EOPNOTSUPP`, other families `-EAFNOSUPPORT`); reserves two slots first
/// and copies `[sv0, sv1]` before installing either.
pub(super) fn sys_socketpair(domain: i32, ty: i32, protocol: i32, sv: UserPtr) -> u64 {
    finish(do_socketpair(domain, ty, protocol, sv))
}

fn do_socketpair(domain: i32, ty: i32, protocol: i32, sv: UserPtr) -> Result<u64, u32> {
    let (base_ty, nonblock, cloexec) = parse_type_flags(ty)?;
    let family = u16::try_from(domain).ok().and_then(Family::from_raw).ok_or(errno::EAFNOSUPPORT)?;
    if family != Family::Unix {
        return Err(errno::EOPNOTSUPP);
    }
    let kind = validate_socket_spec(family, base_ty, protocol)?;
    let slot_a = process::reserve_current_fd().map_err(vfs)?;
    let slot_b = process::reserve_current_fd().map_err(vfs)?;
    let (ops_a, ops_b) = create_socketpair(family, kind, protocol)?;
    let file_a = new_socket_file(ops_a, nonblock)?;
    let file_b = new_socket_file(ops_b, nonblock)?;
    let pair = [slot_a.fd(), slot_b.fd()];
    // Copy `[sv0, sv1]` to user space before installing either descriptor so a
    // faulting pointer leaves no descriptor visible and releases both sockets.
    // SAFETY: `[i32; 2]` is 8 bytes with no padding.
    unsafe { copy_pod_to_user(sv.addr(), &pair) }.map_err(CopyError::to_errno)?;
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
    let file = socket_file(fd)?;
    let sock = file.sock().unwrap();
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
    let file = socket_file(fd)?;
    let sock = file.sock().unwrap();
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
