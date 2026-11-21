//! Minimal static musl x86-64 Linux ELF binary for Lumen M6 verification.
//!
//! Built standalone with `rustc --target x86_64-unknown-linux-musl` and
//! `rust-lld` from the pinned rustup sysroot (`-C target-feature=+crt-static`),
//! exercising real musl `_start`, TLS (`arch_prctl`), `set_tid_address`,
//! stdio (`ioctl`/`writev`/`write`), and `exit_group`.

fn main() {
    println!("hello from static musl on lumen");
}
