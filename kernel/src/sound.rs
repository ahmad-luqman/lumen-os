//! Sound (#99): virtio-sound through the virtio adapter, playing
//! `lumen-synth` sources, and the `beep`, `play`, `sound` and `volume`
//! commands (docs/ARCHITECTURE.md, section 7, "Sound").
//!
//! - **One device, one owner.** The probe takes one sound device (a second
//!   is declined), sets its stream up and hands the `VirtIOSound` to the
//!   `sound` thread, the only code that touches it from then on, so no lock
//!   is ever held across a device wait.
//! - **Requests.** [`play`] puts a boxed [`Source`] in [`PLAYER`]'s slot and
//!   wakes the thread; it returns a [`Sound`] at once, whose
//!   [`outcome`](Sound::outcome) says how it ended. A newer sound replaces
//!   the one playing (faded out by [`Source::stop`]) and one still waiting
//!   to start: consecutive `beep`s without a pause replace each other.
//!   [`stop`] fades a sound out, or keeps it from starting.
//! - **Paced by the device.** For each sound the thread prepares and starts
//!   the stream, keeps up to [`DEPTH`] periods queued (`pcm_xfer_nb`), and
//!   queues another each time the device takes one back (`pcm_xfer_ok`; QEMU
//!   returns a period once its audio backend has consumed it). Between
//!   looks it sleeps ([`POLL_NS`], `StatusHandle::sleep_until`), bounded per
//!   period and ending on `DEVICE_NEEDS_RESET`/`FAILED`, so a melody
//!   doesn't keep a CPU busy. Once the source has ended (its last period
//!   padded with silence) and the queue has drained, it stops and releases
//!   the stream. #96 replaces the sleeps with the device's interrupt.
//! - **Control requests spin unbounded** in the crate (a known limit): the
//!   info queries and `pcm_set_params` in the probe, and prepare, start,
//!   stop and release around every sound, on the thread. One that never
//!   completes wedges the thread: after [`STUCK_NS`] without progress the
//!   player reads as failed, and `play` gives up waiting.
//! - **Failure.** A wait that gives up, a request the device refuses, or
//!   periods coming back faster than they can have played end the thread
//!   and drop the device (resetting it; the runs still posted leak):
//!   `sound` shows `failed: <reason>`, and `beep` and `play` say so. The
//!   crate doesn't read a period's status reply, so a period the device
//!   refused counts as played (the pace check catches a device refusing
//!   them all).

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::{self, Write};
use core::num::{NonZeroU8, NonZeroU64};
use core::ops::RangeInclusive;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use core::time::Duration;

use log::{info, warn};
use lumen_pci::PciAddr;
use lumen_shell::Key;
use lumen_shell::pipe::{self, Align, Column, Kind, Value};
use lumen_synth::{DEFAULT_VOLUME, Freq, Melody, Rate, Sequencer, Source, TooHigh};
use virtio_drivers::device::sound::{PcmFeatures, PcmFormat, PcmFormats, PcmRate, PcmRates, VirtIOSound};
use zerocopy::IntoBytes;

use crate::drivers::Driver;
use crate::pci::{DeviceInfo, DeviceResources};
use crate::sched::WaitQueue;
use crate::shell::Ctx;
use crate::sync::SpinLock;
use crate::virtio::{self, ProbeError, StatusHandle, VirtioHal, VirtioTransport, WaitError};

/// The driver's row in `drivers::DRIVERS`.
pub const DRIVER: Driver = Driver { name: "sound", matches: is_sound, probe };

/// virtio 1.2 §5: the sound device's type.
const VIRTIO_SOUND: u16 = 25;

/// A period: 20 ms of sound, the unit handed to the device.
const PERIOD_MS: u32 = 20;
/// Periods kept queued while a sound plays (160 ms: room for the thread to
/// be late). Each holds 3 DMA runs (the samples behind the stream id, the
/// status reply, the indirect descriptor list).
const DEPTH: usize = 8;
/// How long the thread sleeps between looks at the oldest period.
const POLL_NS: u64 = 1_000_000;
/// How long a period may take to come back before the device counts as
/// stuck: far more than the 160 ms of [`DEPTH`] periods queued ahead of it.
const PERIOD_WAIT_NS: u64 = 2_000_000_000;
/// While playing, this long without progress (a control request done, a
/// period taken back) means the thread is stuck in a control request: the
/// player reads as failed.
const STUCK_NS: u64 = 5_000_000_000;
/// After `play` stops its sound, how long it waits for the sound to end: a
/// release, the periods already queued, and one period's wait.
const STOP_WAIT_NS: u64 = 500_000_000 + DEPTH as u64 * PERIOD_MS as u64 * 1_000_000 + PERIOD_WAIT_NS;
/// Sounds at least this long are checked for having taken at least half
/// their length to play (a device returning periods unplayed).
const PACE_CHECK_MS: u64 = 1000;
/// `beep`'s defaults (also rhai's `beep()`), and the longest beep.
pub const DEFAULT_BEEP_HZ: u32 = 440;
pub const DEFAULT_BEEP_MS: u32 = 200;
pub const MAX_BEEP_MS: u32 = 60_000;
/// How often `play` checks for a key (and whether its sound is over).
const KEY_POLL_NS: u64 = 20_000_000;

fn is_sound(d: &DeviceInfo) -> bool {
    d.virtio_type() == Some(VIRTIO_SOUND)
}

/// The stream the driver plays on and how: signed 16-bit samples, in
/// [`PERIOD_MS`] periods.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    pub stream: u32,
    pub rate: Rate,
    pub channels: NonZeroU8,
    pcm_rate: PcmRate,
}

impl Format {
    /// Frames (a sample per channel) in a period.
    fn period_frames(&self) -> usize {
        self.rate.samples_in_ms(PERIOD_MS) as usize
    }

    /// Samples (16-bit) in a period.
    fn period_samples(&self) -> usize {
        self.period_frames() * usize::from(self.channels.get())
    }

    /// Bytes in a period.
    fn period_bytes(&self) -> usize {
        self.period_samples() * 2
    }
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "stream {}: signed 16-bit, {} channel{}, {}, {PERIOD_MS} ms periods",
            self.stream,
            self.channels,
            if self.channels.get() == 1 { "" } else { "s" },
            self.rate
        )
    }
}

/// The sample rates the driver asks for, best first: 44.1 kHz is the
/// default rate of QEMU's audiodevs (no resampling there), then 48 kHz,
/// then lower ones (enough for beeps), then higher.
const RATES: [(PcmRates, PcmRate, u32); 9] = [
    (PcmRates::RATE_44100, PcmRate::Rate44100, 44_100),
    (PcmRates::RATE_48000, PcmRate::Rate48000, 48_000),
    (PcmRates::RATE_32000, PcmRate::Rate32000, 32_000),
    (PcmRates::RATE_22050, PcmRate::Rate22050, 22_050),
    (PcmRates::RATE_16000, PcmRate::Rate16000, 16_000),
    (PcmRates::RATE_11025, PcmRate::Rate11025, 11_025),
    (PcmRates::RATE_8000, PcmRate::Rate8000, 8_000),
    (PcmRates::RATE_88200, PcmRate::Rate88200, 88_200),
    (PcmRates::RATE_96000, PcmRate::Rate96000, 96_000),
];

/// The format to play `stream` (the first output stream) in, from what it
/// supports: signed 16-bit samples (what `lumen-synth` makes), stereo if it
/// takes 2 channels, else mono, and the first of [`RATES`] it has. `Err`
/// says what is missing.
fn choose(
    stream: u32,
    rates: PcmRates,
    formats: PcmFormats,
    channels: RangeInclusive<u8>,
) -> Result<Format, &'static str> {
    if !formats.contains(PcmFormats::S16) {
        return Err("its first output stream lacks signed 16-bit samples");
    }
    let channels = [2, 1]
        .into_iter()
        .find(|c| channels.contains(c))
        .and_then(NonZeroU8::new)
        .ok_or("its first output stream takes neither 1 nor 2 channels")?;
    let &(_, pcm_rate, hz) = RATES
        .iter()
        .find(|(bit, ..)| rates.contains(*bit))
        .ok_or("its first output stream lacks a sample rate Lumen plays")?;
    Ok(Format { stream, rate: Rate::new(hz).expect("RATES aren't 0"), channels, pcm_rate })
}

/// Where the device is, and how it plays.
#[derive(Clone, Copy, Debug)]
struct Device {
    addr: PciAddr,
    format: Format,
}

/// A sound handed to the player (ids count from 1, never reused).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SoundId(NonZeroU64);

/// How a sound ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// To its end.
    Played,
    /// [`stop`]ped (faded out, or never started).
    Stopped,
    /// A newer sound took its place (faded out, or never started).
    Replaced,
    /// The device failed: why.
    Failed(String),
}

/// Where a sound's [`Outcome`] lands once it is over (its lock is a leaf).
type OutcomeCell = Arc<SpinLock<Option<Outcome>>>;

/// A sound handed to [`play`]: its id, and how it ended once it has. The
/// outcome lives here, not in the player, so it is never lost to later
/// sounds.
pub struct Sound {
    id: SoundId,
    outcome: OutcomeCell,
}

impl Sound {
    /// How it ended, or `None` while it waits or plays.
    pub fn outcome(&self) -> Option<Outcome> {
        self.outcome.lock().clone()
    }
}

fn settle(cell: &OutcomeCell, outcome: Outcome) {
    cell.lock().get_or_insert(outcome);
}

/// A sound waiting for the thread.
struct Pending {
    id: SoundId,
    source: Box<dyn Source + Send>,
    outcome: OutcomeCell,
}

/// What the bound device is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Activity {
    Idle,
    /// Playing `id`; `stop` once [`stop`] asked; `progress_ns` when the
    /// thread last got an answer from the device.
    Playing {
        id: SoundId,
        stop: bool,
        progress_ns: u64,
    },
    /// The device failed: why (the thread has ended).
    Failed(String),
}

/// The bound device, what it does and what it played.
struct Bound {
    device: Device,
    activity: Activity,
    /// Sounds that played to their end or until stopped; sounds a newer one
    /// cut off or kept from starting; frames handed to the device; how often
    /// the queue ran dry before a sound's end.
    played: u64,
    replaced: u64,
    frames: u64,
    underruns: u64,
}

impl Bound {
    /// `Err(why)` if the device failed, or is stuck: playing with no answer
    /// for [`STUCK_NS`] (a control request that never completes).
    fn health(&self, now: u64) -> Result<(), String> {
        match &self.activity {
            Activity::Failed(why) => Err(why.clone()),
            Activity::Playing { progress_ns, .. } if now.saturating_sub(*progress_ns) > STUCK_NS => Err(format!(
                "no answer from the device for {} s (stuck in a control request; reboot to retry)",
                now.saturating_sub(*progress_ns) / 1_000_000_000
            )),
            _ => Ok(()),
        }
    }
}

/// Whether a device is bound (set once, by the probe).
enum Slot {
    Absent,
    Bound(Bound),
}

/// The state shared between [`play`]'s callers and the `sound` thread.
struct Player {
    slot: Slot,
    /// The sound waiting to start.
    next: Option<Pending>,
    /// The last id handed out.
    last_id: u64,
}

static PLAYER: SpinLock<Player> = SpinLock::new(Player { slot: Slot::Absent, next: None, last_id: 0 });

/// The `sound` thread waits here for [`Player::next`].
static WAKE: WaitQueue = WaitQueue::new();

/// The volume new sounds get, in percent.
static VOLUME: AtomicU8 = AtomicU8::new(DEFAULT_VOLUME);

/// Why a sound can't play.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SoundError {
    /// No sound device is bound: the hint says what was found instead.
    NoDevice(String),
    /// The device failed or is stuck: why.
    Failed(String),
    /// A frequency the device's sample rate can't carry.
    TooHigh(TooHigh),
    /// A source made at another rate than the device's.
    WrongRate { source: Rate, device: Rate },
}

impl fmt::Display for SoundError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SoundError::NoDevice(hint) => write!(f, "no sound device{hint}"),
            SoundError::Failed(why) => write!(f, "the sound device failed: {why}"),
            SoundError::TooHigh(e) => write!(f, "{e}"),
            SoundError::WrongRate { source, device } => {
                write!(f, "a sound made at {source} can't play on a {device} device")
            }
        }
    }
}

/// What a missing device's message adds: the sound function `lspci` lists
/// without a working driver, if there is one. Takes PCI's locks: never
/// with [`PLAYER`]'s held.
fn no_device_hint() -> String {
    crate::pci::devices()
        .iter()
        .find(|d| is_sound(d))
        .and_then(|d| crate::pci::binding(d.addr).map(|b| format!(" ({}: {b})", d.addr)))
        .unwrap_or_default()
}

/// The bound device's format, or why sounds can't play (from `slot`, read
/// under [`PLAYER`]'s lock: the hint is made after it is dropped).
fn usable(slot: &Slot, now: u64) -> Result<Format, Option<String>> {
    match slot {
        Slot::Absent => Err(None),
        Slot::Bound(b) => b.health(now).map(|()| b.device.format).map_err(Some),
    }
}

fn unusable(why: Option<String>) -> SoundError {
    why.map_or_else(|| SoundError::NoDevice(no_device_hint()), SoundError::Failed)
}

/// The format sounds must be made in (their rate), or why none can play.
pub fn format() -> Result<Format, SoundError> {
    let now = crate::time::now_ns();
    let checked = usable(&PLAYER.lock().slot, now);
    checked.map_err(unusable)
}

/// Play `source` (made at [`format`]'s rate): it replaces whatever plays or
/// waits to. Returns at once.
pub fn play(source: Box<dyn Source + Send>) -> Result<Sound, SoundError> {
    let outcome: OutcomeCell = Arc::new(SpinLock::new(None));
    let now = crate::time::now_ns();
    let mut p = PLAYER.lock();
    let format = match usable(&p.slot, now) {
        Ok(f) => f,
        Err(why) => {
            drop(p);
            return Err(unusable(why));
        }
    };
    if source.rate() != format.rate {
        drop(p);
        return Err(SoundError::WrongRate { source: source.rate(), device: format.rate });
    }
    p.last_id += 1;
    let id = SoundId(NonZeroU64::new(p.last_id).expect("ids count from 1"));
    let replaced = p.next.replace(Pending { id, source, outcome: outcome.clone() });
    if replaced.is_some()
        && let Slot::Bound(b) = &mut p.slot
    {
        b.replaced += 1;
    }
    drop(p);
    if let Some(r) = replaced {
        settle(&r.outcome, Outcome::Replaced);
    }
    WAKE.wake_one();
    Ok(Sound { id, outcome })
}

/// Fade `sound` out (within its release time; the periods already queued
/// still play), or keep it from starting. Nothing if it is over.
pub fn stop(sound: &Sound) {
    let mut p = PLAYER.lock();
    let unstarted = if p.next.as_ref().is_some_and(|n| n.id == sound.id) { p.next.take() } else { None };
    if let Slot::Bound(b) = &mut p.slot
        && let Activity::Playing { id, stop, .. } = &mut b.activity
        && *id == sound.id
    {
        *stop = true;
    }
    drop(p);
    if let Some(n) = unstarted {
        settle(&n.outcome, Outcome::Stopped);
    }
}

/// The volume new sounds get, in percent (each sound keeps the volume it
/// was made with).
pub fn volume() -> u8 {
    VOLUME.load(Ordering::Relaxed)
}

/// Set the volume new sounds get (more than 100% is 100%).
pub fn set_volume(percent: u8) {
    VOLUME.store(percent.min(100), Ordering::Relaxed);
}

/// A square-wave beep of `hz` for `ms`, at the current volume; returns at
/// once (see [`play`]).
pub fn beep(hz: u32, ms: u32) -> Result<Sound, SoundError> {
    let f = format()?;
    let mut tone = lumen_synth::beep(Freq::from_hz(hz), Duration::from_millis(u64::from(ms)), f.rate)
        .map_err(SoundError::TooHigh)?;
    tone.set_volume(volume());
    play(Box::new(tone))
}

/// Whether a sound device is bound, or being probed: the player is one
/// (one slot, one thread, one format), so a second device is declined
/// before its transport is touched.
static TAKEN: AtomicBool = AtomicBool::new(false);

fn probe(res: DeviceResources) -> Result<(), ProbeError> {
    if TAKEN.swap(true, Ordering::AcqRel) {
        info!(target: "sound", "{}: another sound device is bound; Lumen plays on one", res.info().addr);
        return Err(ProbeError::NotMine);
    }
    let bound = bind(res);
    if bound.is_err() {
        // The next sound device may do better.
        TAKEN.store(false, Ordering::Release);
    }
    bound
}

/// Bring the device up and start its thread (see [`probe`]): every check
/// that can fail comes before the thread starts and the device is
/// published.
fn bind(res: DeviceResources) -> Result<(), ProbeError> {
    let transport = virtio::transport(res)?;
    let status = transport.status();
    let addr = transport.info().addr;
    let mut dev = VirtIOSound::<VirtioHal, VirtioTransport>::new(transport).map_err(ProbeError::Device)?;
    status.check_ready()?;
    let format = set_up(&mut dev)?;
    status.check_ready()?;
    let stream = Stream { dev, status, format, samples: vec![0; format.period_samples()] };
    // Detached: it runs for the kernel's life (or until the device fails).
    let _thread = crate::sched::spawn("sound", move || run(stream)).map_err(ProbeError::Thread)?;
    PLAYER.lock().slot = Slot::Bound(Bound {
        device: Device { addr, format },
        activity: Activity::Idle,
        played: 0,
        replaced: 0,
        frames: 0,
        underruns: 0,
    });
    info!(target: "sound", "{addr}: {format}");
    Ok(())
}

/// Pick the first output stream and a format for it ([`choose`]), and set
/// its parameters: control requests, each an unbounded spin in the crate.
fn set_up(dev: &mut VirtIOSound<VirtioHal, VirtioTransport>) -> Result<Format, ProbeError> {
    let refused = |op: &'static str| move |e| ProbeError::Unusable(format!("{op} failed: {e}"));
    let streams = dev.output_streams().map_err(refused("asking for its streams"))?;
    let &stream = streams.first().ok_or_else(|| ProbeError::Unusable("no output stream".into()))?;
    let rates = dev.rates_supported(stream).map_err(refused("asking for its rates"))?;
    let formats = dev.formats_supported(stream).map_err(refused("asking for its formats"))?;
    let channels = dev.channel_range_supported(stream).map_err(refused("asking for its channels"))?;
    let format = choose(stream, rates, formats, channels).map_err(|why| ProbeError::Unusable(why.into()))?;
    let period = format.period_bytes() as u32;
    dev.pcm_set_params(
        stream,
        period * DEPTH as u32,
        period,
        PcmFeatures::empty(),
        format.channels.get(),
        PcmFormat::S16,
        format.pcm_rate,
    )
    .map_err(refused("setting the stream's parameters"))?;
    Ok(format)
}

/// Why playing a sound failed (the device is given up).
#[derive(Debug)]
enum PlayError {
    /// The device refused `op`.
    Device { op: &'static str, e: virtio_drivers::Error },
    /// A period didn't come back.
    Wait(WaitError),
    /// The device isn't ready (checked before each sound).
    NotReady(ProbeError),
    /// Periods came back faster than they can have played.
    Unplayed { frames: u64, ms: u64 },
}

impl fmt::Display for PlayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlayError::Device { op, e } => write!(f, "{op}: virtio: {e}"),
            PlayError::Wait(e) => write!(f, "waiting for a period: {e}"),
            PlayError::NotReady(e) => write!(f, "{e}"),
            PlayError::Unplayed { frames, ms } => {
                write!(f, "the device returned periods without playing them ({frames} frames in {ms} ms)")
            }
        }
    }
}

/// `Err` naming `op` for a request the device refused.
fn op<T>(op: &'static str, r: virtio_drivers::Result<T>) -> Result<T, PlayError> {
    r.map_err(|e| PlayError::Device { op, e })
}

/// The device and its stream: the `sound` thread's own.
struct Stream {
    dev: VirtIOSound<VirtioHal, VirtioTransport>,
    status: StatusHandle,
    format: Format,
    /// One period's samples (always [`Format::period_samples`] long).
    samples: Vec<i16>,
}

/// What one sound played.
struct Played {
    how: Outcome,
    frames: u64,
    underruns: u64,
}

/// The `sound` thread: play each sound handed over, until the device fails.
fn run(mut stream: Stream) {
    loop {
        WAKE.wait_until(|| PLAYER.lock().next.is_some());
        let Some(Pending { id, source, outcome }) = take_next() else { continue };
        match stream.play(id, source) {
            Ok(played) => {
                let mut p = PLAYER.lock();
                if let Slot::Bound(b) = &mut p.slot {
                    b.activity = Activity::Idle;
                    b.frames += played.frames;
                    b.underruns += played.underruns;
                    match played.how {
                        Outcome::Replaced => b.replaced += 1,
                        _ => b.played += 1,
                    }
                }
                drop(p);
                if played.underruns > 0 {
                    warn!(target: "sound", "sound {}: {} underruns", id.0, played.underruns);
                }
                settle(&outcome, played.how);
            }
            Err(e) => {
                let why = e.to_string();
                let mut p = PLAYER.lock();
                if let Slot::Bound(b) = &mut p.slot {
                    b.activity = Activity::Failed(why.clone());
                }
                let next = p.next.take();
                drop(p);
                warn!(target: "sound", "playing failed: {why}; the device is given up");
                settle(&outcome, Outcome::Failed(why.clone()));
                if let Some(n) = next {
                    settle(&n.outcome, Outcome::Failed(why));
                }
                // Dropping it resets the device (`VirtioTransport`'s `Drop`).
                return;
            }
        }
    }
}

/// Take the sound waiting to start, marking it playing (one critical
/// section, so [`stop`] finds it in one place or the other).
fn take_next() -> Option<Pending> {
    let now = crate::time::now_ns();
    let mut p = PLAYER.lock();
    let next = p.next.take()?;
    if let Slot::Bound(b) = &mut p.slot {
        b.activity = Activity::Playing { id: next.id, stop: false, progress_ns: now };
    }
    Some(next)
}

/// The device answered: the thread isn't stuck.
fn progress() {
    let now = crate::time::now_ns();
    if let Slot::Bound(b) = &mut PLAYER.lock().slot
        && let Activity::Playing { progress_ns, .. } = &mut b.activity
    {
        *progress_ns = now;
    }
}

/// Whether sound `id` is to end early, and as what: stopped, or replaced
/// by a newer sound.
fn cut_short(id: SoundId) -> Option<Outcome> {
    let p = PLAYER.lock();
    let stopped =
        matches!(&p.slot, Slot::Bound(Bound { activity: Activity::Playing { id: i, stop: true, .. }, .. }) if *i == id);
    if stopped {
        Some(Outcome::Stopped)
    } else if p.next.is_some() {
        Some(Outcome::Replaced)
    } else {
        None
    }
}

impl Stream {
    /// Whether the device has taken back period `token`.
    fn taken_back(&mut self, token: u16) -> Result<bool, PlayError> {
        match self.dev.pcm_xfer_ok(token) {
            Ok(()) => Ok(true),
            Err(virtio_drivers::Error::NotReady) => Ok(false),
            Err(e) => Err(PlayError::Device { op: "taking back a period", e }),
        }
    }

    /// Play `source`, paced by the device (the module doc).
    fn play(&mut self, id: SoundId, mut source: Box<dyn Source + Send>) -> Result<Played, PlayError> {
        let stream = self.format.stream;
        let mut played = Played { how: Outcome::Played, frames: 0, underruns: 0 };
        let mut queued: VecDeque<u16> = VecDeque::with_capacity(DEPTH);
        let mut ended = false;
        // A device broken while idle fails here, not in a control request.
        self.status.check_ready().map_err(PlayError::NotReady)?;
        op("preparing the stream", self.dev.pcm_prepare(stream))?;
        progress();
        op("starting the stream", self.dev.pcm_start(stream))?;
        progress();
        let start = crate::time::now_ns();
        loop {
            if played.how == Outcome::Played
                && let Some(how) = cut_short(id)
            {
                source.stop();
                played.how = how;
            }
            while !ended && queued.len() < DEPTH {
                let filled = source.fill(&mut self.samples, self.format.channels);
                ended = filled.ended;
                if filled.frames == 0 {
                    break;
                }
                played.frames += filled.frames as u64;
                queued.push_back(op("queueing a period", self.dev.pcm_xfer_nb(stream, self.samples.as_bytes()))?);
            }
            let Some(&oldest) = queued.front() else { break };
            // Wait for the oldest period, then take back any others done.
            let status = self.status;
            let mut answer = Ok(false);
            status
                .sleep_until(PERIOD_WAIT_NS, POLL_NS, || {
                    answer = self.taken_back(oldest);
                    !matches!(answer, Ok(false))
                })
                .map_err(PlayError::Wait)?;
            answer?;
            queued.pop_front();
            while let Some(&token) = queued.front()
                && self.taken_back(token)?
            {
                queued.pop_front();
            }
            progress();
            if queued.is_empty() && !ended {
                played.underruns += 1;
            }
        }
        let ms = crate::time::now_ns().saturating_sub(start) / 1_000_000;
        let length_ms = played.frames * 1000 / u64::from(self.format.rate.hz());
        if length_ms >= PACE_CHECK_MS && ms < length_ms / 2 {
            return Err(PlayError::Unplayed { frames: played.frames, ms });
        }
        op("stopping the stream", self.dev.pcm_stop(stream))?;
        progress();
        op("releasing the stream", self.dev.pcm_release(stream))?;
        Ok(played)
    }
}

// -- commands ---------------------------------------------------------------

/// `beep [hz] [ms]`: a square-wave beep (default 440 Hz for 200 ms); returns
/// while it plays.
pub fn beep_command(ctx: &mut Ctx<'_>, args: &[String]) {
    let usage =
        || format!("usage: beep [hz] [ms] (default {DEFAULT_BEEP_HZ} {DEFAULT_BEEP_MS}; ms up to {MAX_BEEP_MS})");
    let number = |i: usize, default: u32| args.get(i).map_or(Some(default), |a| a.parse::<u32>().ok());
    let (Some(hz), Some(ms)) = (number(0, DEFAULT_BEEP_HZ), number(1, DEFAULT_BEEP_MS)) else {
        return ctx.fail(usage());
    };
    if args.len() > 2 || ms > MAX_BEEP_MS {
        return ctx.fail(usage());
    }
    if let Err(e) = beep(hz, ms) {
        ctx.fail(format_args!("beep: {e}"));
    }
}

/// `play <melody>`: play a melody (its words joined with spaces), until it
/// ends, `q`/Ctrl-C stops it, a newer sound replaces it, or the device
/// stops answering.
pub fn play_command(ctx: &mut Ctx<'_>, args: &[String]) {
    if args.is_empty() {
        return ctx.fail("usage: play <melody> (e.g. play t=140 C4/8 E4/8 G4/4 r/8 C5/2)");
    }
    let f = match format() {
        Ok(f) => f,
        Err(e) => return ctx.fail(format_args!("play: {e}")),
    };
    let mut seq = match Melody::parse(args.join(" ")).and_then(|m| Sequencer::new(m, f.rate)) {
        Ok(seq) => seq,
        Err(e) => return ctx.fail(format_args!("play: {e}")),
    };
    seq.set_volume(volume());
    let sound = match play(Box::new(seq)) {
        Ok(sound) => sound,
        Err(e) => return ctx.fail(format_args!("play: {e}")),
    };
    let mut stopped_at = None;
    let outcome = loop {
        if let Some(outcome) = sound.outcome() {
            break outcome;
        }
        let now = crate::time::now_ns();
        if stopped_at.is_none()
            && let Some(Key::Char('q') | Key::Ctrl('c')) = ctx.poll_key()
        {
            stop(&sound);
            stopped_at = Some(now);
        }
        // (A failed device settles the outcome too; a stuck one never does.)
        if let Err(SoundError::Failed(why)) = format()
            && sound.outcome().is_none()
        {
            return ctx.fail(format_args!("play: the sound device failed: {why}"));
        }
        if stopped_at.is_some_and(|t| now.saturating_sub(t) > STOP_WAIT_NS) {
            return ctx.fail("play: the sound device isn't responding (reboot to retry)");
        }
        crate::time::sleep(KEY_POLL_NS);
    };
    match outcome {
        Outcome::Played => {}
        Outcome::Stopped => {
            let _ = writeln!(ctx.out, "play: stopped");
        }
        Outcome::Replaced => ctx.fail("play: interrupted by a newer sound"),
        Outcome::Failed(why) => ctx.fail(format_args!("play: the sound device failed: {why}")),
    }
}

/// `volume [percent]`: the volume new sounds get, or set it.
pub fn volume_command(ctx: &mut Ctx<'_>, args: &[String]) {
    const USAGE: &str = "usage: volume [percent] (0-100, e.g. volume 80 or volume 80%)";
    match args {
        [] => {}
        [p] => match p.strip_suffix('%').unwrap_or(p).parse::<u8>() {
            Ok(p) if p <= 100 => set_volume(p),
            _ => return ctx.fail(USAGE),
        },
        _ => return ctx.fail(USAGE),
    }
    let _ = writeln!(ctx.out, "volume: {}%", volume());
}

/// `sound`: the device, its format and what it played (in a pipeline,
/// records: [`table`]).
pub fn sound_command(ctx: &mut Ctx<'_>, args: &[String]) {
    if !args.is_empty() {
        return ctx.fail("usage: sound");
    }
    let device = match &PLAYER.lock().slot {
        Slot::Absent => None,
        Slot::Bound(b) => Some(b.device),
    };
    let mut t = String::new();
    match device {
        Some(d) => {
            let _ = writeln!(t, "sound: virtio sound at {}, {}", d.addr, d.format);
            table().render(&mut t);
        }
        None => {
            let _ = writeln!(t, "sound: no sound device{}", no_device_hint());
        }
    }
    let _ = ctx.out.write_str(&t);
}

/// What `sound`'s one record says.
struct Row {
    state: String,
    played: u64,
    replaced: u64,
    frames: u64,
    underruns: u64,
}

/// `sound`'s records: one row, none without a device.
pub fn table() -> pipe::Table {
    let now = crate::time::now_ns();
    let row = match &PLAYER.lock().slot {
        Slot::Absent => None,
        Slot::Bound(b) => Some(Row {
            state: match (b.health(now), &b.activity) {
                (Err(why), _) => format!("failed: {why}"),
                (Ok(()), Activity::Playing { .. }) => String::from("playing"),
                (Ok(()), _) => String::from("idle"),
            },
            played: b.played,
            replaced: b.replaced,
            frames: b.frames,
            underruns: b.underruns,
        }),
    };
    table_of(row)
}

/// `sound`'s records: `row`, if there is one, at the current volume.
fn table_of(row: Option<Row>) -> pipe::Table {
    const COLUMNS: [Column; 6] = [
        Column::new("state", Kind::Text, Align::Left, 7),
        Column::new("played", Kind::Int, Align::Right, 6),
        Column::new("replaced", Kind::Int, Align::Right, 8),
        Column::new("frames", Kind::Int, Align::Right, 9),
        Column::new("underruns", Kind::Int, Align::Right, 9),
        Column::new("volume", Kind::Percent, Align::Right, 6),
    ];
    let mut t = pipe::Table::new(&COLUMNS);
    if let Some(r) = row {
        t.push(vec![
            Value::Text(r.state),
            int(r.played),
            int(r.replaced),
            int(r.frames),
            int(r.underruns),
            Value::Int(i64::from(volume())),
        ]);
    }
    t
}

/// A note for `sound`'s records when there are none.
pub fn records_note() -> Option<String> {
    let absent = matches!(PLAYER.lock().slot, Slot::Absent);
    absent.then(|| format!("sound: no sound device{}", no_device_hint()))
}

fn int(n: u64) -> Value {
    i64::try_from(n).map_or(Value::Absent("-"), Value::Int)
}

crate::ktest!(
    fn the_record_fits_its_columns() {
        // (The ktest machine has no sound device: `table()` has no row.)
        use crate::kassert_eq;
        let row = Row { state: "failed: x".into(), played: 1, replaced: 2, frames: u64::MAX, underruns: 0 };
        let t = table_of(Some(row));
        kassert_eq!(t.rows().len(), 1);
        kassert_eq!(t.rows()[0][3], Value::Absent("-"));
        kassert_eq!(table_of(None).rows().len(), 0);
        Ok(())
    }
);

crate::ktest!(
    fn a_failed_or_stuck_device_refuses_sounds() {
        // With a made-up bound device (the ktest machine has none): failed,
        // stuck in a control request, and a source at another rate. The
        // player is put back as it was (absent) afterwards.
        use crate::kassert_eq;
        let device_format = choose(0, PcmRates::all(), PcmFormats::S16, 1..=2)?;
        let addr = PciAddr::new(0, 0xff, 0x1f, 7).ok_or("addr")?;
        let bound = |activity| Bound {
            device: Device { addr, format: device_format },
            activity,
            played: 0,
            replaced: 0,
            frames: 0,
            underruns: 0,
        };
        let set = |slot| core::mem::replace(&mut PLAYER.lock().slot, slot);
        kassert_eq!(matches!(set(Slot::Bound(bound(Activity::Failed("x".into())))), Slot::Absent), true);
        let failed = SoundError::Failed("x".into());
        kassert_eq!(format(), Err(failed.clone()));
        kassert_eq!(beep(440, 10).map(drop), Err(failed.clone()));
        kassert_eq!(failed.to_string().as_str(), "the sound device failed: x");
        kassert_eq!(table().rows()[0][0], Value::Text("failed: x".into()));

        let long_ago = crate::time::now_ns().saturating_sub(STUCK_NS + 1_000_000_000);
        let id = SoundId(NonZeroU64::MIN);
        set(Slot::Bound(bound(Activity::Playing { id, stop: false, progress_ns: long_ago })));
        let stuck = matches!(format(), Err(SoundError::Failed(why)) if why.contains("stuck in a control request"));
        kassert_eq!(stuck, true);

        set(Slot::Bound(bound(Activity::Idle)));
        let other = Rate::new(8000).ok_or("rate")?;
        let tone = lumen_synth::beep(Freq::from_hz(440), Duration::from_millis(10), other).map_err(|_| "tone")?;
        kassert_eq!(
            play(Box::new(tone)).map(drop),
            Err(SoundError::WrongRate { source: other, device: device_format.rate })
        );
        kassert_eq!(PLAYER.lock().next.is_none(), true);
        set(Slot::Absent);
        Ok(())
    }
);

crate::ktest!(
    fn formats_are_chosen_from_what_the_stream_supports() {
        use crate::kassert_eq;
        let all = PcmRates::all();
        let s16 = PcmFormats::S16 | PcmFormats::U8;
        // 44.1 kHz stereo when it can.
        let f = choose(0, all, s16, 1..=2)?;
        kassert_eq!((f.rate.hz(), f.channels.get(), f.pcm_rate), (44_100, 2, PcmRate::Rate44100));
        kassert_eq!((f.period_frames(), f.period_samples(), f.period_bytes()), (882, 1764, 3528));
        // Else 48 kHz, mono, a lower rate, a higher one.
        kassert_eq!(choose(1, PcmRates::RATE_48000 | PcmRates::RATE_8000, s16, 1..=1)?.rate.hz(), 48_000);
        kassert_eq!(choose(1, all, s16, 1..=1)?.channels.get(), 1);
        kassert_eq!(choose(1, all, s16, 2..=6)?.channels.get(), 2);
        kassert_eq!(choose(1, PcmRates::RATE_8000 | PcmRates::RATE_96000, s16, 1..=2)?.rate.hz(), 8_000);
        kassert_eq!(choose(1, PcmRates::RATE_96000 | PcmRates::RATE_384000, s16, 1..=2)?.rate.hz(), 96_000);
        kassert_eq!(choose(3, all, s16, 1..=2)?.stream, 3);
        // And what is missing, when nothing fits.
        kassert_eq!(
            choose(0, all, PcmFormats::U8 | PcmFormats::FLOAT, 1..=2),
            Err("its first output stream lacks signed 16-bit samples")
        );
        kassert_eq!(choose(0, all, s16, 3..=8), Err("its first output stream takes neither 1 nor 2 channels"));
        kassert_eq!(
            choose(0, all, s16, RangeInclusive::new(2, 1)),
            Err("its first output stream takes neither 1 nor 2 channels")
        );
        kassert_eq!(
            choose(0, PcmRates::RATE_5512 | PcmRates::RATE_384000, s16, 1..=2),
            Err("its first output stream lacks a sample rate Lumen plays")
        );
        // The probe's refusal, as `lspci` shows it.
        let refused = ProbeError::Unusable(choose(0, all, PcmFormats::U8, 1..=2).err().ok_or("chose")?.into());
        kassert_eq!(
            refused.to_string().as_str(),
            "unusable device: its first output stream lacks signed 16-bit samples"
        );
        Ok(())
    }
);
