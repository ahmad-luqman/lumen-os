"""Linux ABI conformance suites (ARCHITECTURE section 15): musl's libc-test
core in M8 (D4, #396), gVisor's syscall suites in M9 and M10.

The core runs every built static and dynamic test under its own runtest."""

import os
import sys

import pytest
from lumen_harness import libctest
from lumen_harness.crash import BEGIN as CRASH_BEGIN
from lumen_harness.qemu import EXIT_SUCCESS, Devices
from lumen_harness.rootfs import BUSYBOX_EXIT, copy_rootfs, debugfs, e2fsck_check
from lumen_harness.shell import PROMPT, run


def test_musl_libc_test_core(boot, tmp_path, artifacts):
    """D4: every built libc-test runs in Lumen, with strict expected failures.

    Timeouts and listed flaky failures get one retry, preserving both
    attempts. Missing EXIT lines and runner errors cannot be expected away.
    """
    # libc-test is its own target (`make libc-test`, after `make deps-zig`):
    # `make test` leaves it out, so a local run without it skips. CI builds
    # it first, so there a missing build fails rather than skips (as
    # test_libctest.py's _require_build and D1's developer-image guard do).
    if not (libctest.BUILD / libctest.TESTS_FILE).is_file():
        if os.environ.get("CI") and sys.platform == "linux":
            pytest.fail("build/libc-test/tests.txt is missing: CI runs `make libc-test` first")
        pytest.skip("no build/libc-test (run `make deps-zig libc-test`)")
    tests = libctest.read_tests()
    assert len(tests) == len(set(tests)) == 284
    assert sum(libctest.is_static(test) for test in tests) == 140
    assert (libctest.BUILD / "not-built.txt").read_text() == ""
    assert libctest.linkage_problems() == []
    xfail = libctest.load_xfail()
    # The minirootfs supplies musl and busybox; only this test's private
    # image is staged. Keep the build's relative paths for dlopen's DSOs.
    disk = copy_rootfs(tmp_path)
    debugfs(disk, "mkdir /libc-test", write=True)
    debugfs(disk, "mkdir /libc-test/src", write=True)
    for directory in ("common", "functional", "regression"):
        debugfs(disk, f"mkdir /libc-test/src/{directory}", write=True)
    libraries = sorted(
        p.relative_to(libctest.BUILD).as_posix() for p in libctest.BUILD.glob("src/*/*.so")
    )
    for name in [libctest.RUNTEST, *tests, *libraries, libctest.TESTS_FILE]:
        debugfs(disk, f'write "{libctest.BUILD / name}" /libc-test/{name}', write=True)
    script = tmp_path / "libc-test.sh"
    script.write_text(
        "cd /libc-test || exit 1\n"
        f'./{libctest.RUNTEST} -t 30 "$1" </dev/null 2>&1\n'
        "status=$?\n"
        'printf "\\nEXIT %s %s\\n" "$1" "$status"\n'
    )
    debugfs(disk, f'write "{script}" /libc-test.sh', write=True)
    e2fsck_check(disk)
    m = boot("qemutest", memory="512M", devices=Devices(blk_image=disk, rng=True))
    m.expect(PROMPT, 60)
    transcript = artifacts / "libc-test.log"

    def execute(test):
        # An outer harness limit, distinct from runtest's guest timeout.
        raw = run(
            m,
            f"linux /bin/busybox ash /libc-test.sh {test}",
            180,
            # libc-test deliberately faults children; runtest's verdict
            # decides whether that signal was expected by the test.
            allow=r"WARN  user: pid \d+: killed by signal \d+: trap \d+ error=0x[0-9a-f]+ rip=0x[0-9a-f]+ cr2=0x[0-9a-f]+$",
        )
        lines = raw.splitlines()
        done = BUSYBOX_EXIT.fullmatch(lines[-1]) if lines else None
        assert done and int(done.group(1)) == 0, raw
        return "\n".join(lines[:-1]) + "\n"

    with transcript.open("w") as output:
        for test in tests:
            entry = xfail.get(test)
            text = libctest.run_with_retry(
                test,
                lambda test=test: execute(test),
                retry_failure=entry is not None and entry.kind == "flaky",
            )
            output.write(text)
            output.flush()
    verdict = libctest.compare(libctest.parse(transcript.read_text()), xfail, tests)
    assert CRASH_BEGIN not in m.output
    assert run(m, "sync", 60) == "sync: ok\n"
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS
    e2fsck_check(disk)
    assert verdict.ok, verdict.summary()
