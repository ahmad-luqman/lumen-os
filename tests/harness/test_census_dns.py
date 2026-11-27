"""Host tests for the M9 census loopback UDP DNS responder (`tools/census/local_server.py`)
and `tools/census/m9-workloads.sh`'s per-workload port-53 reply guard (#451)."""

from __future__ import annotations

import importlib.util
import re
import socket
import struct
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[2]
LOCAL_SERVER_PY = ROOT / "tools/census/local_server.py"
M9_WORKLOADS_SH = ROOT / "tools/census/m9-workloads.sh"


def _load_local_server():
    spec = importlib.util.spec_from_file_location("census_local_server", LOCAL_SERVER_PY)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


local_server = _load_local_server()


def _encode_qname(name: str) -> bytes:
    return b"".join(bytes([len(part)]) + part.encode("ascii") for part in name.split(".")) + b"\x00"


def _build_query(
    name: str,
    qtype: int,
    *,
    qclass: int = 1,
    txid: int = 0x1234,
    rd: bool = True,
    edns0: bool = False,
) -> bytes:
    flags = 0x0100 if rd else 0x0000
    arcount = 1 if edns0 else 0
    header = struct.pack("!HHHHHH", txid, flags, 1, 0, 0, arcount)
    question = _encode_qname(name) + struct.pack("!HH", qtype, qclass)
    extra = b"\x00" + struct.pack("!HHIH", 41, 4096, 0, 0) if edns0 else b""
    return header + question + extra


def test_dns_reply_answers_a_aaaa_nodata_nxdomain_edns0_case_and_rd():
    # A query for census.test -> 127.0.0.1.
    q_a = _build_query("census.test", 1, txid=0xABCD, rd=True)
    r_a = local_server.dns_reply(q_a, ipv6=True)
    assert r_a is not None
    txid, flags, qd, an, ns, ar = struct.unpack("!HHHHHH", r_a[:12])
    assert (txid, qd, an, ns, ar) == (0xABCD, 1, 1, 0, 0)
    assert flags & 0x8000  # QR=1
    assert flags & 0x0400  # AA=1
    assert flags & 0x0100  # RD=1 preserved
    assert flags & 0x000F == 0  # NOERROR
    assert r_a[-4:] == socket.inet_aton("127.0.0.1")

    # AAAA query -> ::1 when ipv6=True, NODATA when ipv6=False.
    q_aaaa = _build_query("census.test", 28, txid=0x2222, rd=False)
    r_aaaa = local_server.dns_reply(q_aaaa, ipv6=True)
    assert r_aaaa is not None
    txid, flags, qd, an, ns, ar = struct.unpack("!HHHHHH", r_aaaa[:12])
    assert (txid, qd, an, ns, ar) == (0x2222, 1, 1, 0, 0)
    assert flags & 0x0100 == 0  # RD=0 preserved
    assert flags & 0x000F == 0
    assert r_aaaa[-16:] == socket.inet_pton(socket.AF_INET6, "::1")

    r_nodata = local_server.dns_reply(q_aaaa, ipv6=False)
    assert r_nodata is not None
    _, flags, qd, an, ns, ar = struct.unpack("!HHHHHH", r_nodata[:12])
    assert (qd, an, ns, ar) == (1, 0, 0, 0)
    assert flags & 0x000F == 0  # NOERROR NODATA

    # Unknown name -> NXDOMAIN (rcode=3, ancount=0).
    r_nx = local_server.dns_reply(_build_query("example.com", 1), ipv6=True)
    assert r_nx is not None
    _, flags, qd, an, ns, ar = struct.unpack("!HHHHHH", r_nx[:12])
    assert (qd, an, ns, ar) == (1, 0, 0, 0)
    assert flags & 0x000F == 3

    # EDNS0 query (arcount=1 with OPT pseudo-RR) is answered, additional records ignored.
    r_edns = local_server.dns_reply(_build_query("census.test", 1, edns0=True), ipv6=True)
    assert r_edns is not None
    _, flags, qd, an, ns, ar = struct.unpack("!HHHHHH", r_edns[:12])
    assert (qd, an, ns, ar) == (1, 1, 0, 0)
    assert flags & 0x000F == 0
    assert r_edns[-4:] == socket.inet_aton("127.0.0.1")

    # Mixed case matches and echoes the exact question wire bytes from the query.
    q_mixed = _build_query("CeNsUs.TeSt", 1, txid=0x5555)
    question_bytes = q_mixed[12:]
    r_mixed = local_server.dns_reply(q_mixed, ipv6=True)
    assert r_mixed is not None
    assert r_mixed[12 : 12 + len(question_bytes)] == question_bytes
    assert struct.unpack("!HHHHHH", r_mixed[:12])[3] == 1
    assert r_mixed[-4:] == socket.inet_aton("127.0.0.1")


@pytest.mark.parametrize(
    "bad_query",
    [
        b"",
        b"\x00" * 11,  # truncated header
        struct.pack("!HHHHHH", 1, 0x0100, 0, 0, 0, 0),  # qdcount=0
        struct.pack("!HHHHHH", 1, 0x0100, 1, 0, 0, 0) + b"\x06censu",  # truncated label
        struct.pack("!HHHHHH", 1, 0x0100, 1, 0, 0, 0) + _encode_qname("census.test") + b"\x00\x01",
        struct.pack("!HHHHHH", 1, 0x0100, 1, 0, 0, 0) + b"\xc0\x0c\x00\x01\x00\x01",  # compressed
    ],
)
def test_dns_reply_returns_none_on_truncated_or_malformed_query(bad_query):
    assert local_server.dns_reply(bad_query, ipv6=True) is None


def test_m9_workloads_port_53_guard_matches_musl_and_cares_replies():
    script = M9_WORKLOADS_SH.read_text()
    m = re.search(r"grep -Eq '([^']+)' \"\$OUT/\$w\.strace\"", script)
    assert m is not None, "per-workload port-53 reply guard not found in m9-workloads.sh"
    pattern = re.compile(m.group(1))
    assert "htons" not in m.group(1)

    musl_reply = (
        '41    recvmsg(7, {msg_name={sa_family=0x2, sin_port="\\x00\\x35", '
        'sin_addr="\\x7f\\x00\\x00\\x01"}, msg_namelen=16, msg_iov=[...], '
        "msg_iovlen=1, msg_controllen=0, msg_flags=0}, 0) = 45"
    )
    cares_reply = (
        '49    recvfrom(4, ""..., 131069, 0, {sa_family=0x2, sin_port="\\x00\\x35", '
        'sin_addr="\\x7f\\x00\\x00\\x01"}, [128 => 16]) = 45'
    )
    send_only = (
        '41    sendto(7, ""..., 29, 0x4000, {sa_family=0x2, sin_port="\\x00\\x35", '
        'sin_addr="\\x7f\\x00\\x00\\x01"}, 16) = 29'
    )
    other_port = (
        '49    recvfrom(4, ""..., 1024, 0, {sa_family=0x2, sin_port="\\x01\\xbb", '
        'sin_addr="\\x7f\\x00\\x00\\x01"}, [128 => 16]) = 45'
    )
    assert pattern.search(musl_reply)
    assert pattern.search(cares_reply)
    assert not pattern.search(send_only)
    assert not pattern.search(other_port)
