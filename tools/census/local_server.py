"""Local HTTPS, HTTP and DNS server for the M9 syscall census (#451).

Runs inside the Alpine census container outside strace so traced clients
(`apk`, `curl`, `ruby`, `busybox wget`) talk only to loopback servers with zero
external network traffic. Binds ephemeral HTTP/HTTPS ports (dual-stack `::`
when IPv6 is available, else `127.0.0.1` with a notice on stderr), optionally
binds a UDP + TCP DNS responder on `127.0.0.1:<dns-port>` answering A/AAAA for
`census.test`, and atomically writes `HTTPS_PORT`, `HTTP_PORT` (and `DNS_PORT`)
to `--ports-file` only after every socket is listening.
"""

from __future__ import annotations

import argparse
import functools
import http.server
import os
import signal
import socket
import ssl
import struct
import sys
import threading
from pathlib import Path

DNS_NAME = "census.test"
_DNS_TYPE_A = 1
_DNS_TYPE_AAAA = 28
_DNS_CLASS_IN = 1


class QuietHandler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, format: str, *args: object) -> None:
        pass


class DualStackServer(http.server.ThreadingHTTPServer):
    address_family = socket.AF_INET6

    def server_bind(self) -> None:
        self.socket.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 0)
        super().server_bind()


def _make_server(
    handler: type[http.server.BaseHTTPRequestHandler],
) -> tuple[http.server.HTTPServer, bool]:
    try:
        return DualStackServer(("::", 0), handler), True
    except OSError as exc:
        print(
            f"local_server: IPv6 dual-stack bind failed ({exc}); falling back to IPv4 127.0.0.1",
            file=sys.stderr,
        )
        return http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler), False


def dns_reply(query: bytes, *, ipv6: bool = True) -> bytes | None:
    """Build an RFC 1035 DNS reply for `query`, answering `census.test` with
    A 127.0.0.1 and (when `ipv6` is true) AAAA ::1, NODATA for other types on
    `census.test`, and NXDOMAIN for every other name."""
    if len(query) < 12:
        return None
    txid, flags, qdcount, _an, _ns, _ar = struct.unpack("!HHHHHH", query[:12])
    if qdcount < 1:
        return None
    labels: list[bytes] = []
    off = 12
    while off < len(query):
        length = query[off]
        off += 1
        if length == 0:
            break
        if length & 0xC0 or off + length > len(query):
            return None
        labels.append(query[off : off + length])
        off += length
    else:
        return None
    if off + 4 > len(query):
        return None
    qtype, qclass = struct.unpack("!HH", query[off : off + 4])
    question = query[12 : off + 4]
    qname = b".".join(labels).decode("ascii", errors="replace").lower()

    rd = flags & 0x0100
    base_flags = 0x8480 | rd  # QR=1, AA=1, RA=1, preserve RD
    answer = b""
    ancount = 0
    rcode = 0
    if qname != DNS_NAME:
        rcode = 3  # NXDOMAIN
    elif qclass == _DNS_CLASS_IN and qtype == _DNS_TYPE_A:
        rdata = socket.inet_aton("127.0.0.1")
        answer = struct.pack("!HHHIH", 0xC00C, _DNS_TYPE_A, _DNS_CLASS_IN, 60, len(rdata)) + rdata
        ancount = 1
    elif qclass == _DNS_CLASS_IN and qtype == _DNS_TYPE_AAAA and ipv6:
        rdata = socket.inet_pton(socket.AF_INET6, "::1")
        answer = (
            struct.pack("!HHHIH", 0xC00C, _DNS_TYPE_AAAA, _DNS_CLASS_IN, 60, len(rdata)) + rdata
        )
        ancount = 1

    header = struct.pack("!HHHHHH", txid, base_flags | rcode, 1, ancount, 0, 0)
    return header + question + answer


def _serve_dns_udp(sock: socket.socket, stop: threading.Event, ipv6: bool) -> None:
    sock.settimeout(0.2)
    while not stop.is_set():
        try:
            data, peer = sock.recvfrom(4096)
        except TimeoutError:
            continue
        except OSError:
            break
        reply = dns_reply(data, ipv6=ipv6)
        if reply is not None:
            try:
                sock.sendto(reply, peer)
            except OSError:
                pass


def _recv_exact(conn: socket.socket, n: int) -> bytes | None:
    buf = bytearray()
    while len(buf) < n:
        chunk = conn.recv(n - len(buf))
        if not chunk:
            return None
        buf.extend(chunk)
    return bytes(buf)


def _handle_dns_tcp(conn: socket.socket, ipv6: bool) -> None:
    with conn:
        conn.settimeout(2.0)
        try:
            hdr = _recv_exact(conn, 2)
            if hdr is None:
                return
            (length,) = struct.unpack("!H", hdr)
            query = _recv_exact(conn, length)
            if query is None:
                return
            reply = dns_reply(query, ipv6=ipv6)
            if reply is not None:
                conn.sendall(struct.pack("!H", len(reply)) + reply)
        except OSError:
            pass


def _serve_dns_tcp(sock: socket.socket, stop: threading.Event, ipv6: bool) -> None:
    sock.settimeout(0.2)
    while not stop.is_set():
        try:
            conn, _peer = sock.accept()
        except TimeoutError:
            continue
        except OSError:
            break
        threading.Thread(target=_handle_dns_tcp, args=(conn, ipv6), daemon=True).start()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--cert", type=Path, required=True)
    parser.add_argument("--key", type=Path, required=True)
    parser.add_argument("--ports-file", type=Path, required=True)
    parser.add_argument("--dns-port", type=int, default=None)
    args = parser.parse_args()

    handler = functools.partial(QuietHandler, directory=str(args.root))
    http_srv, http_v6 = _make_server(handler)
    https_srv, https_v6 = _make_server(handler)
    ipv6_ready = http_v6 and https_v6

    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(certfile=str(args.cert), keyfile=str(args.key))
    https_srv.socket = ctx.wrap_socket(https_srv.socket, server_side=True)

    stop = threading.Event()

    def _on_signal(_signum: int, _frame: object) -> None:
        stop.set()

    signal.signal(signal.SIGTERM, _on_signal)
    signal.signal(signal.SIGINT, _on_signal)

    threads = [
        threading.Thread(target=http_srv.serve_forever, daemon=True),
        threading.Thread(target=https_srv.serve_forever, daemon=True),
    ]
    dns_sockets: list[socket.socket] = []
    dns_port_actual: int | None = None
    if args.dns_port is not None:
        udp_sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        udp_sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        udp_sock.bind(("127.0.0.1", args.dns_port))
        dns_port_actual = udp_sock.getsockname()[1]

        tcp_sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        tcp_sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        tcp_sock.bind(("127.0.0.1", dns_port_actual))
        tcp_sock.listen(16)

        dns_sockets.extend([udp_sock, tcp_sock])
        threads.append(
            threading.Thread(target=_serve_dns_udp, args=(udp_sock, stop, ipv6_ready), daemon=True)
        )
        threads.append(
            threading.Thread(target=_serve_dns_tcp, args=(tcp_sock, stop, ipv6_ready), daemon=True)
        )

    for t in threads:
        t.start()

    lines = [
        f"HTTPS_PORT={https_srv.server_port}",
        f"HTTP_PORT={http_srv.server_port}",
    ]
    if dns_port_actual is not None:
        lines.append(f"DNS_PORT={dns_port_actual}")
    tmp = args.ports_file.with_suffix(".tmp")
    tmp.write_text("\n".join(lines) + "\n")
    os.replace(tmp, args.ports_file)

    stop.wait()
    http_srv.shutdown()
    https_srv.shutdown()
    http_srv.server_close()
    https_srv.server_close()
    for sock in dns_sockets:
        sock.close()


if __name__ == "__main__":
    main()
