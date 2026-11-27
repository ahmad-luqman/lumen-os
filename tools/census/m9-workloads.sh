#!/bin/sh
# The M9 gate workloads under strace, inside the census container (#451).
# Run by tools/syscall-census.sh m9, never by hand:
#
#   docker run ... -v tools/census:/census:ro -v <harness>:/harness:ro -v <out>:/out IMAGE sh /census/m9-workloads.sh
#
# Writes /out/<workload>.strace (one per workload), the container's facts
# (alpine-release, kernel, packages), /out/m9-proc-net.txt (/proc/net/* probe
# for #450) and /out/m9-census.toml (built by lumen_harness/syscalls.py).
# Every workload talks only to local loopback servers inside the container
# (no internet beyond installing the setup packages) and must succeed.
set -eu

OUT=/out
W=/work
mkdir -p "$W/ca" "$W/srv/mirror/x86_64" "$W/pyext" "$W/gem/ext/census_ext"
cd "$W"

apk add --no-cache -q \
    strace gcc musl-dev make curl openssl ca-certificates \
    python3 python3-dev py3-setuptools ruby ruby-dev sqlite >/dev/null

cat /etc/alpine-release >"$OUT/alpine-release"
uname -r >"$OUT/kernel"
apk info -v >"$OUT/packages"

# The tracer: every child followed; -X raw prints flag and constant arguments
# as numbers; -s 0 drops buffer contents (paths are still printed).
STRACE="strace -f -qq -X raw -s 0 -e signal=none"

trace() {
    name=$1
    shift
    echo "census(m9): $name"
    $STRACE -o "$OUT/$name.strace" "$@"
}

# Sanity first: an x86-64 kernel names its calls (a translated trace prints
# syscall_0x... instead, and syscalls.py refuses it).
strace -f -qq -o "$W/sanity.strace" true
grep -q 'execve(' "$W/sanity.strace" || {
    echo "census(m9): strace sees no execve: not a real x86-64 Linux kernel" >&2
    exit 1
}

# Test CA + localhost server cert (SAN: localhost, 127.0.0.1, ::1).
openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
    -keyout "$W/ca/ca.key" -out "$W/ca/ca.crt" \
    -subj "/CN=Lumen Census CA" \
    -addext "basicConstraints=critical,CA:TRUE" \
    -addext "keyUsage=critical,keyCertSign,cRLSign" 2>/dev/null
cat >"$W/ca/server.ext" <<'EXT'
subjectAltName=DNS:localhost,IP:127.0.0.1,IP:::1
basicConstraints=CA:FALSE
keyUsage=digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
EXT
openssl req -newkey rsa:2048 -nodes \
    -keyout "$W/ca/server.key" -out "$W/ca/server.csr" \
    -subj "/CN=localhost" 2>/dev/null
openssl x509 -req -days 1 \
    -in "$W/ca/server.csr" -CA "$W/ca/ca.crt" -CAkey "$W/ca/ca.key" \
    -CAcreateserial -out "$W/ca/server.crt" -extfile "$W/ca/server.ext" 2>/dev/null

cp "$W/ca/ca.crt" /usr/local/share/ca-certificates/census-ca.crt
update-ca-certificates >/dev/null 2>&1

# Local APK mirror holding one small package not yet installed (`patch`).
# `apk add --no-cache` above stored no index; `apk fetch` needs one.
apk update -q
apk fetch -q -o "$W/srv/mirror/x86_64" patch
apk index -q -o "$W/srv/mirror/x86_64/APKINDEX.tar.gz" "$W/srv/mirror/x86_64"/*.apk
printf 'hello from census\n' >"$W/srv/hello.txt"

# Start local HTTPS + HTTP server outside strace; wait for bound ports.
python3 /census/local_server.py \
    --root "$W/srv" \
    --cert "$W/ca/server.crt" \
    --key "$W/ca/server.key" \
    --ports-file "$W/ports.env" &
SRV_PID=$!
trap 'kill "$SRV_PID" 2>/dev/null || true' EXIT

i=0
while [ ! -f "$W/ports.env" ]; do
    i=$((i + 1))
    if [ "$i" -ge 200 ]; then
        echo "census(m9): local_server.py did not become ready" >&2
        exit 1
    fi
    sleep 0.05
done
# shellcheck source=/dev/null
. "$W/ports.env"

# 1. apk: `apk add` from our local HTTPS mirror (TLS verified against the test CA).
printf 'https://localhost:%s/mirror\n' "$HTTPS_PORT" >"$W/repos"
trace apk apk add --no-cache --allow-untrusted --repositories-file "$W/repos" patch >/dev/null
test -x /usr/bin/patch

# 2. curl: `curl https://` against our local server with the test CA.
trace curl curl -fsS --cacert "$W/ca/ca.crt" -o "$W/curl.out" \
    "https://localhost:$HTTPS_PORT/hello.txt"
test "$(cat "$W/curl.out")" = "hello from census"

# 3. python3 asyncio: TCP echo server and 3 concurrent clients on loopback.
cat >"$W/py_asyncio.py" <<'PY'
import asyncio

async def handle(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
    while data := await reader.readline():
        writer.write(data)
        await writer.drain()
    writer.close()
    await writer.wait_closed()

async def client(port: int, idx: int) -> None:
    reader, writer = await asyncio.open_connection("127.0.0.1", port)
    for n in range(3):
        msg = f"ping-{idx}-{n}\n".encode()
        writer.write(msg)
        await writer.drain()
        got = await reader.readline()
        assert got == msg, (got, msg)
    writer.close()
    await writer.wait_closed()

async def main() -> None:
    server = await asyncio.start_server(handle, "127.0.0.1", 0)
    port = server.sockets[0].getsockname()[1]
    async with server:
        await asyncio.gather(*(client(port, i) for i in range(3)))

asyncio.run(main())
PY
trace python-asyncio python3 "$W/py_asyncio.py"

# 4. python3 sqlite3: WAL table creation, 1000 inserts and query.
cat >"$W/py_sqlite.py" <<'PY'
import sqlite3

con = sqlite3.connect("/work/py.db")
mode = con.execute("PRAGMA journal_mode=WAL;").fetchone()[0]
assert mode == "wal", mode
con.execute("CREATE TABLE items(id INTEGER PRIMARY KEY, v TEXT)")
con.executemany("INSERT INTO items(v) VALUES (?)", [(f"row-{i}",) for i in range(1000)])
con.commit()
assert con.execute("SELECT count(*) FROM items").fetchone()[0] == 1000
con.close()
PY
trace python-sqlite python3 "$W/py_sqlite.py"

# 5. python3 C extension: build offline with setuptools and import.
cat >"$W/pyext/census_pyext.c" <<'C'
#define PY_SSIZE_T_CLEAN
#include <Python.h>

static PyObject *py_add(PyObject *self, PyObject *args) {
    (void)self;
    long a, b;
    if (!PyArg_ParseTuple(args, "ll", &a, &b)) {
        return NULL;
    }
    return PyLong_FromLong(a + b);
}

static PyMethodDef methods[] = {
    {"add", py_add, METH_VARARGS, "add two integers"},
    {NULL, NULL, 0, NULL},
};

static struct PyModuleDef mod = {
    PyModuleDef_HEAD_INIT, "census_pyext", NULL, -1, methods,
};

PyMODINIT_FUNC PyInit_census_pyext(void) {
    return PyModule_Create(&mod);
}
C
cat >"$W/pyext/setup.py" <<'PY'
from setuptools import Extension, setup

setup(
    name="census_pyext",
    version="0.1.0",
    ext_modules=[Extension("census_pyext", ["census_pyext.c"])],
)
PY
trace python-cext sh -c "
    cd '$W/pyext' &&
    python3 setup.py -q build_ext --inplace &&
    python3 -c 'import census_pyext; assert census_pyext.add(20, 22) == 42'
"

# 6. ruby Net::HTTP against our local HTTPS and HTTP servers.
cat >"$W/rb_http.rb" <<'RB'
require "net/http"
require "openssl"
require "uri"

https_uri = URI(ENV.fetch("HTTPS_URL"))
http = Net::HTTP.new(https_uri.host, https_uri.port)
http.use_ssl = true
http.verify_mode = OpenSSL::SSL::VERIFY_PEER
http.ca_file = ENV.fetch("CA_CERT")
res = http.get(https_uri.request_uri)
raise "bad https status #{res.code}" unless res.code == "200"
raise "bad https body #{res.body.inspect}" unless res.body.strip == "hello from census"

http_res = Net::HTTP.get_response(URI(ENV.fetch("HTTP_URL")))
raise "bad http status #{http_res.code}" unless http_res.code == "200"
raise "bad http body #{http_res.body.inspect}" unless http_res.body.strip == "hello from census"
RB
HTTPS_URL="https://localhost:$HTTPS_PORT/hello.txt" \
HTTP_URL="http://127.0.0.1:$HTTP_PORT/hello.txt" \
CA_CERT="$W/ca/ca.crt" \
    trace ruby-http ruby "$W/rb_http.rb"

# 7. ruby Fiber: yield/resume coroutine switching.
cat >"$W/rb_fiber.rb" <<'RB'
f1 = Fiber.new do
  Fiber.yield 10
  20
end
f2 = Fiber.new do |x|
  Fiber.yield x * 2
  x * 3
end
vals = [f1.resume, f2.resume(5), f1.resume, f2.resume]
raise "bad fiber vals #{vals.inspect}" unless vals == [10, 10, 20, 15]
RB
trace ruby-fiber ruby "$W/rb_fiber.rb"

# 8. ruby native gem: build, install locally (offline) and require.
cat >"$W/gem/census_ext.gemspec" <<'RB'
Gem::Specification.new do |s|
  s.name       = "census_ext"
  s.version    = "0.1.0"
  s.summary    = "census native extension"
  s.authors    = ["census"]
  s.files      = ["ext/census_ext/extconf.rb", "ext/census_ext/census_ext.c"]
  s.extensions = ["ext/census_ext/extconf.rb"]
end
RB
cat >"$W/gem/ext/census_ext/extconf.rb" <<'RB'
require "mkmf"
create_makefile("census_ext")
RB
cat >"$W/gem/ext/census_ext/census_ext.c" <<'C'
#include <ruby.h>

static VALUE rb_add(VALUE self, VALUE a, VALUE b) {
    (void)self;
    return LONG2NUM(NUM2LONG(a) + NUM2LONG(b));
}

void Init_census_ext(void) {
    VALUE mod = rb_define_module("CensusExt");
    rb_define_singleton_method(mod, "add", rb_add, 2);
}
C
trace ruby-gem sh -c "
    cd '$W/gem' &&
    gem build census_ext.gemspec >/dev/null &&
    gem install --local --no-document --ignore-dependencies --install-dir '$W/gems' ./census_ext-0.1.0.gem >/dev/null &&
    GEM_PATH='$W/gems' ruby -rcensus_ext -e 'raise unless CensusExt.add(20, 22) == 42'
"

# 9. busybox wget (HTTP + HTTPS against local server).
trace busybox-wget sh -c "
    /bin/busybox wget -qO '$W/wget-http.out' 'http://127.0.0.1:$HTTP_PORT/hello.txt' &&
    /bin/busybox wget -qO '$W/wget-https.out' 'https://localhost:$HTTPS_PORT/hello.txt'
"
test "$(cat "$W/wget-http.out")" = "hello from census"
test "$(cat "$W/wget-https.out")" = "hello from census"

# 10. busybox nc: TCP loopback server + client exchange.
NC_PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); p=s.getsockname()[1]; s.close(); print(p)')
printf 'ping-nc\n' >"$W/nc-client.in"
trace busybox-nc sh -c "
    printf 'pong-nc\n' | /bin/busybox nc -w 5 -l -s 127.0.0.1 -p '$NC_PORT' >'$W/nc-server.out' &
    srv=\$!
    i=0
    while ! /bin/busybox nc -w 2 127.0.0.1 '$NC_PORT' <'$W/nc-client.in' >'$W/nc-client.out' 2>/dev/null; do
        i=\$((i + 1))
        test \$i -lt 50
        sleep 0.05
    done
    wait \$srv
"
grep -q 'ping-nc' "$W/nc-server.out"

# 11. busybox ping: two ICMP echo packets to loopback.
trace busybox-ping /bin/busybox ping -c 2 -W 2 127.0.0.1 >/dev/null

# 12. socketpair/SCM_RIGHTS, epoll, eventfd, timerfd and TCP microbenchmark.
gcc -O2 -Wall -Wextra -Werror -o "$W/m9_net_bench" /census/m9_net_bench.c
trace net-bench "$W/m9_net_bench"

# Record any /proc/net/* opens seen in raw traces for #450.
if grep -Hn '"/proc/net' "$OUT"/*.strace >"$OUT/m9-proc-net.txt" 2>/dev/null; then
    :
else
    printf 'none\n' >"$OUT/m9-proc-net.txt"
fi

python3 /harness/syscalls.py build --milestone M9 --traces "$OUT" --meta "$OUT" -o "$OUT/m9-census.toml"
echo "census(m9): wrote m9-census.toml"
