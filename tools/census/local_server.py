"""Local HTTPS and HTTP file server for the M9 syscall census (#451).

Runs inside the Alpine census container outside strace so traced clients
(`apk`, `curl`, `ruby`, `busybox wget`) talk only to loopback servers with zero
external network traffic. Binds ephemeral ports (dual-stack `::` when IPv6 is
available, else `127.0.0.1`) and atomically writes `HTTPS_PORT` and `HTTP_PORT`
to `--ports-file` only after both sockets are listening.
"""

from __future__ import annotations

import argparse
import functools
import http.server
import os
import signal
import socket
import ssl
import threading
from pathlib import Path


class QuietHandler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, format: str, *args: object) -> None:
        pass


class DualStackServer(http.server.ThreadingHTTPServer):
    address_family = socket.AF_INET6

    def server_bind(self) -> None:
        self.socket.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 0)
        super().server_bind()


def _make_server(handler: type[http.server.BaseHTTPRequestHandler]) -> http.server.HTTPServer:
    try:
        return DualStackServer(("::", 0), handler)
    except OSError:
        return http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--cert", type=Path, required=True)
    parser.add_argument("--key", type=Path, required=True)
    parser.add_argument("--ports-file", type=Path, required=True)
    args = parser.parse_args()

    handler = functools.partial(QuietHandler, directory=str(args.root))
    http_srv = _make_server(handler)
    https_srv = _make_server(handler)

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
    for t in threads:
        t.start()

    tmp = args.ports_file.with_suffix(".tmp")
    tmp.write_text(f"HTTPS_PORT={https_srv.server_port}\nHTTP_PORT={http_srv.server_port}\n")
    os.replace(tmp, args.ports_file)

    stop.wait()
    http_srv.shutdown()
    https_srv.shutdown()
    http_srv.server_close()
    https_srv.server_close()


if __name__ == "__main__":
    main()
