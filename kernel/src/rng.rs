//! Randomness (#97): [`fill`] gives random bytes from a ChaCha20 pool
//! (`lumen_rand::Pool`) that two sources feed:
//!
//! - **RDRAND**, when CPUID says the CPU has it (QEMU's default `qemu64`
//!   model doesn't; `-cpu max` does). Each 64-bit draw is retried up to
//!   [`RDRAND_RETRIES`] times (Intel's guidance: a failure means the DRNG is
//!   momentarily drained, not that it is gone), and a draw that still fails
//!   only leaves RDRAND out of that one [`fill`]. At [`init`] a CPU whose
//!   RDRAND returns the same value 8 times running (a known firmware bug)
//!   isn't used.
//! - **virtio-rng** (virtio device type 4), through the adapter
//!   (`virtio::transport`). The driver drives its own `VirtQueue`
//!   (`add`/`pop_used`) rather than the crate's `VirtIORng`, whose
//!   `request_entropy` spins unbounded: a request goes to the device in
//!   chunks of at most a page (one bounce frame each) and waits a bounded
//!   time ([`REQUEST_WAIT_NS`]) for each, giving up early if the device
//!   needs a reset or failed. The probe makes one request (the pool's first
//!   seed): a device that doesn't answer fails the probe. A device that
//!   stops answering later is dropped (reset; its posted bounce frame is
//!   leaked) and the pool goes on with RDRAND, if there is any.
//!
//! **Each [`fill`]** draws 32 bytes from each source it can use, mixes them
//! into the pool, and hands out a stream keyed from it, so every call gets
//! fresh entropy from every source (and a test of one source alone shows
//! that source works). With neither source, the pool is never seeded and
//! [`fill`] refuses ([`NoSource`]).
//!
//! **Concurrent callers.** The pool sits behind a `SpinLock` held only to
//! mix and split (no waiting); the virtio device sits in an `Option`
//! behind another, taken out for a request (made with no lock held: the
//! wait busy-waits, and a `SpinLock` keeps interrupts off) and put back
//! after. A caller that finds the device taken by another skips it for
//! that call (counted in [`Stats::busy`]): the pool was seeded by the probe,
//! so its output is still unpredictable, and RDRAND still adds fresh input
//! when present. Polling until MSI-X (#96).
//!
//! Locks: `DEVICE` and `POOL` are never held together, and neither across
//! a device wait.

use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use log::{info, warn};
use lumen_mm::FRAME_SIZE;
use lumen_rand::Pool;
use virtio_drivers::device::common::Feature;
use virtio_drivers::queue::VirtQueue;
use virtio_drivers::transport::Transport;

use lumen_pci::PciAddr;

use crate::pci::{DeviceInfo, DeviceResources};
use crate::sync::SpinLock;
use crate::virtio::{self, ProbeError, StatusHandle, VirtioHal, VirtioTransport, WaitError};

/// virtio's entropy device type.
const VIRTIO_ENTROPY: u16 = 4;
/// The one request queue, and its size (as the crate's `VirtIORng`).
const QUEUE: u16 = 0;
const QUEUE_SIZE: usize = 8;
/// How long a virtio-rng request waits for each chunk.
pub const REQUEST_WAIT_NS: u64 = 1_000_000_000;
/// Answers of 0 bytes in a row a request accepts before giving up.
const EMPTY_ANSWERS: u32 = 8;
/// Tries per RDRAND draw before that draw counts as failed.
pub const RDRAND_RETRIES: u32 = 10;
/// Bytes each source gives each [`fill`].
const DRAW: usize = 32;

static POOL: SpinLock<Pool> = SpinLock::new(Pool::new());
/// The virtio-rng device, when bound and not taken by a request.
static DEVICE: SpinLock<Option<Device>> = SpinLock::new(None);
/// A virtio-rng device is bound and hasn't failed since (it may be taken).
static VIRTIO: AtomicBool = AtomicBool::new(false);
/// RDRAND is there and passed `init`'s check.
static RDRAND: AtomicBool = AtomicBool::new(false);

static RDRAND_FAILED: AtomicU64 = AtomicU64::new(0);
static REQUESTS: AtomicU64 = AtomicU64::new(0);
static BUSY: AtomicU64 = AtomicU64::new(0);

/// Neither source exists: [`fill`] refuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoSource;

impl fmt::Display for NoSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("no randomness source (this CPU has no RDRAND, and no virtio-rng device is bound)")
    }
}

/// The sources in use now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sources {
    pub rdrand: bool,
    pub virtio: bool,
}

impl Sources {
    pub fn any(self) -> bool {
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
    Sources { rdrand: RDRAND.load(Ordering::Relaxed), virtio: VIRTIO.load(Ordering::Relaxed) }
}

/// Counters since boot (read by the ktests).
#[cfg_attr(not(feature = "ktest"), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stats {
    /// RDRAND draws that failed every retry.
    pub rdrand_failed: u64,
    /// virtio-rng requests made (the probe's first one included).
    pub requests: u64,
    /// [`fill`]s that found the device taken by another caller.
    pub busy: u64,
}

#[cfg_attr(not(feature = "ktest"), allow(dead_code))]
pub fn stats() -> Stats {
    Stats {
        rdrand_failed: RDRAND_FAILED.load(Ordering::Relaxed),
        requests: REQUESTS.load(Ordering::Relaxed),
        busy: BUSY.load(Ordering::Relaxed),
    }
}

/// Find RDRAND, mix in its first draw, and log the sources: after
/// `drivers::probe_all` (the virtio-rng probe seeds the pool itself).
pub fn init() {
    if crate::arch::has_rdrand() {
        let mut draws = [0u64; 8];
        let got = draws.iter_mut().all(|d| rdrand64().map(|v| *d = v).is_some());
        if !got {
            warn!(target: "rng", "RDRAND failed {RDRAND_RETRIES} times running at boot; not used");
        } else if draws.iter().all(|&d| d == draws[0]) {
            warn!(target: "rng", "RDRAND returns {:#x} every time; not used", draws[0]);
        } else {
            RDRAND.store(true, Ordering::Relaxed);
        }
    }
    let s = sources();
    if s.any() {
        let mut seed = [0u8; DRAW];
        if s.rdrand && rdrand_fill(&mut seed) {
            POOL.lock().mix(&seed);
        }
        info!(target: "rng", "sources: {s}");
    } else {
        // Info, not a warning: QEMU's default machine has neither (no
        // RDRAND in `qemu64`, no virtio-rng unless attached).
        info!(target: "rng", "{NoSource}: `random` refuses");
    }
}

/// Fill `buf` with random bytes (the module doc: fresh input from each
/// source in use, mixed into the pool, then a stream from it).
pub fn fill(buf: &mut [u8]) -> Result<(), NoSource> {
    let mut seed = [0u8; 2 * DRAW];
    let mut got = 0;
    if RDRAND.load(Ordering::Relaxed) && rdrand_fill(&mut seed[..DRAW]) {
        got = DRAW;
    }
    if VIRTIO.load(Ordering::Relaxed) {
        let taken = DEVICE.lock().take();
        match taken {
            Some(mut dev) => match dev.request(&mut seed[got..got + DRAW]) {
                Ok(()) => {
                    *DEVICE.lock() = Some(dev);
                    got += DRAW;
                }
                Err(e) => lose(dev, e),
            },
            // Another caller has it (or it just failed).
            None => {
                BUSY.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    let stream = {
        let mut pool = POOL.lock();
        pool.mix(&seed[..got]);
        pool.stream()
    };
    seed.fill(0);
    let mut stream = stream.ok_or(NoSource)?;
    stream.fill(buf);
    Ok(())
}

/// A random number below `bound` (above 0), without modulo bias.
pub fn below(bound: u64) -> Result<u64, NoSource> {
    assert!(bound > 0, "rng::below(0)");
    // The largest multiple of `bound` that fits, as a limit to reject at.
    let zone = u64::MAX - (u64::MAX % bound + 1) % bound;
    loop {
        let mut b = [0u8; 8];
        fill(&mut b)?;
        let v = u64::from_le_bytes(b);
        if v <= zone {
            return Ok(v % bound);
        }
    }
}

/// A device that failed a request: dropped (its transport resets it first,
/// then its queue is freed; the bounce frame of the request it never
/// answered stays outstanding, leaked).
fn lose(dev: Device, e: RequestError) {
    VIRTIO.store(false, Ordering::Relaxed);
    warn!(target: "rng", "virtio-rng on {}: {e}; not used from now on (sources: {})", dev.addr, sources());
    drop(dev);
}

/// One RDRAND draw, retried (the module doc). Only once `init` found it.
fn rdrand64() -> Option<u64> {
    for _ in 0..RDRAND_RETRIES {
        if let Some(v) = crate::arch::rdrand() {
            return Some(v);
        }
    }
    RDRAND_FAILED.fetch_add(1, Ordering::Relaxed);
    None
}

/// Fill `buf` (a multiple of 8 bytes) from RDRAND: false if a draw failed.
fn rdrand_fill(buf: &mut [u8]) -> bool {
    for chunk in buf.as_chunks_mut::<8>().0.iter_mut() {
        let Some(v) = rdrand64() else {
            buf.fill(0);
            return false;
        };
        chunk.copy_from_slice(&v.to_le_bytes());
    }
    true
}

/// The driver table's row.
pub fn matches(d: &DeviceInfo) -> bool {
    d.virtio_type() == Some(VIRTIO_ENTROPY)
}

/// Bring up a virtio-rng device: negotiate (only `VERSION_1`), set up its
/// queue, then one request, the pool's first seed (a device that doesn't
/// answer fails the probe).
pub fn probe(res: DeviceResources) -> Result<(), ProbeError> {
    let mut transport = virtio::transport(res)?;
    let status = transport.status();
    let addr = transport.info().addr;
    transport.begin_init(Feature::VERSION_1);
    // On an error the transport drops first (resetting the device), and
    // `VirtQueue::new` frees what it made.
    let queue = VirtQueue::new(&mut transport, QUEUE, false, false).map_err(ProbeError::Device)?;
    transport.finish_init();
    let mut dev = Device { transport, queue, status, addr };
    status.check_ready()?;
    let mut seed = [0u8; DRAW];
    dev.request(&mut seed).map_err(|e| {
        warn!(target: "rng", "virtio-rng on {addr}: its first request: {e}");
        e.into_probe_error()
    })?;
    POOL.lock().mix(&seed);
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
}

/// Why a virtio-rng request failed.
#[derive(Debug)]
enum RequestError {
    /// The queue refused the buffer, or popped something else.
    Queue(virtio_drivers::Error),
    /// The bounded wait gave up.
    Wait(WaitError),
    /// The device answered with 0 bytes [`EMPTY_ANSWERS`] times running.
    Empty,
    /// The device says it wrote more than the buffer holds.
    TooLong { used: u32, len: usize },
}

impl RequestError {
    fn into_probe_error(self) -> ProbeError {
        match self {
            RequestError::Queue(e) => ProbeError::Device(e),
            RequestError::Wait(e) => ProbeError::Wait(e),
            // (The details are logged.)
            RequestError::Empty | RequestError::TooLong { .. } => ProbeError::Device(virtio_drivers::Error::IoError),
        }
    }
}

impl fmt::Display for RequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RequestError::Queue(e) => write!(f, "the queue: {e}"),
            RequestError::Wait(e) => write!(f, "{e}"),
            RequestError::Empty => write!(f, "the device answered with 0 bytes {EMPTY_ANSWERS} times running"),
            RequestError::TooLong { used, len } => write!(f, "the device says it wrote {used} bytes into {len}"),
        }
    }
}

impl Device {
    /// Fill `dst` from the device: at most a page per buffer, each waited
    /// for a bounded time; a short answer is asked again for the rest.
    fn request(&mut self, dst: &mut [u8]) -> Result<(), RequestError> {
        REQUESTS.fetch_add(1, Ordering::Relaxed);
        for chunk in dst.chunks_mut(FRAME_SIZE as usize) {
            let mut at = 0;
            let mut empty = 0;
            while at < chunk.len() {
                let rest = &mut chunk[at..];
                let len = rest.len();
                let n = self.one(rest)?;
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

    /// Post `buf` (at most a page), notify, wait for it: the bytes written.
    fn one(&mut self, buf: &mut [u8]) -> Result<u32, RequestError> {
        let mut outputs = [buf];
        // SAFETY: `outputs` stays alive and untouched until `pop_used` with
        // this token below. If the wait gives up instead, the buffer is
        // never popped: the caller drops the device (`lose`), and the device
        // never had this memory anyway (the Hal shares a bounce copy, which
        // stays outstanding), so returning early can't let it write here.
        let token = unsafe { self.queue.add(&[], &mut outputs) }.map_err(RequestError::Queue)?;
        if self.queue.should_notify() {
            self.transport.notify(QUEUE);
        }
        let queue = &self.queue;
        self.status.wait_until(REQUEST_WAIT_NS, || queue.can_pop()).map_err(RequestError::Wait)?;
        // SAFETY: the buffers passed to `add` with this token, still alive.
        unsafe { self.queue.pop_used(token, &[], &mut outputs) }.map_err(RequestError::Queue)
    }
}

/// `n` random bytes as lowercase hex (for `random`).
pub fn hex(n: usize) -> Result<alloc::string::String, NoSource> {
    let mut bytes = alloc::vec![0u8; n];
    fill(&mut bytes)?;
    let mut s = alloc::string::String::with_capacity(2 * n);
    for b in &bytes {
        use core::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    bytes.fill(0);
    Ok(s)
}

crate::ktest!(
    fn rdrand_or_virtio_available() {
        use crate::kassert;
        // The ktest machine has virtio-rng (and the default CPU, so no
        // RDRAND: `test_random_with_only_rdrand` covers that source).
        kassert!(sources().any());
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
        let mut dev = DEVICE.lock().take().ok_or("no virtio-rng device (or it is taken)")?;
        let before = virtio::outstanding_runs();
        let requests = stats().requests;
        // Over a page: two chunks, two bounce frames, both back.
        let mut buf = alloc::vec![0u8; FRAME_SIZE as usize + 904];
        let done = dev.request(&mut buf);
        let after = virtio::outstanding_runs();
        *DEVICE.lock() = Some(dev);
        kassert!(done.is_ok());
        kassert_eq!(after, before);
        kassert_eq!(stats().requests, requests + 1);
        // Both chunks were written (32 zero bytes in a row: 2^-256).
        let (first, second) = buf.split_at(FRAME_SIZE as usize);
        kassert!(first[first.len() - 32..] != [0u8; 32]);
        kassert!(second[second.len() - 32..] != [0u8; 32]);
        Ok(())
    }
);

crate::ktest!(
    fn a_fill_uses_virtio_rng_unless_another_caller_has_it() {
        use crate::{kassert, kassert_eq};
        let s = stats();
        fill(&mut [0u8; 16]).map_err(|_| "fill refused")?;
        kassert_eq!(stats().requests, s.requests + 1);
        kassert_eq!(stats().busy, s.busy);
        // Taken by "another caller": skipped and counted, and the pool
        // (seeded by the probe) still answers.
        let dev = DEVICE.lock().take().ok_or("no virtio-rng device")?;
        let mut out = [0u8; 16];
        let filled = fill(&mut out);
        *DEVICE.lock() = Some(dev);
        kassert!(filled.is_ok());
        kassert!(out.iter().any(|&x| x != 0));
        kassert_eq!(stats().requests, s.requests + 1);
        kassert_eq!(stats().busy, s.busy + 1);
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
