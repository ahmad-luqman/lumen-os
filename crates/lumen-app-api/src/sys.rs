//! The `lumen` host functions as a module imports them (ARCHITECTURE
//! section 8, "The host interface"), declared once for every app: the SDK
//! (`lumen-app`'s `sys`) and the runtime's test apps (`testkit::host`)
//! re-export this. Built for wasm32 only. Pointers and lengths are wasm
//! `i32`s, every result `>= 0` or a negated [`crate::Error`]. The host
//! checks every range against the app's memory before it touches a byte,
//! so the calls that only read the app's memory (or none) are safe: a
//! wrong pointer is an error code (or, for `panic`, which ends the app
//! anyway, the message `<a bad pointer>`). Those that write into it are
//! `unsafe`: the caller owns the range they write. [`crate::host::FUNCTIONS`]
//! is the same list with signatures, which the kernel defines.

#[link(wasm_import_module = "lumen")]
unsafe extern "C" {
    pub safe fn handle_find(kind: i32) -> i32;
    pub safe fn handle_close(h: i32) -> i32;
    pub safe fn args_len() -> i32;
    /// Writes the arguments into `[buf, buf + len)`.
    ///
    /// # Safety
    ///
    /// The caller owns that range (a buffer it may write), whatever the
    /// call returns: Rust knows nothing of the host's writes.
    pub unsafe fn args_read(buf: i32, len: i32) -> i32;
    pub safe fn exit(code: i32) -> !;
    pub safe fn panic(msg: i32, len: i32) -> !;
    pub safe fn log(h: i32, level: i32, msg: i32, len: i32) -> i32;
    pub safe fn time_now(h: i32) -> i64;
    pub safe fn time_wall(h: i32) -> i64;
    pub safe fn time_sleep(h: i32, ns: i64) -> i32;
    pub safe fn window_open(h: i32, title: i32, len: i32, width: i32, height: i32) -> i32;
    pub safe fn window_commit(h: i32, canvas: i32, rects: i32, n: i32) -> i32;
    /// Writes one encoded event into `[buf, buf + len)`.
    ///
    /// # Safety
    ///
    /// As [`args_read`]: the caller owns that range.
    pub unsafe fn window_next_event(h: i32, buf: i32, len: i32) -> i32;
    pub safe fn window_wait(h: i32, deadline: i64, animating: i32) -> i32;
    pub safe fn window_publish(h: i32, tree: i32, len: i32) -> i32;
    pub safe fn window_answer(h: i32, answer: i32, len: i32) -> i32;
    pub safe fn text_line_height(h: i32, px: i32) -> i32;
    pub safe fn text_measure(h: i32, px: i32, text: i32, len: i32) -> i32;
    /// Draws into the canvas's pixels (inside its clip).
    ///
    /// # Safety
    ///
    /// The caller owns the canvas's pixels.
    pub unsafe fn text_draw(h: i32, canvas: i32, args: i32, text: i32, len: i32) -> i32;
    pub safe fn trace_cpus(h: i32) -> i32;
    /// Writes the trace header and events into `[buf, buf + len)`.
    ///
    /// # Safety
    ///
    /// As [`args_read`]: the caller owns that range.
    pub unsafe fn trace_read(h: i32, stream: i32, cpu: i32, since: i64, buf: i32, len: i32) -> i32;
}

/// A pointer as the `i32` a host function takes.
pub fn ptr<T: ?Sized>(p: *const T) -> i32 {
    p as *const u8 as usize as i32
}
