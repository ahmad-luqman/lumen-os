#!/bin/sh
# The M8 gate workloads under strace, inside the census container (#337).
# Run by tools/syscall-census.sh, never by hand:
#
#   docker run ... -v tools/census:/census:ro -v <harness>:/harness:ro -v <out>:/out IMAGE sh /census/workloads.sh
#
# Writes /out/<workload>.strace (one per workload), the container's facts
# (alpine-release, kernel, packages) and /out/m8-census.toml (built by
# lumen_harness/syscalls.py). Every workload must succeed: a census of a
# failed build would miss the calls its success makes.
set -eu

OUT=/out
W=/work
mkdir -p "$W"
cd "$W"

apk add --no-cache -q strace gcc g++ make musl-dev git sqlite vim python3 >/dev/null
cat /etc/alpine-release >"$OUT/alpine-release"
uname -r >"$OUT/kernel"
# Not piped into sort: set -e would miss apk failing.
packages=$(apk info -v)
printf '%s\n' "$packages" | sort >"$OUT/packages"

# The tracer: every child followed; -X raw prints flags and constants as
# numbers; -s 0 drops string contents. Split into words where it's used.
STRACE="strace -f -qq -X raw -s 0 -e signal=none"

# One workload: its name, then the command.
trace() {
    name=$1
    shift
    echo "census: $name"
    $STRACE -o "$OUT/$name.strace" "$@"
}

# Sanity first: an x86-64 kernel names its calls (a translated trace prints
# syscall_0x... instead, and syscalls.py refuses it).
strace -f -qq -o "$W/sanity.strace" true
grep -q 'execve(' "$W/sanity.strace" || {
    echo "census: strace sees no execve: not a real x86-64 Linux kernel" >&2
    exit 1
}

# 1. ash: a pipeline, redirections, a subshell, $(...), a trap, a background job.
cat >ash.sh <<'SH'
set -e
echo c a b | tr ' ' '\n' | sort -r >out.txt
cat <out.txt >>out2.txt 2>&1
n=$(wc -l <out.txt)
(cd /tmp && pwd) >/dev/null
trap 'echo caught INT' INT
kill -INT $$
sleep 0.1 &
wait $!
test "$n" -eq 3
SH
trace ash /bin/busybox ash ash.sh

# 2. gcc: a C program built and run.
cat >hello.c <<'C'
#include <stdio.h>
int main(void) { puts("hello"); return 0; }
C
trace gcc sh -c 'gcc -O2 -o hello hello.c && ./hello'

# 3. g++: an exception thrown across a shared library and caught.
cat >lib.cpp <<'CPP'
#include <stdexcept>
extern "C++" int risky(int x) { if (x > 1) throw std::runtime_error("too big"); return x; }
CPP
cat >main.cpp <<'CPP'
#include <iostream>
#include <stdexcept>
int risky(int x);
int main() {
    try { risky(2); } catch (const std::exception &e) { std::cout << "caught " << e.what() << "\n"; return 0; }
    return 1;
}
CPP
trace gpp sh -c 'g++ -O2 -fPIC -shared -o libthrow.so lib.cpp && g++ -O2 -o main main.cpp -L. -lthrow -Wl,-rpath,. && ./main'

# 4. make: four objects (main and three others), serially and two at a time.
mkdir -p mk
cd mk
for f in a b c; do printf 'int %s(void) { return 1; }\n' "$f" >"$f.c"; done
printf 'int a(void); int b(void); int c(void);\nint main(void) { return a() + b() + c() - 3; }\n' >main.c
printf 'prog: main.o a.o b.o c.o\n\tgcc -o $@ $^\n%%.o: %%.c\n\tgcc -c -o $@ $<\nclean:\n\trm -f *.o prog\n' >Makefile
trace make sh -c 'make -j1 && ./prog && make clean && make -j2 && ./prog'
cd "$W"

# 5. git: a repository's everyday life, gc included.
trace git sh -c '
    git -c init.defaultBranch=main init -q repo && cd repo &&
    git config user.email census@example.invalid && git config user.name census &&
    echo one >f && git add f && git commit -qm one &&
    git checkout -qb side && echo two >>f && git commit -qam two &&
    git checkout -q main && echo other >g && git add g && git commit -qm other &&
    git merge -q --no-edit side && git log --oneline >/dev/null &&
    git diff HEAD~1 >/dev/null && git status --short >/dev/null && git gc -q'

# 6. sqlite3: a thousand rows, with a rollback journal and with WAL.
cat >db.sql <<'SQL'
CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT);
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1000)
INSERT INTO t(v) SELECT 'row' || i FROM n;
SELECT count(*) FROM t;
SQL
trace sqlite sh -c 'sqlite3 journal.db <db.sql && sqlite3 wal.db "PRAGMA journal_mode=WAL;" && sqlite3 wal.db <db.sql'
# The pragma succeeds even when the mode stays as it was; check it took.
test "$(sqlite3 wal.db 'PRAGMA journal_mode;')" = wal

# 7. vim: scripted edits, then an interactive edit over a pty.
trace vim vim -Nu NONE -es +'call setline(1, "hello")' +wq scripted.txt
cat >drive_pty.py <<'PY'
import os, pty, select, sys, time
pid, fd = pty.fork()
if pid == 0:
    os.environ["TERM"] = "xterm"
    os.execvp(sys.argv[1], sys.argv[1:])
def drain(seconds):
    end = time.time() + seconds
    while (left := end - time.time()) > 0:
        if not select.select([fd], [], [], left)[0]:
            return
        try:
            if not os.read(fd, 65536):
                return
        except OSError:
            return
drain(1.0)
os.write(fd, b"ihello from a pty\x1b")
drain(0.5)
os.write(fd, b":wq\r")
drain(1.0)
_, status = os.waitpid(pid, 0)
sys.exit(os.waitstatus_to_exitcode(status))
PY
echo "census: vim-pty"
python3 drive_pty.py $STRACE -o "$OUT/vim-pty.strace" vim -Nu NONE typed.txt
grep -q 'hello from a pty' typed.txt

# 8. pthreads: four threads taking one mutex, joined. Under strace each thread
#    tends to finish before the next starts, so the mutex is rarely contended.
cat >threads.c <<'C'
#include <pthread.h>
#include <stdio.h>
static pthread_mutex_t m = PTHREAD_MUTEX_INITIALIZER;
static long total;
static void *work(void *arg) {
    for (int i = 0; i < 20000; i++) { pthread_mutex_lock(&m); total++; pthread_mutex_unlock(&m); }
    return arg;
}
int main(void) {
    pthread_t t[4];
    for (int i = 0; i < 4; i++) pthread_create(&t[i], 0, work, 0);
    for (int i = 0; i < 4; i++) pthread_join(t[i], 0);
    printf("%ld\n", total);
    return total == 80000 ? 0 : 1;
}
C
gcc -O2 -pthread -o threads threads.c
trace pthread ./threads

# 9. signals: SA_SIGINFO|SA_RESTART, sigaltstack, a caught SIGSEGV, SIGCHLD,
#    and a nanosleep cut short by an alarm.
cat >signals.c <<'C'
#include <setjmp.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
static sigjmp_buf env;
static volatile sig_atomic_t usr1, chld, alrm;
static void on_usr1(int s, siginfo_t *i, void *c) { (void)s; (void)i; (void)c; usr1 = 1; }
static void on_segv(int s) { (void)s; siglongjmp(env, 1); }
static void on_chld(int s) { (void)s; chld = 1; }
static void on_alrm(int s) { (void)s; alrm = 1; }
int main(void) {
    struct sigaction sa = {0};
    sa.sa_sigaction = on_usr1; sa.sa_flags = SA_SIGINFO | SA_RESTART; sigaction(SIGUSR1, &sa, 0);
    raise(SIGUSR1);
    stack_t ss = { .ss_sp = malloc(SIGSTKSZ), .ss_size = SIGSTKSZ };
    sigaltstack(&ss, 0);
    struct sigaction sv = {0}; sv.sa_handler = on_segv; sv.sa_flags = SA_ONSTACK; sigaction(SIGSEGV, &sv, 0);
    if (sigsetjmp(env, 1) == 0) *(volatile int *)0 = 1;
    struct sigaction sc = {0}; sc.sa_handler = on_chld; sigaction(SIGCHLD, &sc, 0);
    pid_t p = fork();
    if (p == 0) _exit(7);
    int st; while (waitpid(p, &st, 0) < 0) {}
    struct sigaction sl = {0}; sl.sa_handler = on_alrm; sigaction(SIGALRM, &sl, 0);
    alarm(1);
    struct timespec ts = { .tv_sec = 5 }, left;
    int r = nanosleep(&ts, &left);
    printf("usr1=%d chld=%d alrm=%d exit=%d eintr=%d\n", usr1, chld, alrm, WEXITSTATUS(st), r < 0);
    return usr1 && chld && alrm && WEXITSTATUS(st) == 7 && r < 0 ? 0 : 1;
}
C
gcc -O2 -o signals signals.c
trace signals ./signals

python3 /harness/syscalls.py build --traces "$OUT" --meta "$OUT" -o "$OUT/m8-census.toml"
echo "census: wrote m8-census.toml"
