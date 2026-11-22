//! ext4's on-disk encodings that the kernel's adapter decodes itself, because
//! `ext4plus` doesn't: a device node's number in `i_block`, and timestamps
//! before 1970 or past 2038 (`ext4plus` reads the low 32 bits of a time as
//! unsigned).
//!
//! Both follow Linux (`fs/ext4/inode.c`'s `ext4_iget`, `fs/ext4/ext4.h`'s
//! `ext4_encode_extra_time`/`ext4_decode_extra_time`, `include/linux/kdev_t.h`).

use core::time::Duration;

use crate::types::{DevId, Timespec};

/// Linux's `old_decode_dev`: a 16-bit device number, 8 bits of major and 8 of
/// minor.
pub const fn old_decode_dev(val: u16) -> DevId {
    DevId::makedev(((val >> 8) & 0xff) as u32, (val & 0xff) as u32)
}

/// Linux's `new_decode_dev`: a 32-bit device number, 12 bits of major and 20
/// of minor (the minor's low 8 bits first).
pub const fn new_decode_dev(dev: u32) -> DevId {
    DevId::makedev((dev & 0xfff00) >> 8, (dev & 0xff) | ((dev >> 12) & 0xfff00))
}

/// A character or block device's number from its inode's first two `i_block`
/// words, as `ext4_iget` reads it: the old encoding in `i_block[0]` when that
/// is non-zero, else the new one in `i_block[1]`.
pub const fn rdev_from_i_block(i_block0: u32, i_block1: u32) -> DevId {
    if i_block0 != 0 { old_decode_dev(i_block0 as u16) } else { new_decode_dev(i_block1) }
}

/// The earliest time an ext4 inode holds (`EXT4_TIMESTAMP_MIN`, 1901-12-13).
pub const TIME_MIN: i64 = i32::MIN as i64;
/// The latest time an inode with the extra time fields holds
/// (`EXT4_EXTRA_TIMESTAMP_MAX`, 2446-05-10): two epoch bits over a signed
/// 32-bit second.
pub const TIME_MAX_EXTRA: i64 = (1 << 34) - 1 + i32::MIN as i64;
/// The latest time an inode without them holds
/// (`EXT4_NON_EXTRA_TIMESTAMP_MAX`, 2038-01-19).
pub const TIME_MAX_NO_EXTRA: i64 = i32::MAX as i64;

/// `ts` as an inode stores it, as Linux's `timestamp_truncate` makes it:
/// seconds clamped to the inode's range (no nanoseconds at either end), and
/// nanoseconds dropped without the extra fields (a second's granularity).
/// `extra`: the inode has the extra time fields.
pub fn truncate_time(ts: Timespec, extra: bool) -> Timespec {
    let max = if extra { TIME_MAX_EXTRA } else { TIME_MAX_NO_EXTRA };
    let sec = ts.sec.clamp(TIME_MIN, max);
    let nsec = if !extra || sec == TIME_MIN || sec == max { 0 } else { ts.nsec.clamp(0, 999_999_999) };
    Timespec { sec, nsec }
}

/// The `Duration` to hand `ext4plus`'s time setters so that they write Linux's
/// bytes for `ts` (after [`truncate_time`]). They write `as_secs()`'s low 32
/// bits as the base word and its next two bits as the extra word's epoch, so
/// this is the signed second's low 32 bits with Linux's epoch above them
/// (`(sec - (s32)sec) >> 32`), never negative.
pub fn time_to_disk(ts: Timespec, extra: bool) -> Duration {
    let ts = truncate_time(ts, extra);
    let low = ts.sec as u32;
    let epoch = ((ts.sec - i64::from(low as i32)) >> 32) as u64 & 3;
    Duration::new((epoch << 32) | u64::from(low), ts.nsec as u32)
}

/// The time an `ext4plus` getter's `Duration` stands for: the base word
/// signed, plus the epoch bits times 2^32 (`ext4_decode_extra_time`). An
/// inode without the extra fields reads with no epoch, so its times run
/// from 1901 to 2038.
pub fn time_from_disk(d: Duration) -> Timespec {
    let secs = d.as_secs();
    let low = i64::from(secs as u32 as i32);
    let epoch = ((secs >> 32) & 3) as i64;
    Timespec { sec: low + (epoch << 32), nsec: i64::from(d.subsec_nanos()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `ext4plus`'s setters write for a `Duration` (its
    /// `duration_to_timestamp`), base word and extra word.
    fn ext4plus_words(d: Duration) -> (u32, u32) {
        let secs = d.as_secs();
        (secs as u32, ((secs >> 32) as u32 & 3) | (d.subsec_nanos() << 2))
    }

    /// What its getters return for the two words (its `timestamp_to_duration`).
    fn ext4plus_read(base: u32, extra: u32) -> Duration {
        Duration::new((u64::from(extra & 3) << 32) | u64::from(base), extra >> 2)
    }

    /// Linux's `ext4_encode_extra_time` and the base word, from `ext4.h`.
    fn linux_words(ts: Timespec) -> (u32, u32) {
        let extra = (((ts.sec - i64::from(ts.sec as i32)) >> 32) as u32 & 3) | ((ts.nsec as u32) << 2);
        (ts.sec as u32, extra)
    }

    /// Linux's `ext4_decode_extra_time`.
    fn linux_decode(base: u32, extra: u32) -> Timespec {
        let mut sec = i64::from(base as i32);
        if extra & 3 != 0 {
            sec += i64::from(extra & 3) << 32;
        }
        Timespec { sec, nsec: i64::from(extra >> 2) }
    }

    fn ts(sec: i64, nsec: i64) -> Timespec {
        Timespec { sec, nsec }
    }

    #[test]
    fn device_numbers_decode_as_linux_does() {
        // Old encoding: 1:3 (`/dev/null`) is 0x0103.
        assert_eq!(rdev_from_i_block(0x0103, 0), DevId::makedev(1, 3));
        assert_eq!(rdev_from_i_block(0x0103, 0xdead_beef), DevId::makedev(1, 3));
        // Only the low 16 bits of i_block[0] count.
        assert_eq!(rdev_from_i_block(0xffff_0801, 0), DevId::makedev(8, 1));
        assert_eq!(rdev_from_i_block(0xffff, 0), DevId::makedev(255, 255));
        // New encoding: major 259, minor 300 (`new_encode_dev`: minor's low
        // byte, major << 8, the rest of minor << 12).
        let new = (300 & 0xff) | (259 << 8) | ((300 & !0xff) << 12);
        assert_eq!(rdev_from_i_block(0, new), DevId::makedev(259, 300));
        assert_eq!(rdev_from_i_block(0, 0), DevId::makedev(0, 0));
        // The widest the new encoding holds: 12-bit major, 20-bit minor.
        assert_eq!(rdev_from_i_block(0, 0xffff_ffff), DevId::makedev(0xfff, 0xfffff));
        assert_eq!(new_decode_dev(0x0010_0000), DevId::makedev(0, 0x100));
    }

    /// The epoch table in Linux's `fs/ext4/ext4.h`: each row's first and last
    /// second, with the extra word's epoch bits and the base word's top bit.
    #[test]
    fn the_epoch_table_round_trips() {
        let rows: [(i64, i64, u32, u32); 8] = [
            (-0x8000_0000, -0x0000_0001, 0, 1),
            (0x0_0000_0000, 0x0_7fff_ffff, 0, 0),
            (0x0_8000_0000, 0x0_ffff_ffff, 1, 1),
            (0x1_0000_0000, 0x1_7fff_ffff, 1, 0),
            (0x1_8000_0000, 0x1_ffff_ffff, 2, 1),
            (0x2_0000_0000, 0x2_7fff_ffff, 2, 0),
            (0x2_8000_0000, 0x2_ffff_ffff, 3, 1),
            (0x3_0000_0000, 0x3_7fff_ffff, 3, 0),
        ];
        for (first, last, epoch, msb) in rows {
            for sec in [first, first + 1, last - 1, last] {
                // Not at the clamp's edges, where nanoseconds go.
                let nsec = if sec == TIME_MIN || sec == TIME_MAX_EXTRA { 0 } else { 123_456_789 };
                let t = ts(sec, nsec);
                let d = time_to_disk(t, true);
                let (base, extra) = ext4plus_words(d);
                assert_eq!((base, extra), linux_words(t), "{sec:#x}");
                assert_eq!(extra & 3, epoch, "{sec:#x}");
                assert_eq!(base >> 31, msb, "{sec:#x}");
                assert_eq!(linux_decode(base, extra), t, "{sec:#x}");
                assert_eq!(time_from_disk(ext4plus_read(base, extra)), t, "{sec:#x}");
                assert_eq!(time_from_disk(d), t, "{sec:#x}");
            }
        }
    }

    #[test]
    fn times_near_the_epoch() {
        for t in [ts(0, 0), ts(0, 1), ts(0, 999_999_999), ts(-1, 0), ts(-1, 999_999_999), ts(1, 5)] {
            let d = time_to_disk(t, true);
            assert_eq!(ext4plus_words(d), linux_words(t), "{t:?}");
            assert_eq!(time_from_disk(d), t, "{t:?}");
        }
        // -1 is 0xffffffff with epoch 0, which ext4plus alone reads as 2106.
        assert_eq!(ext4plus_words(time_to_disk(ts(-1, 0), true)), (0xffff_ffff, 0));
        assert_eq!(time_from_disk(Duration::from_secs(0xffff_ffff)), ts(-1, 0));
    }

    #[test]
    fn out_of_range_times_clamp_as_timestamp_truncate_does() {
        let at_min = ts(TIME_MIN, 0);
        let at_max = ts(TIME_MAX_EXTRA, 0);
        assert_eq!(truncate_time(ts(TIME_MIN - 1, 5), true), at_min);
        assert_eq!(truncate_time(ts(i64::MIN, 5), true), at_min);
        assert_eq!(truncate_time(ts(TIME_MIN, 5), true), at_min);
        assert_eq!(truncate_time(ts(TIME_MIN + 1, 5), true), ts(TIME_MIN + 1, 5));
        assert_eq!(truncate_time(ts(TIME_MAX_EXTRA + 1, 5), true), at_max);
        assert_eq!(truncate_time(ts(i64::MAX, 5), true), at_max);
        assert_eq!(truncate_time(ts(TIME_MAX_EXTRA, 5), true), at_max);
        assert_eq!(truncate_time(ts(TIME_MAX_EXTRA - 1, 5), true), ts(TIME_MAX_EXTRA - 1, 5));
        for t in [ts(i64::MIN, 0), ts(i64::MAX, 999_999_999)] {
            let stored = truncate_time(t, true);
            assert_eq!(time_from_disk(time_to_disk(t, true)), stored);
            assert_eq!(ext4plus_words(time_to_disk(t, true)), linux_words(stored));
        }
    }

    #[test]
    fn without_the_extra_fields_a_time_is_whole_signed_32_bit_seconds() {
        assert_eq!(truncate_time(ts(5, 7), false), ts(5, 0));
        assert_eq!(truncate_time(ts(-5, 7), false), ts(-5, 0));
        assert_eq!(truncate_time(ts(TIME_MAX_NO_EXTRA + 1, 7), false), ts(TIME_MAX_NO_EXTRA, 0));
        assert_eq!(truncate_time(ts(TIME_MIN - 1, 7), false), ts(TIME_MIN, 0));
        // ext4plus then writes only the base word (an extra word of 0 if the
        // inode has one), and reads it back as the same time.
        for t in [ts(-5, 7), ts(TIME_MAX_NO_EXTRA + 9, 0), ts(1_700_000_000, 3)] {
            let d = time_to_disk(t, false);
            assert_eq!(d.subsec_nanos(), 0);
            assert!(d.as_secs() <= u64::from(u32::MAX));
            assert_eq!(time_from_disk(Duration::from_secs(d.as_secs())), truncate_time(t, false));
        }
    }
}
