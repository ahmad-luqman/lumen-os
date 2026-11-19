# Lumen build entry points. Run `make help` for the list.
#
# QEMU command lines live in one place (tools/harness/lumen_harness/qemu.py)
# and are shared by `run`, `debug` and the tests.

SHELL := bash
.SHELLFLAGS := -euo pipefail -c
.DEFAULT_GOAL := help

TARGET      := x86_64-unknown-none
KERNEL_OUT  := target/$(TARGET)/release/lumen-kernel
BUILD       := build
CARGO_KERNEL := cargo build -p lumen-kernel --target $(TARGET) --release
HARNESS     := PYTHONPATH=tools/harness uv run --frozen python -m lumen_harness

# `.rhai`: the built-in scripts, embedded with `include_str!`.
# apps/ (its sources, manifests and lock file): the kernels embed the
# modules `make apps` builds from them (kernel/build.rs), so a change there
# rebuilds the kernels, `apps` first.
KERNEL_SRCS := $(shell find kernel crates apps -name '*.rs' -o -name '*.toml' -o -name '*.ld' -o -name '*.rhai') Cargo.toml Cargo.lock apps/Cargo.lock .cargo/config.toml rust-toolchain.toml

# Both kernels are built into the same cargo output path and copied out, so
# their recipes must never run concurrently (`make -j`).
.NOTPARALLEL:

.PHONY: help deps build apps iso run debug test test-host test-scenarios audit lint fmt fmt-check status status-check census golden clean

help: ## Show this help
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  make %-15s %s\n", $$1, $$2}'

deps: $(BUILD)/deps/limine/.version ## Download pinned Limine + OVMF (checksummed)

$(BUILD)/deps/limine/.version $(BUILD)/deps/ovmf/.version: tools/fetch-deps.sh
	tools/fetch-deps.sh

# Two kernels: the normal one (the shells) and the in-kernel test build.
# `| apps`: built first (order-only: the kernel rebuilds when cargo says).
$(BUILD)/lumen-kernel: $(KERNEL_SRCS) | apps
	$(CARGO_KERNEL)
	@mkdir -p $(BUILD) && cp $(KERNEL_OUT) $@

$(BUILD)/lumen-kernel-ktest: $(KERNEL_SRCS) | apps
	$(CARGO_KERNEL) --features ktest
	@mkdir -p $(BUILD) && cp $(KERNEL_OUT) $@

build: apps $(BUILD)/lumen-kernel $(BUILD)/lumen-kernel-ktest ## Build the apps and both kernels

# Apps (M5; the layout: ARCHITECTURE section 8, "Building and bundling"):
# apps/ is its own workspace, and its crates that build a `cdylib` are WASM
# modules (libraries the apps share, `testkit`, are skipped). `app_out`
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
ISO_DEPS := tools/mkiso.sh boot/limine.conf $(BUILD)/deps/limine/.version
$(BUILD)/lumen.iso: $(BUILD)/lumen-kernel $(ISO_DEPS)
	tools/mkiso.sh $< $@ $(CMDLINE)
$(BUILD)/lumen-desktop.iso: $(BUILD)/lumen-kernel $(ISO_DEPS)
	tools/mkiso.sh $< $@ $(CMDLINE) lumen.desktop
$(BUILD)/lumen-qemutest.iso: $(BUILD)/lumen-kernel $(ISO_DEPS)
	tools/mkiso.sh $< $@ lumen.qemu_exit
$(BUILD)/lumen-ktest.iso: $(BUILD)/lumen-kernel-ktest $(ISO_DEPS)
	tools/mkiso.sh $< $@ lumen.qemu_exit

iso: $(BUILD)/lumen.iso $(BUILD)/lumen-qemutest.iso $(BUILD)/lumen-ktest.iso ## Build all bootable ISOs

# `make run DESKTOP=1`: the same kernel with `lumen.desktop` (the desktop
# shown from the boot on).
RUN_ISO := $(if $(DESKTOP),$(BUILD)/lumen-desktop.iso,$(BUILD)/lumen.iso)

run: $(RUN_ISO) ## Boot in QEMU with a window; serial on this terminal (HEADLESS=1: no window; SOUND=1: virtio-sound on the host's audio; DESKTOP=1: start the desktop; any value counts, 0 too)
	$(HARNESS) run --iso $< $(if $(HEADLESS),--headless) $(if $(SOUND),--sound)

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

test-scenarios: iso ## Boot QEMU headless and run every scenario (incl. in-kernel tests; SCENARIO_JOBS=0: serially)
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
	uv run --frozen ruff format tools tests

fmt-check: ## Check formatting
	cargo fmt --all -- --check
	cd apps && cargo fmt --all -- --check
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
