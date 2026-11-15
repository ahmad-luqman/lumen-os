//! Sound (#99): virtio-sound through the virtio adapter, playing
//! `lumen-synth` sources, and the `beep`, `play`, `sound` and `volume`
//! commands (docs/ARCHITECTURE.md, section 7, "Sound").
//!
//! - **One owner.** The probe sets the device up (its control requests spin
//!   unbounded in the crate: a known limit, done once there) and hands the
//!   `VirtIOSound` to the `sound` thread, the only code that touches it
//!   from then on, so no lock is ever held across a device wait.
//! - **Requests.** [`play`] puts a boxed [`Source`] in [`PLAYER`]'s slot and
//!   wakes the thread; it returns the sound's id at once. A newer sound
//!   replaces the one playing (faded out by [`Source::stop`]) and one still
//!   waiting to start. [`stop`] fades a sound out, [`finished`] says whether
//!   it is over.
//! - **Paced by the device.** For each sound the thread prepares and starts
//!   the stream, keeps up to [`DEPTH`] periods queued (`pcm_xfer_nb`), and
//!   queues another each time the device takes one back (`pcm_xfer_ok`; QEMU
//!   returns a period once its audio backend has consumed it). Between
//!   looks it sleeps ([`POLL_NS`], `StatusHandle::sleep_until`), bounded per
//!   period and ending on `DEVICE_NEEDS_RESET`/`FAILED`, so a melody
//!   doesn't keep a CPU busy. Once the source has ended (its last period
//!   padded with silence) and the queue has drained, it stops and releases
//!   the stream. #96 replaces the sleeps with the device's interrupt.
//! - **Failure.** A wait that gives up, or a request the device refuses,
//!   ends the thread and drops the device (resetting it; the runs still
//!   posted leak): `sound` shows `failed: <reason>`, and `beep` and `play`
//!   say so.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use core::fmt::{self, Write};
use core::num::NonZeroU8;
use core::ops::RangeInclusive;
use core::sync::atomic::{AtomicU8, Ordering};
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
/// stuck: far more than the [`DEPTH`] periods ahead of it.
const PERIOD_WAIT_NS: u64 = 2_000_000_000;
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

    /// Bytes in a period.
    fn period_bytes(&self) -> usize {
        self.period_frames() * usize::from(self.channels.get()) * 2
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

/// The sample rates the driver asks for, best first: 44.1 kHz is QEMU's
/// wav and most hosts' own rate (no resampling), then 48 kHz, then lower
/// ones (enough for beeps), then higher.
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

/// The format to play `stream` in, from what it supports: signed 16-bit
/// samples (what `lumen-synth` makes), stereo if it takes 2 channels, else
/// mono, and the first of [`RATES`] it has. `Err` says what is missing.
fn choose(
    stream: u32,
    rates: PcmRates,
    formats: PcmFormats,
    channels: RangeInclusive<u8>,
) -> Result<Format, &'static str> {
    if !formats.contains(PcmFormats::S16) {
        return Err("its output stream doesn't take signed 16-bit samples");
    }
    let channels = [2, 1]
        .into_iter()
        .find(|c| channels.contains(c))
        .and_then(NonZeroU8::new)
        .ok_or("its output stream takes neither 1 nor 2 channels")?;
    let &(_, pcm_rate, hz) = RATES
        .iter()
        .find(|(bit, ..)| rates.contains(*bit))
        .ok_or("its output stream has no sample rate Lumen plays")?;
    Ok(Format { stream, rate: Rate::new(hz).expect("RATES aren't 0"), channels, pcm_rate })
}

/// Where the device is, and how it plays.
#[derive(Clone, Copy, Debug)]
struct Device {
    addr: PciAddr,
    format: Format,
}

/// What the driver is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
enum State {
    /// No device bound (yet).
    Absent,
    Idle,
    Playing,
    /// The device failed while playing: why.
    Failed(String),
}

/// The state shared between [`play`]'s callers and the `sound` thread.
struct Player {
    device: Option<Device>,
    state: State,
    /// The sound waiting to start, and its id.
    next: Option<(u64, Box<dyn Source + Send>)>,
    /// The last id handed out (ids start at 1).
    last_id: u64,
    /// Every sound up to this id is over (played, stopped or replaced).
    done: u64,
    /// Sounds up to this id are to stop.
    stop_through: u64,
    /// Sounds that have ended, frames of them played, and how often the
    /// queue ran dry before a sound's end (the device had nothing to play).
    played: u64,
    frames: u64,
    underruns: u64,
}

static PLAYER: SpinLock<Player> = SpinLock::new(Player {
    device: None,
    state: State::Absent,
    next: None,
    last_id: 0,
    done: 0,
    stop_through: 0,
    played: 0,
    frames: 0,
    underruns: 0,
});

/// The `sound` thread waits here for [`Player::next`].
static WAKE: WaitQueue = WaitQueue::new();

/// The volume new sounds get, in percent.
static VOLUME: AtomicU8 = AtomicU8::new(DEFAULT_VOLUME);

/// Why a sound can't play.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SoundError {
    /// No sound device is bound: the hint says what was found instead.
    NoDevice(String),
    /// The device failed: why.
    Failed(String),
    /// A frequency the device's sample rate can't carry.
    TooHigh(TooHigh),
}

impl fmt::Display for SoundError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SoundError::NoDevice(hint) => write!(f, "no sound device{hint}"),
            SoundError::Failed(why) => write!(f, "the sound device failed: {why}"),
            SoundError::TooHigh(e) => write!(f, "{e}"),
        }
    }
}

/// What a missing device's message adds: the sound function `lspci` lists
/// without a working driver, if there is one.
fn no_device_hint() -> String {
    crate::pci::devices()
        .iter()
        .find(|d| is_sound(d))
        .and_then(|d| crate::pci::binding(d.addr).map(|b| format!(" ({}: {b})", d.addr)))
        .unwrap_or_default()
}

/// The format sounds must be made in (their rate), or why none can play.
pub fn format() -> Result<Format, SoundError> {
    let p = PLAYER.lock();
    match (&p.state, p.device) {
        (State::Failed(why), _) => Err(SoundError::Failed(why.clone())),
        (_, Some(d)) => Ok(d.format),
        (_, None) => {
            drop(p);
            Err(SoundError::NoDevice(no_device_hint()))
        }
    }
}

/// Play `source` (made at [`format`]'s rate): it replaces whatever plays or
/// waits to. Returns its id at once, for [`stop`] and [`finished`].
pub fn play(source: Box<dyn Source + Send>) -> Result<u64, SoundError> {
    format()?;
    let mut p = PLAYER.lock();
    p.last_id += 1;
    let id = p.last_id;
    let replaced = p.next.replace((id, source));
    drop(p);
    // (Dropped outside the lock.)
    drop(replaced);
    WAKE.wake_one();
    Ok(id)
}

/// Fade sound `id` out (within its release time; the periods already
/// queued still play), or keep it from starting.
pub fn stop(id: u64) {
    let mut p = PLAYER.lock();
    p.stop_through = p.stop_through.max(id);
}

/// Whether sound `id` is over (or will never play: the device failed).
pub fn finished(id: u64) -> bool {
    PLAYER.lock().done >= id
}

/// The volume new sounds get, in percent.
pub fn volume() -> u8 {
    VOLUME.load(Ordering::Relaxed)
}

/// Set the volume new sounds get (more than 100% is 100%).
pub fn set_volume(percent: u8) {
    VOLUME.store(percent.min(100), Ordering::Relaxed);
}

/// A square-wave beep of `hz` for `ms`, at the current volume; returns at
/// once (see [`play`]).
pub fn beep(hz: u32, ms: u32) -> Result<u64, SoundError> {
    let f = format()?;
    let mut tone = lumen_synth::beep(Freq::from_hz(hz), Duration::from_millis(u64::from(ms)), f.rate)
        .map_err(SoundError::TooHigh)?;
    tone.set_volume(volume());
    play(Box::new(tone))
}

fn probe(res: DeviceResources) -> Result<(), ProbeError> {
    let transport = virtio::transport(res)?;
    let status = transport.status();
    let addr = transport.info().addr;
    let mut dev = VirtIOSound::<VirtioHal, VirtioTransport>::new(transport).map_err(ProbeError::Device)?;
    status.check_ready()?;
    let format = set_up(&mut dev)?;
    // Detached: it runs for the kernel's life (or until the device fails).
    let _thread = crate::sched::spawn("sound", move || run(dev, status, format))
        .map_err(|e| ProbeError::Unsupported(format!("no thread to play on: {e}")))?;
    let mut p = PLAYER.lock();
    p.device = Some(Device { addr, format });
    p.state = State::Idle;
    drop(p);
    info!(target: "sound", "{addr}: {format}");
    Ok(())
}

/// Pick the first output stream and a format for it ([`choose`]), and set
/// its parameters: control requests, each an unbounded spin in the crate.
fn set_up(dev: &mut VirtIOSound<VirtioHal, VirtioTransport>) -> Result<Format, ProbeError> {
    let streams = dev.output_streams().map_err(ProbeError::Device)?;
    let &stream = streams.first().ok_or_else(|| ProbeError::Unsupported("no output stream".into()))?;
    let rates = dev.rates_supported(stream).map_err(ProbeError::Device)?;
    let formats = dev.formats_supported(stream).map_err(ProbeError::Device)?;
    let channels = dev.channel_range_supported(stream).map_err(ProbeError::Device)?;
    let format = choose(stream, rates, formats, channels).map_err(|why| ProbeError::Unsupported(why.into()))?;
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
    .map_err(ProbeError::Device)?;
    Ok(format)
}

/// Why playing a sound failed (the device is given up).
#[derive(Debug)]
enum PlayError {
    Device(virtio_drivers::Error),
    Wait(WaitError),
}

impl fmt::Display for PlayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlayError::Device(e) => write!(f, "virtio: {e}"),
            PlayError::Wait(e) => write!(f, "{e}"),
        }
    }
}

impl From<virtio_drivers::Error> for PlayError {
    fn from(e: virtio_drivers::Error) -> PlayError {
        PlayError::Device(e)
    }
}

/// The `sound` thread: play each sound handed over, until the device fails.
fn run(mut dev: VirtIOSound<VirtioHal, VirtioTransport>, status: StatusHandle, format: Format) {
    let mut period = vec![0i16; format.period_frames() * usize::from(format.channels.get())];
    loop {
        WAKE.wait_until(|| PLAYER.lock().next.is_some());
        let (id, source) = {
            let mut p = PLAYER.lock();
            let Some((id, source)) = p.next.take() else { continue };
            // The ones before it were played or replaced.
            p.done = p.done.max(id - 1);
            p.state = State::Playing;
            (id, source)
        };
        let outcome = play_one(&mut dev, status, format, id, source, &mut period);
        let mut p = PLAYER.lock();
        match outcome {
            Ok(played) => {
                p.done = p.done.max(id);
                p.played += 1;
                p.frames += played.frames;
                p.underruns += played.underruns;
                p.state = State::Idle;
            }
            Err(e) => {
                let why = e.to_string();
                p.done = u64::MAX;
                p.state = State::Failed(why.clone());
                let next = p.next.take();
                drop(p);
                drop(next);
                warn!(target: "sound", "playing failed: {why}; the device is given up");
                // Dropping it resets the device (`VirtioTransport`'s `Drop`).
                return;
            }
        }
    }
}

/// What one sound played.
struct Played {
    frames: u64,
    underruns: u64,
}

/// Whether sound `id` is to stop: asked to, or replaced by a newer one.
fn should_stop(id: u64) -> bool {
    let p = PLAYER.lock();
    p.stop_through >= id || p.next.is_some()
}

/// Play `source` on the stream, paced by the device (the module doc).
fn play_one(
    dev: &mut VirtIOSound<VirtioHal, VirtioTransport>,
    status: StatusHandle,
    format: Format,
    id: u64,
    mut source: Box<dyn Source + Send>,
    period: &mut [i16],
) -> Result<Played, PlayError> {
    let stream = format.stream;
    let mut played = Played { frames: 0, underruns: 0 };
    let mut queued: VecDeque<u16> = VecDeque::with_capacity(DEPTH);
    let mut stopping = false;
    let mut ended = false;
    dev.pcm_prepare(stream)?;
    dev.pcm_start(stream)?;
    loop {
        if !stopping && should_stop(id) {
            source.stop();
            stopping = true;
        }
        while !ended && queued.len() < DEPTH {
            let filled = source.fill(period, format.channels);
            ended = filled.ended;
            if filled.frames == 0 {
                break;
            }
            played.frames += filled.frames as u64;
            queued.push_back(dev.pcm_xfer_nb(stream, period.as_bytes())?);
        }
        let Some(&oldest) = queued.front() else { break };
        // Wait for the oldest period, then take back any others done.
        let mut refused = None;
        status
            .sleep_until(PERIOD_WAIT_NS, POLL_NS, || match dev.pcm_xfer_ok(oldest) {
                Ok(()) => true,
                Err(virtio_drivers::Error::NotReady) => false,
                Err(e) => {
                    refused = Some(e);
                    true
                }
            })
            .map_err(PlayError::Wait)?;
        if let Some(e) = refused {
            return Err(e.into());
        }
        queued.pop_front();
        while let Some(&token) = queued.front() {
            match dev.pcm_xfer_ok(token) {
                Ok(()) => queued.pop_front(),
                Err(virtio_drivers::Error::NotReady) => break,
                Err(e) => return Err(e.into()),
            };
        }
        if queued.is_empty() && !ended {
            played.underruns += 1;
        }
    }
    dev.pcm_stop(stream)?;
    dev.pcm_release(stream)?;
    Ok(played)
}

// -- commands ---------------------------------------------------------------

/// `beep [hz] [ms]`: a square-wave beep (default 440 Hz for 200 ms); returns
/// while it plays.
pub fn beep_command(ctx: &mut Ctx<'_>, args: &[String]) {
    const USAGE: &str = "usage: beep [hz] [ms] (default 440 200; ms up to 60000)";
    let number = |i: usize, default: u32| args.get(i).map_or(Some(default), |a| a.parse::<u32>().ok());
    let (Some(hz), Some(ms)) = (number(0, 440), number(1, 200)) else {
        return ctx.fail(USAGE);
    };
    if args.len() > 2 || ms > MAX_BEEP_MS {
        return ctx.fail(USAGE);
    }
    if let Err(e) = beep(hz, ms) {
        ctx.fail(format_args!("beep: {e}"));
    }
}

/// `play <melody>`: play a melody (its words joined with spaces), until it
/// ends or `q`/Ctrl-C stops it.
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
    let id = match play(Box::new(seq)) {
        Ok(id) => id,
        Err(e) => return ctx.fail(format_args!("play: {e}")),
    };
    let mut stopped = false;
    while !finished(id) {
        if !stopped && let Some(Key::Char('q') | Key::Ctrl('c')) = ctx.poll_key() {
            stop(id);
            stopped = true;
        }
        crate::time::sleep(KEY_POLL_NS);
    }
    if let State::Failed(why) = PLAYER.lock().state.clone() {
        ctx.fail(format_args!("play: the sound device failed: {why}"));
    } else if stopped {
        let _ = writeln!(ctx.out, "play: stopped");
    }
}

/// `volume [percent]`: the volume new sounds get, or set it.
pub fn volume_command(ctx: &mut Ctx<'_>, args: &[String]) {
    match args {
        [] => {}
        [p] => match p.trim_end_matches('%').parse::<u8>() {
            Ok(p) if p <= 100 => set_volume(p),
            _ => return ctx.fail("usage: volume [0-100]"),
        },
        _ => return ctx.fail("usage: volume [0-100]"),
    }
    let _ = writeln!(ctx.out, "volume: {}%", volume());
}

/// `sound`: the device, its format and what it played (in a pipeline,
/// records: [`table`]).
pub fn sound_command(ctx: &mut Ctx<'_>, args: &[String]) {
    if !args.is_empty() {
        return ctx.fail("usage: sound");
    }
    let mut t = String::new();
    match PLAYER.lock().device {
        Some(d) => {
            let _ = writeln!(t, "sound: virtio sound at {}, {}", d.addr, d.format);
        }
        None => {
            let _ = writeln!(t, "sound: no sound device{}", no_device_hint());
            let _ = ctx.out.write_str(&t);
            return;
        }
    }
    table().render(&mut t);
    let _ = ctx.out.write_str(&t);
}

/// `sound`'s records: one row, none without a device.
pub fn table() -> pipe::Table {
    let p = PLAYER.lock();
    let row = p.device.map(|_| Row {
        state: match &p.state {
            State::Absent => String::from("absent"),
            State::Idle => String::from("idle"),
            State::Playing => String::from("playing"),
            State::Failed(why) => format!("failed: {why}"),
        },
        played: p.played,
        frames: p.frames,
        underruns: p.underruns,
    });
    drop(p);
    table_of(row)
}

/// What `sound`'s one record says.
struct Row {
    state: String,
    played: u64,
    frames: u64,
    underruns: u64,
}

/// `sound`'s records: `row`, if there is one, at the current volume.
fn table_of(row: Option<Row>) -> pipe::Table {
    const COLUMNS: [Column; 5] = [
        Column::new("state", Kind::Text, Align::Left, 7),
        Column::new("played", Kind::Int, Align::Right, 6),
        Column::new("frames", Kind::Int, Align::Right, 9),
        Column::new("underruns", Kind::Int, Align::Right, 9),
        Column::new("volume", Kind::Percent, Align::Right, 6),
    ];
    let mut t = pipe::Table::new(&COLUMNS);
    if let Some(r) = row {
        t.push(vec![
            Value::Text(r.state),
            int(r.played),
            int(r.frames),
            int(r.underruns),
            Value::Int(i64::from(volume())),
        ]);
    }
    t
}

/// A note for `sound`'s records when there are none.
pub fn records_note() -> Option<String> {
    PLAYER.lock().device.is_none().then(|| format!("sound: no sound device{}", no_device_hint()))
}

fn int(n: u64) -> Value {
    i64::try_from(n).map_or(Value::Absent("-"), Value::Int)
}

crate::ktest!(
    fn the_record_fits_its_columns() {
        // (The ktest machine has no sound device: `table()` has no row.)
        use crate::kassert_eq;
        let row = Row { state: "failed: x".into(), played: 1, frames: u64::MAX, underruns: 0 };
        let t = table_of(Some(row));
        kassert_eq!(t.rows().len(), 1);
        kassert_eq!(t.rows()[0][2], Value::Absent("-"));
        kassert_eq!(table_of(None).rows().len(), 0);
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
        kassert_eq!((f.period_frames(), f.period_bytes()), (882, 3528));
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
            Err("its output stream doesn't take signed 16-bit samples")
        );
        kassert_eq!(choose(0, all, s16, 3..=8), Err("its output stream takes neither 1 nor 2 channels"));
        kassert_eq!(
            choose(0, all, s16, RangeInclusive::new(2, 1)),
            Err("its output stream takes neither 1 nor 2 channels")
        );
        kassert_eq!(
            choose(0, PcmRates::RATE_5512 | PcmRates::RATE_384000, s16, 1..=2),
            Err("its output stream has no sample rate Lumen plays")
        );
        Ok(())
    }
);
