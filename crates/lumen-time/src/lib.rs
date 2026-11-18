//! Timekeeping arithmetic for the Lumen kernel: working out a counter's
//! frequency from a reference clock, converting ticks to and from
//! nanoseconds, the counts the PIT and the local APIC timer are programmed
//! with, how times read in logs (`Stamp`, `Millis`), the rules a
//! calibration window must meet to be trusted (`Window`, `steadiest_windows`
//! with how far off a median of them can be, and the verdict on it against a
//! clock's tolerance),
//! how a PIT channel's two readings become a window (`pit_window`), one
//! clock's reading placed between two reads of another and retaken while too
//! wide (`Bracketed`, `first_narrow`: the ktests' clock cross-checks), busy
//! percentages and rates (`busy_percent`, `percent`, `per_second`), a
//! bootloader's microseconds back as ticks (`ticks_at_usec`), Limine's boot
//! times against the timeline's origin ([`handoff`]), and the
//! CMOS RTC's registers as a date ([`rtc`]). All of it is integer math
//! (the kernel has no floating point) in `u128` where products can
//! overflow, so it holds for centuries of uptime (and then saturates).

#![no_std]

use core::fmt;

pub mod handoff;
pub mod rtc;

pub const NS_PER_SEC: u64 = 1_000_000_000;
/// The PIT's input clock.
pub const PIT_HZ: u64 = 1_193_182;
const FS_PER_SEC: u64 = 1_000_000_000_000_000;
/// The longest HPET tick the spec allows: 100 ns.
const HPET_MAX_PERIOD_FS: u32 = 100_000_000;

/// The frequency of a counter that counted `ticks` while a reference clock
/// of `ref_hz` counted `ref_ticks`. `None` if the reference didn't move, or
/// the result is 0 or doesn't fit.
pub fn frequency(ticks: u64, ref_ticks: u64, ref_hz: u64) -> Option<u64> {
    if ref_ticks == 0 {
        return None;
    }
    let hz = u128::from(ticks) * u128::from(ref_hz) / u128::from(ref_ticks);
    u64::try_from(hz).ok().filter(|&hz| hz > 0)
}

/// The HPET's frequency from the period in its capabilities register
/// (femtoseconds per tick). `None` for 0 or a period over the spec's 100 ns.
pub fn hpet_hz(period_fs: u32) -> Option<u64> {
    (1..=HPET_MAX_PERIOD_FS).contains(&period_fs).then(|| FS_PER_SEC / u64::from(period_fs))
}

/// The PIT count closest to `ns`; `None` if it is 0 or over 16 bits (about
/// 54.9 ms).
pub fn pit_count(ns: u64) -> Option<u16> {
    let count = (u128::from(ns) * u128::from(PIT_HZ) + u128::from(NS_PER_SEC / 2)) / u128::from(NS_PER_SEC);
    u16::try_from(count).ok().filter(|&c| c > 0)
}

/// The initial count of a periodic timer whose input (after any divider)
/// runs at `timer_hz`, to fire `rate` times a second. `None` if that is 0
/// or over 32 bits.
pub fn periodic_count(timer_hz: u64, rate: u64) -> Option<u32> {
    if rate == 0 {
        return None;
    }
    u32::try_from((timer_hz + rate / 2) / rate).ok().filter(|&c| c > 0)
}

/// Ticks from `earlier` to `later`, two readings of a counter `mask` wide
/// (`u64::MAX`, or `0xffff_ffff` for a 32-bit counter): correct across one
/// wrap.
pub fn counter_delta(earlier: u64, later: u64, mask: u64) -> u64 {
    later.wrapping_sub(earlier) & mask
}

/// The midpoint of two readings of a counter `mask` wide, `earlier` then
/// `later` (less than one wrap apart; rounded down): where a reading of
/// another clock taken between them is placed.
fn counter_midpoint(earlier: u64, later: u64, mask: u64) -> u64 {
    earlier.wrapping_add(counter_delta(earlier, later, mask) / 2) & mask
}

/// A reading of one clock (`value`) taken between two reads of another (the
/// bracket), placed at their midpoint: `at` and `width` are in the bracketing
/// clock's units (its counter value, and the time between its two reads).
/// Placed at `at`, the reading is off by at most half of `width`, rounded up.
/// Anything between the two reads widens it: a host stalling the vCPU, a slow
/// emulated read (under KVM each HPET read exits to QEMU), or the bracketing
/// clock itself jumping, so a wide one is evidence of something wrong, not a
/// reading to trust. (A [`Window`]'s `start_gap` and `end_gap` are the same
/// widths, for a calibration's two ends.) Built by [`Bracketed::between`] or
/// [`Bracketed::checked_between`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Bracketed<T> {
    pub value: T,
    pub at: u64,
    pub width: u64,
}

/// A clock that doesn't wrap read going backwards across a bracket: `after`
/// is before `before`. A clock bug, not a stall.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Backwards {
    pub before: u64,
    pub after: u64,
}

impl<T> Bracketed<T> {
    /// `value`, read between `before` and `after`, two reads of a counter
    /// `mask` wide (`u64::MAX` or `0xffff_ffff`) that may wrap once between
    /// them.
    pub fn between(before: u64, value: T, after: u64, mask: u64) -> Self {
        Bracketed { value, at: counter_midpoint(before, after, mask), width: counter_delta(before, after, mask) }
    }

    /// `value`, read between `before` and `after`, two reads of a clock that
    /// doesn't wrap (such as `now_ns`); [`Backwards`] if `after` is before
    /// `before`.
    pub fn checked_between(before: u64, value: T, after: u64) -> Result<Self, Backwards> {
        if after < before {
            return Err(Backwards { before, after });
        }
        Ok(Self::between(before, value, after, u64::MAX))
    }
}

/// How [`first_narrow`] went: the first reading at most its limit wide
/// ([`reading`](Self::reading)), and the widths of all the readings it took,
/// in order (the wide ones too, so a stall stays visible).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NarrowReading<T, const N: usize> {
    reading: Option<Bracketed<T>>,
    widths: [u64; N],
    taken: usize,
}

impl<T: Copy, const N: usize> NarrowReading<T, N> {
    /// The first narrow reading; `None` if every one of the `N` taken was
    /// wider than the limit.
    pub fn reading(&self) -> Option<Bracketed<T>> {
        self.reading
    }

    /// The widths of the readings taken, in order.
    pub fn widths(&self) -> &[u64] {
        &self.widths[..self.taken]
    }

    /// How many readings were taken again because the one before was too
    /// wide: at most `N - 1`.
    pub fn retries(&self) -> usize {
        self.taken.saturating_sub(1)
    }
}

/// Up to `N` readings from `take`, until one is at most `limit` wide (in the
/// bracketing clock's units). An error from `take` (a clock going backwards)
/// is returned at once: it is a bug to report, not a stall to retake.
pub fn first_narrow<T, E, const N: usize>(
    limit: u64,
    mut take: impl FnMut() -> Result<Bracketed<T>, E>,
) -> Result<NarrowReading<T, N>, E> {
    let mut out = NarrowReading { reading: None, widths: [0; N], taken: 0 };
    while out.taken < N {
        let r = take()?;
        out.widths[out.taken] = r.width;
        out.taken += 1;
        if r.width <= limit {
            out.reading = Some(r);
            break;
        }
    }
    Ok(out)
}

/// How far off a placement test's try can be, in the bracketing clock's
/// units: half the reference reading's width, plus half of what a stall added
/// to the paused reading's (anything over the `2 * pause` its two pauses
/// make, which can move the read that far off its midpoint), rounded up.
/// Saturates.
pub fn placement_uncertainty(reference_width: u64, paused_width: u64, pause: u64) -> u64 {
    let stalled = paused_width.saturating_sub(pause.saturating_mul(2));
    reference_width.saturating_add(stalled).div_ceil(2)
}

/// Whether a placement test's try is judged: its uncertainty
/// ([`placement_uncertainty`]) and the reference's width are each at most a
/// quarter of the pause. Then a right placement is at most `pause / 4` off,
/// and one at either end of its bracket (or outside it) at least
/// `3 * pause / 4` even with the reference misplaced the same way, either
/// side of the `pause / 2` the tests allow.
pub fn placement_is_judged(reference_width: u64, paused_width: u64, pause: u64) -> bool {
    reference_width.saturating_mul(4) <= pause
        && placement_uncertainty(reference_width, paused_width, pause).saturating_mul(4) <= pause
}

/// Ticks a down-counter that reloads to `reload` at 0 counted from reading
/// `earlier` to reading `later`: correct across one reload (two readings
/// less than a period apart).
pub fn down_counter_delta(earlier: u32, later: u32, reload: u32) -> u32 {
    if later <= earlier { earlier - later } else { earlier + (reload - later) }
}

/// Is `hz` a believable TSC frequency? A miscounted reference gives wild
/// values. Real TSCs run between tens of MHz and a few GHz; the bounds (10 MHz
/// to 100 GHz) are generous, so only a broken reference fails.
pub fn plausible_tsc_hz(hz: u64) -> bool {
    (10_000_000..=100_000_000_000).contains(&hz)
}

/// One window of the TSC measured against a reference clock: TSC ticks, the
/// reference's ticks over the same stretch, how often the reference was
/// polled, and how uncertain its start and end are, in TSC ticks. Each end is
/// a reading of the reference taken between two TSC reads; the gap between
/// them is how far off that end can be (a vCPU paused mid-reading widens it).
/// An end that can't be placed at all (a PIT counter that may have wrapped)
/// gets a gap of the whole window, so the window isn't steady.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Window {
    pub tsc: u64,
    pub reference: u64,
    pub polls: u64,
    pub start_gap: u64,
    pub end_gap: u64,
}

/// A window is steady if the gaps at its start and end add up to at most
/// 1/`STEADY` of it: 0.5%.
pub const STEADY: u64 = 200;

impl Window {
    /// Were both its ends read promptly enough to trust it (see [`STEADY`])?
    /// An empty window isn't. This is about the reads (their gaps' share of
    /// the window), the same for either rate taken from it: how far off a
    /// rate can be is the caller's bound ([`Window::tsc_error_ppm`],
    /// [`Window::gap_ppm`]), which ranks the unsteady ones and judges the
    /// median.
    pub fn is_steady(&self) -> bool {
        self.tsc > 0 && self.start_gap.saturating_add(self.end_gap).saturating_mul(STEADY) <= self.tsc
    }

    /// The TSC's frequency it gives against a reference counting at
    /// `reference_hz` (see [`frequency`]).
    pub fn tsc_hz(&self, reference_hz: u64) -> Option<u64> {
        frequency(self.tsc, self.reference, reference_hz)
    }

    /// Its gaps added up, as a share of it in parts per million, rounded
    /// up: the most the rate [`reference_hz`](Self::reference_hz) gives can be
    /// off, as a share of the true rate (high or low; the window's own error
    /// only, not that of the TSC it is counted against, nor the reference's
    /// last tick, which is under 2 ppm for the local APIC timer). For
    /// [`tsc_hz`](Self::tsc_hz), see [`tsc_error_ppm`](Self::tsc_error_ppm).
    /// `u64::MAX` for an empty window or gaps as long as it.
    pub fn gap_ppm(&self) -> u64 {
        let gaps = u128::from(self.start_gap) + u128::from(self.end_gap);
        if self.tsc == 0 || gaps >= u128::from(self.tsc) {
            return u64::MAX;
        }
        u64::try_from((gaps * 1_000_000).div_ceil(u128::from(self.tsc))).unwrap_or(u64::MAX)
    }

    /// The most the rate [`tsc_hz`](Self::tsc_hz) gives can be off, as a
    /// share of the true rate in parts per million, rounded up (the window's
    /// own error only, not that of the reference's frequency).
    ///
    /// The reference was read somewhere inside each end's gap, so the TSC
    /// ticks between its two reads, `S`, lie in `[T - end_gap, T + start_gap]`
    /// (`T` is `tsc`); and between those reads it ticked `R ± 1` times (`R` is
    /// `reference`: a counter's ticks, its last one partly elapsed). The rate
    /// it gives, `T / R`, against the true `S / (R ± 1)`, is off by at most
    /// `(T + end_gap·R) / ((T − end_gap)·R)` high, or
    /// `(T + start_gap·R) / ((T + start_gap)·R)` low. Unlike
    /// [`gap_ppm`](Self::gap_ppm) the end's gap counts for more than its share
    /// (the window it stretches is in the divisor), and the reference's tick
    /// adds `1/R`: 84 ppm for a 10 ms PIT window, 1 ppm for the HPET's.
    /// `u64::MAX` (no bound) for an empty window, an end gap as long as it,
    /// fewer than 2 reference ticks, or values so large the arithmetic would
    /// overflow 128 bits (real windows are about 2^25 TSC ticks against at
    /// most 2^20 reference ticks).
    pub fn tsc_error_ppm(&self) -> u64 {
        let (t, r) = (u128::from(self.tsc), u128::from(self.reference));
        let (s, e) = (u128::from(self.start_gap), u128::from(self.end_gap));
        if t == 0 || e >= t || r < 2 {
            return u64::MAX;
        }
        // ceil(1e6 · (t + gap·r) / (span·r)), or None on overflow.
        let ppm = |gap: u128, span: u128| {
            let num = gap.checked_mul(r)?.checked_add(t)?.checked_mul(1_000_000)?;
            Some(num.div_ceil(span.checked_mul(r)?))
        };
        match (ppm(e, t - e), t.checked_add(s).and_then(|span| ppm(s, span))) {
            (Some(high), Some(low)) => u64::try_from(high.max(low)).unwrap_or(u64::MAX),
            _ => u64::MAX,
        }
    }

    /// The other way round: the reference's frequency it gives against a
    /// TSC counting at `tsc_hz` (the local APIC timer's calibration, where
    /// the TSC is the known clock).
    pub fn reference_hz(&self, tsc_hz: u64) -> Option<u64> {
        frequency(self.reference, self.tsc, tsc_hz)
    }
}

impl fmt::Display for Window {
    /// `tsc/reference/polls/start_gap/end_gap`, in decimal.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}/{}/{}", self.tsc, self.reference, self.polls, self.start_gap, self.end_gap)
    }
}

/// Where a window measurement may be paused, for tests: a calibration passes
/// a no-op (or, in a stalled scenario, the kernel's `stall_window_ends`), a
/// test pauses there as a host descheduling the vCPU would. Each
/// end is a reading of the reference between two TSC reads, paused before or
/// after the reference is read (either widens that end's gap); `Started` and
/// `Ended` are only the PIT's (the HPET's first and last readings are its
/// start and end). The kernel's `now_ns`-against-HPET ktests pause at the
/// same four reading points, in [`Bracketed`] readings of the HPET between
/// two `now_ns` reads. In the kernel's local APIC timer windows the start
/// isn't a read but a write (the timer started): `StartBeforeRead` and
/// `StartAfterRead` are before and after that write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pause {
    /// After the PIT started counting, before its start reading. Harmless:
    /// the start is read, not assumed.
    Started,
    /// In the start reading, after the TSC read before it, before the
    /// reference is read.
    StartBeforeRead,
    /// In the start reading, after the reference is read (the PIT's latched),
    /// before the TSC read after it.
    StartAfterRead,
    /// After the PIT ran out, before its end reading. Harmless up to a limit:
    /// the reading says how far past the end it was, while
    /// `pit_end_unambiguous` can show it didn't wrap.
    Ended,
    /// In the end reading, after the TSC read before it, before the
    /// reference is read (the HPET reaches this on every poll).
    EndBeforeRead,
    /// In the end reading, after the reference is read (the PIT's latched),
    /// before the TSC read after it.
    EndAfterRead,
    /// In either PIT reading, after its second TSC read, while the count the
    /// latch froze is read. Harmless: outside the bracket, however slow the
    /// reads are (under KVM each is a VM exit).
    CountRead,
}

/// The unsteady windows a calibration took: how many, the first, the last,
/// and the shortest (by reference ticks) after the first. Each was followed
/// by another window, unless attempts ran out; [`steadiest_windows`] may
/// still keep some, to fill in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SetAside {
    pub count: usize,
    pub first: Option<Window>,
    pub last: Option<Window>,
    pub shortest_after_first: Option<Window>,
}

/// The most a median of `windows`' rates can be off, in parts per million
/// of the true rate, rounded up: the `(N / 2 + 1)`-th smallest of their own
/// bounds, from `bound` (the most the rate taken from a window can be off:
/// [`Window::tsc_error_ppm`] for the TSC's rate against a reference,
/// [`Window::gap_ppm`] for a reference's against the TSC). Those `N / 2 + 1`
/// windows, each within that much, are more than half of them, so the median
/// (for even `N`, the midpoint of the middle two) lies between two of theirs.
/// `u64::MAX` (no bound) if one of those has none, or for no windows.
pub fn median_bound_ppm<const N: usize>(windows: &[Window; N], bound: impl Fn(&Window) -> u64) -> u64 {
    let mut ppm = windows.map(|w| bound(&w));
    ppm.sort_unstable();
    ppm.get(N / 2).copied().unwrap_or(u64::MAX)
}

/// How long, in reference ticks, the next window should be after `window`
/// (taken `current` ticks long), to be steady with the same gaps, for a
/// reference whose reads were slow rather than paused (#139: a slowed KVM
/// host took 250 µs per HPET read, 40 polls in a 10 ms window where 700 to
/// 850 is normal). A gap is a read's latency, so it stays the same whatever
/// the window's length: the window grows until those gaps would be 0.4% of
/// it (`1 / (STEADY · 5/4)`, a fifth under [`STEADY`]'s 0.5%), never shorter
/// than `current` nor longer than `most`. A slow read slows every poll, but
/// a burst of stalls hits one read: each end's gap counts for at most two of
/// the window's mean poll intervals (`tsc / polls`, the polls being what
/// `tsc` spans), what slow reads explain, so one burst at either end barely
/// stretches it; and the gaps are a share of the TSC ticks the reference
/// counted over (`tsc + start_gap`: a stall before `tsc` starts still counts
/// in `reference`), so a stall at the start, however long, counts no more
/// than two slow reads (which, with few polls, may stretch it a little; a
/// window that also lost its CPU mid-way has fewer polls, so more). A
/// steady window keeps
/// `current`; so does an empty one, and one with no polls (nothing to tell a
/// slow read by). `most` if the arithmetic would overflow.
pub fn stretched_for_gaps(current: u64, window: &Window, most: u64) -> u64 {
    if window.is_steady() || window.tsc == 0 || window.polls == 0 {
        return current;
    }
    let two_polls = (u128::from(window.tsc) * 2).div_ceil(u128::from(window.polls));
    let gaps = u128::from(window.start_gap).min(two_polls) + u128::from(window.end_gap).min(two_polls);
    // The TSC ticks the reference counted over: from its start reading, so
    // with the start gap (a stall there counts in `reference`, not `tsc`).
    let span = u128::from(window.tsc) + u128::from(window.start_gap);
    // The reference ticks over which `gaps` (TSC ticks) are 1/(STEADY·5/4).
    let needed = u128::from(window.reference)
        .checked_mul(gaps)
        .and_then(|x| x.checked_mul(u128::from(STEADY) * 5))
        .map(|x| x.div_ceil(4 * span))
        .and_then(|x| u64::try_from(x).ok())
        .unwrap_or(most);
    needed.clamp(current, most.max(current))
}

/// How [`steadiest_windows`] went: `N` windows, the steady ones first (in the
/// order taken), then, if attempts ran out first, the unsteady ones with the
/// smallest bounds (least first, the earlier of two equal ones first); how
/// many are steady; and the unsteady ones taken (how many, and the last).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Steadiest<const N: usize> {
    windows: [Window; N],
    steady: usize,
    aside: SetAside,
}

impl<const N: usize> Steadiest<N> {
    /// The windows: the steady ones first, then any filling in.
    pub fn windows(&self) -> &[Window; N] {
        &self.windows
    }

    /// How many of [`windows`](Self::windows) are steady (the first ones).
    pub fn steady(&self) -> usize {
        self.steady
    }

    /// The unsteady windows taken (some may fill in).
    pub fn aside(&self) -> SetAside {
        self.aside
    }
}

/// `N` windows from `take`, at most `attempts` (at least `N`) in all: steady
/// ones, an unsteady one set aside, and `pause` called before each window
/// taken after an unsteady one (so retakes can be spaced past a burst of
/// stalls), given the fewest TSC ticks any window taken so far with a bound
/// spanned from its start reading (`tsc + start_gap`: a ruler for a TSC not
/// calibrated yet; `u64::MAX` before one; a window that can't be placed
/// doesn't count, and a PIT window whose start was read late spans only the
/// part it counted, so the ruler can come out short of the window by that). If attempts run out first, the rest are the unsteady windows whose
/// `bound` (the most the rate the caller takes from them can be off) was the
/// smallest, rather than none: the caller judges the median's
/// ([`median_bound_ppm`] with the same bound). An error from `take` is
/// returned at once.
pub fn steadiest_windows<E, const N: usize>(
    attempts: usize,
    mut take: impl FnMut() -> Result<Window, E>,
    bound: impl Fn(&Window) -> u64,
    mut pause: impl FnMut(u64),
) -> Result<Steadiest<N>, E> {
    debug_assert!(attempts >= N, "{attempts} attempts can't fill {N} windows");
    let mut out = Steadiest { windows: [Window::default(); N], steady: 0, aside: SetAside::default() };
    // The unsteady windows with the smallest bounds so far, least first.
    let (mut spare, mut spares) = ([Window::default(); N], 0);
    let (mut retake, mut shortest) = (false, u64::MAX);
    while out.steady < N && out.steady + out.aside.count < attempts {
        if retake {
            pause(shortest);
        }
        let w = take()?;
        if bound(&w) < NO_INFORMATION_PPM {
            shortest = shortest.min(w.tsc.saturating_add(w.start_gap));
        }
        retake = !w.is_steady();
        if !retake {
            out.windows[out.steady] = w;
            out.steady += 1;
            continue;
        }
        let a = out.aside;
        out.aside = SetAside {
            count: a.count + 1,
            first: a.first.or(Some(w)),
            last: Some(w),
            shortest_after_first: match (a.first, a.shortest_after_first) {
                (None, _) => None,
                (Some(_), Some(s)) if s.reference <= w.reference => Some(s),
                (Some(_), _) => Some(w),
            },
        };
        let at = spare[..spares].iter().position(|s| bound(&w) < bound(s)).unwrap_or(spares);
        if at < N {
            spares = (spares + 1).min(N);
            spare.copy_within(at..spares - 1, at + 1);
            spare[at] = w;
        }
    }
    let rest = N - out.steady;
    out.windows[out.steady..].copy_from_slice(&spare[..rest]);
    Ok(out)
}

/// A rate's bound at or past which it says nothing (100%): the middle
/// windows' stalls left no bound.
pub const NO_INFORMATION_PPM: u64 = 1_000_000;

/// How far off the TSC's boot calibration may be before boot warns, in ppm:
/// 1%, half of what its tightest consumers allow (the kernel checks `now_ns`
/// against the HPET, and the boot calibration against the PIT measured
/// again, each within 2%: half for this calibration, half for the other
/// clock's measurement).
pub const TSC_TOLERANCE_PPM: u64 = 10_000;

/// How far off the local APIC timer's boot calibration may be before boot
/// warns, in ppm, counting the TSC's error it inherits ([`compose_ppm`]): 2%.
/// Its tightest consumer, the kernel's 1 kHz check, holds the rate the timer
/// counts at (against the HPET) within 5% of its calibration: this leaves 3%
/// for that check's own one-shot windows. (#139's loaded KVM boot had 2 of
/// 20 windows steady and a bound of 0.7%, harmless for a tick that paces
/// preemption, yet warned under the old "fewer than 3 of 5 steady" rule.)
pub const TIMER_TOLERANCE_PPM: u64 = 20_000;

/// What a boot calibration's median bound says, against its clock's
/// tolerance ([`verdict`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Every window steady at the first try, within the tolerance.
    Clean,
    /// Windows taken again, the median within the tolerance.
    Retook,
    /// The median may be off by more than the tolerance.
    Imprecise,
    /// No bound under [`NO_INFORMATION_PPM`]: the rate says nothing.
    Unusable,
}

/// The verdict on a calibration whose median can be off by `bound_ppm`,
/// against `tolerance_ppm`, having taken `retaken` unsteady windows: judged
/// by the bound, whatever the steady count (with most windows steady the
/// bound is a steady window's, at most about 0.51% for the TSC, the PIT's
/// 84 ppm tick included).
pub fn verdict(bound_ppm: u64, tolerance_ppm: u64, retaken: usize) -> Verdict {
    if bound_ppm >= NO_INFORMATION_PPM {
        Verdict::Unusable
    } else if bound_ppm > tolerance_ppm {
        Verdict::Imprecise
    } else if retaken == 0 {
        Verdict::Clean
    } else {
        Verdict::Retook
    }
}

/// The bound of a rate measured against a clock that is itself off by
/// `against_ppm` (the local APIC timer's, against the calibrated TSC), its
/// own measurement off by `own_ppm`: `(1 + own)(1 + against) - 1`, rounded
/// up. `u64::MAX` (none) if either is [`NO_INFORMATION_PPM`] or more.
pub fn compose_ppm(own_ppm: u64, against_ppm: u64) -> u64 {
    if own_ppm >= NO_INFORMATION_PPM || against_ppm >= NO_INFORMATION_PPM {
        return u64::MAX;
    }
    own_ppm + against_ppm + (own_ppm * against_ppm).div_ceil(1_000_000)
}

/// Whether two measurements of one rate, `a` off by at most `a_ppm` of the
/// true rate and `b` by at most `b_ppm`, agree: within those bounds of each
/// other, `|a - b| ≤ (a_ppm + b_ppm) × true` (the true rate is at most
/// `b / (1 - b_ppm)`), but never held tighter than `floor_ppm`, for what the
/// bounds leave out (two measurements at different times against different
/// clocks, which QEMU drives from the host's slewed clock while KVM's TSC
/// isn't slewed). False if either bound is [`NO_INFORMATION_PPM`] or more:
/// such a measurement can't be compared.
pub fn within_bounds(a: u64, b: u64, a_ppm: u64, b_ppm: u64, floor_ppm: u64) -> bool {
    const M: u128 = 1_000_000;
    if a_ppm >= NO_INFORMATION_PPM || b_ppm >= NO_INFORMATION_PPM {
        return false;
    }
    let allowed = u128::from((a_ppm + b_ppm).max(floor_ppm));
    u128::from(a.abs_diff(b)) * (M - u128::from(b_ppm)) <= u128::from(b) * allowed
}

/// Stalls injected into each calibration window, in ppm (see the kernel's
/// `lumen.stall_*_windows`): `end` of how long the window ran, `start` of how
/// long the previous one did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stall {
    pub end: u64,
    pub start: u64,
}

/// The most a [`Stall`] may ask for, in ppm: 10 windows' worth, over three
/// times the most a scenario asks (3,000,000 for the timer's start), short of
/// a hang.
pub const MOST_STALL_PPM: u64 = 10_000_000;

/// `<end ppm>[,<start ppm>]` as a [`Stall`]; `None` for anything else, or a
/// value over [`MOST_STALL_PPM`].
pub fn parse_stall(v: &str) -> Option<Stall> {
    let mut parts = v.splitn(2, ',').map(str::parse::<u64>);
    let stall = match (parts.next(), parts.next()) {
        (Some(Ok(end)), None) => Stall { end, start: 0 },
        (Some(Ok(end)), Some(Ok(start))) => Stall { end, start },
        _ => return None,
    };
    (stall.end <= MOST_STALL_PPM && stall.start <= MOST_STALL_PPM).then_some(stall)
}

/// The TSC ticks a stall of `ppm` (parts per million) of `ticks` lasts,
/// rounded down: what `lumen.stall_*_windows=` spins at a window's end (of
/// its run so far) or start (of the last one's). The scenarios' floor on the
/// bound rests on it (`lumen_harness.calibration.stalled`): the window's gap
/// holds this and its reads. `u64::MAX` if the product overflows a u64,
/// which needs a run of over 2^60 ticks.
pub fn stall_ticks(ticks: u64, ppm: u64) -> u64 {
    u64::try_from(u128::from(ticks) * u128::from(ppm) / 1_000_000).unwrap_or(u64::MAX)
}

/// PIT ticks between two readings of a channel counting down, `start` then
/// `end`. In mode 0 it counts on past 0 (wrapping to 0xffff), so this holds
/// as long as fewer than 65536 ticks passed ([`pit_end_unambiguous`]).
fn pit_elapsed(start: u16, end: u16) -> u64 {
    u64::from(start.wrapping_sub(end))
}

/// Can a PIT window programmed with `programmed` be placed: its start read
/// while it still counted, and its end shown to be fewer than 65536 ticks
/// after the start, so [`pit_elapsed`] is right? (False also means it can't
/// be told.) `running_tsc` is the TSC from the start reading to just before
/// the last poll that still saw the count running, so the start was read
/// before it ran out and fewer than its count (at most `programmed`, which
/// [`pit_window`] then checks) ticks passed in it; `late_tsc` is from then to
/// the end reading. If `late_tsc` is at most 4 × `running_tsc`, fewer than
/// 5 × `programmed` ticks passed in all, which is under 65536 for up to 13107
/// (10 ms is 11932). No poll that saw it running (`running_tsc` 0: the start
/// may have been read after it ran out) places nothing.
fn pit_end_unambiguous(programmed: u16, running_tsc: u64, late_tsc: u64) -> bool {
    running_tsc > 0 && u64::from(programmed) * 5 <= 65536 && late_tsc <= running_tsc.saturating_mul(4)
}

/// A reading of a PIT channel's counter, latched between two TSC reads (the
/// latched count may be read after the second).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PitReading {
    pub count: u16,
    pub tsc_before: u64,
    pub tsc_after: u64,
}

impl PitReading {
    /// How far off the reading can be, in TSC ticks.
    pub fn gap(&self) -> u64 {
        self.tsc_after - self.tsc_before
    }
}

/// How a PIT window went: the channel's counter read just after it started
/// and again once it had run out (in mode 0 it counts on past 0, so the
/// second reading says how far past), the TSC just before the last poll that
/// still saw it running (the start reading's if none did), and how many
/// times its output was polled. The TSC reads come in order:
/// `start.tsc_before <= start.tsc_after <= last_running_tsc <=
/// end.tsc_before <= end.tsc_after`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PitWait {
    pub start: PitReading,
    pub end: PitReading,
    pub last_running_tsc: u64,
    pub polls: u64,
}

/// A PIT window whose readings can't fit how the channel was programmed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PitBadReadback {
    pub programmed: u16,
    pub start: u16,
    pub end: u16,
}

impl fmt::Display for PitBadReadback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { programmed, start, end } = self;
        write!(
            f,
            "programmed with {programmed}, its counter read back {start} after starting and {end} once it \
             had run out (it should start at most {programmed} and run at least {start} ticks)"
        )
    }
}

/// The window a PIT channel programmed with `count` gave: from its start
/// reading to its end reading, so `reference` is the ticks between them (more
/// than the start reading's count if the end was read late). A window that
/// can't be placed (never seen running after its start reading, which may
/// then have come after it ran out, or its end read more than 4 × as long
/// after it was last seen running as it was seen running:
/// [`pit_end_unambiguous`]) gets the whole window as its end gap, so it isn't
/// steady and another is taken. One that can be placed must fit how the
/// channel was programmed (a start at most `count`, and at least that many
/// ticks run), or its read-back is wrong.
pub fn pit_window(count: u16, w: &PitWait) -> Result<Window, PitBadReadback> {
    let (start, end) = (w.start, w.end);
    let tsc = end.tsc_after - start.tsc_after;
    let reference = pit_elapsed(start.count, end.count);
    let running = w.last_running_tsc - start.tsc_after;
    let late = end.tsc_after - w.last_running_tsc;
    let end_gap = if pit_end_unambiguous(count, running, late) {
        if !pit_counts_make_sense(count, start.count, reference) {
            return Err(PitBadReadback { programmed: count, start: start.count, end: end.count });
        }
        end.gap()
    } else {
        tsc
    };
    Ok(Window { tsc, reference, polls: w.polls, start_gap: start.gap(), end_gap })
}

/// Do a PIT window's readings fit a channel programmed with `programmed` that
/// counted `elapsed` ticks from a start reading of `start` until after it ran
/// out? It counts down from what was programmed (`start` at most that) and
/// had reached 0 (`elapsed` at least `start`). Only meaningful for an end
/// that [`pit_end_unambiguous`] places; a read-back garbled past either bound
/// fails it.
fn pit_counts_make_sense(programmed: u16, start: u16, elapsed: u64) -> bool {
    start <= programmed && elapsed >= u64::from(start)
}

/// The median of `samples` (sorted in place).
pub fn median(samples: &mut [u64]) -> Option<u64> {
    samples.sort_unstable();
    match samples.len() {
        0 => None,
        n if n % 2 == 1 => Some(samples[n / 2]),
        n => Some(samples[n / 2 - 1].midpoint(samples[n / 2])),
    }
}

/// How busy a CPU was, in whole percent (rounded), from timer ticks
/// sampled over a span: `ticks` in all, `idle` of them landing on its idle
/// thread. 0 for no ticks; idle ticks past `ticks` count as all idle.
pub fn busy_percent(ticks: u64, idle: u64) -> u64 {
    percent(ticks.saturating_sub(idle), ticks)
}

/// `part` of `whole` in whole percent, rounded (0 for an empty whole; a
/// part past the whole counts as all of it).
pub fn percent(part: u64, whole: u64) -> u64 {
    if whole == 0 {
        return 0;
    }
    let part = u128::from(part.min(whole));
    ((part * 100 + u128::from(whole) / 2) / u128::from(whole)) as u64
}

/// `count` events in `ns` nanoseconds as a rate per second (rounded down;
/// 0 for an empty span, saturating).
pub fn per_second(count: u64, ns: u64) -> u64 {
    if ns == 0 {
        return 0;
    }
    u64::try_from(u128::from(count) * u128::from(NS_PER_SEC) / u128::from(ns)).unwrap_or(u64::MAX)
}

/// A counter of fixed, known frequency.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Clock {
    hz: u64,
}

impl Clock {
    /// `None` for 0 Hz.
    pub const fn new(hz: u64) -> Option<Clock> {
        if hz == 0 { None } else { Some(Clock { hz }) }
    }

    pub const fn hz(&self) -> u64 {
        self.hz
    }

    /// The nanoseconds `ticks` take, rounded down (saturating, which takes
    /// centuries at any real frequency).
    pub fn ns(&self, ticks: u64) -> u64 {
        let ns = u128::from(ticks) * u128::from(NS_PER_SEC) / u128::from(self.hz);
        u64::try_from(ns).unwrap_or(u64::MAX)
    }

    /// The ticks in `ns`, rounded up: waiting that many ticks waits at least
    /// `ns`.
    pub fn ticks(&self, ns: u64) -> u64 {
        let ticks = (u128::from(ns) * u128::from(self.hz)).div_ceil(u128::from(NS_PER_SEC));
        u64::try_from(ticks).unwrap_or(u64::MAX)
    }
}

/// A counter reading a bootloader gave in whole microseconds (Limine's
/// `rdtsc_usec`: the reading times 10^6 / `hz`, rounded down) back in
/// ticks, rounded down: at or before the reading it was taken from, never
/// after, and by fewer than `hz / 10^6 + 1` ticks. (Rounding up would also
/// stay at or before it, the reading being a whole tick; down is pinned.)
/// `None` for 0 Hz or a result past `u64`.
pub fn ticks_at_usec(usec: u64, hz: u64) -> Option<u64> {
    if hz == 0 {
        return None;
    }
    u64::try_from(u128::from(usec) * u128::from(hz) / 1_000_000).ok()
}

/// A frequency for humans: rounded to hundredths of the largest unit it
/// reaches, trailing zeros dropped (2_900_000_000 is "2.9 GHz", 1_193_182
/// "1.19 MHz").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hz(pub u64);

impl fmt::Display for Hz {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [(u64, &str); 3] = [(1_000_000_000, "GHz"), (1_000_000, "MHz"), (1_000, "kHz")];
        let Some((scale, unit)) = UNITS.into_iter().find(|&(s, _)| self.0 >= s) else {
            return write!(f, "{} Hz", self.0);
        };
        // Hundredths of the unit, rounded; then drop trailing zeros.
        let hundredths = (u128::from(self.0) * 100 + u128::from(scale / 2)) / u128::from(scale);
        let (whole, frac) = (hundredths / 100, hundredths % 100);
        match frac {
            0 => write!(f, "{whole} {unit}"),
            _ if frac % 10 == 0 => write!(f, "{whole}.{} {unit}", frac / 10),
            _ => write!(f, "{whole}.{frac:02} {unit}"),
        }
    }
}

/// A log line's timestamp: `[    0.012345]`, nanoseconds since the
/// caller's origin shown as seconds to the microsecond (100000 s and more
/// widen the field), or a placeholder of the same width while the time isn't
/// known (`None`). A zero would read as a measurement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp(pub Option<u64>);

impl fmt::Display for Stamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(ns) => write!(f, "[{:>5}.{:06}]", ns / NS_PER_SEC, ns % NS_PER_SEC / 1000),
            None => f.write_str("[    ?.??????]"),
        }
    }
}

/// A duration given in **nanoseconds**, shown in milliseconds to the
/// microsecond: `0.012 ms`, `3400.000 ms`; with `{:+}`, as an offset:
/// `+0.012 ms`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Millis(pub u64);

impl fmt::Display for Millis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if f.sign_plus() { "+" } else { "" };
        write!(f, "{sign}{}.{:03} ms", self.0 / 1_000_000, self.0 % 1_000_000 / 1000)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::string::ToString;

    use super::*;

    fn window(tsc: u64, start_gap: u64, end_gap: u64) -> Window {
        Window { tsc, reference: 1, polls: 1, start_gap, end_gap }
    }

    #[test]
    fn the_windows_set_aside_are_counted_first_last_and_shortest_after_the_first() {
        // Unsteady windows of 100, 500, 300 and 900 reference ticks among
        // steady ones: the first is the 100, the last the 900, and the
        // shortest after the first the 300 (the first doesn't count).
        let unsteady = |reference| Window { tsc: 1_000, reference, polls: 1, start_gap: 0, end_gap: 500 };
        let steady = |reference| Window { tsc: 1_000_000, reference, polls: 1, start_gap: 0, end_gap: 0 };
        let mut taken = [unsteady(100), steady(1), unsteady(500), unsteady(300), steady(2), unsteady(900), steady(3)]
            .into_iter();
        let r = steadiest_windows::<(), 3>(10, || Ok(taken.next().unwrap()), Window::gap_ppm, |_| {}).unwrap();
        let aside = r.aside();
        assert_eq!(aside.count, 4);
        assert_eq!(aside.first.map(|w| w.reference), Some(100));
        assert_eq!(aside.last.map(|w| w.reference), Some(900));
        assert_eq!(aside.shortest_after_first.map(|w| w.reference), Some(300));
        // One set aside: no shortest after it.
        let mut taken = [unsteady(100), steady(1), steady(2), steady(3)].into_iter();
        let r = steadiest_windows::<(), 3>(10, || Ok(taken.next().unwrap()), Window::gap_ppm, |_| {}).unwrap();
        assert_eq!((r.aside().count, r.aside().shortest_after_first), (1, None));
    }

    #[test]
    fn a_window_is_steady_within_half_a_percent() {
        assert!(window(10_000_000, 0, 0).is_steady());
        assert!(window(10_000_000, 20_000, 30_000).is_steady()); // exactly 0.5%
        assert!(!window(10_000_000, 20_000, 30_001).is_steady());
        assert!(!window(10_000_000, 50_001, 0).is_steady(), "a late start counts too");
        assert!(!window(10_000_000, 0, u64::MAX).is_steady(), "saturates, doesn't wrap");
        // Values where wrapping would give a small product or sum.
        assert!(!window(10_000_000, 0, u64::MAX / 200 + 1).is_steady(), "the product saturates");
        assert!(!window(10_000_000, u64::MAX, 1).is_steady(), "the sum saturates");
        assert!(!window(0, 0, 0).is_steady(), "an empty window isn't steady");
    }

    #[test]
    fn a_window_shows_as_its_five_fields_and_gives_its_frequency() {
        let w = Window { tsc: 29_000_000, reference: 1_000_000, polls: 3, start_gap: 4, end_gap: 5 };
        assert_eq!(w.to_string(), "29000000/1000000/3/4/5");
        assert_eq!(w.tsc_hz(100_000_000), Some(2_900_000_000));
        assert_eq!(w.reference_hz(2_900_000_000), Some(100_000_000));
        assert_eq!(Window { tsc: 0, ..w }.reference_hz(2_900_000_000), None, "an empty window gives none");
    }

    #[test]
    fn steadiest_windows_take_unsteady_ones_again_in_order() {
        // Two unsteady windows among steady ones: they are taken again, and
        // only steady ones are kept, in order; all steady at once, none set
        // aside; done on exactly the last attempt, no filling in.
        let mut taken = [window(100, 0, 0), window(101, 0, 50), window(102, 0, 0), window(103, 9, 0)]
            .into_iter()
            .chain((104..).map(|t| window(t, 0, 0)));
        let r = steadiest_windows::<(), 3>(10, || Ok(taken.next().unwrap()), Window::gap_ppm, |_| {}).unwrap();
        let aside = SetAside {
            count: 2,
            first: Some(window(101, 0, 50)),
            last: Some(window(103, 9, 0)),
            shortest_after_first: Some(window(103, 9, 0)),
        };
        assert_eq!((r.steady(), r.aside()), (3, aside));
        assert_eq!(r.windows().map(|w| w.tsc), [100, 102, 104]);
        let r = steadiest_windows::<(), 3>(3, || Ok(window(7, 0, 0)), Window::gap_ppm, |_| {}).unwrap();
        assert_eq!((r.steady(), r.aside()), (3, SetAside::default()));
        let mut n = 0;
        let late = || {
            n += 1;
            Ok::<_, ()>(window(100, 0, if n <= 2 { 99 } else { 0 }))
        };
        let r = steadiest_windows::<(), 3>(5, late, Window::gap_ppm, |_| {}).unwrap();
        assert_eq!((r.steady(), r.aside().count, n), (3, 2, 5));
    }

    #[test]
    fn pit_elapsed_counts_on_past_zero() {
        assert_eq!(pit_elapsed(11_932, 0), 11_932);
        assert_eq!(pit_elapsed(11_932, 100), 11_832, "read before it ran out");
        assert_eq!(pit_elapsed(11_932, 0xffff), 11_933, "one tick past 0");
        assert_eq!(pit_elapsed(11_932, 0xffff - 999), 12_932, "1000 ticks past 0");
        assert_eq!(pit_elapsed(5, 5), 0);
    }

    /// A PIT wait on a 1 GHz TSC: started at TSC 1000 (read in 1 µs) with the
    /// counter at 11932, running until `running_ns`, and the end read
    /// `late_ns` after that with the counter at `end`.
    fn pit_wait(running_ns: u64, late_ns: u64, end: u16) -> PitWait {
        let start = PitReading { count: 11_932, tsc_before: 0, tsc_after: 1000 };
        let last = 1000 + running_ns;
        PitWait {
            start,
            end: PitReading { count: end, tsc_before: last + late_ns - 500, tsc_after: last + late_ns },
            last_running_tsc: last,
            polls: 42,
        }
    }

    #[test]
    fn a_pit_window_runs_between_its_readings() {
        // Read right at the end: 11932 ticks, both gaps from the readings.
        let w = pit_window(11_932, &pit_wait(9_999_000, 1000, 0)).unwrap();
        assert_eq!(w, Window { tsc: 10_000_000, reference: 11_932, polls: 42, start_gap: 1000, end_gap: 500 });
        // Read 2 ms late: the counter ran on 2386 ticks past 0, and the TSC
        // with it, so the frequency is still right (1 GHz, to 0.01%).
        let w = pit_window(11_932, &pit_wait(9_999_000, 2_001_000, 0u16.wrapping_sub(2386))).unwrap();
        assert_eq!(w.reference, 11_932 + 2386);
        assert!(w.is_steady());
        assert!(w.tsc_hz(PIT_HZ).unwrap().abs_diff(1_000_000_000) < 100_000);
        // The start read 100 ticks into the count: the window is the 11832
        // after it.
        let mut wait = pit_wait(9_915_000, 1000, 0);
        wait.start.count = 11_832;
        assert_eq!(pit_window(11_932, &wait).unwrap().reference, 11_832);
    }

    #[test]
    fn a_pit_end_that_may_have_wrapped_is_not_steady_not_an_error() {
        // Read 50 ms late after running 10 ms: more than 4 × as late.
        let w = pit_window(11_932, &pit_wait(10_000_000, 50_000_000, 7)).unwrap();
        assert_eq!(w.end_gap, w.tsc);
        assert!(!w.is_steady());
        // Exactly 4 × as late (47728 ticks past 0) is placed; a tick later isn't.
        let past = 0u16.wrapping_sub(47_728);
        assert_eq!(pit_window(11_932, &pit_wait(10_000_000, 40_000_000, past)).unwrap().end_gap, 500);
        let w = pit_window(11_932, &pit_wait(10_000_000, 40_000_001, past)).unwrap();
        assert_eq!(w.end_gap, w.tsc);
        // Never seen running (the first poll saw it done): can't be placed.
        let w = pit_window(11_932, &pit_wait(0, 1000, 0)).unwrap();
        assert_eq!(w.end_gap, w.tsc);
    }

    #[test]
    fn a_pit_start_read_after_it_ran_out_is_not_steady_not_an_error() {
        // Paused 12 ms before the start reading: the counter ran out and on
        // (about 63150), and no poll saw it running after. Not a bad read-back.
        let mut w = pit_wait(0, 1000, 60_000);
        w.start.count = 63_150;
        let w = pit_window(11_932, &w).unwrap();
        assert_eq!(w.end_gap, w.tsc);
        assert!(!w.is_steady());
    }

    #[test]
    fn a_pit_read_back_that_cant_be_right_is_an_error() {
        // A start above what was programmed.
        let mut w = pit_wait(9_999_000, 1000, 0);
        w.start.count = 11_933;
        assert_eq!(pit_window(11_932, &w), Err(PitBadReadback { programmed: 11_932, start: 11_933, end: 0 }));
        // All ones (a missing or garbled read), though seen running after it.
        w.start.count = 0xffff;
        assert_eq!(pit_window(11_932, &w), Err(PitBadReadback { programmed: 11_932, start: 0xffff, end: 0 }));
        // An end that says it hadn't run out (it had: its output was high).
        let w = pit_wait(9_999_000, 1000, 5);
        assert_eq!(pit_window(11_932, &w), Err(PitBadReadback { programmed: 11_932, start: 11_932, end: 5 }));
    }

    #[test]
    fn pit_counts_must_fit_how_it_was_programmed() {
        assert!(pit_counts_make_sense(11_932, 11_932, 11_932));
        assert!(pit_counts_make_sense(11_932, 11_900, 14_000), "start read after it began, end read late");
        assert!(!pit_counts_make_sense(11_932, 11_933, 12_000), "more than was programmed");
        assert!(!pit_counts_make_sense(11_932, 11_900, 11_899), "hadn't reached 0");
    }

    #[test]
    fn a_pit_end_read_late_may_have_wrapped() {
        // Late by up to 4 × the running stretch: fewer than 5 × 11932 ticks.
        assert!(pit_end_unambiguous(11_932, 1_000_000, 4_000_000));
        assert!(!pit_end_unambiguous(11_932, 1_000_000, 4_000_001));
        assert!(!pit_end_unambiguous(11_932, 0, 0), "it never saw the count running");
        assert!(pit_end_unambiguous(13_107, 1, 4));
        assert!(!pit_end_unambiguous(13_108, 1, 4), "5 × start could reach 65536");
        assert!(pit_end_unambiguous(11_932, u64::MAX, u64::MAX), "saturates");
    }

    #[test]
    fn frequency_scales_by_the_reference() {
        // 29 M TSC ticks while a 100 MHz HPET counted 1 M (10 ms): 2.9 GHz.
        assert_eq!(frequency(29_000_000, 1_000_000, 100_000_000), Some(2_900_000_000));
        // While the PIT counted 11932 (10 ms).
        assert_eq!(frequency(29_000_000, 11_932, PIT_HZ), Some(2_899_956_252));
        assert_eq!(frequency(1, 0, 100), None, "the reference didn't move");
        assert_eq!(frequency(0, 10, 100), None, "the counter didn't move");
        assert_eq!(frequency(u64::MAX, 1, u64::MAX), None, "doesn't fit");
        assert_eq!(frequency(u64::MAX, u64::MAX, 7), Some(7), "no overflow in the product");
    }

    #[test]
    fn hpet_periods() {
        assert_eq!(hpet_hz(10_000_000), Some(100_000_000), "QEMU: 10 ns");
        assert_eq!(hpet_hz(69_841_279), Some(14_318_179), "the classic 14.318 MHz");
        assert_eq!(hpet_hz(HPET_MAX_PERIOD_FS), Some(10_000_000));
        assert_eq!(hpet_hz(HPET_MAX_PERIOD_FS + 1), None);
        assert_eq!(hpet_hz(0), None);
    }

    #[test]
    fn pit_counts() {
        assert_eq!(pit_count(10_000_000), Some(11_932));
        assert_eq!(pit_count(54_924_500), Some(65_535), "the longest");
        assert_eq!(pit_count(54_925_000), None, "rounds to 65536");
        assert_eq!(pit_count(55_000_000), None);
        assert_eq!(pit_count(100), None, "rounds to 0");
        assert_eq!(pit_count(500), Some(1), "rounds to nearest");
    }

    #[test]
    fn periodic_counts() {
        // A 1 GHz bus divided by 16, at 1 kHz.
        assert_eq!(periodic_count(62_500_000, 1000), Some(62_500));
        assert_eq!(periodic_count(1999, 1000), Some(2), "rounds to nearest");
        assert_eq!(periodic_count(499, 1000), None, "rounds to 0");
        assert_eq!(periodic_count(u64::from(u32::MAX) * 1000 + 1000, 1000), None, "over 32 bits");
        assert_eq!(periodic_count(1000, 0), None);
    }

    #[test]
    fn counter_deltas_wrap_at_the_counter_width() {
        assert_eq!(counter_delta(10, 25, u64::MAX), 15);
        assert_eq!(counter_delta(0xffff_fff0, 0x10, 0xffff_ffff), 0x20, "a 32-bit counter wrapped");
        assert_eq!(counter_delta(u64::MAX - 1, 3, u64::MAX), 5, "a 64-bit counter wrapped");
        assert_eq!(counter_delta(5, 5, 0xffff_ffff), 0);
    }

    #[test]
    fn counter_midpoints_wrap_at_the_counter_width() {
        assert_eq!(counter_midpoint(10, 30, u64::MAX), 20);
        assert_eq!(counter_midpoint(10, 31, u64::MAX), 20, "rounds down");
        assert_eq!(counter_midpoint(7, 7, 0xffff_ffff), 7);
        assert_eq!(counter_midpoint(0xffff_fff0, 0x10, 0xffff_ffff), 0, "a 32-bit counter wrapped");
        assert_eq!(counter_midpoint(0xffff_fff0, 0x8, 0xffff_ffff), 0xffff_fffc, "wrapped, midpoint before it");
        assert_eq!(counter_midpoint(u64::MAX - 1, 4, u64::MAX), 1, "a 64-bit counter wrapped");
    }

    #[test]
    fn a_bracketed_reading_is_placed_within_half_its_width_of_both_ends() {
        let cases = [
            (10, 30, u64::MAX),
            (10, 31, u64::MAX),
            (7, 7, u64::MAX),
            (0xffff_fff0, 0x10, 0xffff_ffff), // a 32-bit counter wrapped
            (0xffff_fff0, 0x9, 0xffff_ffff),
            (u64::MAX - 2, 4, u64::MAX), // a 64-bit one wrapped
        ];
        for (before, after, mask) in cases {
            let b = Bracketed::between(before, 'v', after, mask);
            assert_eq!((b.value, b.width), ('v', counter_delta(before, after, mask)), "{before} {after}");
            let half_up = b.width.div_ceil(2);
            assert!(counter_delta(before, b.at, mask) <= half_up, "{before} {after}: {b:?}");
            assert!(counter_delta(b.at, after, mask) <= half_up, "{before} {after}: {b:?}");
            assert_eq!(counter_delta(before, b.at, mask) + counter_delta(b.at, after, mask), b.width);
        }
        // A clock read going backwards (`now_ns` jumping back) is a huge
        // width, never a narrow reading.
        assert_eq!(Bracketed::between(1_000, (), 999, u64::MAX).width, u64::MAX);
    }

    #[test]
    fn a_clock_going_backwards_across_a_bracket_is_an_error() {
        assert_eq!(Bracketed::checked_between(1_000, 'v', 999), Err(Backwards { before: 1_000, after: 999 }));
        let b = Bracketed::checked_between(1_000, 'v', 1_011).unwrap();
        assert_eq!((b.value, b.at, b.width), ('v', 1_005, 11));
        assert_eq!(Bracketed::checked_between(7, (), 7).map(|b| b.width), Ok(0));
    }

    #[test]
    fn a_clock_going_backwards_stops_first_narrow_at_once() {
        let mut taken = 0;
        let r = first_narrow::<u64, Backwards, 5>(100, || {
            taken += 1;
            if taken == 1 { Ok(bracketed(500)) } else { Bracketed::checked_between(10, 0, 9) }
        });
        assert_eq!(r, Err(Backwards { before: 10, after: 9 }));
        assert_eq!(taken, 2, "not retaken like a stall");
    }

    #[test]
    fn a_placement_try_is_judged_within_a_quarter_of_the_pause() {
        // Pause 400: the reference at most 100 wide, and half of it plus half
        // the paused bracket's excess over 800 at most 100.
        assert_eq!(placement_uncertainty(100, 800, 400), 50);
        assert!(placement_is_judged(100, 800, 400));
        assert!(!placement_is_judged(101, 800, 400), "the reference over a quarter");
        assert_eq!(placement_uncertainty(0, 1_000, 400), 100);
        assert!(placement_is_judged(0, 1_000, 400), "exactly a quarter");
        assert_eq!(placement_uncertainty(0, 1_001, 400), 101, "rounds up");
        assert!(!placement_is_judged(0, 1_001, 400));
        assert!(placement_is_judged(100, 900, 400) && !placement_is_judged(100, 901, 400));
        assert_eq!(placement_uncertainty(0, 700, 400), 0, "narrower than the pauses: no excess");
        // A wrapped or backwards width doesn't overflow: never judged.
        assert_eq!(placement_uncertainty(u64::MAX, u64::MAX, 400), u64::MAX / 2 + 1);
        assert!(!placement_is_judged(0, u64::MAX, 400) && !placement_is_judged(u64::MAX, 800, 400));
        let _ = placement_is_judged(u64::MAX, u64::MAX, u64::MAX); // no overflow panic
    }

    fn bracketed(width: u64) -> Bracketed<u64> {
        Bracketed::between(0, width * 10, width, u64::MAX)
    }

    #[test]
    fn the_first_narrow_reading_is_kept() {
        let mut widths = [5, 100, 7].into_iter();
        let r = first_narrow::<_, (), 5>(100, || Ok(bracketed(widths.next().unwrap()))).unwrap();
        assert_eq!(r.reading(), Some(bracketed(5)));
        assert_eq!((r.widths(), r.retries()), (&[5][..], 0));
        assert_eq!(widths.next(), Some(100), "no more readings taken");
    }

    #[test]
    fn a_wide_reading_is_taken_again_and_its_width_kept() {
        let mut widths = [101, 5_000, 100, 7].into_iter();
        let r = first_narrow::<_, (), 5>(100, || Ok(bracketed(widths.next().unwrap()))).unwrap();
        assert_eq!(r.reading(), Some(bracketed(100)), "the limit is inclusive");
        assert_eq!((r.widths(), r.retries()), (&[101, 5_000, 100][..], 2));
        assert_eq!(widths.next(), Some(7));
    }

    #[test]
    fn no_reading_when_every_one_is_wide() {
        let mut taken = 0u64;
        let r = first_narrow::<_, (), 3>(100, || {
            taken += 1;
            Ok(bracketed(1_000 + taken))
        })
        .unwrap();
        assert_eq!(r.reading(), None);
        assert_eq!((r.widths(), r.retries()), (&[1_001, 1_002, 1_003][..], 2));
        assert_eq!(taken, 3, "stops after N");
        let none = first_narrow::<u64, (), 0>(100, || unreachable!()).unwrap();
        assert_eq!((none.reading(), none.widths(), none.retries()), (None, &[][..], 0));
    }

    #[test]
    fn down_counter_deltas_cross_one_reload() {
        assert_eq!(down_counter_delta(60_000, 50_000, 62_500), 10_000);
        assert_eq!(down_counter_delta(5_000, 60_000, 62_500), 7_500, "reloaded once");
        assert_eq!(down_counter_delta(0, 62_500, 62_500), 0, "at 0, then reloaded");
        assert_eq!(down_counter_delta(7, 7, 62_500), 0);
    }

    /// A window of `tsc` made-up ticks with `gaps` ticks of end gap.
    fn gapped(tsc: u64, gaps: u64) -> Window {
        Window { tsc, reference: tsc / 100, polls: 1, start_gap: 0, end_gap: gaps }
    }

    #[test]
    fn steadiest_windows_are_the_steady_ones_when_there_are_enough() {
        // Two unsteady windows first: both set aside, a pause before each
        // window taken after them (two), and five steady ones kept in order.
        let mut n = 0u64;
        let mut pauses = 0;
        let r = steadiest_windows::<(), 5>(
            20,
            || {
                n += 1;
                Ok(if n <= 2 { gapped(10_000_000 + n, 1_000_000) } else { gapped(10_000_000 + n, 0) })
            },
            Window::gap_ppm,
            |_| pauses += 1,
        )
        .unwrap();
        assert_eq!((r.steady(), r.aside().count, pauses, n), (5, 2, 2, 7));
        assert_eq!(r.aside().last, Some(gapped(10_000_002, 1_000_000)));
        assert_eq!(r.windows().map(|w| w.tsc), [10_000_003, 10_000_004, 10_000_005, 10_000_006, 10_000_007]);
    }

    #[test]
    fn steadiest_windows_fill_up_with_the_least_gapped_when_attempts_run_out() {
        // Steady windows at attempts 3, 9 and 15 of 20; the rest unsteady,
        // their end gaps in units of 10,000 ticks of a 10,000,000-tick
        // window (70,000 and up: over 0.5%). The three steady ones come
        // first, then the two unsteady with the smallest gaps; a pause
        // before every window after an unsteady one.
        let gaps = [9, 8, 0, 7, 60, 61, 70, 55, 0, 80, 51, 90, 52, 99, 0, 98, 97, 96, 95, 94u64];
        let (mut n, mut pauses) = (0usize, 0);
        let r = steadiest_windows::<(), 5>(
            20,
            || {
                n += 1;
                Ok(gapped(10_000_000 + n as u64, gaps[n - 1] * 10_000))
            },
            Window::gap_ppm,
            |_| pauses += 1,
        )
        .unwrap();
        assert_eq!(n, 20);
        assert_eq!((r.steady(), r.aside().count), (3, 17));
        assert_eq!(r.windows().map(|w| w.tsc - 10_000_000), [3, 9, 15, 4, 2], "steady in order, then least gapped");
        assert_eq!(pauses, 16, "before each window after an unsteady one: none after the last");
    }

    #[test]
    fn steadiest_windows_with_none_steady_are_the_least_gapped() {
        let mut n = 0u64;
        let r = steadiest_windows::<(), 5>(
            6,
            || {
                n += 1;
                Ok(gapped(10_000_000, (100 - n) * 1_000))
            },
            Window::gap_ppm,
            |_| {},
        )
        .unwrap();
        assert_eq!((r.steady(), r.aside().count), (0, 6));
        assert_eq!(r.windows().map(|w| w.end_gap / 1_000), [94, 95, 96, 97, 98]);
        assert_eq!(steadiest_windows::<u8, 5>(20, || Err(7), Window::gap_ppm, |_| {}), Err(7), "an error at once");
    }

    #[test]
    fn spares_are_ranked_by_the_bound_the_caller_gives() {
        // Ranked by the gap share (the timer's bound): three unsteady
        // windows, one spare kept (N = 1, attempts 3):
        // A: tsc 10M, start 400k, end 10k: 4.1% of it.
        // B: tsc 10M, start 0, end 300k: 3.0%.
        // C: tsc 40M, start 0, end 1.0M: 2.5% (the most ticks, the least share).
        // By share C wins; by end gap alone A would (10k), by the larger gap
        // B would (300k against A's 400k and C's 1M), by the sum B would.
        let one = |order: &[Window], bound: fn(&Window) -> u64| {
            let mut it = order.iter().copied();
            steadiest_windows::<(), 1>(order.len(), || Ok(it.next().unwrap()), bound, |_| {}).unwrap().windows()[0]
        };
        let a = Window { tsc: 10_000_000, reference: 1, polls: 1, start_gap: 400_000, end_gap: 10_000 };
        let b = Window { tsc: 10_000_000, reference: 1, polls: 1, start_gap: 0, end_gap: 300_000 };
        let c = Window { tsc: 40_000_000, reference: 1, polls: 1, start_gap: 0, end_gap: 1_000_000 };
        for order in [[a, b, c], [c, b, a], [b, c, a]] {
            assert_eq!(one(&order, Window::gap_ppm), c, "{order:?}");
        }
        assert_eq!(one(&[a, b], Window::gap_ppm), b, "3.0% before 4.1%, though B's end gap is 30 times A's");
        // An empty window (no TSC ticks) ranks after any other.
        let empty = Window { tsc: 0, reference: 0, polls: 1, start_gap: 0, end_gap: 0 };
        assert_eq!(one(&[empty, a], Window::gap_ppm), a);
        // Both gaps' sum, not the larger: X (0.6% + 0.6%) after Y (1.0% + 0).
        let x = Window { tsc: 10_000_000, reference: 1, polls: 1, start_gap: 60_000, end_gap: 60_000 };
        let y = Window { tsc: 10_000_000, reference: 1, polls: 1, start_gap: 100_000, end_gap: 0 };
        assert_eq!(one(&[x, y], Window::gap_ppm), y);
        // Of two with the same bound, the earlier is kept first.
        let (p, q) = (Window { polls: 1, ..x }, Window { polls: 2, ..x });
        let mut it = [p, q].into_iter();
        let r = steadiest_windows::<(), 2>(2, || Ok(it.next().unwrap()), Window::gap_ppm, |_| {}).unwrap();
        assert_eq!(r.windows().map(|w| w.polls), [1, 2]);
    }

    #[test]
    fn tsc_spares_are_ranked_by_the_tsc_bound_not_the_gap_share() {
        // For the TSC's rate an end gap counts for more than a start gap
        // (tsc_error_ppm): A (0 + 0.7%) has the smaller gap share, B (0.4% +
        // 0.4%) the smaller TSC bound. Three A's and five B's, five kept:
        // ranked by the gap share, A A A B B, a median within 7,051 ppm;
        // by the TSC bound, the five B's, within 4,018.
        let a = Window { tsc: 10_000_000, reference: 1_000_000, polls: 800, start_gap: 0, end_gap: 70_000 };
        let b = Window { tsc: 10_000_000, reference: 1_000_000, polls: 800, start_gap: 40_000, end_gap: 40_000 };
        assert!(a.gap_ppm() < b.gap_ppm() && a.tsc_error_ppm() > b.tsc_error_ppm());
        let kept = |bound: fn(&Window) -> u64| {
            let mut it = [a, b, a, b, a, b, b, b].into_iter();
            *steadiest_windows::<(), 5>(8, || Ok(it.next().unwrap()), bound, |_| {}).unwrap().windows()
        };
        let by_share = kept(Window::gap_ppm);
        assert_eq!(median_bound_ppm(&by_share, Window::tsc_error_ppm), 7_051);
        let by_bound = kept(Window::tsc_error_ppm);
        assert_eq!(by_bound, [b; 5]);
        assert_eq!(median_bound_ppm(&by_bound, Window::tsc_error_ppm), 4_018);
    }

    #[test]
    fn a_retake_waits_on_the_shortest_window_from_its_start_reading() {
        // The pause gets the fewest TSC ticks any window spanned from its
        // start reading (tsc + start_gap: a stalled start shortens tsc):
        // 10,000,000 after the first; after the second (9,800,000 + 300,000)
        // still 10,000,000; after the third, 9,000,000.
        let ws = [
            Window { tsc: 10_000_000, reference: 1, polls: 1, start_gap: 0, end_gap: 900_000 },
            Window { tsc: 9_800_000, reference: 1, polls: 1, start_gap: 300_000, end_gap: 0 },
            Window { tsc: 9_000_000, reference: 1, polls: 1, start_gap: 0, end_gap: 900_000 },
        ];
        let (mut it, mut rulers) = (ws.into_iter().chain(core::iter::repeat(gapped(10_000_000, 0))), [0u64; 4]);
        let mut n = 0;
        steadiest_windows::<(), 3>(
            10,
            || Ok(it.next().unwrap()),
            Window::gap_ppm,
            |r| {
                rulers[n] = r;
                n += 1;
            },
        )
        .unwrap();
        assert_eq!(&rulers[..n], &[10_000_000, 10_000_000, 9_000_000], "after each unsteady window");
        // A window with no bound (its end gap as long as it) doesn't shorten
        // the ruler: before any other, there is none.
        let unplaced = Window { tsc: 1_000, reference: 1, polls: 1, start_gap: 0, end_gap: 1_000 };
        let mut it =
            [unplaced, gapped(10_000_000, 900_000)].into_iter().chain(core::iter::repeat(gapped(10_000_000, 0)));
        let (mut seen, mut k) = ([0u64; 4], 0);
        steadiest_windows::<(), 3>(
            10,
            || Ok(it.next().unwrap()),
            Window::gap_ppm,
            |r| {
                seen[k] = r;
                k += 1;
            },
        )
        .unwrap();
        assert_eq!(&seen[..k], &[u64::MAX, 10_000_000]);
    }

    #[test]
    fn a_window_gap_share_is_rounded_up_and_unbounded_when_it_fills_the_window() {
        let w = |tsc, start_gap, end_gap| Window { tsc, reference: 1, polls: 1, start_gap, end_gap };
        assert_eq!(w(10_000_000, 30_000, 20_000).gap_ppm(), 5_000); // exactly 0.5%
        assert_eq!(w(3, 0, 1).gap_ppm(), 333_334, "rounded up");
        assert_eq!(w(10, 5, 5).gap_ppm(), u64::MAX, "gaps as long as the window");
        assert_eq!(w(0, 0, 0).gap_ppm(), u64::MAX, "an empty window");
        assert_eq!(w(u64::MAX, u64::MAX / 2, 0).gap_ppm(), 500_000, "no overflow");
    }

    #[test]
    fn a_tsc_rate_is_off_by_at_most_its_end_gap_over_the_rest_plus_a_reference_tick() {
        let w = |tsc, reference, start_gap, end_gap| Window { tsc, reference, polls: 1, start_gap, end_gap };
        // 10 ms against the HPET (1,000,000 ticks), a 0.5% end gap: the
        // window it stretches is in the divisor, so 0.5026%, not 0.5%.
        assert_eq!(w(10_000_000, 1_000_000, 0, 50_000).tsc_error_ppm(), 5_027);
        assert!(w(10_000_000, 1_000_000, 0, 50_000).gap_ppm() < 5_027);
        // The same gap at the start counts for a little less than its share.
        assert_eq!(w(10_000_000, 1_000_000, 50_000, 0).tsc_error_ppm(), 4_977);
        // No gaps: one reference tick, 84 ppm for a 10 ms PIT window.
        assert_eq!(w(24_500_000, 11_932, 0, 0).tsc_error_ppm(), 84);
        // No bound: an end gap as long as the window, an empty window, a
        // reference that ticked less than twice.
        assert_eq!(w(10, 1_000, 0, 10).tsc_error_ppm(), u64::MAX);
        assert_eq!(w(0, 1_000, 0, 0).tsc_error_ppm(), u64::MAX);
        assert_eq!(w(10_000_000, 1, 0, 0).tsc_error_ppm(), u64::MAX);
        // Arithmetic that would overflow claims no bound rather than a wrong one.
        assert_eq!(w(u64::MAX, u64::MAX, u64::MAX, 0).tsc_error_ppm(), u64::MAX);
        assert_eq!(w(1 << 40, 1 << 40, 0, 0).tsc_error_ppm(), 1, "large but in range");
        // The median's: the third smallest of five.
        let ws = [0, 90_000, 20_000, 500_000, 30_000].map(|e| w(10_000_000, 1_000_000, 0, e));
        assert_eq!(median_bound_ppm(&ws, Window::tsc_error_ppm), w(10_000_000, 1_000_000, 0, 30_000).tsc_error_ppm());
    }

    #[test]
    fn a_window_slow_to_read_is_stretched_until_its_gaps_would_be_steady() {
        // #139's loaded KVM boot: 10 ms against the HPET (1,000,000 ticks,
        // 24,547,212 TSC ticks), 436,492 + 529,641 ticks of gaps (3.9%): the
        // same gaps are 0.4% of a window 9.8 times as long.
        let slow = Window { tsc: 24_547_212, reference: 1_000_016, polls: 40, start_gap: 436_492, end_gap: 529_641 };
        let next = stretched_for_gaps(1_000_000, &slow, 16_000_000);
        assert_eq!(next, 9_667_787);
        // Taken that long with the same gaps, it is steady.
        let longer = Window { tsc: slow.tsc * next / 1_000_016, reference: next, ..slow };
        assert!(longer.is_steady(), "{longer}");
        // At most `most`, never shorter than it is now; a steady window keeps
        // its length; so does an empty one.
        assert_eq!(stretched_for_gaps(1_000_000, &slow, 4_000_000), 4_000_000);
        assert_eq!(stretched_for_gaps(12_000_000, &slow, 16_000_000), 12_000_000);
        let steady = Window { start_gap: 1_000, end_gap: 1_000, ..slow };
        assert_eq!(stretched_for_gaps(1_000_000, &steady, 16_000_000), 1_000_000);
        assert_eq!(stretched_for_gaps(1_000_000, &Window::default(), 16_000_000), 1_000_000);
        // Just unsteady (0.6%): a little longer.
        let just = Window { tsc: 10_000_000, reference: 1_000_000, polls: 1, start_gap: 30_000, end_gap: 30_000 };
        assert_eq!(stretched_for_gaps(1_000_000, &just, 16_000_000), 1_495_514, "over tsc + start_gap");
    }

    #[test]
    fn a_burst_at_one_end_barely_stretches_a_window() {
        // A 10 ms HPET window read normally (856 polls, as on KVM) with one
        // 5 ms burst at its end: the burst hits one read, not every poll, so
        // the end gap counts for two mean poll intervals (57,243 ticks) and
        // the window keeps its length; uncapped it would jump to the 16x cap
        // and stay there. The sighting's gaps from 40 polls (every read slow)
        // stretch it.
        let burst = Window { tsc: 24_500_000, reference: 1_000_000, polls: 856, start_gap: 3_000, end_gap: 12_250_000 };
        assert!(!burst.is_steady());
        assert_eq!(stretched_for_gaps(1_000_000, &burst, 16_000_000), 1_000_000);
        let slow = Window { polls: 40, start_gap: 436_492, end_gap: 529_641, ..burst };
        assert!(stretched_for_gaps(1_000_000, &slow, 16_000_000) > 9_000_000);
        // A stall between the start reading and the TSC read after it, of
        // any length, counts in `reference` but not in `tsc` or `polls`: the
        // gaps are taken over `tsc + start_gap`, so it keeps its length.
        // (3 GHz TSC, 100 MHz HPET, 12.5 us per poll, each gap one poll.)
        let poll = 37_500u64;
        for stall_ms in [2u64, 5, 8, 10, 12, 50] {
            let ref_stall = stall_ms * 100_000;
            let (polls, reference) = if ref_stall >= 1_000_000 {
                (1, ref_stall + 1_250)
            } else {
                ((1_000_000 - ref_stall) / 1_250 + 1, 1_000_000)
            };
            let w =
                Window { tsc: polls * poll, reference, polls, start_gap: stall_ms * 3_000_000 + poll, end_gap: poll };
            assert!(!w.is_steady() || stall_ms < 5, "{stall_ms} ms");
            assert_eq!(stretched_for_gaps(1_000_000, &w, 16_000_000), 1_000_000, "{stall_ms} ms: {w}");
        }
        // A steady window (0.45% of gaps) isn't stretched.
        let steady = Window { tsc: 10_000_000, reference: 1_000_000, polls: 800, start_gap: 25_000, end_gap: 20_000 };
        assert!(steady.is_steady());
        assert_eq!(stretched_for_gaps(1_000_000, &steady, 16_000_000), 1_000_000);
        // A gap between one and two poll intervals counts in full: 100 polls
        // of 100,000 ticks, an end gap of 1.5 of them (1.5%), stretches to
        // 3.75 windows (capped at one interval it would be 2.5).
        let between = Window { tsc: 10_000_000, reference: 1_000_000, polls: 100, start_gap: 0, end_gap: 150_000 };
        assert_eq!(stretched_for_gaps(1_000_000, &between, 16_000_000), 3_750_000);
        // Arithmetic that would overflow asks for the most.
        let huge = Window { tsc: 1, reference: u64::MAX, polls: 1, start_gap: u64::MAX, end_gap: u64::MAX };
        assert_eq!(stretched_for_gaps(1, &huge, 77), 77);
    }

    #[test]
    fn a_verdict_is_by_the_bound_against_the_tolerance() {
        assert_eq!(verdict(500, TSC_TOLERANCE_PPM, 0), Verdict::Clean);
        assert_eq!(verdict(500, TSC_TOLERANCE_PPM, 3), Verdict::Retook);
        assert_eq!(verdict(10_000, TSC_TOLERANCE_PPM, 20), Verdict::Retook, "at the tolerance");
        assert_eq!(verdict(10_001, TSC_TOLERANCE_PPM, 20), Verdict::Imprecise);
        assert_eq!(verdict(10_001, TSC_TOLERANCE_PPM, 0), Verdict::Imprecise, "past it even without retakes");
        assert_eq!(verdict(20_000, TIMER_TOLERANCE_PPM, 20), Verdict::Retook, "at the timer's tolerance");
        assert_eq!(verdict(20_001, TIMER_TOLERANCE_PPM, 20), Verdict::Imprecise);
        assert_eq!(verdict(999_999, TIMER_TOLERANCE_PPM, 20), Verdict::Imprecise);
        assert_eq!(verdict(NO_INFORMATION_PPM, TIMER_TOLERANCE_PPM, 20), Verdict::Unusable);
        assert_eq!(verdict(u64::MAX, TIMER_TOLERANCE_PPM, 0), Verdict::Unusable);
        // #139's two stalled boots' timers (0.7% and 0.5% bounds, judged
        // with a steady TSC's 0.12% added) are inside the timer's 2%.
        assert_eq!(verdict(compose_ppm(6_870, 1_200), TIMER_TOLERANCE_PPM, 18), Verdict::Retook);
        assert_eq!(verdict(compose_ppm(5_041, 1_200), TIMER_TOLERANCE_PPM, 19), Verdict::Retook);
        // A timer within 2% on its own but measured against a TSC off by
        // 1% is past it.
        assert_eq!(verdict(compose_ppm(15_000, 10_000), TIMER_TOLERANCE_PPM, 20), Verdict::Imprecise);
    }

    #[test]
    fn a_rate_measured_against_an_imprecise_clock_adds_its_bound() {
        assert_eq!(compose_ppm(0, 0), 0);
        assert_eq!(compose_ppm(10_000, 10_000), 20_100, "(1.01)(1.01) - 1");
        assert_eq!(compose_ppm(1, 1), 3, "the product rounded up");
        assert_eq!(compose_ppm(NO_INFORMATION_PPM, 0), u64::MAX);
        assert_eq!(compose_ppm(NO_INFORMATION_PPM - 1, 0), NO_INFORMATION_PPM - 1, "just under: a bound");
        assert_eq!(compose_ppm(0, NO_INFORMATION_PPM - 1), NO_INFORMATION_PPM - 1);
        assert_eq!(compose_ppm(0, u64::MAX), u64::MAX);
    }

    #[test]
    fn two_measurements_agree_within_their_bounds_or_the_floor() {
        // A window 0.6% high against a median 0.6% low, each within its bound
        // (about 0.6%) of the truth, agree: the bounds are shares of the true
        // rate, which can be up to b / (1 - b_ppm).
        assert!(within_bounds(1_006_000_000, 994_000_000, 6_002, 6_001, 0));
        assert!(!within_bounds(1_006_000_000, 994_000_000, 5_000, 5_000, 0));
        // The floor: never held tighter than it.
        assert!(within_bounds(1_009_000_000, 1_000_000_000, 100, 100, 10_000));
        assert!(!within_bounds(1_011_000_000, 1_000_000_000, 100, 100, 10_000));
        // 3% apart: too far for small bounds, fine when theirs add up to it.
        assert!(!within_bounds(1_030_000_000, 1_000_000_000, 100, 100, 10_000));
        assert!(within_bounds(1_030_000_000, 1_000_000_000, 15_000, 15_000, 0));
        // A measurement with no bound can't be compared, whatever the floor.
        assert!(!within_bounds(1_000_000_000, 1_000_000_000, NO_INFORMATION_PPM, 0, 20_000));
        assert!(!within_bounds(1_000_000_000, 1_000_000_000, 0, u64::MAX, 20_000));
        assert!(!within_bounds(1_000_000_000, 1_000_000_000, 0, NO_INFORMATION_PPM, 20_000));
        assert!(!within_bounds(1_000_000_000, 1_000_000_000, NO_INFORMATION_PPM, 0, 20_000));
        assert!(within_bounds(1_000_000_000, 1_000_000_000, NO_INFORMATION_PPM - 1, 0, 20_000));
    }

    #[test]
    fn a_stall_is_end_and_maybe_start_ppm_up_to_ten_windows() {
        assert_eq!(parse_stall("6000"), Some(Stall { end: 6_000, start: 0 }));
        assert_eq!(parse_stall("0,3000000"), Some(Stall { end: 0, start: 3_000_000 }));
        assert_eq!(parse_stall("10000000"), Some(Stall { end: MOST_STALL_PPM, start: 0 }));
        assert_eq!(parse_stall("0,10000000"), Some(Stall { end: 0, start: MOST_STALL_PPM }));
        for bad in ["", "x", "abc", "1,", ",1", "1,2,3", "-1", "10000001", "0,10000001", "1.5"] {
            assert_eq!(parse_stall(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_stall_lasts_its_share_of_the_window() {
        // 1% of a 24,818,280-tick run (#171's timer windows), 0.6% of 10,000,000.
        assert_eq!(stall_ticks(24_818_280, 10_000), 248_182);
        assert_eq!(stall_ticks(10_000_000, 6_000), 60_000);
        assert_eq!(stall_ticks(999_999, 1), 0, "rounded down");
        assert_eq!(stall_ticks(10_000_000, MOST_STALL_PPM), 100_000_000, "ten windows");
        assert_eq!((stall_ticks(0, 6_000), stall_ticks(10_000_000, 0)), (0, 0));
        assert_eq!(stall_ticks(u64::MAX, MOST_STALL_PPM), u64::MAX, "saturates");
    }

    #[test]
    fn a_tsc_rate_stays_within_its_bound_wherever_the_reads_fell() {
        // A model: a TSC at 2.4576 GHz and a reference at 1.193182 MHz (the
        // PIT) or 100 MHz (the HPET), counted from a random phase. Each end
        // is a reference read at an instant somewhere inside a gap of TSC
        // reads; the window is what the kernel records. The rate the window
        // gives is always within its bound of the true 2.4576 GHz, and the
        // bound isn't loose: windows come within 10% of it.
        let tsc_hz = 2_457_600_000u128;
        let mut seed = 0x139u64;
        let mut next = |n: u64| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) % n
        };
        let mut closest = 0f64;
        for i in 0..20_000 {
            let reference_hz: u128 = if i % 2 == 0 { 1_193_182 } else { 100_000_000 };
            // The reference counter at TSC tick `x`, from a phase in [0, 1).
            let phase = u128::from(next(1_000_000));
            let counter = |x: u128| (x * reference_hz * 1_000_000 + phase * tsc_hz) / (tsc_hz * 1_000_000);
            let a = 1_000_000_000 + u128::from(next(100_000_000)); // the start read
            let span = 24_000_000 + u128::from(next(1_000_000)); // to the end read
            let (before_a, after_a) = (u128::from(next(200_000)), u128::from(next(200_000)));
            let (before_b, after_b) = (u128::from(next(400_000)), u128::from(next(400_000)));
            let b = a + span;
            let (pre, t0, before, t) = (a - before_a, a + after_a, b - before_b, b + after_b);
            let window = Window {
                tsc: (t - t0) as u64,
                reference: (counter(b) - counter(a)) as u64,
                polls: 1,
                start_gap: (t0 - pre) as u64,
                end_gap: (t - before) as u64,
            };
            let measured = u128::from(window.tsc_hz(reference_hz as u64).unwrap());
            let bound = u128::from(window.tsc_error_ppm());
            let off_ppm = measured.abs_diff(tsc_hz) * 1_000_000;
            assert!(off_ppm <= bound * tsc_hz, "{window}: {measured} Hz, bound {bound} ppm");
            closest = closest.max(off_ppm as f64 / (bound * tsc_hz) as f64);
        }
        assert!(closest > 0.9, "the bound is loose: windows come within {closest:.3} of it");
    }

    fn truth_rate() -> u64 {
        62_500_000
    }

    #[test]
    fn a_median_is_off_by_at_most_its_middle_gap_share() {
        // Gap shares 0.1%, 9%, 0.2%, 50%, 0.3%: the third smallest is 0.3%.
        let w = |end_gap| Window { tsc: 1_000_000, reference: 1, polls: 1, start_gap: 0, end_gap };
        assert_eq!(median_bound_ppm(&[w(1_000), w(90_000), w(2_000), w(500_000), w(3_000)], Window::gap_ppm), 3_000);
        // Three windows with no bound: none for the median either.
        assert_eq!(
            median_bound_ppm(&[w(1_000), w(1_000_000), w(2_000), w(1_000_000), w(1_000_000)], Window::gap_ppm),
            u64::MAX
        );
        assert_eq!(median_bound_ppm::<0>(&[], Window::gap_ppm), u64::MAX, "no windows, no bound");
        // Even N: the median is the midpoint of the middle two, so a bound
        // needs N / 2 + 1 windows. Shares 0.1%, 0.1%, 50%, 50%: two 50%-off
        // windows can pull the midpoint 25% off, and the bound says 50%.
        let four = [w(1_000), w(1_000), w(500_000), w(500_000)];
        assert_eq!(median_bound_ppm(&four, Window::gap_ppm), 500_000);
        let mut rates = [truth_rate(), truth_rate(), truth_rate() / 2, truth_rate() / 2];
        let m = median(&mut rates).unwrap();
        assert!(m.abs_diff(truth_rate()) * 1_000_000 <= truth_rate() * 500_000, "{m}");
        assert!(m.abs_diff(truth_rate()) * 1_000_000 > truth_rate() * 1_000, "the third smallest would be wrong");
        // The bound holds: rates each off by their own share, the median
        // within the third smallest share of the true rate.
        let truth = 62_500_000u64;
        let off = [(1_000, -1i64), (90_000, 1), (2_000, 1), (500_000, -1), (3_000, -1)];
        let mut rates = off.map(|(g, sign)| (truth as i64 + sign * (truth as i64 * g / 1_000_000)) as u64);
        let m = median(&mut rates).unwrap();
        assert!(m.abs_diff(truth) * 1_000_000 <= truth * 3_000, "{m}");
    }

    /// Made-up windows of the local APIC timer against a 2.5 GHz TSC, 10 ms
    /// each, at `hz`, with `end_gap_us` µs (2,500 ticks each) of end gap:
    /// 50 µs or less is steady (0.5%).
    fn timer_window_at(hz: u64, end_gap_us: u64) -> Window {
        Window { tsc: 25_000_000, reference: hz / 100, polls: 1, start_gap: 0, end_gap: end_gap_us * 2_500 }
    }

    /// The median rate of `windows` against a 2.5 GHz TSC.
    fn timer_rate<const N: usize>(windows: &[Window; N]) -> u64 {
        median(&mut windows.map(|w| w.reference_hz(2_500_000_000).unwrap())).unwrap()
    }

    #[test]
    fn made_up_timer_windows_paused_first_are_taken_again() {
        // The first three windows paused at the end (#129's stall: 10% low
        // each, enough to move a median of five), then steady ones at
        // 62.5 MHz: all three are taken again, each after a pause, and the
        // rate is the steady windows'.
        let (mut taken, mut pauses) = (0, 0);
        let r = steadiest_windows::<(), 5>(
            20,
            || {
                taken += 1;
                Ok(if taken <= 3 { timer_window_at(56_250_000, 1_000) } else { timer_window_at(62_500_000, 10) })
            },
            Window::gap_ppm,
            |_| pauses += 1,
        )
        .unwrap();
        assert_eq!((r.steady(), r.aside().count, pauses, taken), (5, 3, 3, 8));
        assert_eq!(timer_rate(r.windows()), 62_500_000);
        assert_eq!(median_bound_ppm(r.windows(), Window::gap_ppm), 1_000, "10 µs of 10 ms");
    }

    #[test]
    fn made_up_timer_windows_short_of_steady_ones_fill_in() {
        // Only 3 (then 2) of 20 windows steady at 62.5 MHz; two unsteady
        // ones 2% low with 200 µs end gaps (2%), the rest 20% low with just
        // over 2 ms (20%): the steady ones and the two 2% ones are kept,
        // least gapped first. With 3 steady the median is a steady one's
        // (bound 0.1%, the third smallest share); with 2 it is a filled-in
        // one's, 2% off and within its bound of 2%.
        for (steady_at, hz, bound) in
            [(&[4usize, 11, 17][..], 62_500_000u64, 1_000u64), (&[4, 11][..], 61_250_000, 20_000)]
        {
            let mut taken = 0usize;
            let r = steadiest_windows::<(), 5>(
                20,
                || {
                    taken += 1;
                    Ok(if steady_at.contains(&taken) {
                        timer_window_at(62_500_000, 10)
                    } else if taken == 7 || taken == 9 {
                        timer_window_at(61_250_000, 200)
                    } else {
                        timer_window_at(50_000_000, 2_000 + taken as u64)
                    })
                },
                Window::gap_ppm,
                |_| {},
            )
            .unwrap();
            assert_eq!((taken, r.steady()), (20, steady_at.len()));
            let fill = r.windows()[r.steady()..].iter().map(|w| w.end_gap / 2_500);
            // The 20% ones' least end gap is the first window's, 2,001 µs.
            assert!(fill.eq(core::iter::repeat_n(200, 2).chain(core::iter::repeat_n(2_001, 3 - steady_at.len()))));
            assert_eq!(timer_rate(r.windows()), hz);
            assert_eq!(median_bound_ppm(r.windows(), Window::gap_ppm), bound);
        }
    }

    #[test]
    fn tsc_frequencies_must_be_believable() {
        assert!(plausible_tsc_hz(2_900_000_000) && plausible_tsc_hz(10_000_000) && plausible_tsc_hz(100_000_000_000));
        assert!(!plausible_tsc_hz(9_999_999), "a reference that finished far too soon");
        assert!(!plausible_tsc_hz(100_000_000_001));
    }

    #[test]
    fn busy_percent_rounds_and_clamps() {
        assert_eq!(busy_percent(0, 0), 0);
        assert_eq!(busy_percent(1000, 1000), 0);
        assert_eq!(busy_percent(1000, 0), 100);
        assert_eq!(busy_percent(1000, 875), 13); // 12.5 rounds up
        assert_eq!(busy_percent(1000, 876), 12);
        assert_eq!(busy_percent(3, 1), 67);
        assert_eq!(busy_percent(10, 11), 0);
        assert_eq!(busy_percent(u64::MAX, 0), 100);
    }

    #[test]
    fn percent_rounds_and_clamps() {
        assert_eq!(percent(1, 3), 33);
        assert_eq!(percent(2, 3), 67);
        assert_eq!(percent(5, 0), 0);
        assert_eq!(percent(7, 5), 100);
    }

    #[test]
    fn per_second_scales_by_the_span() {
        assert_eq!(per_second(1000, NS_PER_SEC), 1000);
        assert_eq!(per_second(1500, 1_500_000_000), 1000);
        assert_eq!(per_second(999, 2 * NS_PER_SEC), 499);
        assert_eq!(per_second(5, 0), 0);
        assert_eq!(per_second(u64::MAX, 1), u64::MAX);
    }

    #[test]
    fn median_ignores_outliers() {
        assert_eq!(median(&mut [5, 1, 900, 3, 4]), Some(4));
        assert_eq!(median(&mut [4, 2]), Some(3));
        assert_eq!(median(&mut [u64::MAX, u64::MAX]), Some(u64::MAX), "no overflow");
        assert_eq!(median(&mut [3, 4]), Some(3), "rounds down");
        assert_eq!(median(&mut [5, 7]), Some(6));
        assert_eq!(median(&mut []), None);
    }

    #[test]
    fn clocks_convert_both_ways() {
        let tsc = Clock::new(2_900_000_000).unwrap();
        assert_eq!(tsc.ns(2_900_000_000), NS_PER_SEC);
        assert_eq!(tsc.ns(2), 0, "rounds down");
        assert_eq!(tsc.ns(3), 1);
        assert_eq!(tsc.ticks(1), 3, "rounds up: at least 1 ns");
        assert_eq!(tsc.ticks(10_000_000), 29_000_000);
        // Ten years of uptime: no overflow in either direction.
        let decade = 10 * 365 * 24 * 3600 * NS_PER_SEC;
        assert_eq!(tsc.ns(tsc.ticks(decade)), decade);
        assert_eq!(Clock::new(1).unwrap().ns(u64::MAX), u64::MAX, "saturates");
        assert_eq!(Clock::new(u64::MAX).unwrap().ticks(u64::MAX), u64::MAX, "saturates");
        assert_eq!(Clock::new(0), None);
    }

    /// Limine's `rdtsc_usec` (common/sys/cpu.h, v12.9.4), on a given reading.
    fn limine_usec(ticks: u64, hz: u64) -> u64 {
        ticks / hz * 1_000_000 + ticks % hz * 1_000_000 / hz
    }

    #[test]
    fn a_bootloaders_microseconds_come_back_at_or_before_the_reading() {
        assert_eq!(ticks_at_usec(1_000_000, 2_900_000_000), Some(2_900_000_000));
        assert_eq!(ticks_at_usec(1, 2_900_000_001), Some(2_900), "rounds down");
        assert_eq!(ticks_at_usec(5, 0), None);
        assert_eq!(ticks_at_usec(u64::MAX, 2_000_000), None, "past u64");
        assert_eq!(ticks_at_usec(u64::MAX, 1_000_000), Some(u64::MAX), "just fits");
        // Readings near a microsecond's edges, at frequencies a TSC has (and
        // odd ones a calibration gives), and pseudo-random ones up to
        // centuries: never after the reading, never a microsecond before.
        let mut x: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for hz in [1_000_000u64, 1_193_182, 999_999_937, 2_899_956_252, 3_000_000_000, 5_123_456_789] {
            let per_us = hz.div_ceil(1_000_000);
            let mut readings = std::vec![0, 1, hz - 1, hz, hz + 1, per_us - 1, per_us, per_us + 1];
            readings.extend((0..2000).map(|_| next() % (hz * 3_600 * 24 * 365 * 100)));
            for t in readings {
                let back = ticks_at_usec(limine_usec(t, hz), hz).unwrap();
                assert!(back <= t, "after the reading: {back} > {t} at {hz} Hz");
                assert!(t - back <= per_us, "a microsecond early: {back} for {t} at {hz} Hz");
            }
        }
    }

    #[test]
    fn a_bootloaders_microseconds_come_back_right_over_a_small_range() {
        // Every reading below 20000 at every frequency up to 3000 Hz (a tick
        // spans many microseconds), and every reading below 200000 at
        // frequencies around 1 MHz (where a tick is about a microsecond).
        let check = |hz: u64, t: u64| {
            let back = ticks_at_usec(limine_usec(t, hz), hz).unwrap();
            assert!(
                back <= t && u128::from(t - back) * 1_000_000 < u128::from(hz) + 1_000_000,
                "{t} at {hz} Hz: {back}"
            );
        };
        for hz in 1..=3_000 {
            (0..20_000).for_each(|t| check(hz, t));
        }
        for hz in [999_999, 1_000_000, 1_000_001, 1_193_182, 2_000_003] {
            (0..200_000).for_each(|t| check(hz, t));
        }
    }

    #[test]
    fn stamps_have_one_width() {
        assert_eq!(Stamp(Some(12_345_678)).to_string(), "[    0.012345]");
        assert_eq!(Stamp(Some(0)).to_string(), "[    0.000000]");
        assert_eq!(Stamp(Some(3 * NS_PER_SEC + 999)).to_string(), "[    3.000000]", "rounds down to the µs");
        assert_eq!(Stamp(None).to_string(), "[    ?.??????]");
        assert_eq!(Stamp(None).to_string().len(), Stamp(Some(1)).to_string().len());
        assert_eq!(Stamp(Some(123_456 * NS_PER_SEC)).to_string(), "[123456.000000]", "wider than the field: grows");
    }

    #[test]
    fn durations_read_in_milliseconds() {
        use std::format;
        assert_eq!(format!("{:+}", Millis(12_000)), "+0.012 ms");
        assert_eq!(format!("{:+}", Millis(3_400_000_000)), "+3400.000 ms");
        assert_eq!(format!("{:+}", Millis(999)), "+0.000 ms", "rounds down to the µs");
        assert_eq!(Millis(1_234_567).to_string(), "1.234 ms");
        assert_eq!(Millis(3_400_000_000).to_string(), "3400.000 ms");
    }

    #[test]
    fn frequencies_read_well() {
        assert_eq!(Hz(2_900_000_000).to_string(), "2.9 GHz");
        assert_eq!(Hz(2_899_956_252).to_string(), "2.9 GHz");
        assert_eq!(Hz(3_456_789_012).to_string(), "3.46 GHz");
        assert_eq!(Hz(1_000_000_000).to_string(), "1 GHz");
        assert_eq!(Hz(PIT_HZ).to_string(), "1.19 MHz");
        assert_eq!(Hz(100_000_000).to_string(), "100 MHz");
        assert_eq!(Hz(14_318_180).to_string(), "14.32 MHz");
        assert_eq!(Hz(999_999_999).to_string(), "1000 MHz", "rounding stays in the unit");
        assert_eq!(Hz(1_500).to_string(), "1.5 kHz");
        assert_eq!(Hz(999).to_string(), "999 Hz");
        assert_eq!(Hz(0).to_string(), "0 Hz");
    }
}
