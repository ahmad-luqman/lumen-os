extern crate alloc;
extern crate std;

use alloc::vec;
use alloc::vec::Vec;
use core::mem::{offset_of, size_of};
use core::time::Duration;

use crate::abi::{
    self, Cmsghdr, EpollEvent, Msghdr, RawLinger, RawTimeval, RawUcred, SockaddrIn, SockaddrIn6, SockaddrUn,
};
use crate::{
    AF_INET, AF_INET6, AF_UNIX, AF_UNSPEC, CMSG_ALIGN_SIZE, CMSG_HDRLEN, Call, Cmsg, CmsgIter, CmsgWriter,
    EPOLL_EVENT_LEN, Errno, Family, IPPROTO_IP, IPPROTO_IPV6, IPPROTO_TCP, Ipv4Addr, Ipv6Addr, LINGER_LEN, Linger,
    MAX_OPT_LEN, MAX_TIMEOUT_SECS, MSG_CMSG_CLOEXEC, MSG_CTRUNC, MSGHDR_LEN, OptKind, OptVal, SCM_CREDENTIALS,
    SCM_MAX_FD, SCM_RIGHTS, SIN6_LEN_RFC2133, SOCKADDR_IN_LEN, SOCKADDR_IN6_LEN, SOCKADDR_STORAGE_LEN, SOCKADDR_UN_LEN,
    SOL_SOCKET, SockAddr, SockOpt, SocketAddrV4, TIMEOUT_HZ, TIMEVAL_LEN, Timeout, Timeval, UCRED_LEN, UNIX_PATH_MAX,
    Ucred, UnixName, check_optlen, clamp_msg_namelen, cmsg_align, cmsg_len, cmsg_space, count_rights_fds, decode_opt,
    peek_family, write_opt, write_opt_for,
};

const ALL_CALLS: [Call; 4] = [Call::Bind, Call::Connect, Call::SendTo, Call::SendMsg];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn fill(&mut self, buf: &mut [u8]) {
        for b in buf {
            *b = self.next() as u8;
        }
    }
}

fn make_cmsg_bytes(level: i32, cmsg_type: i32, data: &[u8], pad_byte: u8) -> Vec<u8> {
    let len = CMSG_HDRLEN + data.len();
    let space = cmsg_space(data.len()).unwrap();
    let mut buf = vec![pad_byte; space];
    buf[0..8].copy_from_slice(&(len as u64).to_le_bytes());
    buf[8..12].copy_from_slice(&level.to_le_bytes());
    buf[12..16].copy_from_slice(&cmsg_type.to_le_bytes());
    buf[16..16 + data.len()].copy_from_slice(data);
    buf
}

// ---- Pinned layouts & constants

#[test]
fn pinned_layout_sizes_and_offsets_match_x86_64_uapi() {
    assert_eq!(SOCKADDR_IN_LEN, 16);
    assert_eq!(SOCKADDR_IN6_LEN, 28);
    assert_eq!(SIN6_LEN_RFC2133, 24);
    assert_eq!(UNIX_PATH_MAX, 108);
    assert_eq!(SOCKADDR_UN_LEN, 110);
    assert_eq!(SOCKADDR_STORAGE_LEN, 128);
    assert_eq!(MSGHDR_LEN, 56);
    assert_eq!(CMSG_HDRLEN, 16);
    assert_eq!(CMSG_ALIGN_SIZE, 8);
    assert_eq!(UCRED_LEN, 12);
    assert_eq!(LINGER_LEN, 8);
    assert_eq!(TIMEVAL_LEN, 16);
    assert_eq!(EPOLL_EVENT_LEN, 12);
    assert_eq!(SCM_MAX_FD, 253);

    assert_eq!(size_of::<SockaddrIn>(), 16);
    assert_eq!(offset_of!(SockaddrIn, sin_port), 2);
    assert_eq!(offset_of!(SockaddrIn, sin_addr), 4);
    assert_eq!(offset_of!(SockaddrIn, pad), 8);

    assert_eq!(size_of::<SockaddrIn6>(), 28);
    assert_eq!(offset_of!(SockaddrIn6, sin6_port), 2);
    assert_eq!(offset_of!(SockaddrIn6, sin6_flowinfo), 4);
    assert_eq!(offset_of!(SockaddrIn6, sin6_addr), 8);
    assert_eq!(offset_of!(SockaddrIn6, sin6_scope_id), 24);

    assert_eq!(size_of::<SockaddrUn>(), 110);
    assert_eq!(offset_of!(SockaddrUn, sun_path), 2);

    assert_eq!(size_of::<Msghdr>(), 56);
    assert_eq!(offset_of!(Msghdr, msg_namelen), 8);
    assert_eq!(offset_of!(Msghdr, msg_iov), 16);
    assert_eq!(offset_of!(Msghdr, msg_iovlen), 24);
    assert_eq!(offset_of!(Msghdr, msg_control), 32);
    assert_eq!(offset_of!(Msghdr, msg_controllen), 40);
    assert_eq!(offset_of!(Msghdr, msg_flags), 48);

    assert_eq!(size_of::<Cmsghdr>(), 16);
    assert_eq!(offset_of!(Cmsghdr, cmsg_level), 8);
    assert_eq!(offset_of!(Cmsghdr, cmsg_type), 12);

    assert_eq!(size_of::<RawUcred>(), 12);
    assert_eq!(offset_of!(RawUcred, uid), 4);
    assert_eq!(offset_of!(RawUcred, gid), 8);

    assert_eq!(size_of::<RawLinger>(), 8);
    assert_eq!(offset_of!(RawLinger, l_linger), 4);

    assert_eq!(size_of::<RawTimeval>(), 16);
    assert_eq!(offset_of!(RawTimeval, tv_usec), 8);

    // x86-64 `struct epoll_event` is packed to 12 bytes (`include/uapi/linux/eventpoll.h`).
    assert_eq!(size_of::<EpollEvent>(), 12);
    assert_eq!(offset_of!(EpollEvent, events), 0);
    assert_eq!(offset_of!(EpollEvent, data), 4);

    assert_eq!(MSG_CTRUNC, 8);
    assert_eq!(MSG_CMSG_CLOEXEC, 0x4000_0000);
    assert_eq!(Errno::EINVAL.to_errno(), 22);
    assert_eq!(Errno::EDOM.to_errno(), 33);
    assert_eq!(Errno::ENOPROTOOPT.to_errno(), 92);
    assert_eq!(Errno::EAFNOSUPPORT.to_errno(), 97);
}

#[test]
fn msghdr_and_epoll_event_byte_codecs_round_trip_and_zero_padding() {
    let hdr = Msghdr {
        msg_name: 0x1122_3344_5566_7788,
        msg_namelen: 0x0102_0304,
        msg_iov: 0x2233_4455_6677_8899,
        msg_iovlen: 7,
        msg_control: 0x3344_5566_7788_99aa,
        msg_controllen: 64,
        msg_flags: MSG_CTRUNC | MSG_CMSG_CLOEXEC,
    };
    let mut raw = hdr.to_bytes();
    assert_eq!(&raw[12..16], &[0; 4]);
    assert_eq!(&raw[52..56], &[0; 4]);
    // Garbage in padding slots must be ignored by `from_bytes`.
    raw[12..16].fill(0xee);
    raw[52..56].fill(0xdd);
    assert_eq!(Msghdr::from_bytes(&raw), hdr);

    let ev = EpollEvent { events: 0x0000_2005, data: 0xfedc_ba98_7654_3210 };
    let ev_raw = ev.to_bytes();
    assert_eq!(&ev_raw[0..4], &0x0000_2005u32.to_le_bytes());
    assert_eq!(&ev_raw[4..12], &0xfedc_ba98_7654_3210u64.to_le_bytes());
    assert_eq!(EpollEvent::from_bytes(&ev_raw), ev);
}

// ---- SockAddr: AF_INET, AF_INET6, AF_UNIX per-call length, family, and decode_dest rules

#[test]
fn inet_decode_length_family_and_per_call_rules() {
    // Linux v6.12 net/ipv4/af_inet.c (inet_bind_sk), net/ipv4/tcp_ipv4.c (tcp_v4_connect),
    // and net/ipv4/udp.c (udp_sendmsg): addr_len < 16 -> EINVAL (checked before family).
    for call in ALL_CALLS {
        for len in 0..SOCKADDR_IN_LEN {
            let mut buf = vec![0xffu8; len];
            if len >= 2 {
                buf[0..2].copy_from_slice(&AF_UNIX.to_le_bytes());
            }
            assert_eq!(SockAddr::decode(Family::Inet, call, &buf), Err(Errno::EINVAL), "call={call:?} len={len}");
        }
    }

    // Exact 16 bytes with non-zero __pad (ignored on decode).
    let mut exact = [0xaa; 16];
    exact[0..2].copy_from_slice(&AF_INET.to_le_bytes());
    exact[2..4].copy_from_slice(&8080u16.to_be_bytes());
    exact[4..8].copy_from_slice(&[127, 0, 0, 1]);
    let want = SockAddr::Inet(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 8080));
    for call in ALL_CALLS {
        assert_eq!(SockAddr::decode(Family::Inet, call, &exact), Ok(want));
    }

    // Trailing bytes up to sizeof(struct sockaddr_storage) (128) accepted.
    for len in [17, 28, 64, SOCKADDR_STORAGE_LEN] {
        let mut long = vec![0x55; len];
        long[..16].copy_from_slice(&exact);
        assert_eq!(SockAddr::decode(Family::Inet, Call::Bind, &long), Ok(want), "len={len}");
    }

    // Linux v6.12 net/socket.c move_addr_to_kernel: len > 128 -> EINVAL for Bind/Connect/SendTo.
    // Linux v6.12 net/socket.c __copy_msghdr: msg_namelen < 0 -> EINVAL, > 128 clamped to 128.
    let mut over_storage = vec![0u8; SOCKADDR_STORAGE_LEN + 1];
    over_storage[..16].copy_from_slice(&exact);
    for call in [Call::Bind, Call::Connect, Call::SendTo] {
        assert_eq!(SockAddr::decode(Family::Inet, call, &over_storage), Err(Errno::EINVAL));
    }
    assert_eq!(SockAddr::decode(Family::Inet, Call::SendMsg, &over_storage), Ok(want));
    assert_eq!(clamp_msg_namelen(-1), Err(Errno::EINVAL));
    assert_eq!(clamp_msg_namelen(i32::MIN), Err(Errno::EINVAL));
    assert_eq!(clamp_msg_namelen(0), Ok(0));
    assert_eq!(clamp_msg_namelen(128), Ok(128));
    assert_eq!(clamp_msg_namelen(129), Ok(128));
    assert_eq!(clamp_msg_namelen(i32::MAX), Ok(128));

    // AF_UNSPEC rules:
    // - Bind (inet_bind_sk, net/ipv4/af_inet.c): accepted ONLY with INADDR_ANY (0.0.0.0).
    // - SendTo/SendMsg (udp_sendmsg, net/ipv4/udp.c): accepted with any IPv4 address.
    // - Connect (tcp_v4_connect, net/ipv4/tcp_ipv4.c): EAFNOSUPPORT.
    let mut unspec_any = [0u8; 16];
    unspec_any[0..2].copy_from_slice(&AF_UNSPEC.to_le_bytes());
    unspec_any[2..4].copy_from_slice(&9000u16.to_be_bytes());
    let want_any = SockAddr::Inet(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 9000));
    assert_eq!(SockAddr::decode(Family::Inet, Call::Bind, &unspec_any), Ok(want_any));
    assert_eq!(SockAddr::decode(Family::Inet, Call::SendTo, &unspec_any), Ok(want_any));
    assert_eq!(SockAddr::decode(Family::Inet, Call::SendMsg, &unspec_any), Ok(want_any));
    assert_eq!(SockAddr::decode(Family::Inet, Call::Connect, &unspec_any), Err(Errno::EAFNOSUPPORT));

    let mut unspec_non_any = exact;
    unspec_non_any[0..2].copy_from_slice(&AF_UNSPEC.to_le_bytes());
    assert_eq!(SockAddr::decode(Family::Inet, Call::Bind, &unspec_non_any), Err(Errno::EAFNOSUPPORT));
    assert_eq!(SockAddr::decode(Family::Inet, Call::Connect, &unspec_non_any), Err(Errno::EAFNOSUPPORT));
    assert_eq!(SockAddr::decode(Family::Inet, Call::SendTo, &unspec_non_any), Ok(want));
    assert_eq!(SockAddr::decode(Family::Inet, Call::SendMsg, &unspec_non_any), Ok(want));

    // Other wrong families -> EAFNOSUPPORT across all calls.
    for bad_fam in [AF_UNIX, AF_INET6, 99] {
        let mut bad = exact;
        bad[0..2].copy_from_slice(&bad_fam.to_le_bytes());
        for call in ALL_CALLS {
            assert_eq!(SockAddr::decode(Family::Inet, call, &bad), Err(Errno::EAFNOSUPPORT));
        }
    }
}

#[test]
fn inet6_decode_rfc2133_full_and_sendmsg_clamp_edge_cases() {
    // Linux v6.12 net/ipv6/af_inet6.c (inet6_bind_sk) & net/ipv6/tcp_ipv6.c (tcp_v6_connect):
    // addr_len < SIN6_LEN_RFC2133 (24) -> EINVAL (checked before family).
    for len in 0..SIN6_LEN_RFC2133 {
        let mut buf = vec![0u8; len];
        if len >= 2 {
            buf[0..2].copy_from_slice(&AF_INET.to_le_bytes());
        }
        assert_eq!(SockAddr::decode(Family::Inet6, Call::Bind, &buf), Err(Errno::EINVAL), "len={len}");
    }

    let ip = Ipv6Addr::new(0xfe80, 0, 0, 0, 0x0211, 0x22ff, 0xfe33, 0x4455);
    let mut full = [0u8; 28];
    full[0..2].copy_from_slice(&AF_INET6.to_le_bytes());
    full[2..4].copy_from_slice(&443u16.to_be_bytes());
    full[4..8].copy_from_slice(&0x0012_3456u32.to_be_bytes());
    full[8..24].copy_from_slice(&ip.octets());
    full[24..28].copy_from_slice(&7u32.to_le_bytes());

    // 24..28: valid RFC 2133 address, scope_id defaults to 0.
    for len in SIN6_LEN_RFC2133..SOCKADDR_IN6_LEN {
        assert_eq!(
            SockAddr::decode(Family::Inet6, Call::Connect, &full[..len]),
            Ok(SockAddr::Inet6 { addr: ip, port: 443, flowinfo: 0x0012_3456, scope_id: 0 }),
            "len={len}"
        );
    }

    // 28..=128: reads scope_id (LE) and ignores trailing bytes.
    let want_full = SockAddr::Inet6 { addr: ip, port: 443, flowinfo: 0x0012_3456, scope_id: 7 };
    assert_eq!(SockAddr::decode(Family::Inet6, Call::Bind, &full), Ok(want_full));
    let mut storage = [0x99u8; SOCKADDR_STORAGE_LEN];
    storage[..28].copy_from_slice(&full);
    assert_eq!(SockAddr::decode(Family::Inet6, Call::Bind, &storage), Ok(want_full));

    // > 128: EINVAL for Bind/Connect/SendTo (move_addr_to_kernel), clamped to 128 for SendMsg (__copy_msghdr).
    let mut over_storage = vec![0u8; SOCKADDR_STORAGE_LEN + 1];
    over_storage[..28].copy_from_slice(&full);
    for call in [Call::Bind, Call::Connect, Call::SendTo] {
        assert_eq!(SockAddr::decode(Family::Inet6, call, &over_storage), Err(Errno::EINVAL));
    }
    assert_eq!(SockAddr::decode(Family::Inet6, Call::SendMsg, &over_storage), Ok(want_full));

    // Wrong family at 24 and 28 bytes -> EAFNOSUPPORT.
    for len in [SIN6_LEN_RFC2133, SOCKADDR_IN6_LEN] {
        let mut bad = full[..len].to_vec();
        bad[0..2].copy_from_slice(&AF_INET.to_le_bytes());
        assert_eq!(SockAddr::decode(Family::Inet6, Call::Bind, &bad), Err(Errno::EAFNOSUPPORT));
    }
}

#[test]
fn unix_decode_length_path_abstract_and_per_call_unnamed_rules() {
    // Linux v6.12 net/unix/af_unix.c (unix_bind, unix_validate_addr, unix_mkname_bsd):
    // addr_len < 2 -> EINVAL.
    assert_eq!(SockAddr::decode(Family::Unix, Call::Bind, &[]), Err(Errno::EINVAL));
    assert_eq!(SockAddr::decode(Family::Unix, Call::Bind, &[1]), Err(Errno::EINVAL));

    // addr_len == 2 with AF_UNIX: valid ONLY for Bind (unix_bind autobind);
    // EINVAL for Connect/SendTo/SendMsg (unix_validate_addr rejects addr_len <= 2).
    let unnamed = AF_UNIX.to_le_bytes();
    assert_eq!(SockAddr::decode(Family::Unix, Call::Bind, &unnamed), Ok(SockAddr::Unix(UnixName::Unnamed)));
    for call in [Call::Connect, Call::SendTo, Call::SendMsg] {
        assert_eq!(SockAddr::decode(Family::Unix, call, &unnamed), Err(Errno::EINVAL), "call={call:?}");
    }

    // addr_len == 2 with wrong family -> EINVAL (unix_bind).
    assert_eq!(SockAddr::decode(Family::Unix, Call::Bind, &AF_INET.to_le_bytes()), Err(Errno::EINVAL));
    assert_eq!(SockAddr::decode(Family::Unix, Call::Bind, &AF_UNSPEC.to_le_bytes()), Err(Errno::EINVAL));

    // Wrong family with path -> EINVAL (unix_validate_addr).
    let mut wrong_fam = vec![0u8; 10];
    wrong_fam[0..2].copy_from_slice(&AF_INET.to_le_bytes());
    wrong_fam[2..7].copy_from_slice(b"/tmp/");
    assert_eq!(SockAddr::decode(Family::Unix, Call::Connect, &wrong_fam), Err(Errno::EINVAL));

    // Filesystem path without NUL terminator accepted (unix_mkname_bsd).
    let mut no_nul = AF_UNIX.to_le_bytes().to_vec();
    no_nul.extend_from_slice(b"/run/sock");
    for call in ALL_CALLS {
        assert_eq!(
            SockAddr::decode(Family::Unix, call, &no_nul),
            Ok(SockAddr::Unix(UnixName::path(b"/run/sock").unwrap()))
        );
    }

    // Full 108-byte path without NUL (addrlen == 110) accepted.
    let mut max_path = AF_UNIX.to_le_bytes().to_vec();
    max_path.extend_from_slice(&[b'x'; UNIX_PATH_MAX]);
    assert_eq!(max_path.len(), SOCKADDR_UN_LEN);
    assert_eq!(
        SockAddr::decode(Family::Unix, Call::Bind, &max_path),
        Ok(SockAddr::Unix(UnixName::path(&[b'x'; UNIX_PATH_MAX]).unwrap()))
    );

    // Path with NUL and trailing garbage stops at the first NUL (strlen).
    let mut with_tail = AF_UNIX.to_le_bytes().to_vec();
    with_tail.extend_from_slice(b"/tmp/s\0garbage\0more");
    assert_eq!(
        SockAddr::decode(Family::Unix, Call::Connect, &with_tail),
        Ok(SockAddr::Unix(UnixName::path(b"/tmp/s").unwrap()))
    );

    // Abstract namespace: leading NUL, length-delimited, preserves embedded NULs.
    let mut empty_abs = AF_UNIX.to_le_bytes().to_vec();
    empty_abs.push(0);
    assert_eq!(
        SockAddr::decode(Family::Unix, Call::Connect, &empty_abs),
        Ok(SockAddr::Unix(UnixName::abstract_name(b"").unwrap()))
    );

    let mut embedded_nul = AF_UNIX.to_le_bytes().to_vec();
    embedded_nul.extend_from_slice(b"\0foo\0bar\0");
    assert_eq!(
        SockAddr::decode(Family::Unix, Call::SendTo, &embedded_nul),
        Ok(SockAddr::Unix(UnixName::abstract_name(b"foo\0bar\0").unwrap()))
    );

    // Max 107-byte abstract name (addrlen == 110).
    let mut max_abs = AF_UNIX.to_le_bytes().to_vec();
    max_abs.push(0);
    max_abs.extend_from_slice(&[0xab; UNIX_PATH_MAX - 1]);
    assert_eq!(max_abs.len(), SOCKADDR_UN_LEN);
    assert_eq!(
        SockAddr::decode(Family::Unix, Call::Bind, &max_abs),
        Ok(SockAddr::Unix(UnixName::abstract_name(&[0xab; UNIX_PATH_MAX - 1]).unwrap()))
    );

    // addrlen > 110 -> EINVAL even on SendMsg (clamping 200 to 128 still exceeds 110).
    let mut too_long = max_path;
    too_long.push(0);
    assert_eq!(too_long.len(), SOCKADDR_UN_LEN + 1);
    for call in ALL_CALLS {
        assert_eq!(SockAddr::decode(Family::Unix, call, &too_long), Err(Errno::EINVAL));
    }
    let mut huge_unix = vec![b'a'; 200];
    huge_unix[0..2].copy_from_slice(&AF_UNIX.to_le_bytes());
    assert_eq!(SockAddr::decode(Family::Unix, Call::SendMsg, &huge_unix), Err(Errno::EINVAL));

    // Constructor validation boundaries.
    assert_eq!(UnixName::path(b""), Err(Errno::EINVAL));
    assert_eq!(UnixName::path(b"a\0b"), Err(Errno::EINVAL));
    assert_eq!(UnixName::path(&[b'a'; UNIX_PATH_MAX + 1]), Err(Errno::EINVAL));
    assert_eq!(UnixName::abstract_name(&[0; UNIX_PATH_MAX]), Err(Errno::EINVAL));

    // peek_family helper.
    assert_eq!(peek_family(&[]), Err(Errno::EINVAL));
    assert_eq!(peek_family(&[1]), Err(Errno::EINVAL));
    assert_eq!(peek_family(&AF_UNSPEC.to_le_bytes()), Ok(AF_UNSPEC));
    assert_eq!(peek_family(&AF_INET6.to_le_bytes()), Ok(AF_INET6));
    assert_eq!(Family::from_raw(AF_UNIX), Some(Family::Unix));
    assert_eq!(Family::from_raw(AF_INET), Some(Family::Inet));
    assert_eq!(Family::from_raw(AF_INET6), Some(Family::Inet6));
    assert_eq!(Family::from_raw(AF_UNSPEC), None);
}

#[test]
fn decode_dest_handles_no_address_cases_across_families() {
    // Linux v6.12 net/socket.c (__copy_msghdr): msg_namelen == 0 means no address in every family.
    for family in [Family::Unix, Family::Inet, Family::Inet6] {
        assert_eq!(SockAddr::decode_dest(family, Call::SendMsg, &[]), Ok(None));
    }

    // Linux v6.12 net/unix/af_unix.c (unix_dgram_sendmsg checks `if (msg->msg_namelen)`):
    // AF_UNIX SendTo with len == 0 -> no address; UDP/UDPv6 SendTo with len == 0 -> EINVAL.
    assert_eq!(SockAddr::decode_dest(Family::Unix, Call::SendTo, &[]), Ok(None));
    assert_eq!(SockAddr::decode_dest(Family::Inet, Call::SendTo, &[]), Err(Errno::EINVAL));
    assert_eq!(SockAddr::decode_dest(Family::Inet6, Call::SendTo, &[]), Err(Errno::EINVAL));

    // Bind/Connect always require an address (len == 0 -> EINVAL).
    for family in [Family::Unix, Family::Inet, Family::Inet6] {
        assert_eq!(SockAddr::decode_dest(family, Call::Bind, &[]), Err(Errno::EINVAL));
        assert_eq!(SockAddr::decode_dest(family, Call::Connect, &[]), Err(Errno::EINVAL));
    }

    // Linux v6.12 net/ipv6/udp.c (udpv6_sendmsg): AF_UNSPEC with len >= 2 -> no address (Ok(None)).
    let unspec2 = AF_UNSPEC.to_le_bytes();
    assert_eq!(SockAddr::decode_dest(Family::Inet6, Call::SendTo, &unspec2), Ok(None));
    assert_eq!(SockAddr::decode_dest(Family::Inet6, Call::SendMsg, &unspec2), Ok(None));
    let mut unspec28 = [0u8; 28];
    unspec28[0..2].copy_from_slice(&AF_UNSPEC.to_le_bytes());
    assert_eq!(SockAddr::decode_dest(Family::Inet6, Call::SendTo, &unspec28), Ok(None));
    assert_eq!(SockAddr::decode_dest(Family::Inet6, Call::SendMsg, &unspec28), Ok(None));
    // Len == 1 stays EINVAL (< offsetof(struct sockaddr, sa_data)).
    assert_eq!(SockAddr::decode_dest(Family::Inet6, Call::SendTo, &[0]), Err(Errno::EINVAL));
    assert_eq!(SockAddr::decode_dest(Family::Inet6, Call::SendMsg, &[0]), Err(Errno::EINVAL));
    // Over 128 bytes with AF_UNSPEC: SendTo -> EINVAL (move_addr_to_kernel), SendMsg -> Ok(None) (clamped).
    let over_unspec = vec![0u8; SOCKADDR_STORAGE_LEN + 1];
    assert_eq!(SockAddr::decode_dest(Family::Inet6, Call::SendTo, &over_unspec), Err(Errno::EINVAL));
    assert_eq!(SockAddr::decode_dest(Family::Inet6, Call::SendMsg, &over_unspec), Ok(None));

    // Concrete destination addresses decode to Ok(Some(..)).
    let mut v6 = [0u8; 28];
    let want_v6 = SockAddr::Inet6 { addr: Ipv6Addr::LOCALHOST, port: 53, flowinfo: 0, scope_id: 2 };
    want_v6.encode(&mut v6);
    assert_eq!(SockAddr::decode_dest(Family::Inet6, Call::SendTo, &v6), Ok(Some(want_v6)));
    assert_eq!(SockAddr::decode_dest(Family::Inet6, Call::SendMsg, &v6), Ok(Some(want_v6)));

    let mut path = AF_UNIX.to_le_bytes().to_vec();
    path.extend_from_slice(b"/tmp/dgram");
    assert_eq!(
        SockAddr::decode_dest(Family::Unix, Call::SendTo, &path),
        Ok(Some(SockAddr::Unix(UnixName::path(b"/tmp/dgram").unwrap())))
    );
}

#[test]
fn sockaddr_encode_decode_round_trips_and_truncating_copy_out() {
    let cases = [
        SockAddr::Inet(SocketAddrV4::new(Ipv4Addr::new(10, 0, 2, 15), 53)),
        SockAddr::Inet(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)),
        SockAddr::Inet6 { addr: Ipv6Addr::LOCALHOST, port: 65535, flowinfo: 0x000f_abcd, scope_id: 0x1234_5678 },
        SockAddr::Unix(UnixName::Unnamed),
        SockAddr::Unix(UnixName::path(b"/var/run/dbus").unwrap()),
        SockAddr::Unix(UnixName::path(&[b'p'; UNIX_PATH_MAX - 1]).unwrap()),
        SockAddr::Unix(UnixName::abstract_name(b"").unwrap()),
        SockAddr::Unix(UnixName::abstract_name(b"a\0\x01\0z").unwrap()),
        SockAddr::Unix(UnixName::abstract_name(&[0x7f; UNIX_PATH_MAX - 1]).unwrap()),
    ];

    for addr in cases {
        let want_len = addr.encoded_len();
        let mut full = vec![0xccu8; want_len + 8];
        let got_len = addr.encode(&mut full);
        assert_eq!(got_len, want_len);
        assert!(full[want_len..].iter().all(|&b| b == 0xcc));
        if matches!(addr, SockAddr::Inet(_)) {
            assert_eq!(&full[8..16], &[0; 8]);
        }
        assert_eq!(SockAddr::decode(addr.family(), Call::Bind, &full[..want_len]), Ok(addr));

        for cap in 0..=want_len + 2 {
            let mut out = vec![0xeeu8; cap];
            let reported = addr.encode(&mut out);
            assert_eq!(reported, want_len);
            let copied = cap.min(want_len);
            assert_eq!(&out[..copied], &full[..copied]);
            if cap > want_len {
                assert!(out[want_len..].iter().all(|&b| b == 0xee));
            }
        }
    }

    // 108-byte AF_UNIX path: Linux unix_mkname_bsd/unix_getname reports addrlen = 111
    // (2 + 108 + trailing NUL), truncating the NUL when copied into a 110-byte sockaddr_un.
    let max_unix = SockAddr::Unix(UnixName::path(&[b'z'; UNIX_PATH_MAX]).unwrap());
    assert_eq!(max_unix.encoded_len(), 111);
    let mut sun = [0u8; SOCKADDR_UN_LEN];
    assert_eq!(max_unix.encode(&mut sun), 111);
    assert_eq!(SockAddr::decode(Family::Unix, Call::Bind, &sun), Ok(max_unix));
    let mut storage = [0xffu8; SOCKADDR_STORAGE_LEN];
    assert_eq!(max_unix.encode(&mut storage), 111);
    assert_eq!(storage[110], 0);
}

// ---- CmsgIter: malformed headers, alignment, back-to-back, SCM_RIGHTS / SCM_CREDENTIALS

#[test]
fn cmsg_alignment_helpers_and_overflow() {
    assert_eq!(cmsg_align(0), Some(0));
    assert_eq!(cmsg_align(1), Some(8));
    assert_eq!(cmsg_align(7), Some(8));
    assert_eq!(cmsg_align(8), Some(8));
    assert_eq!(cmsg_align(9), Some(16));
    assert_eq!(cmsg_align(usize::MAX), None);
    assert_eq!(cmsg_align(usize::MAX - 6), None);
    assert_eq!(cmsg_align(usize::MAX - 7), Some(usize::MAX - 7));

    assert_eq!(cmsg_len(0), Some(16));
    assert_eq!(cmsg_len(4), Some(20));
    assert_eq!(cmsg_len(usize::MAX), None);

    assert_eq!(cmsg_space(0), Some(16));
    assert_eq!(cmsg_space(1), Some(24));
    assert_eq!(cmsg_space(4), Some(24));
    assert_eq!(cmsg_space(8), Some(24));
    assert_eq!(cmsg_space(12), Some(32));
    assert_eq!(cmsg_space(usize::MAX), None);
}

#[test]
fn cmsg_iter_handles_malformed_and_short_buffers() {
    // Empty and < CMSG_HDRLEN (16) buffers yield nothing (CMSG_FIRSTHDR == NULL).
    for len in 0..CMSG_HDRLEN {
        let buf = vec![0u8; len];
        assert!(CmsgIter::new(&buf).next().is_none(), "len={len}");
    }

    // cmsg_len < 16 -> EINVAL, then fused (None).
    for bad_len in [0u64, 1, 8, 15] {
        let mut buf = [0u8; 24];
        buf[0..8].copy_from_slice(&bad_len.to_le_bytes());
        let mut it = CmsgIter::new(&buf);
        assert_eq!(it.next(), Some(Err(Errno::EINVAL)), "bad_len={bad_len}");
        assert_eq!(it.next(), None);
    }

    // cmsg_len > remaining bytes (including u64::MAX overflow) -> EINVAL, then fused.
    for bad_len in [17u64, 24, 100, usize::MAX as u64, u64::MAX] {
        let mut buf = [0u8; 16];
        buf[0..8].copy_from_slice(&bad_len.to_le_bytes());
        let mut it = CmsgIter::new(&buf);
        assert_eq!(it.next(), Some(Err(Errno::EINVAL)), "bad_len={bad_len}");
        assert_eq!(it.next(), None);
    }

    // Zero-payload cmsg (cmsg_len == 16) is valid.
    let empty_payload = make_cmsg_bytes(SOL_SOCKET, SCM_RIGHTS, &[], 0);
    let items: Vec<_> = CmsgIter::new(&empty_payload).collect();
    assert_eq!(items, vec![Ok(Cmsg { level: SOL_SOCKET, cmsg_type: SCM_RIGHTS, data: &[] })]);
    assert_eq!(items[0].unwrap().rights_fds().unwrap().count(), 0);

    // Unpadded final cmsg (cmsg_len == 20, buf.len() == 20) is valid (Linux CMSG_OK).
    let mut unpadded = [0u8; 20];
    unpadded[0..8].copy_from_slice(&20u64.to_le_bytes());
    unpadded[8..12].copy_from_slice(&SOL_SOCKET.to_le_bytes());
    unpadded[12..16].copy_from_slice(&SCM_RIGHTS.to_le_bytes());
    unpadded[16..20].copy_from_slice(&42i32.to_le_bytes());
    let mut it = CmsgIter::new(&unpadded);
    let item = it.next().unwrap().unwrap();
    assert_eq!(item.rights_fds().unwrap().collect::<Vec<_>>(), vec![42]);
    assert_eq!(it.next(), None);
}

#[test]
fn cmsg_iter_walks_back_to_back_headers_and_stops_on_second_error() {
    let mut buf = Vec::new();
    let payloads: [&[u8]; 7] = [
        &[],
        &[0x11],
        &[0x21, 0x22, 0x23],
        &7i32.to_le_bytes(),
        &[1, 2, 3, 4, 5, 6, 7],
        &[8; 8],
        &Ucred { pid: 10, uid: 20, gid: 30 }.to_bytes(),
    ];
    for (i, payload) in payloads.iter().enumerate() {
        buf.extend_from_slice(&make_cmsg_bytes(SOL_SOCKET, (i + 1) as i32, payload, 0xaa));
    }

    let got: Vec<Cmsg<'_>> = CmsgIter::new(&buf).map(|r| r.unwrap()).collect();
    assert_eq!(got.len(), payloads.len());
    for (i, (msg, &want)) in got.iter().zip(payloads.iter()).enumerate() {
        assert_eq!(msg.level, SOL_SOCKET);
        assert_eq!(msg.cmsg_type, (i + 1) as i32);
        assert_eq!(msg.data, want);
    }

    // Valid first header followed by malformed second header: yields Ok then Err then None.
    let mut broken = make_cmsg_bytes(SOL_SOCKET, SCM_RIGHTS, &9i32.to_le_bytes(), 0);
    let mut bad_second = [0u8; 24];
    bad_second[0..8].copy_from_slice(&25u64.to_le_bytes());
    broken.extend_from_slice(&bad_second);
    let mut it = CmsgIter::new(&broken);
    assert_eq!(it.next().unwrap().unwrap().rights_fds().unwrap().collect::<Vec<_>>(), vec![9]);
    assert_eq!(it.next(), Some(Err(Errno::EINVAL)));
    assert_eq!(it.next(), None);
}

#[test]
fn cmsg_scm_rights_cumulative_max_fds_and_scm_credentials_exact_len() {
    // Linux v6.12 net/core/scm.c (scm_fp_copy): single cmsg SCM_MAX_FD (253) ok, 254 -> EINVAL.
    let fds_ok: Vec<u8> = (0..SCM_MAX_FD as i32).flat_map(|fd| fd.to_le_bytes()).collect();
    let msg_ok = Cmsg { level: SOL_SOCKET, cmsg_type: SCM_RIGHTS, data: &fds_ok };
    let decoded: Vec<i32> = msg_ok.rights_fds().unwrap().collect();
    assert_eq!(decoded.len(), SCM_MAX_FD);
    assert_eq!(decoded[0], 0);
    assert_eq!(decoded[SCM_MAX_FD - 1], (SCM_MAX_FD - 1) as i32);

    // Trailing 1..3 bytes are ignored by `num = data.len() / 4` (Linux scm_fp_copy).
    let mut with_tail = 5i32.to_le_bytes().to_vec();
    with_tail.extend_from_slice(&[0xff, 0xee, 0xdd]);
    let msg_tail = Cmsg { level: SOL_SOCKET, cmsg_type: SCM_RIGHTS, data: &with_tail };
    assert_eq!(msg_tail.rights_fds().unwrap().collect::<Vec<_>>(), vec![5]);

    let fds_too_many: Vec<u8> = (0..=SCM_MAX_FD as i32).flat_map(|fd| fd.to_le_bytes()).collect();
    let msg_too_many = Cmsg { level: SOL_SOCKET, cmsg_type: SCM_RIGHTS, data: &fds_too_many };
    assert_eq!(msg_too_many.rights_fds().unwrap_err(), Errno::EINVAL);

    // Cumulative SCM_MAX_FD across multiple SCM_RIGHTS headers in one message
    // (Linux v6.12 net/core/scm.c scm_fp_copy: fpl->count + num > SCM_MAX_FD -> EINVAL).
    let make_rights_cmsg = |n: usize| {
        let data: Vec<u8> = (0..n as i32).flat_map(|fd| fd.to_le_bytes()).collect();
        make_cmsg_bytes(SOL_SOCKET, SCM_RIGHTS, &data, 0)
    };
    let mut two_of_200 = make_rights_cmsg(200);
    two_of_200.extend_from_slice(&make_rights_cmsg(200));
    assert_eq!(count_rights_fds(&two_of_200), Err(Errno::EINVAL));

    let mut exact_253 = make_rights_cmsg(126);
    exact_253.extend_from_slice(&make_cmsg_bytes(
        SOL_SOCKET,
        SCM_CREDENTIALS,
        &Ucred { pid: 1, uid: 0, gid: 0 }.to_bytes(),
        0,
    ));
    exact_253.extend_from_slice(&make_rights_cmsg(127));
    assert_eq!(count_rights_fds(&exact_253), Ok(SCM_MAX_FD));

    let mut over_by_one = make_rights_cmsg(127);
    over_by_one.extend_from_slice(&make_rights_cmsg(127));
    assert_eq!(count_rights_fds(&over_by_one), Err(Errno::EINVAL));

    // Wrong level/type rejected.
    assert_eq!(Cmsg { level: IPPROTO_TCP, cmsg_type: SCM_RIGHTS, data: &[] }.rights_fds().unwrap_err(), Errno::EINVAL);

    // SCM_CREDENTIALS requires exact 12-byte payload (net/core/scm.c __scm_send) and signed pid.
    let cred = Ucred { pid: -1, uid: 1000, gid: 1000 };
    let cred_bytes = cred.to_bytes();
    assert_eq!(Cmsg { level: SOL_SOCKET, cmsg_type: SCM_CREDENTIALS, data: &cred_bytes }.decode_creds(), Ok(cred));
    assert_eq!(
        Cmsg { level: SOL_SOCKET, cmsg_type: SCM_CREDENTIALS, data: &cred_bytes[..11] }.decode_creds(),
        Err(Errno::EINVAL)
    );
    let mut too_big = cred_bytes.to_vec();
    too_big.push(0);
    assert_eq!(
        Cmsg { level: SOL_SOCKET, cmsg_type: SCM_CREDENTIALS, data: &too_big }.decode_creds(),
        Err(Errno::EINVAL)
    );
}

// ---- CmsgWriter: rights_capacity, EMFILE total truncation, padding hygiene, combined round trips

#[test]
fn cmsg_writer_push_rights_capacity_and_truncation_for_every_buffer_size() {
    // Empty fds with total == 0: writes nothing, never sets MSG_CTRUNC even on 0-byte buffer.
    let mut empty = [];
    let mut w = CmsgWriter::new(&mut empty);
    assert_eq!(w.rights_capacity(), 0);
    assert_eq!(w.push_rights(&[], 0), 0);
    assert_eq!(w.finish(), (0, 0));

    // EMFILE partway through installation (net/core/scm.c scm_detach_fds):
    // buffer has plenty of room, 3 pending fds, but only 2 could be installed -> MSG_CTRUNC set.
    let mut big = [0xeeu8; 64];
    let mut w = CmsgWriter::new(&mut big);
    assert_eq!(w.rights_capacity(), (64 - 16) / 4);
    assert_eq!(w.push_rights(&[10, 11], 3), 2);
    assert_eq!(w.finish(), (24, MSG_CTRUNC));

    // EMFILE on the very first fd (0 installed out of 2 pending) -> nothing written, MSG_CTRUNC set.
    let mut big2 = [0xeeu8; 64];
    let mut w = CmsgWriter::new(&mut big2);
    assert_eq!(w.push_rights(&[], 2), 0);
    assert_eq!(w.finish(), (0, MSG_CTRUNC));
    assert!(big2.iter().all(|&b| b == 0xee));

    // Explicit mark_truncated().
    let mut scratch = [0u8; 32];
    let mut w = CmsgWriter::new(&mut scratch);
    assert!(!w.truncated());
    w.mark_truncated();
    assert!(w.truncated());
    assert_eq!(w.finish(), (0, MSG_CTRUNC));

    let fds = [3, 4, 5];
    for cap in 0..=40 {
        let mut buf = vec![0xeeu8; cap];
        let mut w = CmsgWriter::new(&mut buf);
        let expected_cap = cap.saturating_sub(CMSG_HDRLEN) / 4;
        assert_eq!(w.rights_capacity(), expected_cap, "cap={cap}");
        let installed = &fds[..fds.len().min(w.rights_capacity())];
        let fit = w.push_rights(installed, fds.len());
        let (written, flags) = w.finish();

        let expected_fit = expected_cap.min(fds.len());
        assert_eq!(fit, expected_fit, "cap={cap}");

        if expected_fit == 0 {
            assert_eq!(written, 0, "cap={cap}");
            assert_eq!(flags, MSG_CTRUNC, "cap={cap}");
            assert!(buf.iter().all(|&b| b == 0xee));
        } else {
            let expected_written = cmsg_space(expected_fit * 4).unwrap().min(cap);
            assert_eq!(written, expected_written, "cap={cap}");
            let expected_flags = if expected_fit < fds.len() { MSG_CTRUNC } else { 0 };
            assert_eq!(flags, expected_flags, "cap={cap}");
            let data_end = CMSG_HDRLEN + expected_fit * 4;
            assert!(buf[data_end..written].iter().all(|&b| b == 0), "cap={cap}");
            assert!(buf[written..].iter().all(|&b| b == 0xee), "cap={cap}");
            let msgs: Vec<_> = CmsgIter::new(&buf[..written]).map(|r| r.unwrap()).collect();
            assert_eq!(msgs.len(), 1);
            assert_eq!(msgs[0].rights_fds().unwrap().collect::<Vec<_>>(), &fds[..expected_fit]);
        }
    }
}

#[test]
fn cmsg_writer_push_creds_truncation_and_combined_round_trip() {
    let creds = Ucred { pid: 1234, uid: 0, gid: 5 };
    let cred_raw = creds.to_bytes();

    for cap in 0..=36 {
        let mut buf = vec![0xdd; cap];
        let mut w = CmsgWriter::new(&mut buf);
        let ok = w.push_creds(creds);
        let (written, flags) = w.finish();

        if cap < CMSG_HDRLEN {
            assert!(!ok);
            assert_eq!(written, 0);
            assert_eq!(flags, MSG_CTRUNC);
        } else if cap < CMSG_HDRLEN + UCRED_LEN {
            assert!(!ok);
            assert_eq!(written, cap);
            assert_eq!(flags, MSG_CTRUNC);
            let msgs: Vec<_> = CmsgIter::new(&buf[..written]).map(|r| r.unwrap()).collect();
            assert_eq!(msgs.len(), 1);
            assert_eq!(msgs[0].data, &cred_raw[..cap - CMSG_HDRLEN]);
        } else {
            assert!(ok);
            let expected_written = cmsg_space(UCRED_LEN).unwrap().min(cap);
            assert_eq!(written, expected_written);
            assert_eq!(flags, 0);
            assert!(buf[28..written].iter().all(|&b| b == 0));
            assert!(buf[written..].iter().all(|&b| b == 0xdd));
            let msgs: Vec<_> = CmsgIter::new(&buf[..written]).map(|r| r.unwrap()).collect();
            assert_eq!(msgs.len(), 1);
            assert_eq!(msgs[0].decode_creds(), Ok(creds));
        }
    }

    // Back-to-back: creds (32 bytes) + 2 rights (24 bytes) = 56 bytes.
    let mut buf = [0xaa; 64];
    let mut w = CmsgWriter::new(&mut buf);
    assert!(w.push_creds(creds));
    assert_eq!(w.rights_capacity(), 4);
    assert_eq!(w.push_rights(&[7, 8], 2), 2);
    assert!(!w.truncated());
    let (written, flags) = w.finish();
    assert_eq!((written, flags), (56, 0));
    let msgs: Vec<_> = CmsgIter::new(&buf[..written]).map(|r| r.unwrap()).collect();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].decode_creds(), Ok(creds));
    assert_eq!(msgs[1].rights_fds().unwrap().collect::<Vec<_>>(), vec![7, 8]);

    // Room for creds (32 bytes) + only 1 of 2 rights (20 bytes) = 52 bytes -> MSG_CTRUNC.
    let mut tight = [0xaa; 52];
    let mut w = CmsgWriter::new(&mut tight);
    assert!(w.push_creds(creds));
    assert_eq!(w.rights_capacity(), 1);
    assert_eq!(w.push_rights(&[7], 2), 1);
    assert!(w.truncated());
    let (written, flags) = w.finish();
    assert_eq!((written, flags), (52, MSG_CTRUNC));
    let msgs: Vec<_> = CmsgIter::new(&tight[..written]).map(|r| r.unwrap()).collect();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].decode_creds(), Ok(creds));
    assert_eq!(msgs[1].rights_fds().unwrap().collect::<Vec<_>>(), vec![7]);
}

// ---- Option values, Timeout, M9 census option table, get-only options, string & 1-byte IP options

#[test]
fn option_value_lengths_and_timeout_semantics() {
    // Int: < 4 -> EINVAL (including 3-byte boundary), >= 4 reads LE i32 (net/core/sock.c sk_setsockopt).
    for len in 0..4 {
        assert_eq!(OptVal::int(&vec![1; len]), Err(Errno::EINVAL), "len={len}");
    }
    assert_eq!(OptVal::int(&(-5i32).to_le_bytes()), Ok(-5));
    let mut extra = 42i32.to_le_bytes().to_vec();
    extra.extend_from_slice(&[0xff, 0xff]);
    assert_eq!(OptVal::int(&extra), Ok(42));

    // Linger: < 8 -> EINVAL, >= 8 reads two LE i32s.
    for len in 0..LINGER_LEN {
        assert_eq!(OptVal::linger(&vec![0; len]), Err(Errno::EINVAL));
    }
    let ling = Linger { onoff: 1, linger: 30 };
    assert_eq!(OptVal::linger(&ling.to_bytes()), Ok(ling));

    // Timeval & Timeout (Linux v6.12 net/core/sock.c sock_set_timeout / sock_get_timeout):
    for len in 0..TIMEVAL_LEN {
        assert_eq!(OptVal::timeval(&vec![0; len]), Err(Errno::EINVAL));
        assert_eq!(OptVal::timeout(&vec![0; len]), Err(Errno::EINVAL));
    }
    assert_eq!(MAX_TIMEOUT_SECS, i64::MAX / TIMEOUT_HZ - 1);

    let tv_imm = Timeval { tv_sec: -1, tv_usec: 0 };
    assert_eq!(OptVal::timeval(&tv_imm.to_bytes()), Ok(tv_imm));
    assert_eq!(OptVal::timeout(&tv_imm.to_bytes()), Ok(Timeout::Immediate));
    assert_eq!(Timeout::Immediate.to_timeval(), Timeval { tv_sec: 0, tv_usec: 0 });

    let tv_inf = Timeval { tv_sec: 0, tv_usec: 0 };
    assert_eq!(OptVal::timeout(&tv_inf.to_bytes()), Ok(Timeout::Infinite));
    assert_eq!(Timeout::Infinite.to_timeval(), Timeval { tv_sec: 0, tv_usec: 0 });

    // Whole-second timeout `{5, 0}` must be Duration(5s), never Infinite.
    let tv_five = Timeval { tv_sec: 5, tv_usec: 0 };
    assert_eq!(OptVal::timeout(&tv_five.to_bytes()), Ok(Timeout::Duration(Duration::from_secs(5))));
    assert_eq!(Timeout::Duration(Duration::from_secs(5)).to_timeval(), tv_five);

    let tv_dur = Timeval { tv_sec: 1, tv_usec: 500_000 };
    assert_eq!(OptVal::timeout(&tv_dur.to_bytes()), Ok(Timeout::Duration(Duration::new(1, 500_000_000))));
    assert_eq!(Timeout::Duration(Duration::new(1, 500_000_000)).to_timeval(), tv_dur);

    let tv_micro = Timeval { tv_sec: 0, tv_usec: 1 };
    assert_eq!(tv_micro.to_timeout(), Ok(Timeout::Duration(Duration::from_micros(1))));
    // Sub-microsecond nanoseconds round up (DIV_ROUND_UP), carrying across 1e6.
    assert_eq!(Timeout::Duration(Duration::from_nanos(1)).to_timeval(), Timeval { tv_sec: 0, tv_usec: 1 });
    assert_eq!(Timeout::Duration(Duration::new(2, 999_999_001)).to_timeval(), Timeval { tv_sec: 3, tv_usec: 0 });

    // Cap boundary (sock_set_timeout: tv_sec < MAX_SCHEDULE_TIMEOUT/HZ - 1 -> finite, else Infinite).
    let at_cap_minus_1 = Timeval { tv_sec: MAX_TIMEOUT_SECS - 1, tv_usec: 999_999 };
    assert_eq!(
        at_cap_minus_1.to_timeout(),
        Ok(Timeout::Duration(Duration::new((MAX_TIMEOUT_SECS - 1) as u64, 999_999_000)))
    );
    let at_cap = Timeval { tv_sec: MAX_TIMEOUT_SECS, tv_usec: 0 };
    assert_eq!(at_cap.to_timeout(), Ok(Timeout::Infinite));
    assert_eq!(Timeval { tv_sec: i64::MAX, tv_usec: 0 }.to_timeout(), Ok(Timeout::Infinite));

    for bad_usec in [-1, i64::MIN, 1_000_000, i64::MAX] {
        let bad = Timeval { tv_sec: 1, tv_usec: bad_usec };
        assert_eq!(OptVal::timeval(&bad.to_bytes()), Err(Errno::EDOM), "bad_usec={bad_usec}");
        assert_eq!(OptVal::timeout(&bad.to_bytes()), Err(Errno::EDOM), "bad_usec={bad_usec}");
    }

    // Ucred: < 12 -> EINVAL, >= 12 reads signed pid + u32 uid/gid.
    for len in 0..UCRED_LEN {
        assert_eq!(OptVal::ucred(&vec![0; len]), Err(Errno::EINVAL));
    }
    let cred = Ucred { pid: -42, uid: 2, gid: 3 };
    assert_eq!(OptVal::ucred(&cred.to_bytes()), Ok(cred));
}

#[test]
fn option_table_decodes_m9_census_get_only_string_and_ip_byte_options() {
    let one = 1i32.to_le_bytes();
    let val = 65536i32.to_le_bytes();

    // Every settable setsockopt pair observed in the M9 census (#460).
    let census_settable_pairs = [
        (SOL_SOCKET, abi::SO_REUSEADDR, SockOpt::ReuseAddr, &one),
        (SOL_SOCKET, abi::SO_BROADCAST, SockOpt::Broadcast, &one),
        (SOL_SOCKET, abi::SO_RCVBUF, SockOpt::RcvBuf, &val),
        (SOL_SOCKET, abi::SO_KEEPALIVE, SockOpt::KeepAlive, &one),
        (IPPROTO_TCP, abi::TCP_NODELAY, SockOpt::TcpNoDelay, &one),
        (IPPROTO_TCP, abi::TCP_CORK, SockOpt::TcpCork, &one),
        (IPPROTO_TCP, abi::TCP_KEEPIDLE, SockOpt::TcpKeepIdle, &val),
        (IPPROTO_TCP, abi::TCP_KEEPINTVL, SockOpt::TcpKeepIntvl, &val),
        (IPPROTO_TCP, abi::TCP_KEEPCNT, SockOpt::TcpKeepCnt, &val),
    ];
    for (level, name, expected_opt, bytes) in census_settable_pairs {
        assert_eq!(SockOpt::from_raw(level, name), Ok(expected_opt));
        assert!(expected_opt.settable());
        assert_eq!(expected_opt.kind(), OptKind::Int);
        let want_int = i32::from_le_bytes(*bytes);
        assert_eq!(decode_opt(level, name, bytes), Ok((expected_opt, OptVal::Int(want_int))));
        assert_eq!(decode_opt(level, name, &bytes[..3]), Err(Errno::EINVAL));
    }

    // Get-only options (Linux v6.12 net/core/sock.c sk_setsockopt):
    // SO_TYPE, SO_ERROR, SO_PEERCRED, SO_SNDLOWAT, SO_ACCEPTCONN, SO_PROTOCOL, SO_DOMAIN.
    let get_only = [
        (abi::SO_TYPE, SockOpt::Type, OptKind::Int),
        (abi::SO_ERROR, SockOpt::Error, OptKind::Int),
        (abi::SO_PEERCRED, SockOpt::PeerCred, OptKind::Ucred),
        (abi::SO_SNDLOWAT, SockOpt::SndLowat, OptKind::Int),
        (abi::SO_ACCEPTCONN, SockOpt::AcceptConn, OptKind::Int),
        (abi::SO_PROTOCOL, SockOpt::Protocol, OptKind::Int),
        (abi::SO_DOMAIN, SockOpt::Domain, OptKind::Int),
    ];
    for (raw_name, expected_opt, expected_kind) in get_only {
        assert_eq!(SockOpt::from_raw(SOL_SOCKET, raw_name), Ok(expected_opt));
        assert!(!expected_opt.settable());
        assert_eq!(expected_opt.kind(), expected_kind);
        for short_len in 0..4 {
            assert_eq!(
                decode_opt(SOL_SOCKET, raw_name, &vec![0u8; short_len]),
                Err(Errno::EINVAL),
                "opt={expected_opt:?} len={short_len}"
            );
        }
        assert_eq!(decode_opt(SOL_SOCKET, raw_name, &one), Err(Errno::ENOPROTOOPT));
        assert_eq!(decode_opt(SOL_SOCKET, raw_name, &[0u8; 16]), Err(Errno::ENOPROTOOPT));
    }

    // String options skip the optlen < 4 check and return ENOPROTOOPT at any length.
    for (level, optname) in [
        (SOL_SOCKET, abi::SO_BINDTODEVICE),
        (IPPROTO_TCP, abi::TCP_CONGESTION),
        (IPPROTO_TCP, abi::TCP_ULP),
        (IPPROTO_TCP, abi::TCP_FASTOPEN_KEY),
    ] {
        for len in 0..=4 {
            assert_eq!(
                decode_opt(level, optname, &vec![b'a'; len]),
                Err(Errno::ENOPROTOOPT),
                "level={level} optname={optname} len={len}"
            );
        }
    }

    // SO_RCVTIMEO / SO_SNDTIMEO old (20/21) and new (66/67) decode to OptVal::Timeout.
    let tv = Timeval { tv_sec: 2, tv_usec: 250_000 };
    let want_to = Timeout::Duration(Duration::new(2, 250_000_000));
    let tv_bytes = tv.to_bytes();
    assert_eq!(
        decode_opt(SOL_SOCKET, abi::SO_RCVTIMEO_OLD, &tv_bytes),
        Ok((SockOpt::RcvTimeo, OptVal::Timeout(want_to)))
    );
    assert_eq!(
        decode_opt(SOL_SOCKET, abi::SO_RCVTIMEO_NEW, &tv_bytes),
        Ok((SockOpt::RcvTimeo, OptVal::Timeout(want_to)))
    );
    assert_eq!(
        decode_opt(SOL_SOCKET, abi::SO_SNDTIMEO_OLD, &tv_bytes),
        Ok((SockOpt::SndTimeo, OptVal::Timeout(want_to)))
    );
    assert_eq!(
        decode_opt(SOL_SOCKET, abi::SO_SNDTIMEO_NEW, &tv_bytes),
        Ok((SockOpt::SndTimeo, OptVal::Timeout(want_to)))
    );

    // SO_LINGER.
    let ling = Linger { onoff: 1, linger: 5 };
    assert_eq!(decode_opt(SOL_SOCKET, abi::SO_LINGER, &ling.to_bytes()), Ok((SockOpt::Linger, OptVal::Linger(ling))));
    assert_eq!(decode_opt(SOL_SOCKET, abi::SO_LINGER, &one), Err(Errno::EINVAL));

    // IPPROTO_IP 1-byte/int options (Linux v6.12 net/ipv4/ip_sockglue.c do_ip_setsockopt):
    // IP_TTL with optlen 0 -> EINVAL (`if (optlen < 1)`), while IP_TOS/IP_RECVERR with optlen 0 -> 0.
    assert_eq!(decode_opt(IPPROTO_IP, abi::IP_TTL, &[]), Err(Errno::EINVAL));
    for (raw_name, expected_opt) in [(abi::IP_TOS, SockOpt::IpTos), (abi::IP_RECVERR, SockOpt::IpRecvErr)] {
        assert_eq!(decode_opt(IPPROTO_IP, raw_name, &[]), Ok((expected_opt, OptVal::IntOrByte(0))));
    }
    for (raw_name, expected_opt) in
        [(abi::IP_TOS, SockOpt::IpTos), (abi::IP_TTL, SockOpt::IpTtl), (abi::IP_RECVERR, SockOpt::IpRecvErr)]
    {
        assert_eq!(expected_opt.kind(), OptKind::IntOrByte);
        assert_eq!(decode_opt(IPPROTO_IP, raw_name, &[0xff]), Ok((expected_opt, OptVal::IntOrByte(255))));
        assert_eq!(decode_opt(IPPROTO_IP, raw_name, &[0x40, 0xaa, 0xbb]), Ok((expected_opt, OptVal::IntOrByte(0x40))));
        assert_eq!(decode_opt(IPPROTO_IP, raw_name, &val), Ok((expected_opt, OptVal::IntOrByte(65536))));
    }

    // IPv6 option.
    assert_eq!(decode_opt(IPPROTO_IPV6, abi::IPV6_V6ONLY, &one), Ok((SockOpt::Ipv6V6Only, OptVal::Int(1))));

    // Unknown IPPROTO_IP / IPPROTO_IPV6 options return ENOPROTOOPT at optlen 0..=3 (and >= 4).
    for level in [IPPROTO_IP, IPPROTO_IPV6] {
        for len in 0..=4 {
            assert_eq!(decode_opt(level, 9999, &vec![1u8; len]), Err(Errno::ENOPROTOOPT), "level={level} len={len}");
        }
    }

    // Linux sk_setsockopt / do_tcp_setsockopt check optlen < 4 (including 3 bytes!) before unknown optname.
    for level in [SOL_SOCKET, IPPROTO_TCP] {
        for short_len in 0..4 {
            assert_eq!(
                decode_opt(level, 9999, &vec![1u8; short_len]),
                Err(Errno::EINVAL),
                "level={level} len={short_len}"
            );
        }
        assert_eq!(decode_opt(level, 9999, &one), Err(Errno::ENOPROTOOPT));
    }
    // Unknown level returns ENOPROTOOPT immediately regardless of optlen.
    assert_eq!(decode_opt(999, 1, &[]), Err(Errno::ENOPROTOOPT));
}

#[test]
fn write_opt_and_ip_byte_getsockopt_truncation() {
    assert_eq!(check_optlen(-1), Err(Errno::EINVAL));
    assert_eq!(check_optlen(i32::MIN), Err(Errno::EINVAL));
    assert_eq!(check_optlen(0), Ok(0));
    assert_eq!(check_optlen(16), Ok(16));

    let values = [
        (OptVal::Int(0x1122_3344), 0x1122_3344i32.to_le_bytes().to_vec()),
        (OptVal::Linger(Linger { onoff: 1, linger: 10 }), Linger { onoff: 1, linger: 10 }.to_bytes().to_vec()),
        (OptVal::Ucred(Ucred { pid: 7, uid: 8, gid: 9 }), Ucred { pid: 7, uid: 8, gid: 9 }.to_bytes().to_vec()),
        (
            OptVal::Timeout(Timeout::Duration(Duration::new(12, 345_000))),
            Timeval { tv_sec: 12, tv_usec: 345 }.to_bytes().to_vec(),
        ),
        (OptVal::Timeout(Timeout::Infinite), Timeval { tv_sec: 0, tv_usec: 0 }.to_bytes().to_vec()),
        (OptVal::Timeout(Timeout::Immediate), Timeval { tv_sec: 0, tv_usec: 0 }.to_bytes().to_vec()),
    ];

    for (val, expected) in values {
        assert_eq!(val.size(), expected.len());
        for optlen in 0..=MAX_OPT_LEN + 4 {
            let (buf, written) = write_opt(val, optlen);
            let want_written = optlen.min(expected.len());
            assert_eq!(written, want_written, "val={val:?} optlen={optlen}");
            assert_eq!(&buf[..written], &expected[..written]);
            assert!(buf[written..].iter().all(|&b| b == 0));
        }
    }

    // Decoded IP_TOS carries OptVal::IntOrByte so write_opt cannot lose the 1-byte rule.
    let (opt, decoded_tos) = decode_opt(IPPROTO_IP, abi::IP_TOS, &[0x28]).unwrap();
    assert_eq!(opt, SockOpt::IpTos);
    let (tos_buf, tos_len) = write_opt(decoded_tos, 3);
    assert_eq!((tos_buf[0], tos_len), (0x28, 1));

    // Linux v6.12 net/ipv4/ip_sockglue.c (do_ip_getsockopt):
    // 0 and 255 use the 1-byte path (written = 1 for optlen 1..3); 256 and -1 use the 4-byte path.
    assert_eq!(write_opt_for(SockOpt::IpTos, OptVal::Int(0), 0), ([0; MAX_OPT_LEN], 0));
    for edge in [0i32, 255] {
        for optlen in 1..4 {
            let (buf, written) = write_opt_for(SockOpt::IpTos, OptVal::Int(edge), optlen);
            assert_eq!(written, 1, "edge={edge} optlen={optlen}");
            assert_eq!(buf[0], edge as u8);
            assert!(buf[1..].iter().all(|&b| b == 0));
        }
        let (buf4, written4) = write_opt_for(SockOpt::IpTos, OptVal::Int(edge), 4);
        assert_eq!(written4, 4);
        assert_eq!(&buf4[..4], &edge.to_le_bytes());
    }

    let (buf_256, written_256) = write_opt_for(SockOpt::IpTtl, OptVal::Int(256), 3);
    assert_eq!(written_256, 3);
    assert_eq!(&buf_256[..3], &[0x00, 0x01, 0x00]);
    let (buf_neg, written_neg) = write_opt_for(SockOpt::IpTtl, OptVal::Int(-1), 2);
    assert_eq!(written_neg, 2);
    assert_eq!(&buf_neg[..2], &[0xff, 0xff]);
}

// ---- Fuzz-style randomized robustness test (never panics, exercises success paths)

#[test]
fn random_bytes_never_panic_and_exercise_success_paths() {
    let mut rng = Rng(0x50c4_ab10_cafe_babe);
    let mut raw = [0u8; 256];

    let mut inet_ok = 0usize;
    let mut inet6_ok = 0usize;
    let mut unix_ok = 0usize;
    let mut cmsg_ok = 0usize;
    let mut cmsg_err = 0usize;
    let mut opt_ok = 0usize;

    for iter in 0..4000usize {
        let len = (rng.next() as usize) % (raw.len() + 1);
        let slice = &mut raw[..len];
        rng.fill(slice);

        // 1. SockAddr::decode + decode_dest + encode round-trip check across all families and calls.
        let _ = peek_family(slice);
        for family in [Family::Inet, Family::Inet6, Family::Unix] {
            let call = ALL_CALLS[(rng.next() as usize) % ALL_CALLS.len()];
            if iter % 4 != 0 && slice.len() >= 2 {
                let fam = if family == Family::Inet && (rng.next() & 7) == 0 { AF_UNSPEC } else { family.to_raw() };
                slice[0..2].copy_from_slice(&fam.to_le_bytes());
            }
            let _ = SockAddr::decode_dest(family, call, slice);
            if let Ok(addr) = SockAddr::decode(family, call, slice) {
                match addr {
                    SockAddr::Inet(_) => inet_ok += 1,
                    SockAddr::Inet6 { .. } => inet6_ok += 1,
                    SockAddr::Unix(_) => unix_ok += 1,
                }
                assert_eq!(addr.family(), family);
                let out_len = (rng.next() as usize) % 140;
                let mut out = vec![0u8; out_len];
                let true_len = addr.encode(&mut out);
                assert_eq!(true_len, addr.encoded_len());
                let mut full = [0u8; SOCKADDR_STORAGE_LEN];
                let n = addr.encode(&mut full);
                let redecode_len = if family == Family::Unix { n.min(SOCKADDR_UN_LEN) } else { n };
                assert_eq!(SockAddr::decode(family, Call::Bind, &full[..redecode_len]), Ok(addr));
            }
        }

        // 2. CmsgIter over both raw noise and chained headers with cmsg_len in 16..=rem+8.
        let mut cbuf = Vec::new();
        if iter % 4 == 0 {
            cbuf.extend_from_slice(slice);
        } else {
            let total = (rng.next() as usize) % 192;
            cbuf.resize(total, 0);
            rng.fill(&mut cbuf);
            let mut off = 0usize;
            while off + CMSG_HDRLEN <= cbuf.len() {
                let rem = cbuf.len() - off;
                let span = (rem + 8).saturating_sub(CMSG_HDRLEN) + 1;
                let clen = CMSG_HDRLEN + ((rng.next() as usize) % span);
                let level = if (rng.next() & 1) == 0 { SOL_SOCKET } else { (rng.next() as i32) % 50 };
                let ctype = [SCM_RIGHTS, SCM_CREDENTIALS, 99][(rng.next() as usize) % 3];
                cbuf[off..off + 8].copy_from_slice(&(clen as u64).to_le_bytes());
                cbuf[off + 8..off + 12].copy_from_slice(&level.to_le_bytes());
                cbuf[off + 12..off + 16].copy_from_slice(&ctype.to_le_bytes());
                match cmsg_align(clen).and_then(|step| off.checked_add(step)) {
                    Some(next) if next > off && next <= cbuf.len() => off = next,
                    _ => break,
                }
            }
        }
        let mut steps = 0usize;
        let max_steps = cbuf.len() / CMSG_HDRLEN + 1;
        for item in CmsgIter::new(&cbuf) {
            steps += 1;
            assert!(steps <= max_steps);
            match item {
                Ok(cmsg) => {
                    cmsg_ok += 1;
                    let _ = cmsg.rights_fds().map(|it| it.count());
                    let _ = cmsg.decode_creds();
                }
                Err(_) => cmsg_err += 1,
            }
        }
        let _ = count_rights_fds(&cbuf);

        // 3. CmsgWriter with random buffer size, pre-capped installed fds, and total >= installed.
        let wcap = (rng.next() as usize) % 128;
        let mut wbuf = vec![0u8; wcap];
        let nfds = (rng.next() as usize) % 16;
        let total_fds = nfds + ((rng.next() as usize) % 4);
        let fds: Vec<i32> = (0..nfds).map(|i| i as i32).collect();
        let mut writer = CmsgWriter::new(&mut wbuf);
        let _ = writer.push_creds(Ucred { pid: 1, uid: 2, gid: 3 });
        let cap_before = writer.rights_capacity();
        let installed = &fds[..fds.len().min(cap_before)];
        let fit = writer.push_rights(installed, total_fds);
        assert_eq!(fit, installed.len());
        let _ = writer.push_raw(SOL_SOCKET, 99, &slice[..len.min(32)]);
        let (written, _flags) = writer.finish();
        assert!(written <= wcap);
        for item in CmsgIter::new(&wbuf[..written]) {
            assert!(item.is_ok(), "CmsgWriter output must always parse cleanly");
        }

        // 4. Option decoders and write_opt / write_opt_for.
        let _ = OptVal::int(slice);
        let _ = OptVal::int_or_byte(slice);
        let _ = OptVal::linger(slice);
        let _ = OptVal::timeval(slice);
        let _ = OptVal::timeout(slice);
        let _ = OptVal::ucred(slice);
        let level = [SOL_SOCKET, IPPROTO_TCP, IPPROTO_IP, IPPROTO_IPV6, 999][(rng.next() as usize) % 5];
        let optname = (rng.next() % 70) as i32;
        if let Ok((opt, val)) = decode_opt(level, optname, slice) {
            opt_ok += 1;
            let optlen = (rng.next() as usize) % 32;
            let (_, written) = write_opt_for(opt, val, optlen);
            assert!(written <= MAX_OPT_LEN);
        }

        // 5. Fixed-size wire structs.
        if len >= MSGHDR_LEN {
            let arr: &[u8; MSGHDR_LEN] = slice[..MSGHDR_LEN].try_into().unwrap();
            let m = Msghdr::from_bytes(arr);
            assert_eq!(Msghdr::from_bytes(&m.to_bytes()), m);
        }
        if len >= EPOLL_EVENT_LEN {
            let arr: &[u8; EPOLL_EVENT_LEN] = slice[..EPOLL_EVENT_LEN].try_into().unwrap();
            let e = EpollEvent::from_bytes(arr);
            assert_eq!(EpollEvent::from_bytes(&e.to_bytes()), e);
        }
    }

    // Assert meaningful floors so the fuzz test actually exercises the success paths.
    assert!(inet_ok >= 1000, "inet_ok={inet_ok}");
    assert!(inet6_ok >= 1000, "inet6_ok={inet6_ok}");
    assert!(unix_ok >= 800, "unix_ok={unix_ok}");
    assert!(cmsg_ok >= 2000, "cmsg_ok={cmsg_ok}");
    assert!(cmsg_err >= 300, "cmsg_err={cmsg_err}");
    assert!(opt_ok >= 300, "opt_ok={opt_ok}");
}
