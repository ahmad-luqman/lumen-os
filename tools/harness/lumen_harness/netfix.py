"""Offline M9 services (#453), scoped to one context-managed fixture.

Use ``with NetFixture(artifacts=path) as net`` before
``Machine(devices=Devices(services=net.services, ...))``;
close the machine first. HTTP, HTTPS and TCP echo use private Unix sockets,
never host TCP ports. QEMU starts this file's --relay helper per guest TCP
connection. ``prepare_image`` copies an ext4 image and installs this run's
CA, apk public key, local repository URL and /etc/hosts entries into the copy.

DHCP stays with slirp. No host UDP service is offered: guestfwd is TCP only.
M9 scenarios resolve fixture names via /etc/hosts; DNS and UDP scenarios
must use guest loopback.
Setting QEMU's dns= changes the *guest-visible* proxy address, not its upstream
resolver. Verified in libslirp v4.7.0 and master: src/udp.c, udp_input,
allows DHCP and TFTP before dropping other restricted UDP; src/tcp_input.c
resets TCP outside configured guest forwards under restriction. Sources:
https://gitlab.freedesktop.org/slirp/libslirp/-/blob/v4.7.0/src/udp.c
https://gitlab.freedesktop.org/slirp/libslirp/-/blob/9741541ae91b1b4aa34288045b7e8e5039c8aa42/src/udp.c
https://gitlab.freedesktop.org/slirp/libslirp/-/blob/v4.7.0/src/tcp_input.c
https://gitlab.freedesktop.org/slirp/libslirp/-/blob/9741541ae91b1b4aa34288045b7e8e5039c8aa42/src/tcp_input.c
The packet test checks installed QEMU: Homebrew locally, Ubuntu 8.2.2 in CI.
"""

from __future__ import annotations

# libslirp fork_exec attaches stdin, stdout AND stderr to the guest socket.
# Redirect before argument parsing or service-module imports can emit errors.
# Parse --log here for that early redirect, then let argparse validate the full CLI.
import os
import sys

if __name__ == "__main__":
    try:
        log_path = sys.argv[sys.argv.index("--log") + 1]
        log_fd = os.open(log_path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    except (ValueError, IndexError, OSError):
        # Logging must neither corrupt nor refuse otherwise valid guest traffic.
        log_fd = os.open(os.devnull, os.O_WRONLY)
    os.dup2(log_fd, 2)
    if log_fd != 2:
        os.close(log_fd)

import argparse
import errno
import gzip
import hashlib
import http.server
import importlib.util
import io
import itertools
import logging
import re
import shutil
import socket
import socketserver
import ssl
import subprocess
import tarfile
import tempfile
import threading
from contextlib import ExitStack
from functools import lru_cache
from pathlib import Path
from types import MappingProxyType
from typing import Self

# The standalone relay needs only the standard library, not the harness package.
if __package__:
    from . import BUILD_DIR, REPO_ROOT, rootfs
    from .qemu import SERVICE_ADDRESS, Service, require_user_network

NAMES = ("http.lumen.test", "https.lumen.test", "repo.lumen.test", "echo.lumen.test")
APK_KEY_NAME = "lumen-run.rsa.pub"
LOG_FORMAT = "%(asctime)s %(process)d %(levelname)s %(message)s"
# Repository requests use the HTTPS listener; both names intentionally share 443.
SERVICE_PORTS = MappingProxyType({"http": 80, "https": 443, "repo": 443, "echo": 7})


def _openssl(*args: str, data: bytes | None = None) -> bytes:
    executable = _alpine().find_openssl()
    run = subprocess.run(
        [str(executable), *args], input=data, capture_output=True, timeout=30, check=False
    )
    if run.returncode:
        raise RuntimeError(f"openssl {args[0]} failed: {run.stderr.decode(errors='replace')}")
    return run.stdout


def _tar(files: dict[str, bytes], *, end: bool = True) -> bytes:
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w", format=tarfile.USTAR_FORMAT) as archive:
        for name, data in files.items():
            info = tarfile.TarInfo(name)
            info.size, info.mode = len(data), 0o644
            archive.addfile(info, io.BytesIO(data))
        size = archive.offset
    # APK's signature segment must have no tar end marker: the next gzip
    # member continues the same logical tar archive.
    return buf.getvalue() if end else buf.getvalue()[:size]


@lru_cache(maxsize=1)
def _alpine():
    """Reuse the lockfile and APK readers; none of the downloader paths are called."""
    spec = importlib.util.spec_from_file_location(
        "_netfix_alpine_lock", REPO_ROOT / "tools/alpine-lock.py"
    )
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def _index_entry(blob: bytes, name: str, version: str) -> str:
    al = _alpine()
    al.split_apk(blob)  # Validate control/data segments and datahash too.
    control = al.gzip_members(blob)[-2]
    with tarfile.open(fileobj=io.BytesIO(control), mode="r:gz") as archive:
        stream = archive.extractfile(".PKGINFO")
        assert stream is not None
        fields: dict[str, list[str]] = {}
        for line in stream.read().decode().splitlines():
            key, sep, value = line.partition(" = ")
            if sep:
                fields.setdefault(key, []).append(value)
    if fields.get("pkgname") != [name] or fields.get("pkgver") != [version]:
        raise ValueError(f"{name}-{version}: .PKGINFO identity disagrees with the lock")
    if fields.get("arch") not in (["x86_64"], ["noarch"]):
        raise ValueError(f"{name}: expected an x86_64 or noarch package")
    mapping = {
        "pkgname": "P",
        "pkgver": "V",
        "arch": "A",
        "size": "I",
        "pkgdesc": "T",
        "url": "U",
        "license": "L",
        "origin": "o",
        "maintainer": "m",
        "builddate": "t",
        "commit": "c",
        "depend": "D",
        "provides": "p",
        "install_if": "i",
        "provider_priority": "k",
    }
    lines = [f"C:{al.control_checksum(blob)}", f"S:{len(blob)}"]
    lines += [f"{short}:{' '.join(fields[key])}" for key, short in mapping.items() if key in fields]
    return "\n".join(lines) + "\n\n"


class _HTTP(http.server.SimpleHTTPRequestHandler):
    def log_message(self, format: str, *args: object) -> None:
        self.server.record("connection=%s %s", self.client_address[1], format % args)


class _Server(socketserver.ThreadingMixIn, socketserver.UnixStreamServer):
    """One connection per worker; stop interrupts idle and TLS clients."""

    request_queue_size = 128

    def __init__(
        self,
        service: Service,
        web: Path | None,
        tls: ssl.SSLContext | None,
        log: logging.Logger,
        errors: list[str],
    ):
        self.service, self.web, self.tls = service, web, tls
        self.log, self.errors = log, errors
        self.names = "/".join(name for name, port in SERVICE_PORTS.items() if port == service.port)
        self.connections = itertools.count(1)
        self.stopping = False
        self.active: set[socket.socket] = set()
        self.lock = threading.Lock()
        super().__init__(str(service.path), _Connection)
        self.worker = threading.Thread(target=self.serve_forever, daemon=True)

    def record(self, message: str, *args: object) -> None:
        self.log.info("service=%s " + message, self.names, *args)

    def handle_error(self, request, client_address):
        self.errors.append(f"service={self.names}: {sys.exception()!r}")
        self.log.exception("service=%s unexpected exception", self.names)

    def get_request(self):
        client, address = super().get_request()
        with self.lock:
            self.active.add(client)
        return client, address

    def close_clients(self) -> None:
        with self.lock:
            for client in self.active:
                try:
                    # Bypass SSLSocket.shutdown: it mutates _sslobj while workers
                    # may be wrapping, handshaking or reading through it.
                    socket.socket.shutdown(client, socket.SHUT_RDWR)
                except OSError as error:
                    if isinstance(error, ConnectionError) or error.errno in (
                        errno.ENOTCONN,
                        errno.EINVAL,
                    ):
                        self.record("disconnected during shutdown: %s", error)
                    else:
                        self.handle_error(client, "shutdown")

    def stop(self) -> None:
        """Stop accepting, interrupt clients, and join all connection workers."""
        self.shutdown()
        self.worker.join()
        self.stopping = True
        self.close_clients()
        self.server_close()


class _Connection(socketserver.BaseRequestHandler):
    def handle(self) -> None:
        server = self.server
        client = self.request
        connection = next(server.connections)
        server.record("connection=%s connected", connection)
        try:
            if server.tls:
                with server.lock:
                    server.active.discard(client)
                    client = server.tls.wrap_socket(
                        client, server_side=True, do_handshake_on_connect=False
                    )
                    server.active.add(client)
                try:
                    client.do_handshake()
                except ssl.SSLError as error:
                    if server.stopping:
                        raise
                    server.record("connection=%s TLS handshake failed: %s", connection, error)
                    return
            if server.web is not None:
                _HTTP(client, ("local", connection), server, directory=str(server.web))
            else:
                count = 0
                while chunk := client.recv(65536):
                    client.sendall(chunk)
                    count += len(chunk)
                server.record("connection=%s echo bytes=%s status=EOF", connection, count)
        except Exception as error:
            if server.stopping or isinstance(error, (ConnectionError, ssl.SSLError)):
                server.record("connection=%s disconnected: %s", connection, error)
            else:
                raise
        finally:
            with server.lock:
                server.active.discard(client)
            client.close()


class NetFixture:
    """Per-run CA, signed pinned repository, HTTP(S), and TCP echo; no host ports.

    ``lock`` and ``packages`` default to the checked-in Alpine lock and already
    downloaded cache. Missing or mismatched packages fail; nothing is fetched.
    Generated keys and sockets are private and removed on exit, including when
    initialization fails. With ``artifacts``, relay.log and services.log survive
    teardown there. Late relay output after teardown may be lost.
    Unexpected service exceptions fail context exit, or annotate an existing
    body exception without replacing it. A fixture
    is single-use; compose ``Devices`` with its ``services`` tuple of ``Service``
    values. ``SERVICE_PORTS`` maps names to ports (repo and https share 443).
    """

    def __init__(
        self,
        *,
        lock: Path | None = None,
        packages: Path | None = None,
        artifacts: Path | None = None,
    ):
        self.lockfile = lock or REPO_ROOT / "tools/alpine-packages.lock"
        self.packages = packages or BUILD_DIR / "deps/alpine-packages"
        self.artifacts = artifacts
        self.errors: list[str] = []
        self._servers: list[_Server] = []
        self._stack = ExitStack()
        self._entered = False
        self._live = False

    def __enter__(self) -> Self:
        if self._entered:
            raise RuntimeError("NetFixture is single-use")
        self._entered = True
        require_user_network()
        try:
            # macOS sockaddr_un paths have a small limit; pytest paths do not fit.
            self.directory = Path(
                self._stack.enter_context(
                    tempfile.TemporaryDirectory(prefix="lumen-net-", dir="/tmp")
                )
            )
            self.relay_log = self.directory / "relay.log"
            self.relay_log.touch(mode=0o600)
            self.services_log = self.directory / "services.log"
            self.log = logging.Logger("lumen_harness.netfix.services")  # noqa: LOG001 - fixture-owned lifetime
            handler = logging.FileHandler(self.services_log, encoding="utf-8")
            handler.setFormatter(logging.Formatter(LOG_FORMAT))
            self.log.addHandler(handler)
            self._stack.callback(handler.close)
            self._stack.callback(self._finish)
            self.web = self.directory / "www"
            self.web.mkdir()
            (self.web / "index.html").write_bytes(b"Lumen offline network fixture\n")
            self.ca = self.directory / "ca.pem"
            self.apk_public_key = self.directory / APK_KEY_NAME
            self._certificates()
            self._repository()
            tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            tls.load_cert_chain(self.directory / "server.pem", self.directory / "server.key")
            self.services: tuple[Service, ...] = tuple(
                Service(port, self.directory / f"{port}.sock")
                for port in dict.fromkeys(SERVICE_PORTS.values())
            )
            for service in self.services:
                web = None if service.port == SERVICE_PORTS["echo"] else self.web
                context = tls if service.port == SERVICE_PORTS["https"] else None
                server = _Server(service, web, context, self.log, self.errors)
                try:
                    server.worker.start()
                except BaseException:
                    server.server_close()
                    raise
                self._servers.append(server)
            self._live = True
            return self
        except BaseException:
            self._stack.close()
            raise

    def socket(self, name: str) -> Path:
        """The private socket for a named service; repo and https share one."""
        port = SERVICE_PORTS[name]
        return next(service.path for service in self.services if service.port == port)

    def _certificates(self) -> None:
        root = self.directory
        _openssl(
            "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-sha256", "-days", "2",
            "-subj", "/CN=Lumen run CA", "-keyout", str(root / "ca.key"), "-out", str(self.ca),
            "-addext", "basicConstraints=critical,CA:TRUE",
            "-addext", "keyUsage=critical,keyCertSign,cRLSign",
        )  # fmt: skip
        _openssl(
            "req", "-new", "-newkey", "rsa:2048", "-nodes", "-sha256",
            "-subj", "/CN=https.lumen.test", "-keyout", str(root / "server.key"),
            "-out", str(root / "server.csr"),
        )  # fmt: skip
        extensions = root / "server.ext"
        extensions.write_text(
            "basicConstraints=critical,CA:FALSE\n"
            "keyUsage=critical,digitalSignature,keyEncipherment\n"
            "extendedKeyUsage=serverAuth\n"
            f"subjectAltName={','.join('DNS:' + name for name in NAMES)}\n"
        )
        _openssl(
            "x509", "-req", "-in", str(root / "server.csr"), "-CA", str(self.ca),
            "-CAkey", str(root / "ca.key"), "-CAcreateserial", "-days", "2", "-sha256",
            "-extfile", str(extensions), "-out", str(root / "server.pem"),
        )  # fmt: skip
        _openssl("genrsa", "-out", str(root / "apk.key"), "2048")
        _openssl("rsa", "-in", str(root / "apk.key"), "-pubout", "-out", str(self.apk_public_key))
        for name in ("ca.key", "server.key", "apk.key"):
            (root / name).chmod(0o600)

    def _repository(self) -> None:
        al = _alpine()
        packages = al.parse_lock(self.lockfile.read_text()).packages
        repo = self.web / "repo/x86_64"
        repo.mkdir(parents=True)
        entries = []
        for package in packages:
            source = self.packages / package.filename
            try:
                blob = source.read_bytes()
            except FileNotFoundError as error:
                raise FileNotFoundError(
                    f"{source}: missing package cache; run make deps-dev"
                ) from error
            if hashlib.sha256(blob).hexdigest() != package.sha256:
                raise ValueError(f"{source}: sha256 disagrees with {self.lockfile}")
            entries.append(_index_entry(blob, package.name, package.version))
            # The HTTP handler only reads packages. Never chmod/write this
            # inode: it belongs to the shared, verified dependency cache.
            try:
                os.link(source, repo / package.filename)
            except OSError as error:
                if error.errno not in (errno.EXDEV, errno.EPERM, errno.EACCES):
                    raise
                # Cross-filesystem or protected caches are served read-only too.
                (repo / package.filename).symlink_to(source.resolve())
        unsigned = gzip.compress(
            _tar(
                {
                    "DESCRIPTION": b"Lumen per-run offline repository\n",
                    "APKINDEX": "".join(entries).encode(),
                }
            ),
            mtime=0,
        )
        signature = _openssl(
            "dgst", "-sha1", "-sign", str(self.directory / "apk.key"), data=unsigned
        )
        prefix = gzip.compress(_tar({f".SIGN.RSA.{APK_KEY_NAME}": signature}, end=False), mtime=0)
        self.apk_index = repo / "APKINDEX.tar.gz"
        self.apk_index.write_bytes(prefix + unsigned)

    def prepare_image(self, dest: Path, *, src: Path | None = None) -> Path:
        """Copy an Alpine ext4 image, replace trust with run keys and set local routes.

        The default source is build/alpine-rootfs.ext4; the copy is named
        alpine-rootfs.ext4 in dest. Every existing entry in /etc/apk/keys is
        removed before installing only this run's key. Existing hosts and
        repository/resolver settings are replaced. Inherited CA directory
        entries are removed; only the run CA remains, in cert.pem and
        ca-certificates.crt bundles, ca-cert-lumen-run.pem and its OpenSSL
        new-style subject-hash link. /etc/ssl/cert.pem is replaced too, so
        CAfile and CApath users both trust only the run CA.
        The source is never written. The copy's resolver points at guest
        loopback; a DNS scenario supplies its own responder there. DHCP clients
        must not replace this resolv.conf with slirp's advertised DNS proxy.
        """
        if not self._live:
            raise RuntimeError("prepare_image requires an active NetFixture")
        image = rootfs.copy_rootfs(dest, src=src)

        def remove_entry(path: Path) -> None:
            if "Type: directory" in rootfs.debugfs(image, f"stat {path}"):
                for name in rootfs.debugfs_names(image, str(path)):
                    remove_entry(path / name)
                rootfs.debugfs(image, f"rmdir {path}", write=True)
            else:
                rootfs.debugfs(image, f"rm {path}", write=True)

        ca = self.ca.read_bytes()
        ca_hash = (
            _openssl(
                "x509",
                "-in",
                str(self.ca),
                "-noout",
                "-subject_hash",
            )
            .decode()
            .strip()
        )
        if not re.fullmatch(r"[0-9a-f]{8}", ca_hash):
            raise RuntimeError(f"OpenSSL returned an invalid CA subject hash: {ca_hash!r}")
        files = {
            "/etc/hosts": (
                "127.0.0.1 localhost\n" + SERVICE_ADDRESS + " " + " ".join(NAMES) + "\n"
            ).encode(),
            "/etc/resolv.conf": b"nameserver 127.0.0.1\n",
            "/etc/ssl/cert.pem": ca,
            "/etc/ssl/certs/ca-certificates.crt": ca,
            "/etc/ssl/certs/cert.pem": ca,
            "/etc/ssl/certs/ca-cert-lumen-run.pem": ca,
            "/etc/apk/repositories": b"https://repo.lumen.test/repo\n",
            f"/etc/apk/keys/{APK_KEY_NAME}": self.apk_public_key.read_bytes(),
        }
        for target, content in files.items():
            path = Path(target)
            for parent in reversed(path.parents):
                if parent == Path("/"):
                    continue
                if parent.name not in rootfs.debugfs_names(image, str(parent.parent)):
                    rootfs.debugfs(image, f"mkdir {parent}", write=True)
            # Clear the CA directory before its first bundle write; remove
            # every inherited trust entry, including nested directories/links.
            if target in (f"/etc/apk/keys/{APK_KEY_NAME}", "/etc/ssl/certs/ca-certificates.crt"):
                for name in rootfs.debugfs_names(image, str(path.parent)):
                    remove_entry(path.parent / name)
            if path.name in rootfs.debugfs_names(image, str(path.parent)):
                rootfs.debugfs(image, f"rm {path}", write=True)
            payload = self.directory / "image-payload"
            payload.write_bytes(content)
            rootfs.debugfs(image, f"write {payload} {path}", write=True)
            if rootfs.debugfs(image, f"cat {path}").encode() != content:
                raise RuntimeError(f"image write did not preserve {path}")
        rootfs.debugfs(
            image, f"symlink /etc/ssl/certs/{ca_hash}.0 ca-cert-lumen-run.pem", write=True
        )
        rootfs.e2fsck_check(image)
        return image

    def __exit__(self, exc_type=None, exc: BaseException | None = None, traceback=None) -> None:
        self._live = False
        self._stack.close()
        if self.errors:
            path = (self.artifacts or self.directory) / "services.log"
            error = RuntimeError(
                f"network fixture service failures (see {path}): " + "; ".join(self.errors)
            )
            if exc is not None:
                exc.add_note(str(error))
            else:
                raise error

    def _finish(self) -> None:
        # Stop services explicitly before ExitStack removes keys and sockets.
        for server in self._servers:
            server.stop()
        self._servers.clear()
        if self.artifacts is not None:
            self.artifacts.mkdir(parents=True, exist_ok=True)
            for name in ("relay.log", "services.log"):
                log = self.directory / name
                if log.exists():
                    shutil.copyfile(log, self.artifacts / log.name)


def relay(path: str, *, input_fd: int = 0, output_fd: int = 1) -> int:
    """Relay both half-closes independently, without idle deadlines.

    Guest EOF shuts down the service's write side and keeps draining replies.
    Service EOF shuts down the guest's write side, then drains and drops guest
    bytes until guest EOF (including TLS close_notify after an HTTP response).
    Expected disconnects are informational; other I/O errors return non-zero.
    """
    log = logging.getLogger("lumen_harness.netfix.relay")
    errors = []
    service_eof = threading.Event()

    def disconnected(error: OSError) -> bool:
        return isinstance(error, ConnectionError) or error.errno in (errno.ENOTCONN, errno.EINVAL)

    with (
        socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client,
        socket.socket(fileno=os.dup(output_fd)) as guest,
    ):
        client.settimeout(30)  # Only connecting has a deadline.
        client.connect(path)
        client.settimeout(None)

        def abort() -> None:
            for connection in (client, guest):
                try:
                    connection.shutdown(socket.SHUT_RDWR)
                except OSError:
                    log.debug("relay shutdown", exc_info=True)

        def send() -> None:
            try:
                while data := os.read(input_fd, 65536):
                    if not service_eof.is_set():
                        try:
                            client.sendall(data)
                        except OSError as error:
                            if not disconnected(error):
                                raise
                            log.info("relay sender disconnected: %s", error)
                            service_eof.set()
                client.shutdown(socket.SHUT_WR)
            except OSError as error:
                if disconnected(error):
                    log.info("relay sender disconnected: %s", error)
                else:
                    errors.append(error)
                    log.exception("relay sender failed")
                abort()

        sender = threading.Thread(target=send, daemon=False)
        sender.start()
        try:
            while data := client.recv(65536):
                guest.sendall(data)
            service_eof.set()
            guest.shutdown(socket.SHUT_WR)
        except OSError as error:
            if disconnected(error):
                log.info("relay receiver disconnected: %s", error)
            else:
                errors.append(error)
                log.exception("relay receiver failed")
            abort()
        finally:
            sender.join()
        return 1 if errors else 0


if __name__ == "__main__":
    logging.basicConfig(level=logging.INFO, format=LOG_FORMAT)
    parser = argparse.ArgumentParser()
    parser.add_argument("--relay", required=True)
    parser.add_argument("--log", required=True)
    args = parser.parse_args()
    try:
        sys.exit(relay(args.relay))
    except OSError:
        logging.getLogger("lumen_harness.netfix.relay").exception("guestfwd relay failed")
        sys.exit(1)
