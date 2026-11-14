# Lumen

An AI-native, observable, safe-by-default hobby operating system for x86-64,
written in Rust. It's also a teaching OS: every subsystem should be able to
show you how it works while it runs.

**Status:** Milestone 0. It boots via Limine on UEFI, installs its own
GDT/IDT, logs over serial, draws a framebuffer "hello", and turns every CPU
exception into a symbolized crash report. See [docs/STATUS.md](docs/STATUS.md)
(generated from tests) and [docs/ROADMAP.md](docs/ROADMAP.md).

```bash
make deps    # pinned Limine + OVMF
make run     # boot in QEMU (type `help` at the lumen> prompt, try `crash pf`)
make test    # host tests, codegen audit, QEMU scenarios
```

Needs: rustup, `qemu-system-x86_64`, `xorriso`, `uv`, `make`.

Where it's going: Claude as a system service with capability-scoped agents,
time-travel (copy-on-write) storage, an X-ray view of the live kernel, a
structured shell, and a GUI where every app exposes an automation tree. Read
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

Contributing (human or agent): [CLAUDE.md](CLAUDE.md).

License: MIT.
