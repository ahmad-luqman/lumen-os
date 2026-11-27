//! Randomness (#97): [`fill`] (and [`u64`], [`below`]) give random bytes
//! from a ChaCha20 pool (`lumen_rand::Pool`) that two sources feed:
//!
//! - **RDRAND**, when CPUID says the CPU has it (QEMU's default `qemu64`
//!   model doesn't; `-cpu max` does). Each 64-bit draw is tried up to 10
//!   times (`lumen_rand::retry`; Intel's guidance: a failure means the
//!   generator is momentarily drained, not gone), and a draw that still
//!   fails only leaves RDRAND out of that one request (counted, and warned
//!   about at the 1st, 1024th, 2048th... failure: `lumen_rand::should_warn`).
//!   [`init`] draws 64 bytes as the seed: if this CPU's RDRAND returned one
//!   value for all 8 words (a known firmware bug) it is never used; if a
//!   draw failed, the seed is tried again on the next request (logged when
//!   it then works); otherwise the seed is mixed in and RDRAND is in use from
//!   then on (never before it seeded the pool). A later draw of one value
//!   for all 4 words marks it stuck too.
//! - **virtio-rng** (virtio device type 4; one device: a second is
//!   declined), through the adapter (`virtio::transport`). The driver
//!   drives its own `VirtQueue` (`add`/`pop_used`) rather than the crate's
//!   `VirtIORng`, whose `request_entropy` spins unbounded: a request goes to
//!   the device in buffers of at most a page (one bounce frame each), all
//!   within one deadline ([`REQUEST_WAIT_NS`] per request; no buffer is
//!   posted after it), giving up early if the device needs a reset or
//!   failed. The probe makes one request (the pool's first seed): a device
//!   that doesn't answer, or answers one repeated byte, fails the probe.
//!   **A request that fails, or [`STUCK_ANSWERS`] answers of one repeated
//!   byte in a row, consume the device** ([`Device::request`] takes it by
//!   value): it is dropped (reset; the bounce frame of a buffer it never
//!   answered, after a wait that gave up, stays outstanding, leaked, and
//!   the warning and `lspci` say how many), `lspci` shows it failed
//!   (`pci::failed_after_bind`), and it is never re-initialized. A single
//!   answer of one repeated byte is not mixed in (counted, warned about
//!   rate-limited).
//!
//! **Each [`fill`]** draws 32 bytes from each source it can use, mixes them
//! into the pool, and hands out a stream keyed from it, so every call gets
//! fresh entropy from every source that answers (and a test of one source
//! alone shows that source works). **Once seeded, the pool always
//! answers**, even after every source is lost: it is a cryptographic
//! generator, its output unpredictable without its key. Only a pool that
//! was never seeded (no source has answered since boot) refuses
//! ([`NoSource`], saying why).
//!
//! **Concurrent callers.** The pool sits behind a `SpinLock` held only to
//! mix and split (no waiting); the virtio device sits in an `Option`
//! behind another, taken out for a request (made with no lock held: the
//! wait blocks, and a `SpinLock` keeps interrupts off) and put back
//! after. A caller that finds the device taken by another skips it for
//! that call (counted in [`Stats::busy`]): the pool was seeded by the probe,
//! so its output is still unpredictable, and RDRAND still adds fresh input
//! when in use.
//!
//! **The device's interrupt** (MSI-X, through `virtio::transport`) rings
//! [`BELL`]; a request waits for it (`StatusHandle::wait_for`), blocking
//! between looks at the queue, and spins instead only where its caller
//! can't block (a lock held, interrupts off). A device that can't take its
//! vector fails the probe.
//!
//! Locks: `DEVICE`, `POOL` and `TOKEN` are never held together, and none
//! across a device wait.

use alloc::format;
use alloc::string::ToString;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use log::{info, warn};
use lumen_mm::FRAME_SIZE;
use lumen_pci::PciAddr;
use lumen_rand::{DrawFailed, Pool, RETRIES, all_same, clear, fill_words, should_warn};
use virtio_drivers::device::common::Feature;
use virtio_drivers::queue::VirtQueue;
use virtio_drivers::transport::Transport;

use crate::arch::Rdrand;
use crate::pci::{DeviceInfo, DeviceResources};
use crate::sync::SpinLock;
use crate::virtio::{self, Doorbell, ProbeError, StatusHandle, VirtioHal, VirtioTransport, WaitError};

/// The driver table's name for the virtio-rng row.
pub const DRIVER: &str = "virtio-rng";
/// virtio's entropy device type.
const VIRTIO_ENTROPY: u16 = 4;
/// The one request queue, and its size (as the crate's `VirtIORng`).
const QUEUE: u16 = 0;
const QUEUE_SIZE: usize = 8;
/// How long one virtio-rng request may take, all its buffers together.
const REQUEST_WAIT_NS: u64 = 1_000_000_000;
/// Answers of 0 bytes in a row a request accepts before giving up.
const EMPTY_ANSWERS: u32 = 8;
/// Answers of one repeated byte in a row (in [`fill`]) before the device
/// is dropped.
const STUCK_ANSWERS: u32 = 8;
/// Bytes each source gives each [`fill`] (a multiple of 8, for RDRAND).
const DRAW: usize = 32;
const _: () = assert!(DRAW.is_multiple_of(8));
/// RDRAND's seed, in 64-bit words (at boot, or at the first request after a
/// failed draw): also the stuck check's sample.
const SEED_WORDS: usize = 8;

static POOL: SpinLock<Pool> = SpinLock::new(Pool::new());
/// The virtio-rng device, when bound and not taken by a request.
static DEVICE: SpinLock<Option<Device>> = SpinLock::new(None);
/// A virtio-rng device is bound and hasn't failed since (it may be taken).
static VIRTIO: AtomicBool = AtomicBool::new(false);
/// A virtio-rng device's probe failed (for [`NoSource`]'s message).
static PROBE_FAILED: AtomicBool = AtomicBool::new(false);
/// RDRAND's token, from [`init`] (no `cpuid` per request).
static TOKEN: SpinLock<Option<Rdrand>> = SpinLock::new(None);

/// RDRAND's state ([`RDRAND`]).
const RDRAND_ABSENT: u8 = 0;
/// The CPU has it, but it hasn't seeded the pool yet (a seed draw failed).
const RDRAND_PENDING: u8 = 1;
const RDRAND_IN_USE: u8 = 2;
/// It returned one value for every word of a draw: never used again.
const RDRAND_STUCK: u8 = 3;
static RDRAND: AtomicU8 = AtomicU8::new(RDRAND_ABSENT);

static RDRAND_FAILED: AtomicU64 = AtomicU64::new(0);
static REQUESTS: AtomicU64 = AtomicU64::new(0);
static POSTS: AtomicU64 = AtomicU64::new(0);
static BUSY: AtomicU64 = AtomicU64::new(0);
static REJECTED: AtomicU64 = AtomicU64::new(0);
/// Answers of one repeated byte since the last good one.
static REJECTED_RUN: AtomicU32 = AtomicU32::new(0);

/// RDRAND's state, as `random --stats` shows it.
fn rdrand_state(state: u8) -> &'static str {
    match state {
        RDRAND_ABSENT => "absent",
        RDRAND_PENDING => "pending",
        RDRAND_IN_USE => "in use",
        _ => "stuck",
    }
}

/// The pool was never seeded: no source has answered since boot (once
/// seeded, [`fill`] always answers). Says why: RDRAND's state, and whether
/// a virtio-rng device failed; QEMU's options only when neither exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoSource {
    rdrand: u8,
    virtio_failed: bool,
}

impl NoSource {
    fn now() -> NoSource {
        NoSource { rdrand: RDRAND.load(Ordering::Relaxed), virtio_failed: PROBE_FAILED.load(Ordering::Relaxed) }
    }
}

impl fmt::Display for NoSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rdrand = match self.rdrand {
            RDRAND_ABSENT => "no RDRAND",
            RDRAND_PENDING => "RDRAND hasn't given its seed yet",
            RDRAND_STUCK => "this CPU's RDRAND is stuck",
            _ => "RDRAND in use",
        };
        let virtio = if self.virtio_failed {
            "the virtio-rng device failed (lspci says why)"
        } else {
            "no virtio-rng device bound"
        };
        write!(f, "no randomness source has answered ({rdrand}, and {virtio}")?;
        if self.rdrand == RDRAND_ABSENT && !self.virtio_failed {
            f.write_str("; QEMU: -cpu max or -device virtio-rng-pci")?;
        }
        f.write_str(")")
    }
}

/// The sources in use now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sources {
    pub rdrand: bool,
    pub virtio: bool,
}

impl Sources {
    fn any(self) -> bool {
        self.rdrand || self.virtio
    }
}

impl fmt::Display for Sources {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.rdrand, self.virtio) {
            (true, true) => f.write_str("RDRAND, virtio-rng"),
            (true, false) => f.write_str("RDRAND"),
            (false, true) => f.write_str("virtio-rng"),
            (false, false) => f.write_str("none"),
        }
    }
}

pub fn sources() -> Sources {
    Sources { rdrand: RDRAND.load(Ordering::Relaxed) == RDRAND_IN_USE, virtio: VIRTIO.load(Ordering::Relaxed) }
}

/// Counters since boot (`random --stats`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stats {
    /// RDRAND: absent, pending, in use or stuck.
    pub rdrand: &'static str,
    /// RDRAND draws that failed every try.
    pub rdrand_failed: u64,
    /// virtio-rng requests made (the probe's first one included).
    pub requests: u64,
    /// Buffers posted to virtio-rng (each at most a page).
    pub posts: u64,
    /// [`fill`]s that found the device taken by another caller (or just
    /// lost).
    pub busy: u64,
    /// virtio-rng answers of one repeated byte, not mixed in.
    pub rejected: u64,
    /// Bytes mixed into the pool.
    pub mixed: usize,
}

impl fmt::Display for Stats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "pool: {} bytes mixed in; RDRAND: {}, {} draws failed; virtio-rng: {} requests, {} buffers posted, {} \
             answers rejected; busy skips: {}",
            self.mixed, self.rdrand, self.rdrand_failed, self.requests, self.posts, self.rejected, self.busy
        )
    }
}

pub fn stats() -> Stats {
    Stats {
        rdrand: rdrand_state(RDRAND.load(Ordering::Relaxed)),
        rdrand_failed: RDRAND_FAILED.load(Ordering::Relaxed),
        requests: REQUESTS.load(Ordering::Relaxed),
        posts: POSTS.load(Ordering::Relaxed),
        busy: BUSY.load(Ordering::Relaxed),
        rejected: REJECTED.load(Ordering::Relaxed),
        mixed: POOL.lock().credited(),
    }
}

/// Find RDRAND and take its seed (the module doc), then log the sources:
/// after `drivers::probe_all` (the virtio-rng probe seeds the pool itself).
pub fn init() {
    if let Some(r) = Rdrand::detect() {
        *TOKEN.lock() = Some(r);
        RDRAND.store(RDRAND_PENDING, Ordering::Relaxed);
        seed_from_rdrand(r);
    }
    let s = sources();
    if s.any() {
        info!(target: "rng", "sources: {s}");
    } else {
        // Info, not a warning: QEMU's default machine has neither (no
        // RDRAND in `qemu64`, no virtio-rng unless attached).
        info!(target: "rng", "{}: `random` refuses until one does", NoSource::now());
    }
}

/// Mark RDRAND stuck: it gave `value` for every word of a draw.
fn rdrand_stuck(value: [u8; 8]) {
    RDRAND.store(RDRAND_STUCK, Ordering::Relaxed);
    warn!(target: "rng", "this CPU's RDRAND returns {:#x} every time; not used (sources: {})", u64::from_le_bytes(value), sources());
}

/// RDRAND's seed: 64 bytes, checked for a stuck value, mixed in; then
/// RDRAND is in use. Only while it is pending (a no-op otherwise): true if
/// it seeded the pool now.
fn seed_from_rdrand(r: Rdrand) -> bool {
    if RDRAND.load(Ordering::Relaxed) != RDRAND_PENDING {
        return false;
    }
    let mut words = [[0u8; 8]; SEED_WORDS];
    let seeded = match fill_words(&mut words, || r.try_once()) {
        Err(DrawFailed) => {
            rdrand_failed("its seed (tried again on the next request)");
            false
        }
        Ok(()) if all_same(&words) => {
            rdrand_stuck(words[0]);
            false
        }
        Ok(()) => {
            POOL.lock().mix(words.as_flattened());
            // (Two callers seeding at once: both mix, one store wins.)
            RDRAND.store(RDRAND_IN_USE, Ordering::Relaxed);
            true
        }
    };
    clear(words.as_flattened_mut());
    seeded
}

/// Count an RDRAND draw that failed every try, warning at the 1st, 1024th,
/// 2048th... (`what`: what it was for).
fn rdrand_failed(what: &str) {
    let n = RDRAND_FAILED.fetch_add(1, Ordering::Relaxed) + 1;
    if should_warn(n) {
        warn!(target: "rng", "RDRAND failed {RETRIES} tries in a row for {what} ({n} such draws since boot)");
    }
}

/// Fill `buf` with random bytes (the module doc: fresh input from each
/// source in use, mixed into the pool, then a stream from it). Refuses
/// only if the pool was never seeded.
pub fn fill(buf: &mut [u8]) -> Result<(), NoSource> {
    let mut seed = [0u8; 2 * DRAW];
    let (rdrand, virtio) = seed.split_at_mut(DRAW);
    let mut got = 0;
    if draw_rdrand(rdrand.try_into().expect("DRAW bytes")) {
        got += DRAW;
    }
    // (Contiguous with RDRAND's bytes when they were taken.)
    if draw_virtio(virtio.try_into().expect("DRAW bytes")) {
        if got == 0 {
            seed.copy_within(DRAW.., 0);
        }
        got += DRAW;
    }
    let stream = {
        let mut pool = POOL.lock();
        pool.mix(&seed[..got]);
        pool.stream()
    };
    clear(&mut seed);
    let mut stream = stream.ok_or_else(NoSource::now)?;
    stream.fill(buf);
    Ok(())
}

/// RDRAND's part of a [`fill`]: true if `out` holds its draw (to mix in).
/// While pending, it tries the seed instead (mixed in by itself).
fn draw_rdrand(out: &mut [u8; DRAW]) -> bool {
    let state = RDRAND.load(Ordering::Relaxed);
    if state != RDRAND_PENDING && state != RDRAND_IN_USE {
        return false;
    }
    let r = (*TOKEN.lock()).expect("RDRAND pending or in use, so init found it");
    if state == RDRAND_PENDING {
        if seed_from_rdrand(r) {
            info!(target: "rng", "RDRAND seeded on retry; sources: {}", sources());
        }
        return false;
    }
    let (words, _) = out.as_chunks_mut::<8>();
    match fill_words(words, || r.try_once()) {
        Ok(()) if all_same(words) => {
            rdrand_stuck(words[0]);
            clear(out);
            false
        }
        Ok(()) => true,
        Err(DrawFailed) => {
            rdrand_failed("a request (left out of it)");
            false
        }
    }
}

/// virtio-rng's part of a [`fill`]: true if `out` holds its answer (to mix
/// in). Skips a device another caller has; rejects (and after
/// [`STUCK_ANSWERS`] in a row, drops) answers of one repeated byte.
fn draw_virtio(out: &mut [u8; DRAW]) -> bool {
    if !VIRTIO.load(Ordering::Relaxed) {
        return false;
    }
    let Some(dev) = DEVICE.lock().take() else {
        // Another caller has it (or it was just lost).
        BUSY.fetch_add(1, Ordering::Relaxed);
        return false;
    };
    let dev = match dev.request(out) {
        Ok(dev) => dev,
        Err(lost) => {
            lose(lost);
            return false;
        }
    };
    if !all_same(out) {
        REJECTED_RUN.store(0, Ordering::Relaxed);
        *DEVICE.lock() = Some(dev);
        return true;
    }
    let n = REJECTED.fetch_add(1, Ordering::Relaxed) + 1;
    let run = REJECTED_RUN.fetch_add(1, Ordering::Relaxed) + 1;
    let byte = out[0];
    clear(out);
    if run >= STUCK_ANSWERS {
        let addr = dev.addr;
        drop(dev);
        lose(Lost { addr, why: RequestError::Stuck { byte }, leaked: 0 });
    } else {
        *DEVICE.lock() = Some(dev);
        if should_warn(n) {
            warn!(
                target: "rng",
                "virtio-rng answered {DRAW} bytes of {byte:#04x}; not mixed in ({n} such answers since boot)"
            );
        }
    }
    false
}

/// A random 64-bit number.
pub fn u64() -> Result<u64, NoSource> {
    let mut b = [0u8; 8];
    fill(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

/// A random number below `bound` (above 0; panics on 0), without modulo
/// bias (`lumen_rand::below`).
pub fn below(bound: u64) -> Result<u64, NoSource> {
    lumen_rand::below(bound, u64)
}

/// A request that failed: the address of the device it consumed (already
/// dropped), why, and the bounce runs it left outstanding (leaked).
struct Lost {
    addr: PciAddr,
    why: RequestError,
    leaked: usize,
}

/// After a failed request (the device already dropped): `lspci` shows it
/// failed (unless that is already recorded), and the warning says what is
/// left.
fn lose(Lost { addr, why, leaked }: Lost) {
    VIRTIO.store(false, Ordering::Relaxed);
    let leak = match leaked {
        0 => alloc::string::String::new(),
        1 => " (1 DMA run leaked)".to_string(),
        n => format!(" ({n} DMA runs leaked)"),
    };
    // False: already recorded (nothing else changes this slot today).
    let _recorded = crate::pci::failed_after_bind(addr, DRIVER, format!("{why}{leak}"));
    let left = sources();
    if left.any() {
        warn!(target: "rng", "virtio-rng on {addr}: {why}; dropped{leak}; {left} carries on");
    } else {
        warn!(
            target: "rng",
            "virtio-rng on {addr}: {why}; dropped{leak}; no source left: answering from the pool seeded earlier"
        );
    }
}

/// The driver table's row.
pub fn matches(d: &DeviceInfo) -> bool {
    d.virtio_type() == Some(VIRTIO_ENTROPY)
}

/// Bring up a virtio-rng device: declined if one is already bound;
/// negotiate (only `VERSION_1`), set up its queue, then one request, the
/// pool's first seed (a request that fails, or an answer of one repeated
/// byte, fails the probe). Publishes the device only after its last
/// fallible check.
pub fn probe(res: DeviceResources) -> Result<(), ProbeError> {
    if VIRTIO.load(Ordering::Relaxed) {
        info!(target: "rng", "{}: a virtio-rng device is already bound; one is enough", res.info().addr);
        return Err(ProbeError::NotMine);
    }
    let probed = bring_up(res);
    if probed.is_err() {
        PROBE_FAILED.store(true, Ordering::Relaxed);
    }
    probed
}

/// Rung by the device's MSI-X interrupt: whatever it signals (its one
/// queue's buffer came back, or its configuration changed).
static BELL: Doorbell = Doorbell::new();

fn on_interrupt() {
    BELL.ring();
}

fn bring_up(res: DeviceResources) -> Result<(), ProbeError> {
    let mut transport = virtio::transport(res, on_interrupt)?;
    let status = transport.status();
    let addr = transport.info().addr;
    transport.begin_init(Feature::VERSION_1);
    // On an error the transport drops first (resetting the device), and
    // `VirtQueue::new` frees what it made.
    let queue = VirtQueue::new(&mut transport, QUEUE, false, false).map_err(ProbeError::Device)?;
    transport.finish_init();
    let dev = Device { transport, queue, status, addr, in_flight: 0 };
    virtio::check_device(&status)?;
    let mut seed = [0u8; DRAW];
    let dev = dev.request(&mut seed).map_err(|lost| {
        warn!(target: "rng", "virtio-rng on {addr}: its first request: {}", lost.why);
        lost.why.into_probe_error()
    })?;
    if all_same(&seed) {
        let why = format!("its first answer is {DRAW} bytes of {:#04x}", seed[0]);
        warn!(target: "rng", "virtio-rng on {addr}: {why}");
        // (The device drops here, reset.)
        return Err(ProbeError::Unusable(why));
    }
    // Published only now, after the last fallible check.
    POOL.lock().mix(&seed);
    clear(&mut seed);
    *DEVICE.lock() = Some(dev);
    VIRTIO.store(true, Ordering::Relaxed);
    Ok(())
}

/// A bound virtio-rng device. Field order matters: the transport drops
/// (resetting the device) before the queue's memory is freed.
struct Device {
    transport: VirtioTransport,
    queue: VirtQueue<VirtioHal, QUEUE_SIZE>,
    status: StatusHandle,
    addr: PciAddr,
    /// Buffers posted and not popped: each holds a bounce run (leaked if
    /// the device is dropped now).
    in_flight: usize,
}

/// Why a virtio-rng request failed (or the device was given up).
#[derive(Debug)]
enum RequestError {
    /// The queue refused the buffer, or popped something else.
    Queue(virtio_drivers::Error),
    /// The bounded wait gave up (or the request's time was up before the
    /// next buffer).
    Wait(WaitError),
    /// The device answered with 0 bytes [`EMPTY_ANSWERS`] times running.
    Empty,
    /// The device says it wrote more than the buffer holds.
    TooLong { used: u32, len: usize },
    /// [`STUCK_ANSWERS`] answers in a row were this one byte repeated.
    Stuck { byte: u8 },
}

impl RequestError {
    fn into_probe_error(self) -> ProbeError {
        match self {
            RequestError::Queue(e) => ProbeError::Device(e),
            RequestError::Wait(e) => ProbeError::Wait(e),
            e @ (RequestError::Empty | RequestError::TooLong { .. } | RequestError::Stuck { .. }) => {
                ProbeError::Unusable(e.to_string())
            }
        }
    }
}

impl fmt::Display for RequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RequestError::Queue(e) => write!(f, "the queue: {e}"),
            RequestError::Wait(WaitError::Timeout { status, .. }) => write!(
                f,
                "the device didn't answer within the request's {} ms (status {status:#04x})",
                REQUEST_WAIT_NS / 1_000_000
            ),
            RequestError::Wait(e) => write!(f, "{e}"),
            RequestError::Empty => write!(f, "the device answered with 0 bytes {EMPTY_ANSWERS} times running"),
            RequestError::TooLong { used, len } => write!(f, "the device says it wrote {used} bytes into {len}"),
            RequestError::Stuck { byte } => {
                write!(f, "the device answered {DRAW} bytes of {byte:#04x} {STUCK_ANSWERS} times running")
            }
        }
    }
}

impl Device {
    /// Fill `dst` from the device within one deadline ([`REQUEST_WAIT_NS`]):
    /// at most a page per buffer; a short answer is asked again for the
    /// rest. Consumes the device: it comes back only if the request worked;
    /// on an error it is dropped here (reset), and [`Lost`] says why and
    /// how many bounce runs it leaked.
    fn request(mut self, dst: &mut [u8]) -> Result<Device, Lost> {
        REQUESTS.fetch_add(1, Ordering::Relaxed);
        let deadline = crate::time::now_ns().saturating_add(REQUEST_WAIT_NS);
        match self.read_into(dst, deadline) {
            Ok(()) => Ok(self),
            Err(why) => {
                let (addr, leaked) = (self.addr, self.in_flight);
                drop(self);
                Err(Lost { addr, why, leaked })
            }
        }
    }

    fn read_into(&mut self, dst: &mut [u8], deadline: u64) -> Result<(), RequestError> {
        for chunk in dst.chunks_mut(FRAME_SIZE as usize) {
            let mut at = 0;
            let mut empty = 0;
            while at < chunk.len() {
                let rest = &mut chunk[at..];
                let len = rest.len();
                let n = self.one(rest, deadline)?;
                if n == 0 {
                    empty += 1;
                    if empty == EMPTY_ANSWERS {
                        return Err(RequestError::Empty);
                    }
                } else if n as usize > len {
                    return Err(RequestError::TooLong { used: n, len });
                } else {
                    empty = 0;
                    at += n as usize;
                }
            }
        }
        Ok(())
    }

    /// Post `buf` (at most a page), notify, wait for it until `deadline`:
    /// the bytes written. Posts nothing once the deadline has passed.
    fn one(&mut self, buf: &mut [u8], deadline: u64) -> Result<u32, RequestError> {
        debug_assert!(buf.len() <= FRAME_SIZE as usize, "a virtio-rng buffer over a page");
        if crate::time::now_ns() >= deadline {
            let status = self.status.raw();
            return Err(RequestError::Wait(WaitError::Timeout { waited_ns: REQUEST_WAIT_NS, status }));
        }
        let mut outputs = [buf];
        // SAFETY: `outputs` stays alive and untouched until `pop_used` with
        // this token below. If the wait gives up or `pop_used` refuses
        // instead, the buffer is never popped: the error consumes the device
        // (`request` drops it, resetting it), and the device never had this
        // memory anyway (the Hal shares a bounce copy, which stays
        // outstanding), so returning early can't let it write here. This
        // relies on the Hal always bouncing: a no-bounce fast path must
        // reset the device before this returns.
        let token = unsafe { self.queue.add(&[], &mut outputs) }.map_err(RequestError::Queue)?;
        POSTS.fetch_add(1, Ordering::Relaxed);
        self.in_flight += 1;
        if self.queue.should_notify() {
            self.transport.notify(QUEUE);
        }
        let queue = &self.queue;
        let left = deadline.saturating_sub(crate::time::now_ns());
        self.status.wait_for(&BELL, left, || queue.can_pop()).map_err(RequestError::Wait)?;
        // SAFETY: the buffers passed to `add` with this token, still alive.
        let n = unsafe { self.queue.pop_used(token, &[], &mut outputs) }.map_err(RequestError::Queue)?;
        self.in_flight -= 1;
        Ok(n)
    }
}

/// Ktests: the bound virtio-rng device's address, if one is bound.
#[cfg(feature = "ktest")]
pub fn ktest_device() -> Option<PciAddr> {
    DEVICE.lock().as_ref().map(|d| d.addr)
}

/// Ktests: how many times the device's interrupt has rung its doorbell.
#[cfg(feature = "ktest")]
pub fn ktest_rings() -> u64 {
    BELL.rings()
}

/// Ktests: block until the device's doorbell has rung past `seen`, or
/// `deadline` (`time::now_ns`) passes. Returns
/// [`crate::virtio::Wake::CannotBlock`] at once where the thread can't block.
#[cfg(feature = "ktest")]
#[must_use = "a deadline or no block at all means nothing rang"]
pub fn ktest_wait_ring(seen: u64, deadline: u64) -> crate::virtio::Wake {
    BELL.wait_after(seen, deadline)
}

/// Ktests: the bound device's `msix_config` and queue 0's
/// `queue_msix_vector`, as read now.
#[cfg(feature = "ktest")]
pub fn ktest_vectors() -> Option<(u16, u16)> {
    DEVICE.lock().as_mut().map(|d| d.transport.ktest_vectors())
}

crate::ktest!(
    fn rdrand_or_virtio_available() {
        use crate::kassert;
        // The ktest machine has virtio-rng (and the default CPU, so no
        // RDRAND: `test_dev::test_random_with_only_rdrand` covers that
        // source).
        kassert!(sources().virtio);
        let (mut a, mut b) = ([0u8; 32], [0u8; 32]);
        fill(&mut a).map_err(|_| "fill refused")?;
        fill(&mut b).map_err(|_| "fill refused")?;
        kassert!(a != b);
        kassert!(a != [0u8; 32]);
        Ok(())
    }
);

crate::ktest!(
    fn no_bounce_outstanding_after_a_request() {
        use crate::{kassert, kassert_eq};
        let dev = DEVICE.lock().take().ok_or("no virtio-rng device (or it is taken)")?;
        let before = virtio::outstanding_runs();
        let s = stats();
        // Over a page: two buffers, two bounce frames, both back.
        let mut buf = alloc::vec![0u8; FRAME_SIZE as usize + 904];
        let done = dev.request(&mut buf);
        let after = virtio::outstanding_runs();
        match done {
            Ok(dev) => {
                kassert_eq!(dev.in_flight, 0);
                *DEVICE.lock() = Some(dev);
            }
            Err(lost) => {
                lose(lost);
                return Err("the request failed");
            }
        }
        kassert_eq!(after, before);
        kassert_eq!(stats().requests, s.requests + 1);
        kassert_eq!(stats().posts, s.posts + 2);
        // Both buffers were written (32 zero bytes in a row: 2^-256).
        let (first, second) = buf.split_at(FRAME_SIZE as usize);
        kassert!(first[first.len() - 32..] != [0u8; 32]);
        kassert!(second[second.len() - 32..] != [0u8; 32]);
        Ok(())
    }
);

crate::ktest!(
    fn a_request_past_its_deadline_posts_nothing() {
        use crate::{kassert, kassert_eq};
        let mut dev = DEVICE.lock().take().ok_or("no virtio-rng device (or it is taken)")?;
        let (posts, runs) = (stats().posts, virtio::outstanding_runs());
        let mut buf = [0u8; 16];
        // A deadline already past: refused before posting, the device intact
        // (so this test can put it back; `request` itself would drop it).
        let late = dev.read_into(&mut buf, crate::time::now_ns());
        let in_flight = dev.in_flight;
        *DEVICE.lock() = Some(dev);
        kassert!(matches!(late, Err(RequestError::Wait(WaitError::Timeout { .. }))));
        kassert_eq!(in_flight, 0);
        kassert_eq!(stats().posts, posts);
        kassert_eq!(virtio::outstanding_runs(), runs);
        // And the device still answers.
        fill(&mut buf).map_err(|_| "fill refused")?;
        kassert!(sources().virtio);
        Ok(())
    }
);

crate::ktest!(
    fn a_fill_uses_virtio_rng_unless_another_caller_has_it() {
        use crate::{kassert, kassert_eq};
        // The ktest machine has no RDRAND: virtio-rng's 32 bytes alone.
        let s = stats();
        fill(&mut [0u8; 16]).map_err(|_| "fill refused")?;
        kassert_eq!(stats().requests, s.requests + 1);
        kassert_eq!(stats().busy, s.busy);
        kassert_eq!(stats().mixed, s.mixed + DRAW);
        // Taken by "another caller": skipped and counted, nothing mixed
        // in, and the pool (seeded by the probe) still answers.
        let dev = DEVICE.lock().take().ok_or("no virtio-rng device")?;
        let mut out = [0u8; 16];
        let filled = fill(&mut out);
        *DEVICE.lock() = Some(dev);
        kassert!(filled.is_ok());
        kassert!(out.iter().any(|&x| x != 0));
        kassert_eq!(stats().requests, s.requests + 1);
        kassert_eq!(stats().busy, s.busy + 1);
        kassert_eq!(stats().mixed, s.mixed + DRAW);
        Ok(())
    }
);

crate::ktest!(
    fn below_stays_below_its_bound() {
        use crate::kassert;
        let mut seen = [false; 6];
        for _ in 0..200 {
            let v = below(6).map_err(|_| "refused")?;
            kassert!(v < 6);
            seen[v as usize] = true;
        }
        // Each of 6 values in 200 draws: missing one is (5/6)^200 ~ 1e-16.
        kassert!(seen.iter().all(|&s| s));
        kassert!(below(1).map_err(|_| "refused")? == 0);
        Ok(())
    }
);
