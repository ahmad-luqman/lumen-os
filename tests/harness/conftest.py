"""Small offline APK inputs shared by fixture and slirp harness tests."""

import gzip
import hashlib

import pytest
from netfix_fakes import archive


@pytest.fixture
def packages(tmp_path):
    payload = gzip.compress(archive({"usr/bin/hello": b"hello\n"}) + bytes(1024), mtime=0)
    info = (
        "pkgname = hello\npkgver = 1.0-r0\narch = x86_64\nsize = 6\n"
        "pkgdesc = local fixture\nlicense = MIT\ndepend = musl\ndepend = so:libc.musl-x86_64.so.1\n"
        "provides = cmd:hello=1.0-r0\ninstall_if = musl hello-helper\n"
        "provider_priority = 7\norigin = hello-source\nreplaces = old-hello\n"
        f"datahash = {hashlib.sha256(payload).hexdigest()}\n"
    ).encode()
    control = gzip.compress(archive({".PKGINFO": info}), mtime=0)
    blob = (
        gzip.compress(archive({".SIGN.RSA.upstream.pub": b"upstream"}), mtime=0) + control + payload
    )
    package = tmp_path / "hello-1.0-r0.apk"
    package.write_bytes(blob)
    lock = tmp_path / "packages.lock"
    lock.write_text(f"# minirootfs 3.21.10\nhello 1.0-r0 main {hashlib.sha256(blob).hexdigest()}\n")
    return {"lock": lock, "packages": tmp_path}
