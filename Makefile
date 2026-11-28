# Lumen build entry points. Run `make help` for the list.
#
# QEMU command lines live in one place (tools/harness/lumen_harness/qemu.py)
# and are shared by `run`, `debug` and the tests.

SHELL := bash
.SHELLFLAGS := -euo pipefail -c
.DEFAULT_GOAL := help

# smoltcp's build script gives environment overrides precedence over the pinned
# Cargo features. Reject even empty/unknown names before any recipe can run.
# Also catches command-line Make assignments, which Make exports to recipes.
LUMEN_SMOLTCP_OVERRIDES := $(filter SMOLTCP_%,$(.VARIABLES))
ifneq ($(strip $(LUMEN_SMOLTCP_OVERRIDES)),)
$(error smoltcp configuration is pinned in Cargo.toml; unset $(LUMEN_SMOLTCP_OVERRIDES))
endif

TARGET      := x86_64-unknown-none
KERNEL_OUT  := target/$(TARGET)/release/lumen-kernel
BUILD       := build
CARGO_KERNEL := cargo build -p lumen-kernel --target $(TARGET) --release
HARNESS     := PYTHONPATH=tools/harness uv run --frozen python -m lumen_harness

# `.rhai`: the built-in scripts, embedded with `include_str!`.
# apps/ (its sources, manifests and lock file): the kernels embed the
# modules `make apps` builds from them (kernel/build.rs), so a change there
# rebuilds the kernels, `apps` first. vendor/: patched dependencies (#313).
KERNEL_SRCS := $(shell find kernel crates apps vendor -name '*.rs' -o -name '*.toml' -o -name '*.ld' -o -name '*.rhai') Cargo.toml Cargo.lock apps/Cargo.lock .cargo/config.toml rust-toolchain.toml

# Both kernels are built into the same cargo output path and copied out, so
# their recipes must never run concurrently (`make -j`).
.NOTPARALLEL:

.PHONY: help deps build app apps iso rootfs run debug test test-host test-scenarios audit lint fmt fmt-check status status-check census golden clean

help: ## Show this help
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  make %-15s %s\n", $$1, $$2}'

deps: $(BUILD)/deps/limine/.version $(BUILD)/deps/ovmf/.version $(BUILD)/deps/busybox/.version $(BUILD)/deps/alpine-minirootfs/.version ## Download pinned Limine + OVMF + Alpine busybox + Alpine minirootfs (checksummed)

$(BUILD)/deps/limine/.version $(BUILD)/deps/ovmf/.version $(BUILD)/deps/busybox/.version $(BUILD)/deps/alpine-minirootfs/.version: tools/fetch-deps.sh
	tools/fetch-deps.sh

# Static musl x86-64 hello binary (M6, #238): cross-compiled with rustc + rust-lld
# from the pinned rustup sysroot (zero external C compiler required).
$(BUILD)/linux/hello: tools/linux-hello/hello.rs rust-toolchain.toml
	@mkdir -p $(BUILD)/linux
	SYSROOT=$$(rustc --print sysroot) && HOST=$$(rustc --print host-tuple) && \
	rustc --edition 2024 -O --target x86_64-unknown-linux-musl \
		-C linker="$$SYSROOT/lib/rustlib/$$HOST/bin/rust-lld" \
		-C linker-flavor=ld.lld \
		-C target-feature=+crt-static \
		-C strip=symbols \
		$< -o $@

# Dynamic musl x86-64 M7 test library and executable (#252): cross-compiled with
# rustc + rust-lld against Alpine's ld-musl-x86_64.so.1 (no C cross-compiler).
$(BUILD)/linux/libm7.so: tools/linux-m7/libm7.rs $(BUILD)/deps/alpine-minirootfs/.version rust-toolchain.toml
	@mkdir -p $(BUILD)/linux
	SYSROOT=$$(rustc --print sysroot) && HOST=$$(rustc --print host-tuple) && \
	MUSL_SC="$$SYSROOT/lib/rustlib/x86_64-unknown-linux-musl/lib/self-contained" && \
	rustc --edition 2024 -O --crate-type cdylib --target x86_64-unknown-linux-musl \
		-C panic=abort \
		-C strip=symbols \
		-C target-feature=-crt-static \
		-C linker="$$SYSROOT/lib/rustlib/$$HOST/bin/rust-lld" \
		-C linker-flavor=ld.lld \
		-C link-self-contained=no \
		-C default-linker-libraries=no \
		-C link-arg="$$MUSL_SC/crti.o" \
		-C link-arg="$$MUSL_SC/crtn.o" \
		-C link-arg="$(BUILD)/deps/alpine-minirootfs/lib/ld-musl-x86_64.so.1" \
		$< -o $@
	PYTHONPATH=tools/harness uv run --frozen python -m lumen_harness.rootfs $@ --rootfs-dir $(BUILD)/deps/alpine-minirootfs --no-interp

# The dynamic musl test executables' recipe, linked against Alpine's ld-musl:
# M7's and M8's.
define MUSL_EXE
	@mkdir -p $(BUILD)/linux
	SYSROOT=$$(rustc --print sysroot) && HOST=$$(rustc --print host-tuple) && \
	MUSL_SC="$$SYSROOT/lib/rustlib/x86_64-unknown-linux-musl/lib/self-contained" && \
	rustc --edition 2024 -O --target x86_64-unknown-linux-musl \
		-C panic=abort \
		-C strip=symbols \
		-C target-feature=-crt-static \
		-C linker="$$SYSROOT/lib/rustlib/$$HOST/bin/rust-lld" \
		-C linker-flavor=ld.lld \
		-C link-self-contained=no \
		-C default-linker-libraries=no \
		-C link-arg="$$MUSL_SC/Scrt1.o" \
		-C link-arg="$$MUSL_SC/crti.o" \
		-C link-arg="$$MUSL_SC/crtn.o" \
		-C link-arg="$(BUILD)/deps/alpine-minirootfs/lib/ld-musl-x86_64.so.1" \
		-C link-arg=--dynamic-linker=/lib/ld-musl-x86_64.so.1 \
		$< -o $@
	PYTHONPATH=tools/harness uv run --frozen python -m lumen_harness.rootfs $@ --rootfs-dir $(BUILD)/deps/alpine-minirootfs
endef

# The M7 ones: m7_test (dlopen of libm7.so) and m7_vfs (#254's VFS exit
# scenarios, tests/scenarios/test_fs.py).
$(BUILD)/linux/m7_test $(BUILD)/linux/m7_vfs: $(BUILD)/linux/%: tools/linux-m7/%.rs $(BUILD)/deps/alpine-minirootfs/.version rust-toolchain.toml
	$(MUSL_EXE)

$(BUILD)/linux/m7_test: $(BUILD)/linux/libm7.so

# M8's (#371; ARCHITECTURE section 15, "M8's tests"); each `mod common;`s
# tools/linux-m8/common.rs.
M8_LINUX := m8_proc m8_io m8_vm ptyrun m8_pty
M8_LINUX_OUT := $(addprefix $(BUILD)/linux/,$(M8_LINUX))
$(M8_LINUX_OUT): $(BUILD)/linux/%: tools/linux-m8/%.rs tools/linux-m8/common.rs $(BUILD)/deps/alpine-minirootfs/.version rust-toolchain.toml
	$(MUSL_EXE)

$(BUILD)/linux/m8_proc: tools/linux-m8/signal_waits.rs tools/linux-m8/signals.rs tools/linux-m8/exec.rs tools/linux-m8/futex.rs tools/linux-m8/small.rs
$(BUILD)/linux/m8_proc: tools/linux-m8/threads.rs tools/linux-m8/timers.rs
$(BUILD)/linux/m8_proc: tools/linux-m8/sessions.rs

# Every Linux test binary: built by `build`, staged by both images.
LINUX_TEST_BINS := $(BUILD)/linux/hello $(BUILD)/linux/m7_test $(BUILD)/linux/m7_vfs $(BUILD)/linux/libm7.so $(M8_LINUX_OUT)

# Two kernels: the normal one (the shells) and the in-kernel test build.
# `| apps`: built first (order-only: the kernel rebuilds when cargo says).
$(BUILD)/lumen-kernel: $(KERNEL_SRCS) | apps
	$(CARGO_KERNEL)
	@mkdir -p $(BUILD) && cp $(KERNEL_OUT) $@

$(BUILD)/lumen-kernel-ktest: $(KERNEL_SRCS) | apps
	$(CARGO_KERNEL) --features ktest
	@mkdir -p $(BUILD) && cp $(KERNEL_OUT) $@

build: apps $(LINUX_TEST_BINS) $(BUILD)/lumen-kernel $(BUILD)/lumen-kernel-ktest ## Build the apps, Linux test binaries, and both kernels

# Apps (M5; the layout: ARCHITECTURE section 8, "Building and bundling"):
# apps/ is its own workspace, and its crates that build a `cdylib` are WASM
# modules (libraries the apps share, `lumen-app` and `testkit`, are skipped). `app_out`
# says where each goes, and APP_OUT and APP_COPIES both come from it.
# Cargo runs inside apps/, whose .cargo/config.toml sets the target and the
# output directory (target-apps/; cargo names a module after its crate, `-`
# as `_`). Always run: cargo knows what changed. Copied with `cp -p` (a
# module's time is when cargo last built it) and only stale modules
# removed, so the kernels, which embed build/apps (kernel/build.rs),
# rebuild only when a module did. `|| exit 1`: make 3.81 (macOS's) ignores
# .SHELLFLAGS, so `-e` can't be relied on in a loop.
APP_TARGET := wasm32-unknown-unknown
APP_CRATES := $(patsubst apps/%/Cargo.toml,%,$(shell grep -l '"cdylib"' apps/*/Cargo.toml))
app_out     = $(if $(filter test-%,$(1)),$(BUILD)/apps/tests/$(patsubst test-%,%,$(1)).wasm,$(BUILD)/apps/$(1).wasm)
APP_OUT    := $(foreach c,$(APP_CRATES),$(call app_out,$(c)))
APP_COPIES := $(foreach c,$(APP_CRATES),$(subst -,_,$(c)).wasm:$(call app_out,$(c)))

# `make app NAME=foo`: apps/foo, a copy of apps/template (tools/new-app.sh).
app: ## Make a new app copied from apps/template: make app NAME=foo makes apps/foo
	tools/new-app.sh "$(NAME)"

apps: ## Build the WASM apps into build/apps/ (the test apps into build/apps/tests/)
	cd apps && cargo build --release
	@mkdir -p $(BUILD)/apps/tests && for copy in $(APP_COPIES); do \
		cp -p target-apps/$(APP_TARGET)/release/$${copy%%:*} $${copy#*:} || exit 1; done
	@for f in $(BUILD)/apps/*.wasm $(BUILD)/apps/tests/*.wasm; do \
		[ -e "$$f" ] || continue; case " $(APP_OUT) " in *" $$f "*) ;; *) rm -f "$$f";; esac; done

# Bootable images:
#   lumen.iso           interactive (`make run`)
#   lumen-qemutest.iso  normal kernel, may exit QEMU (crash -> exit 37)
#   lumen-ktest.iso     in-kernel test runner
ISO_DEPS := tools/mkiso.sh boot/limine.conf $(BUILD)/deps/limine/.version $(BUILD)/deps/busybox/.version $(BUILD)/linux/hello
$(BUILD)/lumen.iso: $(BUILD)/lumen-kernel $(ISO_DEPS)
	tools/mkiso.sh $< $@ $(CMDLINE)
$(BUILD)/lumen-desktop.iso: $(BUILD)/lumen-kernel $(ISO_DEPS)
	tools/mkiso.sh $< $@ $(CMDLINE) lumen.desktop
$(BUILD)/lumen-qemutest.iso: $(BUILD)/lumen-kernel $(ISO_DEPS)
	tools/mkiso.sh $< $@ lumen.qemu_exit
$(BUILD)/lumen-ktest.iso: $(BUILD)/lumen-kernel-ktest $(ISO_DEPS)
	tools/mkiso.sh $< $@ lumen.qemu_exit

iso: $(BUILD)/lumen.iso $(BUILD)/lumen-qemutest.iso $(BUILD)/lumen-ktest.iso ## Build all bootable ISOs

$(BUILD)/alpine-rootfs.ext4: tools/mkrootfs.sh tools/ext4-signed-hash.sh $(BUILD)/deps/alpine-minirootfs/.version $(LINUX_TEST_BINS)
	tools/mkrootfs.sh $@

rootfs: $(BUILD)/alpine-rootfs.ext4 ## Build deterministic Alpine minirootfs ext4 image

# `make run DESKTOP=1`: the same kernel with `lumen.desktop` (the desktop
# shown from the boot on).
RUN_ISO := $(if $(DESKTOP),$(BUILD)/lumen-desktop.iso,$(BUILD)/lumen.iso)
RUN_ROOTFS := $(if $(ROOTFS),$(BUILD)/alpine-rootfs.ext4)

run: $(RUN_ISO) $(RUN_ROOTFS) ## Boot in QEMU with a window; serial on this terminal (HEADLESS=1: no window; SOUND=1: virtio-sound on the host's audio; DESKTOP=1: start the desktop; ROOTFS=1: attach build/alpine-rootfs.ext4 via virtio-blk; any value counts, 0 too)
	$(HARNESS) run --iso $< $(if $(HEADLESS),--headless) $(if $(SOUND),--sound) $(if $(ROOTFS),--rootfs $(RUN_ROOTFS))

debug: $(BUILD)/lumen.iso ## Boot paused with a gdb stub on :1234, then attach gdb (stops at kmain)
	$(HARNESS) debug --iso $< --kernel $(BUILD)/lumen-kernel

test: test-host audit test-scenarios ## Everything CI runs: host tests, flag audit, QEMU scenarios

# Each step first deletes its own previous result file, so a failed run can
# never leave a stale "passed" behind for `make status`.
RESULTS := $(BUILD)/test-results

# The harness checks the built apps (tests/harness/test_wasm_module.py),
# built after the old results are gone: a failed apps build leaves none.
test-host: ## Host-side unit tests (Rust crates + harness self-tests)
	@mkdir -p $(RESULTS) && rm -f $(RESULTS)/cargo.json $(RESULTS)/harness.xml
	$(MAKE) --no-print-directory apps
	cargo test
	# The stacks tests again, optimized: compiler-reordering mistakes in the seqlock are likelier to surface.
	cargo test --release -p lumen-mm stacks
	@printf '{%s}\n' "$$(ls crates | sed 's/.*/"&": true/' | paste -sd, -)" > $(RESULTS)/cargo.json
	uv run --frozen pytest tests/harness -q --junitxml=$(RESULTS)/harness.xml

# Scenarios run in parallel, one QEMU at a time per worker (pytest-xdist):
# `auto` is one worker per logical CPU; SCENARIO_JOBS=0 runs them one at a time
# in the main pytest process. Don't run more workers than CPUs: a starved TCG
# guest loses local APIC timer ticks, and test_time and test_acpi count them.
SCENARIO_JOBS ?= auto

test-scenarios: iso rootfs ## Boot QEMU headless and run every scenario (incl. in-kernel tests; SCENARIO_JOBS=0: serially)
	@mkdir -p $(RESULTS) && rm -f $(RESULTS)/scenarios.xml $(RESULTS)/ktest.tap
	uv run --frozen pytest tests/scenarios -v -n $(SCENARIO_JOBS) --junitxml=$(RESULTS)/scenarios.xml

audit: build ## Prove kernel codegen flags and image layout (no SSE/AVX, no red zone, stack probes, kernel code model, sections in place)
	@mkdir -p $(RESULTS) && rm -f $(RESULTS)/audit.json
	$(HARNESS) audit $(BUILD)/lumen-kernel $(BUILD)/lumen-kernel-ktest

lint: ## clippy on host crates (incl. their tests), the kernel and the apps; ruff on the harness
	uv run --frozen ruff check tools tests
	cargo clippy --all-targets -- -D warnings
	cargo clippy -p lumen-kernel --target $(TARGET) --release -- -D warnings
	cargo clippy -p lumen-kernel --target $(TARGET) --release --features ktest -- -D warnings
	cd apps && cargo clippy --release -- -D warnings

fmt: ## Format Rust and Python
	cargo fmt --all
	cd apps && cargo fmt --all
	rustfmt --edition 2024 tools/linux-hello/hello.rs tools/linux-m7/*.rs tools/linux-m8/*.rs
	uv run --frozen ruff format tools tests

fmt-check: ## Check formatting
	cargo fmt --all -- --check
	cd apps && cargo fmt --all -- --check
	rustfmt --edition 2024 --check tools/linux-hello/hello.rs tools/linux-m7/*.rs tools/linux-m8/*.rs
	uv run --frozen ruff format --check tools tests

status: ## Regenerate docs/STATUS.md from the last test results
	$(HARNESS) status --write

status-check: ## Feature gate: fail unless every feature in tests/features/ passes
	$(HARNESS) status --check

DAYS ?= 7
census: ## CI's red first attempts over the last DAYS days, and the tests behind them (needs gh)
	$(HARNESS) census --days $(DAYS)

golden: iso ## Re-record golden screenshots (review the diff before committing!)
	LUMEN_UPDATE_GOLDEN=1 uv run --frozen pytest tests/scenarios -k golden -v

clean: ## Remove build outputs (keeps downloaded deps)
	cargo clean
	cd apps && cargo clean
	find $(BUILD) -mindepth 1 -maxdepth 1 ! -name deps -exec rm -rf {} +

# The Alpine developer image (#344): the minirootfs plus the closure of
# gcc, g++, make, musl-dev, binutils, git, sqlite and vim pinned in
# tools/alpine-packages.lock. Not part of `make deps`, `build` or `test`; CI
# runs both targets as steps of their own (ci.yml, its packages cached).
# deps-dev always runs the fetch (it skips files already verified), so it
# also repairs a deleted or corrupt .apk; the stamp orders the image after a
# fetch of the current lock, and deps-dev rewrites it only when the lock
# changed, so `make deps-dev rootfs-dev` doesn't rebuild an image that is
# up to date.
.PHONY: deps-dev rootfs-dev
$(BUILD)/deps/alpine-packages/.lock: tools/alpine-packages.lock tools/fetch-alpine-packages.sh tools/alpine-lock.py
	tools/fetch-alpine-packages.sh
	cp tools/alpine-packages.lock $@

deps-dev: ## Download the Alpine developer image's pinned packages (tools/alpine-packages.lock, sha256-checked)
	tools/fetch-alpine-packages.sh
	@mkdir -p $(BUILD)/deps/alpine-packages
	@cmp -s tools/alpine-packages.lock $(BUILD)/deps/alpine-packages/.lock || cp tools/alpine-packages.lock $(BUILD)/deps/alpine-packages/.lock

$(BUILD)/alpine-dev.ext4: tools/mkrootfs-dev.sh tools/alpine-lock.py tools/ext4-signed-hash.sh $(BUILD)/deps/alpine-packages/.lock $(BUILD)/deps/alpine-minirootfs/.version $(LINUX_TEST_BINS)
	tools/mkrootfs-dev.sh $@

rootfs-dev: $(BUILD)/alpine-dev.ext4 ## Build the deterministic Alpine developer image (gcc, g++, make, git, sqlite3, vim, curl, python3, ruby) for M8/M9's gates

# zig, pinned, for C cross-builds only (#345): the kernel, the apps and the
# Rust test binaries keep rustc + rust-lld. The script checks its own stamp
# (version and host), so the target always runs it.
.PHONY: deps-zig
deps-zig: ## Download the pinned zig, the C cross-compiler for host-built Linux test binaries (sha256-checked)
	tools/fetch-zig.sh

# musl's libc-test (#345) at its pinned commit, built static and dynamic for
# x86_64-linux-musl with the pinned zig into build/libc-test/ (its tests.txt,
# and not-built.txt with each test that didn't build and why). Both scripts
# check their own stamps, so a second run does nothing.
.PHONY: libc-test
libc-test: deps-zig ## Fetch musl libc-test (pinned commit) and build it with the pinned zig into build/libc-test/
	tools/fetch-libc-test.sh
	tools/build-libc-test.sh
