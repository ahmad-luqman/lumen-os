"""Offline service, trust, package and lifetime checks for #453 (no guest driver)."""

import base64
import errno
import gzip
import hashlib
import http.client
import io
import logging
import os
import re
import shutil
import socket
import ssl
import subprocess
import sys
import tarfile
import tempfile
import threading
import zlib
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import pytest
from lumen_harness import BUILD_DIR, REPO_ROOT, netfix, rootfs
from lumen_harness.netfix import APK_KEY_NAME, SERVICE_PORTS, NetFixture
from lumen_harness.qemu import SERVICE_ADDRESS, Devices, qemu_binary
from netfix_fakes import archive


def connect(net, name, context=None, hostname="https.lumen.test"):
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    client.settimeout(10)
    try:
        client.connect(str(net.socket(name)))
        return context.wrap_socket(client, server_hostname=hostname) if context else client
    except BaseException:
        client.close()
        raise


def get(net, path="/", *, context=None, hostname="https.lumen.test"):
    with connect(net, "https" if context else "http", context, hostname) as client:
        client.sendall(f"GET {path} HTTP/1.0\r\nHost: {hostname}\r\n\r\n".encode())
        response = http.client.HTTPResponse(client)
        response.begin()
        return response.status, response.read()


def test_https_server_trusts_only_the_run_ca(packages):
    with NetFixture(**packages) as first, NetFixture(**packages) as second:
        own = ssl.create_default_context(cafile=str(first.ca))
        other = ssl.create_default_context(cafile=str(second.ca))
        assert first.ca.read_bytes() != second.ca.read_bytes()
        assert get(first, context=own) == (200, b"Lumen offline network fixture\n")
        assert get(first, context=own, hostname="repo.lumen.test")[0] == 200
        for context, name in (
            (other, "https.lumen.test"),
            (own, "wrong.lumen.test"),
            (ssl.create_default_context(), "https.lumen.test"),
        ):
            with pytest.raises(ssl.SSLCertVerificationError):
                get(first, context=context, hostname=name)
        # Rejected handshakes cannot strand the listener.
        assert get(first, context=own)[0] == 200
        log = first.services_log.read_text()
        assert log.count("TLS handshake failed") == 3
        assert "service=https/repo connection=" in log
        assert "connected" in log and "peer=" not in log
        assert re.search(r"\d{4}-\d\d-\d\d \d\d:\d\d:\d\d,\d{3} \d+ INFO service=", log)
        assert '"GET / HTTP/1.0" 200' in log
        connected = set(re.findall(r"service=(\S+) connection=(\d+) connected", log))
        requests = re.findall(r'service=(\S+) connection=(\d+) "GET ', log)
        assert requests and set(requests) <= connected
        for name in ("ca.key", "apk.key", "server.key"):
            assert (first.directory / name).stat().st_mode & 0o777 == 0o600
            assert get(first, "/" + name)[0] == 404
            assert get(first, "/../" + name)[0] == 404
    assert not first.directory.exists() and not second.directory.exists()


def split_signature(blob):
    decoder = zlib.decompressobj(31)
    prefix = decoder.decompress(blob)
    assert decoder.eof and decoder.unused_data
    with tarfile.open(fileobj=io.BytesIO(prefix), mode="r:") as tar:
        assert tar.getnames() == [f".SIGN.RSA.{APK_KEY_NAME}"]
        signature = tar.extractfile(tar.getmembers()[0]).read()
    return signature, decoder.unused_data


def verify(public, signature, signed, tmp_path):
    path = tmp_path / "signature"
    path.write_bytes(signature)
    return (
        subprocess.run(
            [
                str(netfix._alpine().find_openssl()),
                "dgst",
                "-sha1",
                "-verify",
                str(public),
                "-signature",
                str(path),
            ],
            input=signed,
            capture_output=True,
            timeout=10,
            check=False,
        ).returncode
        == 0
    )


def test_apk_index_signed_by_the_run_key(packages, tmp_path):
    with NetFixture(**packages) as first, NetFixture(**packages) as second:
        status, blob = get(first, "/repo/x86_64/APKINDEX.tar.gz")
        assert status == 200 and blob == first.apk_index.read_bytes()
        signature, signed = split_signature(blob)
        assert verify(first.apk_public_key, signature, signed, tmp_path)
        assert not verify(second.apk_public_key, signature, signed, tmp_path)
        assert not verify(first.apk_public_key, signature, signed + b"tampered", tmp_path)
        with tarfile.open(fileobj=io.BytesIO(gzip.decompress(blob)), mode="r:") as tar:
            # A tar terminator after the signature would hide APKINDEX from apk.
            assert tar.getnames() == [f".SIGN.RSA.{APK_KEY_NAME}", "DESCRIPTION", "APKINDEX"]
            index = tar.extractfile("APKINDEX").read().decode()
        status, apk = get(first, "/repo/x86_64/hello-1.0-r0.apk")
        assert status == 200 and apk == (packages["packages"] / "hello-1.0-r0.apk").read_bytes()
        decoder = zlib.decompressobj(31)
        decoder.decompress(apk)
        control_and_data = decoder.unused_data
        decoder = zlib.decompressobj(31)
        decoder.decompress(control_and_data)
        control = control_and_data[: len(control_and_data) - len(decoder.unused_data)]
        checksum = "Q1" + base64.b64encode(hashlib.sha1(control).digest()).decode()
        for line in (
            f"C:{checksum}",
            f"S:{len(apk)}",
            "P:hello",
            "V:1.0-r0",
            "A:x86_64",
            "I:6",
            "D:musl so:libc.musl-x86_64.so.1",
            "p:cmd:hello=1.0-r0",
            "i:musl hello-helper",
            "k:7",
            "o:hello-source",
        ):
            assert line in index.splitlines()
        # apk-tools v2.14.4 src/package.c:1123-1191 writes repository entries
        # without replaces; src/database.c:942-950 writes r:/q: only to the
        # installed database. Do not leak installed-only fields into APKINDEX.
        assert not any(line.startswith(("r:", "q:")) for line in index.splitlines())


def echo(net, payload):
    with connect(net, "echo") as client:
        client.sendall(payload)
        client.shutdown(socket.SHUT_WR)
        with client.makefile("rb") as reply:
            return reply.read()


def test_parallel_fixtures_share_no_host_port(packages, monkeypatch):
    binds = []
    real_bind = socket.socket.bind

    def bind(client, address):
        binds.append((client.family, address))
        return real_bind(client, address)

    monkeypatch.setattr(socket.socket, "bind", bind)
    barrier = threading.Barrier(3, timeout=60)

    def run(number):
        with NetFixture(**packages) as net:
            paths = [path for _, path in net.services]
            barrier.wait()  # all three listeners/keys/repositories coexist
            marker = f"fixture {number}".encode() * 100
            (net.web / "marker").write_bytes(marker)
            assert get(net, "/marker") == (200, marker)
            assert echo(net, marker) == marker
            return paths, net.directory

    with ThreadPoolExecutor(max_workers=3) as pool:
        results = list(pool.map(run, range(3)))
    paths = [path for sockets, _ in results for path in sockets]
    assert len(set(paths)) == 9
    assert binds and all(family == socket.AF_UNIX for family, _ in binds)
    assert not any(path.exists() for path in paths)
    assert not any(directory.exists() for _, directory in results)


def relay_process(path, log, peer):
    return subprocess.Popen(
        [sys.executable, "-W", "ignore", netfix.__file__, "--relay", str(path), "--log", str(log)],
        stdin=peer,
        stdout=peer,
        stderr=peer,
    )


def test_guestfwd_relay_preserves_parallel_streams_and_half_close(packages):
    with NetFixture(**packages) as net:

        def run(number):
            payload = (f"stream {number}".encode() + bytes(range(256))) * 800
            guest, peer = socket.socketpair()
            with guest, peer:
                guest.settimeout(60)
                proc = relay_process(net.socket("echo"), net.relay_log, peer)
                peer.close()
                try:
                    with (
                        ThreadPoolExecutor(max_workers=1) as reader,
                        guest.makefile("rb") as stream,
                    ):
                        result = reader.submit(stream.read)
                        guest.sendall(payload)
                        guest.shutdown(socket.SHUT_WR)
                        assert result.result(timeout=60) == payload
                    assert proc.wait(timeout=60) == 0, net.relay_log.read_text()
                finally:
                    proc.kill()
                    proc.wait(timeout=10)

        with ThreadPoolExecutor(max_workers=4) as pool:
            list(pool.map(run, range(4)))


def test_bad_package_hash_cleans_up_before_serving(packages):
    (packages["packages"] / "hello-1.0-r0.apk").write_bytes(b"corrupted cache")
    net = NetFixture(**packages)
    with pytest.raises(ValueError, match="sha256 disagrees"), net:
        pytest.fail("a corrupt package was served")
    assert not net.directory.exists()


def test_fixture_shutdown_closes_idle_clients(packages, monkeypatch):
    shutdowns = []
    real_shutdown = socket.socket.shutdown

    def shutdown(client, how):
        shutdowns.append(how)
        return real_shutdown(client, how)

    monkeypatch.setattr(socket.socket, "shutdown", shutdown)
    handle = netfix._Connection.handle

    def handle_without_idle_deadline(connection):
        assert connection.request.gettimeout() is None
        handle(connection)

    monkeypatch.setattr(netfix._Connection, "handle", handle_without_idle_deadline)
    with (
        NetFixture(**packages) as net,
        connect(net, "http") as http,
        connect(net, "https") as tls,
        connect(net, "echo") as tcp,
    ):
        # Force the listeners to accept these clients before closing, using
        # another completed request through each accept loop.
        assert get(net)[0] == 200
        assert get(net, context=ssl.create_default_context(cafile=str(net.ca)))[0] == 200
        assert echo(net, b"ready") == b"ready"
        guarded(net.__exit__)
        # Explicit interruption without relying on any idle deadline.
        # No timing ceiling: a busy host can delay shutdown arbitrarily.
        assert shutdowns.count(socket.SHUT_RDWR) == 3
        assert http.recv(1) == tls.recv(1) == tcp.recv(1) == b""
    assert not net.directory.exists()


def test_prepare_image_installs_local_routes_and_run_trust_only_in_copy(packages, tmp_path):
    source = tmp_path / "master.ext4"
    source.write_bytes(b"")
    with source.open("wb") as stream:
        stream.truncate(16 << 20)
    mkfs, _ = rootfs.find_e2fsprogs()
    stage = tmp_path / "stage"
    for directory in ("etc/apk/keys", "etc/ssl/certs"):
        (stage / directory).mkdir(parents=True)
    for name in (
        "etc/hosts",
        "etc/apk/repositories",
        "etc/resolv.conf",
        "etc/ssl/certs/ca-certificates.crt",
    ):
        (stage / name).write_text("old configuration\n")
    keys = stage / "etc/apk/keys"
    for name in ("upstream.pub", "another-upstream.pub", APK_KEY_NAME):
        (keys / name).write_text("upstream key\n")
    (keys / "linked.pub").symlink_to("upstream.pub")
    (keys / "old").mkdir()
    (keys / "old/retired.pub").write_text("retired key\n")
    (stage / "etc/ssl/cert.pem").symlink_to("certs/ca-certificates.crt")
    openssl = netfix._alpine().find_openssl()
    upstream = stage / "usr/share/ca-certificates/upstream.crt"
    upstream.parent.mkdir(parents=True)
    upstream_key = tmp_path / "upstream.key"
    upstream_server = tmp_path / "upstream-server.pem"
    server_csr = tmp_path / "upstream-server.csr"
    netfix._openssl(
        "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "2",
        "-subj", "/CN=Lumen upstream CA", "-keyout", str(upstream_key), "-out", str(upstream),
        "-addext", "basicConstraints=critical,CA:TRUE",
    )  # fmt: skip
    netfix._openssl(
        "req", "-new", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=upstream.lumen.test",
        "-keyout", str(tmp_path / "upstream-server.key"), "-out", str(server_csr),
    )  # fmt: skip
    netfix._openssl(
        "x509", "-req", "-in", str(server_csr), "-CA", str(upstream),
        "-CAkey", str(upstream_key), "-CAcreateserial", "-days", "2",
        "-out", str(upstream_server),
    )  # fmt: skip
    upstream_hash = (
        netfix._openssl("x509", "-in", str(upstream), "-noout", "-subject_hash").decode().strip()
    )
    certs = stage / "etc/ssl/certs"
    # Relative links work in the guest and in the dumped image without
    # accidentally resolving a guest absolute path against the host root.
    (certs / "ca-cert-upstream.pem").symlink_to("../../../usr/share/ca-certificates/upstream.crt")
    (certs / f"{upstream_hash}.0").symlink_to("ca-cert-upstream.pem")
    (certs / f"{upstream_hash}.1").symlink_to("ca-cert-upstream.pem")
    (certs / "cert.pem").write_text("old bundle\n")
    (certs / "old").mkdir()
    (certs / "old/retired.pem").write_bytes(upstream.read_bytes())

    def verify_capath(directory, certificate):
        return subprocess.run(
            [str(openssl), "verify", "-no-CAfile", "-no-CAstore", "-CApath", str(directory),
             str(certificate)],
            capture_output=True, text=True, timeout=30, check=False,
        )  # fmt: skip

    # Prove the inherited hash chain really trusts the other server first.
    verified = verify_capath(certs, upstream_server)
    assert verified.returncode == 0, verified.stdout + verified.stderr
    subprocess.run([str(mkfs), "-q", "-t", "ext4", "-F", "-d", str(stage), str(source)], check=True)
    before = hashlib.sha256(source.read_bytes()).digest()
    with NetFixture(**packages) as net:
        image = net.prepare_image(tmp_path / "copy", src=source)
        assert image != source
        expected = {
            f"/etc/apk/keys/{APK_KEY_NAME}": net.apk_public_key.read_text(),
            "/etc/ssl/cert.pem": net.ca.read_text(),
            "/etc/ssl/certs/ca-certificates.crt": net.ca.read_text(),
            "/etc/ssl/certs/cert.pem": net.ca.read_text(),
            "/etc/ssl/certs/ca-cert-lumen-run.pem": net.ca.read_text(),
            "/etc/apk/repositories": "https://repo.lumen.test/repo\n",
            "/etc/resolv.conf": "nameserver 127.0.0.1\n",
            "/etc/hosts": f"127.0.0.1 localhost\n{SERVICE_ADDRESS} {' '.join(netfix.NAMES)}\n",
        }
        for path, content in expected.items():
            assert rootfs.debugfs(image, f"cat {path}") == content
        assert "Type: regular" in rootfs.debugfs(image, "stat /etc/ssl/cert.pem")
        assert rootfs.debugfs_names(image, "/etc/apk/keys") == {APK_KEY_NAME}
        ca_hash = (
            netfix._openssl("x509", "-in", str(net.ca), "-noout", "-subject_hash").decode().strip()
        )
        assert ca_hash != upstream_hash
        assert rootfs.debugfs_names(image, "/etc/ssl/certs") == {
            "cert.pem",
            "ca-certificates.crt",
            "ca-cert-lumen-run.pem",
            f"{ca_hash}.0",
        }
        dumped = tmp_path / "dumped"
        dumped.mkdir()
        # As in test_rootfs_dev.dev_root, an unprivileged dump cannot restore
        # root ownership. Keep every other debugfs diagnostic fatal.
        dump = subprocess.run(
            [str(mkfs.parent / "debugfs"), "-R", f"rdump / {dumped}", str(image)],
            capture_output=True, text=True, timeout=60, check=False,
            env={**os.environ, "LC_ALL": "C"},
        )  # fmt: skip
        errors = [
            line
            for line in dump.stderr.splitlines()
            if line
            and not line.startswith("debugfs ")
            and "Operation not permitted while changing ownership" not in line
        ]
        assert dump.returncode == 0 and not errors, dump.stderr
        capath = dumped / "etc/ssl/certs"
        assert (capath / f"{ca_hash}.0").is_symlink()
        assert os.readlink(capath / f"{ca_hash}.0") == "ca-cert-lumen-run.pem"
        assert (capath / f"{ca_hash}.0").read_bytes() == net.ca.read_bytes()
        verified = verify_capath(capath, net.directory / "server.pem")
        assert verified.returncode == 0, verified.stdout + verified.stderr
        rejected = verify_capath(capath, upstream_server)
        assert rejected.returncode != 0
        assert "unable to get local issuer certificate" in rejected.stderr
        rootfs.e2fsck_check(image)
        assert hashlib.sha256(source.read_bytes()).digest() == before
    with pytest.raises(RuntimeError, match="active NetFixture"):
        net.prepare_image(tmp_path / "after", src=source)


def test_repository_serves_the_pinned_package_cache():
    # Use the real lock/cache too: synthetic metadata alone cannot prove that
    # the pinned packages are indexable. Missing deps are a failure, no fetch.
    with NetFixture() as net:
        with tarfile.open(net.apk_index, "r:gz") as tar:
            index = tar.extractfile("APKINDEX").read().decode()
        lines = (REPO_ROOT / "tools/alpine-packages.lock").read_text().splitlines()
        for line in lines:
            if line.startswith("#"):
                continue
            name, version, _, checksum = line.split()
            assert f"P:{name}\nV:{version}\n" in index
            package = f"{name}-{version}.apk"
            assert (net.web / "repo/x86_64" / package).samefile(
                BUILD_DIR / "deps/alpine-packages" / package
            )
            status, data = get(net, "/repo/x86_64/" + package)
            assert status == 200 and hashlib.sha256(data).hexdigest() == checksum
            assert data == (BUILD_DIR / "deps/alpine-packages" / package).read_bytes()


def test_relay_drains_guest_after_server_eof(tmp_path):
    with tempfile.TemporaryDirectory(prefix="lumen-half-", dir="/tmp") as short:
        path = Path(short) / "service.sock"
        log = tmp_path / "relay.log"
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener:
            listener.bind(str(path))
            listener.listen()
            listener.settimeout(60)
            guest, peer = socket.socketpair()
            with guest, peer, ThreadPoolExecutor(max_workers=1) as pool:
                guest.settimeout(60)

                def server():
                    with listener.accept()[0] as client:
                        client.settimeout(60)
                        client.sendall(b"response finished")
                        client.shutdown(socket.SHUT_WR)
                        with client.makefile("rb") as stream:
                            return stream.read()

                upload = pool.submit(server)
                proc = relay_process(path, log, peer)
                peer.close()
                try:
                    with guest.makefile("rb") as stream:
                        assert stream.read() == b"response finished"
                    # Service EOF closes the response; late guest bytes are drained and dropped.
                    assert proc.poll() is None
                    payload = bytes(range(256)) * 1000
                    guest.sendall(payload)
                    guest.shutdown(socket.SHUT_WR)
                    assert upload.result(timeout=60) == b""
                    assert proc.wait(timeout=60) == 0, log.read_text()
                finally:
                    proc.kill()
                    proc.wait(timeout=10)


def test_relay_sender_error_is_logged_and_fails(packages, monkeypatch, caplog):
    with NetFixture(**packages) as net:
        guest, peer = socket.socketpair()
        with guest, peer:
            guest.settimeout(60)
            read = os.read
            settimeout = socket.socket.settimeout
            timeouts = []

            def fail_read(fd, size):
                if fd == peer.fileno():
                    raise OSError(errno.EIO, "injected sender failure")
                return read(fd, size)

            def record_timeout(client, timeout):
                timeouts.append((client.fileno(), timeout))
                return settimeout(client, timeout)

            monkeypatch.setattr(netfix.os, "read", fail_read)
            monkeypatch.setattr(socket.socket, "settimeout", record_timeout)
            assert (
                guarded(
                    lambda: netfix.relay(
                        str(net.socket("echo")),
                        input_fd=peer.fileno(),
                        output_fd=peer.fileno(),
                    )
                )
                == 1
            )
            assert guest.recv(1) == b""
            assert "injected sender failure" in caplog.text
            assert "relay sender failed" in caplog.text
            assert "relay receiver failed" not in caplog.text
            assert [timeout for _, timeout in timeouts] == [30, None]


def test_service_exceptions_fail_fixture_and_preserve_logs(packages, tmp_path, monkeypatch):
    def broken_get(handler):
        raise OSError(errno.EIO, "injected service failure")

    monkeypatch.setattr(netfix._HTTP, "do_GET", broken_get)
    artifacts = tmp_path / "artifacts"
    with (
        pytest.raises(RuntimeError, match="injected service failure") as caught,
        NetFixture(**packages, artifacts=artifacts) as net,
        pytest.raises(http.client.RemoteDisconnected),
    ):
        get(net)
    assert str(artifacts / "services.log") in str(caught.value)
    assert "injected service failure" in (artifacts / "services.log").read_text()
    assert (artifacts / "relay.log").exists()
    assert not net.directory.exists()


@pytest.mark.parametrize(
    ("field", "replacement", "message"),
    [
        ("pkgname = hello", "pkgname = other", "identity disagrees"),
        ("pkgver = 1.0-r0", "pkgver = 2.0-r0", "identity disagrees"),
        ("arch = x86_64", "arch = aarch64", "x86_64 or noarch"),
    ],
)
def test_index_rejects_wrong_identity_or_arch(packages, field, replacement, message):
    blob = (packages["packages"] / "hello-1.0-r0.apk").read_bytes()
    members = netfix._alpine().gzip_members(blob)
    with tarfile.open(fileobj=io.BytesIO(members[-2]), mode="r:gz") as tar:
        info = tar.extractfile(".PKGINFO").read().decode().replace(field, replacement)
    members[-2] = gzip.compress(archive({".PKGINFO": info.encode()}), mtime=0)
    with pytest.raises(ValueError, match=message):
        netfix._index_entry(b"".join(members), "hello", "1.0-r0")


def test_missing_package_cache_names_make_deps_dev(packages):
    (packages["packages"] / "hello-1.0-r0.apk").unlink()
    with pytest.raises(FileNotFoundError, match="make deps-dev"), NetFixture(**packages):
        pytest.fail("missing cache was accepted")


def test_service_mapping_is_composable_and_repository_shares_https(packages, tmp_path):
    with NetFixture(**packages) as net:
        assert SERVICE_PORTS["repo"] == SERVICE_PORTS["https"] == 443
        assert {service.port for service in net.services} == set(SERVICE_PORTS.values())
        devices = Devices(blk_image=tmp_path / "rootfs", rng=True, services=net.services)
        assert devices.services == net.services and devices.wants_net and devices.wants_blk
        for name, port in SERVICE_PORTS.items():
            assert net.socket(name).is_socket()
            assert net.socket(name) == next(s.path for s in net.services if s.port == port)
        assert net.socket("repo") == net.socket("https")
        with pytest.raises(KeyError):
            net.socket("unknown")


def guarded(call):
    """A deadlock fails the test instead of blocking the pytest main thread."""
    results, errors = [], []

    def run():
        try:
            results.append(call())
        except BaseException as error:  # noqa: BLE001 - re-raised on the pytest thread
            errors.append(error)

    worker = threading.Thread(target=run, daemon=True)
    worker.start()
    worker.join(timeout=60)
    assert not worker.is_alive(), "call did not finish (60 s hang guard)"
    if errors:
        raise errors[0]
    return results[0]


@pytest.mark.parametrize("service", ["http", "https"])
def test_relay_http_response_then_guest_close_is_not_an_error(packages, service):
    with NetFixture(**packages) as net:
        guest, peer = socket.socketpair()
        with guest, peer:
            guest.settimeout(60)
            proc = relay_process(net.socket(service), net.relay_log, peer)
            peer.close()
            try:
                if service == "https":
                    context = ssl.create_default_context(cafile=str(net.ca))
                    guest = context.wrap_socket(guest, server_hostname="https.lumen.test")
                with guest:
                    guest.sendall(b"GET / HTTP/1.0\r\nHost: https.lumen.test\r\n\r\n")
                    with guest.makefile("rb") as stream:
                        response = stream.read()
                    assert b"200 OK" in response and response.endswith(
                        b"Lumen offline network fixture\n"
                    )
                    if service == "https":
                        # HTTP/1.0 closes without TLS close_notify. unwrap still sends
                        # ours; OpenSSL reports the peer's missing notification.
                        try:
                            guest.unwrap().close()
                        except ssl.SSLError:
                            pass
                assert proc.wait(timeout=60) == 0, net.relay_log.read_text()
                assert "ERROR" not in net.relay_log.read_text()
            finally:
                proc.kill()
                proc.wait(timeout=60)


def test_relay_with_unwritable_log_still_carries_traffic(packages, tmp_path):
    with NetFixture(**packages) as net:
        guest, peer = socket.socketpair()
        with guest, peer:
            guest.settimeout(60)
            # A directory cannot be opened as an append-only log.
            proc = relay_process(net.socket("echo"), tmp_path, peer)
            peer.close()
            try:
                guest.sendall(b"no diagnostics in this stream")
                guest.shutdown(socket.SHUT_WR)
                with guest.makefile("rb") as stream:
                    assert stream.read() == b"no diagnostics in this stream"
                assert proc.wait(timeout=60) == 0
            finally:
                proc.kill()
                proc.wait(timeout=60)


@pytest.mark.parametrize("operation", ["recv", "sendall", "shutdown"])
@pytest.mark.parametrize(
    "code", [errno.EPIPE, errno.ECONNRESET, errno.ENOTCONN, errno.EINVAL, errno.EIO]
)
def test_relay_receiver_disconnects_and_failures(packages, monkeypatch, caplog, operation, code):
    caplog.set_level(logging.INFO, logger="lumen_harness.netfix.relay")
    with NetFixture(**packages) as net:
        guest, peer = socket.socketpair()
        with guest, peer:
            guest.settimeout(60)
            guest.sendall(b"reply to trigger receiver sendall")
            guest.shutdown(socket.SHUT_WR)
            original = getattr(socket.socket, operation)
            injected = []
            receiver = None

            def fail(client, *args):
                # Retain the Thread object: a finished thread's numeric ident
                # can be reused by a service worker before teardown.
                if threading.current_thread() is receiver and (
                    operation != "shutdown" or args[0] == socket.SHUT_WR
                ):
                    injected.append(code)
                    raise OSError(code, "injected receiver failure")
                return original(client, *args)

            def run_relay():
                nonlocal receiver
                receiver = threading.current_thread()
                return netfix.relay(
                    str(net.socket("echo")), input_fd=peer.fileno(), output_fd=peer.fileno()
                )

            with monkeypatch.context() as patch:
                patch.setattr(socket.socket, operation, fail)
                result = guarded(run_relay)
            assert injected
            assert result == (1 if code == errno.EIO else 0)
            assert "relay sender failed" not in caplog.text
            if code == errno.EIO:
                assert "relay receiver failed" in caplog.text
            else:
                assert "relay receiver disconnected" in caplog.text
                assert "ERROR" not in caplog.text


@pytest.mark.parametrize("race", ["wrap", "handshake", "read"])
def test_fixture_shutdown_tolerates_tls_worker_races(packages, monkeypatch, race):
    held, closed = threading.Event(), threading.Event()
    original_close = netfix._Server.close_clients

    def close_clients(server):
        original_close(server)
        if server.tls:
            closed.set()

    monkeypatch.setattr(netfix._Server, "close_clients", close_clients)

    def ssl_shutdown(*args):
        pytest.fail("close_clients must bypass the SSL object's shutdown")

    monkeypatch.setattr(ssl.SSLSocket, "shutdown", ssl_shutdown)

    def failure(*args, **kwargs):
        held.set()
        assert closed.wait(60), "close_clients never ran"
        if race == "wrap":
            raise OSError(errno.EINVAL, "injected wrap_socket shutdown race")
        if race == "handshake":
            raise AttributeError("injected do_handshake shutdown race")
        raise ValueError("injected read shutdown race")

    # wrap_socket holds the active-client lock, so pause just before handle
    # in that case; the other races pause inside the worker operation itself.
    handle = netfix._Connection.handle
    if race == "wrap":

        def held_handle(connection):
            if connection.server.tls:
                held.set()
                assert closed.wait(60)
            handle(connection)

        monkeypatch.setattr(netfix._Connection, "handle", held_handle)
        monkeypatch.setattr(ssl.SSLContext, "wrap_socket", failure)
    else:
        monkeypatch.setattr(
            ssl.SSLSocket, "do_handshake", failure if race == "handshake" else lambda _: None
        )
        if race == "read":
            monkeypatch.setattr(netfix, "_HTTP", failure)

    net = NetFixture(**packages)
    with net, connect(net, "https"):
        assert held.wait(60), "TLS worker never reached the seam"
        guarded(net.__exit__)
    assert not net.errors
    assert not net.directory.exists()


def test_service_failures_annotate_the_body_exception(packages, tmp_path, monkeypatch):
    def broken_get(handler):
        raise OSError(errno.EIO, "injected service failure")

    monkeypatch.setattr(netfix._HTTP, "do_GET", broken_get)
    artifacts = tmp_path / "artifacts"
    body_error = ValueError("original body failure")
    with (
        pytest.raises(ValueError, match="original body failure") as caught,
        NetFixture(**packages, artifacts=artifacts) as net,
    ):
        with pytest.raises(http.client.RemoteDisconnected):
            get(net)
        raise body_error
    assert caught.value is body_error
    assert len(body_error.__notes__) == 1
    assert str(artifacts / "services.log") in body_error.__notes__[0]
    assert "injected service failure" in body_error.__notes__[0]
    assert "service=http unexpected exception" in (artifacts / "services.log").read_text()


@pytest.mark.parametrize("operation", ["read", "sendall", "shutdown"])
@pytest.mark.parametrize(
    "code", [errno.EPIPE, errno.ECONNRESET, errno.ECONNABORTED, errno.ENOTCONN, errno.EINVAL]
)
def test_relay_sender_expected_disconnects_are_informational(
    packages, monkeypatch, caplog, operation, code
):
    caplog.set_level(logging.INFO, logger="lumen_harness.netfix.relay")
    with NetFixture(**packages) as net:
        guest, peer = socket.socketpair()
        with guest, peer:
            guest.settimeout(60)
            guest.sendall(b"late guest bytes")
            guest.shutdown(socket.SHUT_WR)
            read = os.read
            sender, injected = None, []

            def sender_read(fd, size):
                nonlocal sender
                if fd == peer.fileno():
                    sender = threading.current_thread()
                    if operation == "read":
                        injected.append(code)
                        raise OSError(code, "injected sender disconnect")
                return read(fd, size)

            with monkeypatch.context() as patch:
                patch.setattr(netfix.os, "read", sender_read)
                if operation != "read":
                    original = getattr(socket.socket, operation)

                    def fail(client, *args):
                        if threading.current_thread() is sender and (
                            operation != "shutdown" or args[0] == socket.SHUT_WR
                        ):
                            injected.append(code)
                            raise OSError(code, "injected sender disconnect")
                        return original(client, *args)

                    patch.setattr(socket.socket, operation, fail)
                result = guarded(
                    lambda: netfix.relay(
                        str(net.socket("echo")), input_fd=peer.fileno(), output_fd=peer.fileno()
                    )
                )
            assert result == 0
            assert injected
            assert "relay sender disconnected" in caplog.text
            assert "ERROR" not in caplog.text


def test_fixture_honors_openssl_override_without_path(packages, tmp_path, monkeypatch):
    openssl = netfix._alpine().find_openssl()
    qemu = shutil.which(qemu_binary())
    assert qemu is not None
    mkfs, _ = rootfs.find_e2fsprogs()
    source = tmp_path / "master.ext4"
    with source.open("wb") as stream:
        stream.truncate(16 << 20)
    subprocess.run([str(mkfs), "-q", "-t", "ext4", "-F", str(source)], check=True)
    empty = tmp_path / "empty-path"
    empty.mkdir()
    monkeypatch.setenv("PATH", str(empty))
    monkeypatch.setenv("LUMEN_OPENSSL", str(openssl))
    monkeypatch.setenv("LUMEN_QEMU", qemu)
    monkeypatch.setenv("LUMEN_E2FSPROGS_SBIN", str(mkfs.parent))
    assert shutil.which("openssl") is None
    with NetFixture(**packages) as net:
        # Generation must produce a usable server chain, signing a verifiable
        # repository, and prepare_image a hashed CApath, all via the override.
        context = ssl.create_default_context(cafile=str(net.ca))
        assert get(net, context=context)[0] == 200
        signature, signed = split_signature(net.apk_index.read_bytes())
        signature_file = tmp_path / "signature"
        signature_file.write_bytes(signature)
        assert b"Verified OK" in netfix._openssl(
            "dgst",
            "-sha1",
            "-verify",
            str(net.apk_public_key),
            "-signature",
            str(signature_file),
            data=signed,
        )
        image = net.prepare_image(tmp_path / "copy", src=source)
        ca_hash = (
            netfix._openssl("x509", "-in", str(net.ca), "-noout", "-subject_hash").decode().strip()
        )
        assert f"{ca_hash}.0" in rootfs.debugfs_names(image, "/etc/ssl/certs")
        assert (
            rootfs.debugfs(image, "cat /etc/ssl/certs/ca-cert-lumen-run.pem") == net.ca.read_text()
        )


def test_fixture_missing_openssl_override_is_actionable(packages, tmp_path, monkeypatch):
    missing = tmp_path / "missing-openssl"
    monkeypatch.setenv("LUMEN_OPENSSL", str(missing))
    net = NetFixture(**packages)
    # An invalid explicit override must fail even when a usable OpenSSL exists
    # on PATH or in Homebrew; silently falling back hides configuration errors.
    with pytest.raises(netfix._alpine().LockError) as caught, net:
        pytest.fail("invalid OpenSSL override was ignored")
    message = str(caught.value)
    assert "OpenSSL >= 3 is required" in message
    assert "install openssl@3 or set LUMEN_OPENSSL" in message
    assert str(missing) in message
    assert not net.directory.exists()
