//! Sound (#99): virtio-sound through the virtio adapter, playing
//! `lumen-synth` sources, and the `beep`, `play`, `sound` and `volume`
//! commands (docs/ARCHITECTURE.md, section 7, "Sound").
//!
//! - **One device, one owner.** The probe takes one sound device (a second
//!   is declined), sets its stream up and hands the `VirtIOSound` to the
//!   `sound` thread, the only code that touches it from then on, so no lock
//!   is ever held across a device wait.
//! - **Requests.** [`play`] puts a boxed [`Source`] in [`PLAYER`]'s `next`
//!   and wakes the thread; it returns a [`Sound`] at once, whose
//!   [`outcome`](Sound::outcome) says how it ended. A newer sound cuts the
//!   playing one off at its next period (up to 160 ms already queued still
//!   plays; one that has made all its samples plays out); a sound still
//!   waiting is dropped. [`stop`] fades a sound out, or keeps it from
//!   starting.
//! - **Paced by the device.** For each sound the thread prepares and starts
//!   the stream, keeps up to [`DEPTH`] periods queued (`pcm_xfer_nb`), and
//!   queues another each time the device returns one (`pcm_xfer_ok` takes
//!   it back; QEMU returns a period once its audio backend has consumed
//!   it). Between looks it blocks until the device's MSI-X interrupt
//!   (through `virtio::transport`) rings [`BELL`] (`StatusHandle::wait_for`),
//!   bounded per period and ending on `DEVICE_NEEDS_RESET`/`FAILED`, so a
//!   melody doesn't keep a CPU busy. Once the source has ended (its last
//!   period padded with silence) and the queue has drained, it stops and
//!   releases the stream. A device that can't take its vector fails the
//!   probe.
//! - **Control requests spin unbounded** in the crate (a known limit): the
//!   info queries and `pcm_set_params` in the probe, and prepare, start,
//!   stop and release around every sound, on the thread. One that never
//!   completes wedges the thread: the first look (`sound`, `beep`, `play`,
//!   or `play` polling [`format()`]) over [`STUCK_NS`] after the last
//!   progress gives the player up for good (a warning; `lspci` shows it
//!   failed); nothing looks on a timer. A thread that comes back
//!   afterwards drops the device.
//! - **Failure.** A device that isn't ready before a sound, a wait that
//!   gives up, a request the device refuses, or a sound of 1 s or more whose
//!   periods came back in under half its length ends the thread: it drops
//!   the device (resetting it; the runs still posted leak) and records it
//!   failed (`pci::failed_after_bind`). `sound` shows `failed: <reason>`,
//!   and `beep` and `play` say so. A device that reports itself broken
//!   while idle (its status) is given up by the next look too. The crate
//!   doesn't read a period's status reply, so a period the device refused
//!   counts as played (the pace check catches a device refusing them all).
//! - **Outcomes.** A sound waiting to start when the device is given up,
//!   however that happens, settles failed with the reason
//!   (`Bound::give_up`). Until a stuck thread is given up (by a look), the
//!   sound waiting behind it is unsettled; the sound it plays settles only
//!   when the thread gets an answer, so on a thread stuck for good it stays
//!   unsettled: wait on an outcome with [`format()`] too, which is also the
//!   look that gives a stuck thread up. A `Settle` dropped unsettled (a
//!   bug) settles [`Outcome::Lost`] and logs an error.

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

use log::{error, info, warn};
use lumen_pci::PciAddr;
use lumen_shell::Key;
use lumen_shell::pipe::{self, Align, Column, Kind, Value};
use lumen_synth::{DEFAULT_VOLUME, Freq, Melody, ParseError, Rate, Sequencer, Source, TooHigh};
use virtio_drivers::device::sound::{PcmFeatures, PcmFormat, PcmFormats, PcmRate, PcmRates, VirtIOSound};
use virtio_drivers::transport::DeviceStatus;
use zerocopy::IntoBytes;

use crate::drivers::Driver;
use crate::pci::{DeviceInfo, DeviceResources};
use crate::sched::WaitQueue;
use crate::shell::Ctx;
use crate::sync::SpinLock;
use crate::virtio::{self, Doorbell, ProbeError, StatusHandle, VirtioHal, VirtioTransport, WaitError};

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
/// How long a period may take to come back before the device counts as
/// stuck: far more than the 160 ms of [`DEPTH`] periods queued ahead of it.
const PERIOD_WAIT_NS: u64 = 2_000_000_000;
/// While playing, more than this without progress (the sound starting, a
/// control request done, a period taken back; a stopped sound's fade-out
/// and its stop and release requests included) means the thread is stuck
/// in a control request: the next look ([`Bound::check`]) gives the player
/// up. Nothing looks on a timer.
const STUCK_NS: u64 = 5_000_000_000;
/// Sounds at least this long are checked for having taken at least half
/// their length to play (a device returning periods unplayed).
const PACE_CHECK_MS: u64 = 1000;
/// `beep`'s default frequency (also rhai's `beep()`).
pub const DEFAULT_BEEP_HZ: u32 = 440;
/// `beep`'s default length.
pub const DEFAULT_BEEP_MS: u32 = 200;
/// The longest `beep`.
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
struct SoundId(NonZeroU64);

/// How a sound ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// To its end.
    Played,
    /// [`stop`]ped (faded out, or never started).
    Stopped,
    /// A newer sound took its place (cut off, or never started).
    Replaced,
    /// The device failed or was given up: why.
    Failed(String),
    /// The driver dropped the sound without settling it: a bug in the
    /// driver, not a device failure (the device may well be fine).
    Lost,
}

/// How the thread ended a sound it played (failing is an `Err`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ended {
    Played,
    Stopped,
    Replaced,
}

impl From<Ended> for Outcome {
    fn from(e: Ended) -> Outcome {
        match e {
            Ended::Played => Outcome::Played,
            Ended::Stopped => Outcome::Stopped,
            Ended::Replaced => Outcome::Replaced,
        }
    }
}

/// Where a sound's [`Outcome`] lands once it is over (its lock is a leaf).
type OutcomeCell = Arc<SpinLock<Option<Outcome>>>;

/// A sound handed to [`play`], and how it ended once it has. The outcome
/// lives here, not in the player, so it is never lost to later sounds.
pub struct Sound {
    id: SoundId,
    outcome: OutcomeCell,
}

impl Sound {
    /// How it ended, or `None` while it waits or plays. One exception: a
    /// sound playing on a thread stuck for good (in a control request) stays
    /// `None`, since only that thread settles it. A caller waiting for an
    /// outcome should also poll [`format()`], which says when the player
    /// has been given up and is itself the look that gives a stuck thread
    /// up (over [`STUCK_NS`] without an answer).
    pub fn outcome(&self) -> Option<Outcome> {
        self.outcome.lock().clone()
    }
}

/// The player's side of sound `id`'s outcome: settled once, consuming it.
/// Every path settles it explicitly, with its reason; one dropped unsettled
/// (a bug) settles [`Outcome::Lost`] and logs an error, so a lost `Settle`
/// can't leave an outcome `None` for good (#135).
struct Settle {
    id: SoundId,
    /// Taken by [`Settle::settle`] or by the drop; `Some` until then.
    cell: Option<OutcomeCell>,
}

impl Settle {
    fn new(id: SoundId, cell: OutcomeCell) -> Settle {
        Settle { id, cell: Some(cell) }
    }

    fn settle(mut self, outcome: Outcome) {
        let cell = self.cell.take().expect("only `settle`, which consumes it, or the drop takes the cell");
        *cell.lock() = Some(outcome);
    }
}

impl Drop for Settle {
    fn drop(&mut self) {
        // Safe anywhere logging is allowed (not under the logger's own
        // locks, `LINE_LOCK` or `SCREEN`), also under `PLAYER`'s lock
        // (the logger's locks never lead back to it): the outcome's lock
        // is a leaf, held only to look and to store an outcome built
        // before it was taken, and released before the error is logged.
        let Some(cell) = self.cell.take() else { return };
        let lost = Some(Outcome::Lost);
        let mut outcome = cell.lock();
        if outcome.is_some() {
            return;
        }
        *outcome = lost;
        drop(outcome);
        error!(target: "sound", "sound {}: dropped without an outcome (a bug): settled it lost", self.id.0);
    }
}

/// A sound waiting for the thread.
struct Pending {
    id: SoundId,
    source: Box<dyn Source + Send>,
    settle: Settle,
}

/// What the bound device is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Activity {
    Idle,
    /// Playing `id`; `stop` once [`stop`] asked; `progress_ns` when the
    /// sound started or the thread last got an answer from the device.
    Playing {
        id: SoundId,
        stop: bool,
        progress_ns: u64,
    },
    /// Given up: why. The thread has ended, or ends when it next gets an
    /// answer from the device (stuck), or when it is woken to drop the
    /// device (given up while idle, by [`refusal`]).
    Failed(String),
}

/// What the player has played.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Counts {
    /// Sounds that played to their end or until stopped.
    played: u64,
    /// Sounds a newer one cut off or kept from starting.
    replaced: u64,
    /// Frames handed to the device (a last period's padding not counted).
    frames: u64,
    /// How often the queue ran dry before a sound's end.
    underruns: u64,
}

/// The bound device, what it does and what it played.
struct Bound {
    device: Device,
    /// The device's status register (read by [`Bound::check`]).
    status: StatusHandle,
    activity: Activity,
    /// The sound waiting to start.
    next: Option<Pending>,
    counts: Counts,
}

/// Why sounds can't play, as found under [`PLAYER`]'s lock; [`refusal`]
/// makes it a [`SoundError`] once the lock is dropped. Every one must reach
/// [`refusal`]: a [`GivenUp`](Refusal::GivenUp) dropped elsewhere loses
/// `refusal`'s warning, `pci::failed_after_bind` (stuck) or the thread's
/// wake (idle), and its waiting sound settles only by [`Settle`]'s drop,
/// as [`Outcome::Lost`]. (`#[must_use]` rarely fires, since a `Refusal`
/// comes inside a `Result`; the drop is the real guard.)
#[must_use = "pass it to `refusal`: a `GivenUp` needs its warning, its device recorded or woken, and its waiting sound settled"]
enum Refusal {
    NoDevice,
    Failed(String),
    /// Just given up: why, whether the thread is stuck (else idle, to be
    /// woken to drop the device), and the sound that was waiting to start
    /// ([`Bound::give_up`]), for [`refusal`] to settle.
    GivenUp {
        addr: PciAddr,
        why: String,
        stuck: bool,
        waiting: Option<Pending>,
    },
}

impl Bound {
    /// Whether a sound is under way: playing, or handed to [`play`] and
    /// not yet taken by the thread (`sound` says `playing` for both: the
    /// command that started it has returned by then).
    fn busy(&self) -> bool {
        self.next.is_some() || matches!(self.activity, Activity::Playing { .. })
    }

    /// Whether sounds can play: not if the device failed. A thread stuck
    /// (playing with no answer for over [`STUCK_NS`]) or an idle device
    /// whose status says it is broken is given up here, for good. This runs
    /// only when asked (the next `sound`, `beep` or `play`), not on a timer:
    /// a stuck thread is given up by the first look after [`STUCK_NS`]. The
    /// [`Refusal`] must reach [`refusal`], which settles the sound that was
    /// waiting.
    fn check(&mut self, now: u64) -> Result<Format, Refusal> {
        let raw = self.status.raw();
        let reports = || format!("the device reports status {raw:#04x}: {}", virtio::not_ready_why(raw));
        let (why, stuck) = match &self.activity {
            Activity::Failed(why) => return Err(Refusal::Failed(why.clone())),
            Activity::Playing { progress_ns, .. } => match now.saturating_sub(*progress_ns) {
                quiet if quiet <= STUCK_NS => return Ok(self.device.format),
                _ if broken(raw) => (reports(), true),
                quiet => (
                    format!(
                        "no answer from the device for {} s (stuck in a control request; reboot to retry)",
                        quiet / 1_000_000_000
                    ),
                    true,
                ),
            },
            Activity::Idle if self.status.check_ready().is_ok() => return Ok(self.device.format),
            Activity::Idle => (reports(), false),
        };
        // (Not failed yet, above: the reason is always this one.)
        let (recorded, waiting) = self.give_up(why);
        Err(Refusal::GivenUp { addr: self.device.addr, why: recorded.into_reason(), stuck, waiting })
    }

    /// Give the player up for good with `why` (unless it already was: the
    /// first reason stays) and take the sound waiting to start. Returns
    /// which reason stands ([`Recorded`]) and that sound, to be settled
    /// failed with it ([`settle_failed`]) once [`PLAYER`]'s lock is
    /// dropped: by [`fail`] itself, or by [`refusal`] for [`Bound::check`].
    ///
    /// Every give-up comes through here ([`Bound::check`], [`fail`]) and
    /// [`play`] refuses a failed player, so a failed player holds no
    /// waiting sound: none is left unsettled once the device is given up,
    /// whether the thread is idle or stuck for good (#135). A stuck thread
    /// is given up only by the next look ([`Bound::check`]), so until then
    /// the sound waiting behind it is unsettled too.
    #[must_use = "settle the waiting sound it returns (`settle_failed`)"]
    fn give_up(&mut self, why: String) -> (Recorded, Option<Pending>) {
        let recorded = match &self.activity {
            Activity::Failed(given) => Recorded::Kept(given.clone()),
            _ => {
                self.activity = Activity::Failed(why.clone());
                Recorded::New(why)
            }
        };
        (recorded, self.next.take())
    }
}

/// The reason [`Bound::give_up`] left standing.
enum Recorded {
    /// The one given: the player's first.
    New(String),
    /// The one it was given up with earlier (the one given isn't kept).
    Kept(String),
}

impl Recorded {
    fn into_reason(self) -> String {
        match self {
            Recorded::New(why) | Recorded::Kept(why) => why,
        }
    }
}

/// Settle `waiting` (a sound that never started) as failed with `why`.
fn settle_failed(waiting: Option<Pending>, why: &str) {
    if let Some(w) = waiting {
        w.settle.settle(Outcome::Failed(why.into()));
    }
}

/// Whether a status byte says the device is broken (`DEVICE_NEEDS_RESET`
/// or `FAILED`; all ones reads as `FAILED`).
fn broken(raw: u8) -> bool {
    DeviceStatus::from_bits_truncate(u32::from(raw)).intersects(DeviceStatus::DEVICE_NEEDS_RESET | DeviceStatus::FAILED)
}

/// The state shared between [`play`]'s callers and the `sound` thread.
struct Player {
    /// The bound device (set once, by the probe).
    bound: Option<Bound>,
    /// The last id handed out.
    last_id: u64,
}

impl Player {
    /// [`Bound::check`] on the bound device.
    fn check(&mut self, now: u64) -> Result<Format, Refusal> {
        self.bound.as_mut().ok_or(Refusal::NoDevice)?.check(now)
    }
}

static PLAYER: SpinLock<Player> = SpinLock::new(Player { bound: None, last_id: 0 });

/// The `sound` thread waits here for a sound, or to give its device up.
static WAKE: WaitQueue = WaitQueue::new();

/// The volume new sounds get, in percent.
static VOLUME: AtomicU8 = AtomicU8::new(DEFAULT_VOLUME);

/// Why a sound can't play.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SoundError {
    /// No sound device is bound: the hint says what was found instead.
    NoDevice(String),
    /// The device failed or was given up: why.
    Failed(String),
    /// A frequency the device's sample rate can't carry.
    TooHigh(TooHigh),
    /// A melody that doesn't parse, or has a note the rate can't carry.
    Melody(ParseError),
    /// A source made at another rate than the device's.
    WrongRate { source: Rate, device: Rate },
}

impl fmt::Display for SoundError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SoundError::NoDevice(hint) => write!(f, "no sound device{hint}"),
            SoundError::Failed(why) => write!(f, "the sound device failed: {why}"),
            SoundError::TooHigh(e) => write!(f, "{e}"),
            SoundError::Melody(e) => write!(f, "{e}"),
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

/// `sound`'s line without a device.
fn no_device_line() -> String {
    format!("sound: no sound device{}", no_device_hint())
}

/// `r` as the error callers see, once [`PLAYER`]'s lock is dropped: the
/// no-device hint added, and a device just given up reported (a warning;
/// the sound that was waiting settled failed; a stuck thread can't drop the
/// device, so it is recorded failed here; an idle one is woken to drop it
/// and record it).
fn refusal(r: Refusal) -> SoundError {
    match r {
        Refusal::NoDevice => SoundError::NoDevice(no_device_hint()),
        Refusal::Failed(why) => SoundError::Failed(why),
        Refusal::GivenUp { addr, why, stuck, waiting } => {
            settle_failed(waiting, &why);
            warn!(target: "sound", "{addr}: the device is given up: {why}");
            if stuck {
                // False (ignored) if already recorded failed, by a thread
                // that came back in between and failed, or for a ktest's
                // made-up address: either way nothing is left to record.
                crate::pci::failed_after_bind(addr, DRIVER.name, why.clone());
            } else {
                WAKE.wake_one();
            }
            SoundError::Failed(why)
        }
    }
}

/// The format sounds must be made in (their rate), or why none can play.
pub fn format() -> Result<Format, SoundError> {
    format_at(crate::time::now_ns())
}

/// [`format`], looking at `now` (ktests pass a time past [`STUCK_NS`]).
fn format_at(now: u64) -> Result<Format, SoundError> {
    let checked = PLAYER.lock().check(now);
    checked.map_err(refusal)
}

/// Play `source` (made at [`format`]'s rate): it cuts off whatever plays at
/// its next period and drops one waiting. Returns at once.
pub fn play(source: Box<dyn Source + Send>) -> Result<Sound, SoundError> {
    let outcome: OutcomeCell = Arc::new(SpinLock::new(None));
    let now = crate::time::now_ns();
    let mut p = PLAYER.lock();
    let format = match p.check(now) {
        Ok(f) => f,
        Err(r) => {
            drop(p);
            return Err(refusal(r));
        }
    };
    if source.rate() != format.rate {
        drop(p);
        return Err(SoundError::WrongRate { source: source.rate(), device: format.rate });
    }
    p.last_id += 1;
    let id = SoundId(NonZeroU64::new(p.last_id).expect("ids count from 1"));
    let b = p.bound.as_mut().expect("checked: bound");
    let dropped = b.next.replace(Pending { id, source, settle: Settle::new(id, outcome.clone()) });
    if dropped.is_some() {
        b.counts.replaced += 1;
    }
    drop(p);
    if let Some(d) = dropped {
        d.settle.settle(Outcome::Replaced);
    }
    WAKE.wake_one();
    Ok(Sound { id, outcome })
}

/// Fade `sound` out (within its release time; the periods already queued
/// still play), or keep it from starting. Nothing if it is over.
pub fn stop(sound: &Sound) {
    let mut p = PLAYER.lock();
    let Some(b) = p.bound.as_mut() else { return };
    let unstarted = b.next.take_if(|n| n.id == sound.id);
    if let Activity::Playing { id, stop, .. } = &mut b.activity
        && *id == sound.id
    {
        *stop = true;
    }
    drop(p);
    if let Some(n) = unstarted {
        n.settle.settle(Outcome::Stopped);
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

/// `text` as a melody at `rate`, at the current volume.
fn melody(text: String, rate: Rate) -> Result<Sequencer<String>, SoundError> {
    let mut seq = Melody::parse(text).and_then(|m| Sequencer::new(m, rate)).map_err(SoundError::Melody)?;
    seq.set_volume(volume());
    Ok(seq)
}

/// Whether a sound of 1 s or more ([`PACE_CHECK_MS`]) of `frames` at
/// `rate` came back in `ms`, under half its length: periods returned
/// without being played.
fn played_too_fast(frames: u64, rate: Rate, ms: u64) -> bool {
    let length_ms = frames * 1000 / u64::from(rate.hz());
    length_ms >= PACE_CHECK_MS && ms < length_ms / 2
}

/// Rung by the device's MSI-X interrupt: whatever it signals (a buffer
/// back on any of its queues, a period among them, or a configuration
/// change).
static BELL: Doorbell = Doorbell::new();

fn on_interrupt() {
    BELL.ring();
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
    let transport = virtio::transport(res, on_interrupt)?;
    let status = transport.status();
    let addr = transport.info().addr;
    let mut dev = VirtIOSound::<VirtioHal, VirtioTransport>::new(transport).map_err(ProbeError::Device)?;
    virtio::check_device(&status)?;
    let format = set_up(&mut dev)?;
    status.check_ready()?;
    let device = Device { addr, format };
    let stream = Stream { dev, status, device, samples: vec![0; format.period_samples()] };
    // Detached: it runs for the kernel's life (or until the device fails).
    let _thread = crate::sched::spawn("sound", move || run(stream)).map_err(ProbeError::Thread)?;
    PLAYER.lock().bound =
        Some(Bound { device, status, activity: Activity::Idle, next: None, counts: Counts::default() });
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
    /// The device wasn't ready before a sound: its status.
    NotReady { status: u8 },
    /// Periods came back faster than they can have played.
    Unplayed { frames: u64, ms: u64 },
    /// The player was given up while the thread was stuck: why.
    GivenUp(String),
}

impl fmt::Display for PlayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlayError::Device { op, e } => write!(f, "{op}: virtio: {e}"),
            PlayError::Wait(e) => write!(f, "waiting for a period: {e}"),
            PlayError::NotReady { status } => {
                let why = virtio::not_ready_why(*status);
                write!(f, "checking the device before a sound: status {status:#04x}: {why}")
            }
            PlayError::Unplayed { frames, ms } => {
                write!(f, "the device returned periods without playing them ({frames} frames in {ms} ms)")
            }
            PlayError::GivenUp(why) => f.write_str(why),
        }
    }
}

/// `r`, or an `Err` naming the request `op` the device refused.
fn requested<T>(op: &'static str, r: virtio_drivers::Result<T>) -> Result<T, PlayError> {
    r.map_err(|e| PlayError::Device { op, e })
}

/// The device and its stream: the `sound` thread's own.
struct Stream {
    dev: VirtIOSound<VirtioHal, VirtioTransport>,
    status: StatusHandle,
    device: Device,
    /// One period's samples (always [`Format::period_samples`] long).
    samples: Vec<i16>,
}

/// What one sound played (also when it failed).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Played {
    frames: u64,
    underruns: u64,
}

/// What the thread is to do next.
enum Next {
    Play(Pending),
    /// The player was given up, while idle, or as stuck and the thread has
    /// come back since: why. Drop the device.
    GiveUp(String),
}

/// Why the `sound` thread drops its device: the reason either way.
enum Last {
    /// It found the player given up ([`Next::GiveUp`]).
    GivenUp(String),
    /// Its sound failed ([`fail`]'s reason).
    PlayingFailed(String),
}

/// The `sound` thread: play each sound handed over, until the device fails
/// or is given up.
fn run(mut stream: Stream) {
    let addr = stream.device.addr;
    loop {
        WAKE.wait_until(|| {
            PLAYER.lock().bound.as_ref().is_some_and(|b| b.next.is_some() || matches!(b.activity, Activity::Failed(_)))
        });
        let last = match take_next() {
            None => continue,
            Some(Next::GiveUp(why)) => Last::GivenUp(why),
            Some(Next::Play(Pending { id, source, settle })) => {
                let mut played = Played::default();
                match stream.play(id, source, &mut played) {
                    Ok(ended) => {
                        record(played, ended, settle);
                        continue;
                    }
                    Err(e) => Last::PlayingFailed(fail(&e.to_string(), played, settle)),
                }
            }
        };
        // Dropping it resets the device (`VirtioTransport`'s `Drop`); then
        // `lspci` says why.
        drop(stream);
        let (Last::GivenUp(failed) | Last::PlayingFailed(failed)) = &last;
        if !crate::pci::failed_after_bind(addr, DRIVER.name, failed.clone()) {
            // Already recorded failed: given up as stuck (`refusal`), and
            // the thread has come back since (`fail` warned if with a new
            // error).
            warn!(target: "sound", "{addr}: the thread came back after the device was given up ({failed}); dropped it");
        } else if let Last::PlayingFailed(_) = last {
            // (Also a stuck thread that came back and failed between
            // `Bound::check`'s give-up and `refusal`'s record of it: the
            // reason printed is still the stuck one.)
            warn!(target: "sound", "{addr}: playing failed: {failed}; the device is given up");
        } else {
            warn!(target: "sound", "{addr}: given up while idle: {failed}; dropped the device");
        }
        return;
    }
}

/// Take the sound waiting to start, marking it playing (one critical
/// section, so [`stop`] finds it in one place or the other), or learn the
/// player was given up (settling a sound still waiting then as failed,
/// logging an error: a give-up takes it already, [`Bound::give_up`], so
/// this fires only if that invariant breaks).
fn take_next() -> Option<Next> {
    let now = crate::time::now_ns();
    let mut p = PLAYER.lock();
    let b = p.bound.as_mut()?;
    if let Activity::Failed(why) = &b.activity {
        let (why, waiting) = (why.clone(), b.next.take());
        drop(p);
        if let Some(w) = &waiting {
            error!(target: "sound", "sound {}: left waiting on the given-up player (a bug): settled it failed", w.id.0);
        }
        settle_failed(waiting, &why);
        return Some(Next::GiveUp(why));
    }
    let next = b.next.take()?;
    b.activity = Activity::Playing { id: next.id, stop: false, progress_ns: now };
    Some(Next::Play(next))
}

/// A sound ended: count it and settle its outcome.
fn record(played: Played, ended: Ended, settle: Settle) {
    // The device may have been given up meanwhile (as stuck, while the
    // thread waited on a control request that then completed): the sound
    // still played to `ended` (`pcm_release` answered), so it settles as
    // that; the thread then finds the player given up and drops the device.
    let mut p = PLAYER.lock();
    if let Some(b) = p.bound.as_mut() {
        if let Activity::Playing { .. } = b.activity {
            b.activity = Activity::Idle;
        }
        b.counts.frames += played.frames;
        b.counts.underruns += played.underruns;
        match ended {
            Ended::Played | Ended::Stopped => b.counts.played += 1,
            Ended::Replaced => b.counts.replaced += 1,
        }
    }
    drop(p);
    if played.underruns > 0 {
        warn!(target: "sound", "a sound found the queue empty {} times (underruns)", played.underruns);
    }
    settle.settle(ended.into());
}

/// A sound failed with `why`: count what it played, mark the player failed
/// (keeping the reason it was given up with, if it was; `why` is then
/// warned about if it differs, so it isn't lost), and settle the sound and
/// the one
/// waiting as failed. Returns the reason that stands.
fn fail(why: &str, played: Played, settle: Settle) -> String {
    let mut p = PLAYER.lock();
    let (recorded, waiting) = match p.bound.as_mut() {
        Some(b) => {
            b.counts.frames += played.frames;
            b.counts.underruns += played.underruns;
            b.give_up(why.into())
        }
        None => (Recorded::New(why.into()), None),
    };
    drop(p);
    if let Recorded::Kept(kept) = &recorded
        && kept != why
    {
        warn!(target: "sound", "a sound failed after the device was given up ({kept}): {why}");
    }
    let reason = recorded.into_reason();
    settle.settle(Outcome::Failed(reason.clone()));
    settle_failed(waiting, &reason);
    reason
}

/// The device answered: the thread isn't stuck; `Err` if the player was
/// given up meanwhile.
fn progress() -> Result<(), PlayError> {
    let now = crate::time::now_ns();
    let mut p = PLAYER.lock();
    match p.bound.as_mut().map(|b| &mut b.activity) {
        Some(Activity::Playing { progress_ns, .. }) => {
            *progress_ns = now;
            Ok(())
        }
        Some(Activity::Failed(why)) => Err(PlayError::GivenUp(why.clone())),
        _ => Ok(()),
    }
}

/// Whether sound `id` is to end early, and as what: stopped, or cut off by
/// a newer sound.
fn cut_short(id: SoundId) -> Option<Ended> {
    let p = PLAYER.lock();
    let b = p.bound.as_ref()?;
    match b.activity {
        Activity::Playing { id: i, stop: true, .. } if i == id => Some(Ended::Stopped),
        _ if b.next.is_some() => Some(Ended::Replaced),
        _ => None,
    }
}

impl Stream {
    /// Whether the device has returned period `token` (`pcm_xfer_ok` takes
    /// it back).
    fn taken_back(&mut self, token: u16) -> Result<bool, PlayError> {
        match self.dev.pcm_xfer_ok(token) {
            Ok(()) => Ok(true),
            Err(virtio_drivers::Error::NotReady) => Ok(false),
            Err(e) => Err(PlayError::Device { op: "taking back a period", e }),
        }
    }

    /// Wait (bounded, blocking until the device's interrupt) until the
    /// device returns period `token`.
    fn wait_taken_back(&mut self, token: u16) -> Result<(), PlayError> {
        let status = self.status;
        let mut answer = Ok(false);
        status
            .wait_for(&BELL, PERIOD_WAIT_NS, || {
                answer = self.taken_back(token);
                !matches!(answer, Ok(false))
            })
            .map_err(PlayError::Wait)?;
        answer.map(drop)
    }

    /// Play `source`, paced by the device (the module doc), counting into
    /// `played`.
    fn play(
        &mut self,
        id: SoundId,
        mut source: Box<dyn Source + Send>,
        played: &mut Played,
    ) -> Result<Ended, PlayError> {
        let format = self.device.format;
        let stream = format.stream;
        let mut how = Ended::Played;
        let mut queued: VecDeque<u16> = VecDeque::with_capacity(DEPTH);
        let mut ended = false;
        // A device broken while idle fails here, not in a control request.
        if self.status.check_ready().is_err() {
            return Err(PlayError::NotReady { status: self.status.raw() });
        }
        requested("preparing the stream", self.dev.pcm_prepare(stream))?;
        progress()?;
        requested("starting the stream", self.dev.pcm_start(stream))?;
        progress()?;
        let start = crate::time::now_ns();
        loop {
            // (A sound that has made all its samples plays out.)
            if !ended
                && how == Ended::Played
                && let Some(cut) = cut_short(id)
            {
                source.stop();
                how = cut;
            }
            while !ended && queued.len() < DEPTH {
                let filled = source.fill(&mut self.samples, format.channels);
                ended = filled.ended;
                if filled.frames == 0 {
                    break;
                }
                played.frames += filled.frames as u64;
                queued
                    .push_back(requested("queueing a period", self.dev.pcm_xfer_nb(stream, self.samples.as_bytes()))?);
            }
            let Some(&oldest) = queued.front() else { break };
            // Wait for the oldest period, then take back any others done.
            self.wait_taken_back(oldest)?;
            queued.pop_front();
            while let Some(&token) = queued.front()
                && self.taken_back(token)?
            {
                queued.pop_front();
            }
            progress()?;
            if queued.is_empty() && !ended {
                played.underruns += 1;
            }
        }
        let ms = crate::time::now_ns().saturating_sub(start) / 1_000_000;
        if played_too_fast(played.frames, format.rate, ms) {
            return Err(PlayError::Unplayed { frames: played.frames, ms });
        }
        requested("stopping the stream", self.dev.pcm_stop(stream))?;
        progress()?;
        requested("releasing the stream", self.dev.pcm_release(stream))?;
        Ok(how)
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
/// ends, `q`/Ctrl-C stops it (other keys typed meanwhile are discarded), a
/// newer sound cuts it off, or the device fails or is given up (then it
/// stops its sound, so nothing plays on unseen).
pub fn play_command(ctx: &mut Ctx<'_>, args: &[String]) {
    if args.is_empty() {
        return ctx.fail("usage: play <melody> (e.g. play t=140 C4/8 E4/8 G4/4 r/8 C5/2)");
    }
    let started = format().and_then(|f| melody(args.join(" "), f.rate)).and_then(|seq| play(Box::new(seq)));
    let sound = match started {
        Ok(sound) => sound,
        Err(e) => return ctx.fail(format_args!("play: {e}")),
    };
    let mut stopped = false;
    let outcome = loop {
        if let Some(outcome) = sound.outcome() {
            break outcome;
        }
        if !stopped && let Some(Key::Char('q') | Key::Ctrl('c') | Key::Closed) = ctx.poll_key() {
            stop(&sound);
            stopped = true;
        }
        // (A failed device settles the outcome too. A stuck thread isn't
        // given up on a timer: this `format()` is the look that gives it up
        // once over `STUCK_NS` without an answer, and a sound playing on it
        // then never settles.)
        if let Err(SoundError::Failed(why)) = format()
            && sound.outcome().is_none()
        {
            stop(&sound);
            if stopped {
                warn!(target: "sound", "play gave up waiting for its stopped sound: {why}");
            }
            break Outcome::Failed(why);
        }
        crate::time::sleep(KEY_POLL_NS);
    };
    match outcome {
        Outcome::Played => {}
        Outcome::Stopped => {
            let _ = writeln!(ctx.out, "play: stopped");
        }
        Outcome::Replaced => ctx.fail("play: interrupted by a newer sound"),
        Outcome::Failed(why) => ctx.fail(format_args!("play: {}", SoundError::Failed(why))),
        Outcome::Lost => ctx.fail("play: the sound was lost by the driver (a bug)"),
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
    let device = PLAYER.lock().bound.as_ref().map(|b| b.device);
    let mut t = String::new();
    match device {
        Some(d) => {
            let _ = writeln!(t, "sound: virtio sound at {}, {}", d.addr, d.format);
            table().render(&mut t);
        }
        None => {
            t = no_device_line();
            t.push('\n');
        }
    }
    let _ = ctx.out.write_str(&t);
}

/// What `sound`'s one record says.
struct Row {
    state: String,
    counts: Counts,
}

/// `sound`'s records: one row, none without a device. A sound handed to
/// [`play`] shows `playing` at once, before the thread has taken it (the
/// command that started it has returned by then: `beep`), until it is
/// over.
pub fn table() -> pipe::Table {
    table_at(crate::time::now_ns())
}

/// [`table`], looking at `now` (ktests pass a time past [`STUCK_NS`]).
fn table_at(now: u64) -> pipe::Table {
    let mut p = PLAYER.lock();
    let checked = p.check(now);
    let seen = p.bound.as_ref().map(|b| (b.counts, b.busy()));
    drop(p);
    let row = seen.map(|(counts, playing)| Row {
        state: match checked.map_err(refusal) {
            Err(SoundError::Failed(why)) => format!("failed: {why}"),
            Err(other) => other.to_string(),
            Ok(_) if playing => String::from("playing"),
            Ok(_) => String::from("idle"),
        },
        counts,
    });
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
    if let Some(Row { state, counts: c }) = row {
        t.push(vec![
            Value::Text(state),
            int(c.played),
            int(c.replaced),
            int(c.frames),
            int(c.underruns),
            Value::Int(i64::from(volume())),
        ]);
    }
    t
}

/// A note for `sound`'s records when there are none.
pub fn records_note() -> Option<String> {
    let absent = PLAYER.lock().bound.is_none();
    absent.then(no_device_line)
}

fn int(n: u64) -> Value {
    i64::try_from(n).map_or(Value::Absent("-"), Value::Int)
}

/// Ktests: put the player back as the ktest machine has it (no device),
/// however the test ends.
#[cfg(feature = "ktest")]
struct Unbind;

#[cfg(feature = "ktest")]
impl Drop for Unbind {
    fn drop(&mut self) {
        let old = PLAYER.lock().bound.take();
        settle_leftover(old);
    }
}

/// A made-up player's sound a test left waiting, settled failed (`ktest
/// unbound`), so [`Settle`]'s drop error only ever means a bug.
#[cfg(feature = "ktest")]
fn settle_leftover(old: Option<Bound>) {
    if let Some(Bound { next: Some(w), .. }) = old {
        w.settle.settle(Outcome::Failed("ktest unbound".into()));
    }
}

/// Bind a made-up device for the ktests (the ktest machine has none, so no
/// thread either: a sound handed to [`play`] stays waiting), doing
/// `activity`, its status read from `status`. Returns its sample rate.
#[cfg(feature = "ktest")]
fn bind_made_up(status: &'static AtomicU8, activity: Activity) -> Result<Rate, &'static str> {
    let format = choose(0, PcmRates::all(), PcmFormats::S16, 1..=2)?;
    let addr = PciAddr::new(0, 0xff, 0x1f, 7).ok_or("addr")?;
    let bound = Bound {
        device: Device { addr, format },
        status: StatusHandle::ktest_reading(status, None),
        activity,
        next: None,
        counts: Counts::default(),
    };
    let old = PLAYER.lock().bound.replace(bound);
    settle_leftover(old);
    Ok(format.rate)
}

/// A 10 ms beep at `rate`, boxed for [`play`].
#[cfg(feature = "ktest")]
fn made_up_tone(rate: Rate) -> Result<Box<dyn Source + Send>, &'static str> {
    Ok(Box::new(lumen_synth::beep(Freq::from_hz(440), Duration::from_millis(10), rate).map_err(|_| "tone")?))
}

/// Whether `sound` settled failed with a reason containing `what`.
#[cfg(feature = "ktest")]
fn failed_with(sound: &Sound, what: &str) -> bool {
    matches!(sound.outcome(), Some(Outcome::Failed(why)) if why.contains(what))
}

/// Whether `r` is a refusal because the device failed, the reason
/// containing `what`.
#[cfg(feature = "ktest")]
fn refused_with<T>(r: Result<T, SoundError>, what: &str) -> bool {
    matches!(r, Err(SoundError::Failed(why)) if why.contains(what))
}

/// Whether the made-up player has no sound waiting.
#[cfg(feature = "ktest")]
fn none_waiting() -> bool {
    PLAYER.lock().bound.as_ref().is_some_and(|b| b.next.is_none())
}

/// How many lines of the kernel log (as much as its ring holds) contain
/// `what`: compared before and after, to tell a warning was logged.
#[cfg(feature = "ktest")]
fn logged(what: &str) -> Result<usize, &'static str> {
    let mut log = alloc::vec![0u8; crate::klog::RING_BYTES];
    let (n, _) = crate::klog::copy_recent(&mut log);
    let log = core::str::from_utf8(&log[..n]).map_err(|_| "the log isn't UTF-8")?;
    Ok(log.lines().filter(|l| l.contains(what)).count())
}

/// A status byte showing `DEVICE_NEEDS_RESET` (and the bits of a device
/// that was ready).
#[cfg(feature = "ktest")]
const NEEDS_RESET: u8 = 0x4f;

crate::ktest!(
    fn the_record_fits_its_columns() {
        // (The ktest machine has no sound device: `table()` has no row.)
        use crate::kassert_eq;
        let counts = Counts { played: 1, replaced: 2, frames: u64::MAX, underruns: 0 };
        let t = table_of(Some(Row { state: "failed: x".into(), counts }));
        kassert_eq!(t.rows().len(), 1);
        kassert_eq!(t.rows()[0][3], Value::Absent("-"));
        kassert_eq!(table_of(None).rows().len(), 0);
        Ok(())
    }
);

crate::ktest!(
    fn a_sound_handed_over_shows_playing_before_the_thread_takes_it() {
        // (#120) With a made-up bound device (no thread: the sound is never
        // taken), `sound` says `playing` from the moment `play` returns,
        // and `idle` again once the sound is kept from starting.
        use crate::kassert_eq;
        static STATUS: AtomicU8 = AtomicU8::new(0x0f);
        let _unbind = Unbind;
        let rate = bind_made_up(&STATUS, Activity::Idle)?;
        let state = || table().rows()[0][0].clone();
        kassert_eq!(state(), Value::Text("idle".into()));
        let sound = play(made_up_tone(rate)?).map_err(|_| "play")?;
        kassert_eq!(state(), Value::Text("playing".into()));
        stop(&sound);
        kassert_eq!(state(), Value::Text("idle".into()));
        Ok(())
    }
);

crate::ktest!(
    fn the_pace_check_catches_periods_returned_unplayed() {
        use crate::kassert;
        let rate = Rate::new(44_100).ok_or("rate")?;
        // A 2 s sound: in 999 ms too fast, in 1000 ms fine.
        kassert!(played_too_fast(88_200, rate, 999));
        kassert!(!played_too_fast(88_200, rate, 1000));
        // Under 1 s, never judged; 1 s in no time is.
        kassert!(!played_too_fast(44_099, rate, 0));
        kassert!(played_too_fast(44_100, rate, 0));
        Ok(())
    }
);

crate::ktest!(
    fn a_failed_or_stuck_device_refuses_sounds() {
        // With a made-up bound device (the ktest machine has none, so
        // nothing plays) whose status byte the test sets: replacing and
        // stopping waiting sounds, a failure settling the waiting sound, a
        // failed device, a stuck thread, a device reporting itself broken
        // while idle, and a source at another rate. `Unbind` puts the
        // player back.
        use crate::{kassert, kassert_eq};
        static STATUS: AtomicU8 = AtomicU8::new(0x0f);
        let _unbind = Unbind;
        kassert!(PLAYER.lock().bound.is_none());
        let rate = bind_made_up(&STATUS, Activity::Idle)?;
        let tone = || made_up_tone(rate);

        // Two sounds waiting: the first is dropped, counted as replaced;
        // the second, stopped, never starts.
        let first = play(tone()?).map_err(|_| "play 1")?;
        let second = play(tone()?).map_err(|_| "play 2")?;
        kassert_eq!(first.outcome(), Some(Outcome::Replaced));
        kassert_eq!(table().rows()[0][2], Value::Int(1));
        stop(&second);
        kassert_eq!(second.outcome(), Some(Outcome::Stopped));
        kassert!(none_waiting());

        // A failure counts what the sound played and settles it and the
        // one waiting.
        let waiting = play(tone()?).map_err(|_| "play 3")?;
        let cell: OutcomeCell = Arc::new(SpinLock::new(None));
        let why = fail("x", Played { frames: 5, underruns: 1 }, Settle::new(SoundId(NonZeroU64::MIN), cell.clone()));
        kassert_eq!(why.as_str(), "x");
        kassert_eq!(cell.lock().clone(), Some(Outcome::Failed("x".into())));
        kassert_eq!(waiting.outcome(), Some(Outcome::Failed("x".into())));
        let counts = PLAYER.lock().bound.as_ref().map(|b| b.counts);
        kassert_eq!(counts.map(|c| (c.frames, c.underruns)), Some((5, 1)));

        // Failed: refused with the reason, everywhere.
        let failed = SoundError::Failed("x".into());
        kassert_eq!(format(), Err(failed.clone()));
        kassert_eq!(beep(440, 10).map(drop), Err(failed.clone()));
        kassert_eq!(play(tone()?).map(drop), Err(failed.clone()));
        kassert_eq!(failed.to_string().as_str(), "the sound device failed: x");
        kassert_eq!(table().rows()[0][0], Value::Text("failed: x".into()));

        // Stuck (a look over `STUCK_NS` after the last progress, not at
        // it): given up for good, `sound` saying so; with a broken status,
        // the status says why.
        let now = crate::time::now_ns();
        let id = SoundId(NonZeroU64::MIN);
        bind_made_up(&STATUS, Activity::Playing { id, stop: false, progress_ns: now })?;
        kassert!(format_at(now + STUCK_NS).is_ok());
        let shown = matches!(&table_at(now + STUCK_NS + 1).rows()[0][0], Value::Text(s) if s.contains("stuck in a control request"));
        kassert!(shown);
        kassert!(refused_with(format(), "stuck in a control request"));
        STATUS.store(NEEDS_RESET, Ordering::Relaxed);
        bind_made_up(&STATUS, Activity::Playing { id, stop: false, progress_ns: now })?;
        kassert!(refused_with(format_at(now + STUCK_NS + 1), "status 0x4f: DEVICE_NEEDS_RESET"));

        // Broken while idle: the next sound fails at once.
        bind_made_up(&STATUS, Activity::Idle)?;
        kassert!(refused_with(play(tone()?), "status 0x4f"));
        STATUS.store(0x0f, Ordering::Relaxed);

        // A source at another rate.
        bind_made_up(&STATUS, Activity::Idle)?;
        let other = Rate::new(8000).ok_or("rate")?;
        let slow = lumen_synth::beep(Freq::from_hz(440), Duration::from_millis(10), other).map_err(|_| "tone")?;
        kassert_eq!(play(Box::new(slow)).map(drop), Err(SoundError::WrongRate { source: other, device: rate }));
        kassert!(none_waiting());
        Ok(())
    }
);

crate::ktest!(
    fn an_idle_device_given_up_settles_the_sound_waiting() {
        // (#135) A sound waits (no thread takes it), then the made-up idle
        // device's status shows it broken. Whichever look gives the player
        // up, `play`, `sound` (`table`) or `format`, the waiting sound
        // settles failed with the status, nothing is left waiting, and a
        // later `play` is refused without leaving one or changing it.
        use crate::{kassert, kassert_eq};
        static STATUS: AtomicU8 = AtomicU8::new(0x0f);
        let _unbind = Unbind;
        let reset = "status 0x4f: DEVICE_NEEDS_RESET";
        for way in ["play", "sound", "format"] {
            STATUS.store(0x0f, Ordering::Relaxed);
            let rate = bind_made_up(&STATUS, Activity::Idle)?;
            let sound = play(made_up_tone(rate)?).map_err(|_| "play")?;
            kassert_eq!(sound.outcome(), None);
            STATUS.store(NEEDS_RESET, Ordering::Relaxed);
            let refused = match way {
                "play" => refused_with(play(made_up_tone(rate)?), reset),
                "sound" => matches!(&table().rows()[0][0], Value::Text(s) if s.contains(reset)),
                "format" => refused_with(format(), reset),
                _ => unreachable!(),
            };
            let settled = sound.outcome();
            if !(refused && failed_with(&sound, reset)) {
                crate::ktest::diag(format_args!(
                    "given up by `{way}`: refused {refused}, the sound waiting {settled:?}"
                ));
            }
            kassert!(refused);
            kassert!(failed_with(&sound, reset));
            kassert!(none_waiting());
            kassert!(refused_with(play(made_up_tone(rate)?), reset));
            kassert!(none_waiting());
            kassert_eq!(sound.outcome(), settled);
        }
        Ok(())
    }
);

crate::ktest!(
    fn a_stuck_thread_given_up_settles_the_sound_waiting_behind_it() {
        // (#135) Playing (the made-up thread never answers), a sound
        // waiting behind it, then a look over `STUCK_NS` later: the player
        // is given up while the thread is stuck, which may never take the
        // sound, so the give-up settles it. A later `play` is refused
        // without leaving a sound waiting. When the thread comes back and
        // fails with another reason (`fail`), the stuck one stays and the
        // other is warned about; failing with the stuck one warns nothing.
        // (Progress is set an hour ahead, so `play` reading the real clock
        // can't find it stuck however long the guest stalls.)
        use crate::{kassert, kassert_eq};
        static STATUS: AtomicU8 = AtomicU8::new(0x0f);
        let _unbind = Unbind;
        let ahead = crate::time::now_ns() + 3_600_000_000_000;
        let id = SoundId(NonZeroU64::MIN);
        let rate = bind_made_up(&STATUS, Activity::Playing { id, stop: false, progress_ns: ahead })?;
        let sound = play(made_up_tone(rate)?).map_err(|_| "play")?;
        kassert_eq!(sound.outcome(), None);
        let stuck = "stuck in a control request";
        kassert!(refused_with(format_at(ahead + STUCK_NS + 1), stuck));
        kassert!(failed_with(&sound, stuck));
        kassert!(none_waiting());
        let settled = sound.outcome();
        kassert!(refused_with(play(made_up_tone(rate)?), stuck));
        kassert!(none_waiting());
        kassert_eq!(sound.outcome(), settled);

        // The thread comes back with another error: the stuck reason stays,
        // for the sound it played too, and the other is warned about.
        let after_give_up = "a sound failed after the device was given up";
        let warned = logged(after_give_up)?;
        let cell: OutcomeCell = Arc::new(SpinLock::new(None));
        let kept = fail("y", Played::default(), Settle::new(id, cell.clone()));
        kassert!(kept.contains(stuck));
        kassert!(matches!(cell.lock().clone(), Some(Outcome::Failed(why)) if why == kept));
        kassert_eq!(sound.outcome(), settled);
        let line = format!("WARN  sound: {after_give_up} ({kept}): y");
        kassert_eq!(logged(&line)?, 1);
        kassert_eq!(logged(after_give_up)?, warned + 1);
        // With the stuck reason itself: nothing new to warn about.
        let again = fail(&kept, Played::default(), Settle::new(id, Arc::new(SpinLock::new(None))));
        kassert_eq!(again, kept);
        kassert_eq!(logged(after_give_up)?, warned + 1);
        Ok(())
    }
);

crate::ktest!(
    fn the_thread_settles_a_sound_left_waiting_on_a_given_up_player() {
        // (#135) The backstop. A normal give-up leaves nothing waiting, so
        // the thread finding the player given up logs no error. Then, on a
        // hand-built state no give-up leaves (it bypasses `Bound::give_up`),
        // a player failed with a sound still waiting: the thread settles
        // that sound failed with the reason, and logs an error.
        use crate::{kassert, kassert_eq};
        static STATUS: AtomicU8 = AtomicU8::new(0x0f);
        let _unbind = Unbind;
        let left = "left waiting on the given-up player (a bug)";
        let errors = logged(left)?;
        let rate = bind_made_up(&STATUS, Activity::Failed("w".into()))?;
        kassert!(matches!(take_next(), Some(Next::GiveUp(why)) if why == "w"));
        kassert_eq!(logged(left)?, errors);

        bind_made_up(&STATUS, Activity::Idle)?;
        let sound = play(made_up_tone(rate)?).map_err(|_| "play")?;
        if let Some(b) = PLAYER.lock().bound.as_mut() {
            b.activity = Activity::Failed("x".into());
        }
        kassert!(matches!(take_next(), Some(Next::GiveUp(why)) if why == "x"));
        kassert_eq!(sound.outcome(), Some(Outcome::Failed("x".into())));
        kassert!(none_waiting());
        kassert_eq!(logged(&format!("ERROR sound: sound {}: {left}", sound.id.0))?, 1);
        kassert_eq!(logged(left)?, errors + 1);
        Ok(())
    }
);

crate::ktest!(
    fn a_settle_dropped_unsettled_loses_its_sound() {
        // (No device.) A `Settle` dropped without settling (a bug) settles
        // its sound lost and logs an error naming it, but never overwrites
        // a settled outcome; an explicit settle stays as it was. `play`
        // tells a lost sound from a device failure.
        use crate::kassert_eq;
        let lost = "dropped without an outcome (a bug)";
        let errors = logged(lost)?;
        let id = SoundId(NonZeroU64::new(7).ok_or("id")?);
        let cell: OutcomeCell = Arc::new(SpinLock::new(None));
        drop(Settle::new(id, cell.clone()));
        kassert_eq!(cell.lock().clone(), Some(Outcome::Lost));
        kassert_eq!(logged(&format!("ERROR sound: sound 7: {lost}"))?, 1);
        let played: OutcomeCell = Arc::new(SpinLock::new(Some(Outcome::Played)));
        drop(Settle::new(id, played.clone()));
        kassert_eq!(played.lock().clone(), Some(Outcome::Played));
        let stopped: OutcomeCell = Arc::new(SpinLock::new(None));
        Settle::new(id, stopped.clone()).settle(Outcome::Stopped);
        kassert_eq!(stopped.lock().clone(), Some(Outcome::Stopped));
        kassert_eq!(logged(lost)?, errors + 1);
        Ok(())
    }
);

crate::ktest!(
    fn a_waiting_sound_dropped_under_the_players_lock_is_lost() {
        // `Settle`'s drop under `PLAYER`'s lock (as a `Refusal` dropped
        // there would): no deadlock, the outcome lost. A `GivenUp` from a
        // look that skips `refusal` (a bug) loses its waiting sound the
        // same way rather than leaving it unsettled.
        use crate::{kassert, kassert_eq};
        static STATUS: AtomicU8 = AtomicU8::new(0x0f);
        let _unbind = Unbind;
        let cell: OutcomeCell = Arc::new(SpinLock::new(None));
        {
            let _p = PLAYER.lock();
            drop(Settle::new(SoundId(NonZeroU64::MIN), cell.clone()));
        }
        kassert_eq!(cell.lock().clone(), Some(Outcome::Lost));

        let rate = bind_made_up(&STATUS, Activity::Idle)?;
        let sound = play(made_up_tone(rate)?).map_err(|_| "play")?;
        STATUS.store(NEEDS_RESET, Ordering::Relaxed);
        let refused = PLAYER.lock().check(crate::time::now_ns()).is_err();
        STATUS.store(0x0f, Ordering::Relaxed);
        kassert!(refused);
        kassert_eq!(sound.outcome(), Some(Outcome::Lost));
        kassert!(none_waiting());
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
