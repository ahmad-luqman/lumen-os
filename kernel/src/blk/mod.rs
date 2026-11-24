//! Block device subsystem: `virtio-blk` driver, [`lumen_bcache`] cache registry,
//! and the `blk` shell command.

mod virtio_blk;

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write;

use lumen_bcache::{BlockDevice, SECTOR_SIZE};
use lumen_shell::pipe::{self, Align, Column, Kind, Value};

use crate::shell::{Ctx, sysinfo, write_in_chunks};

#[allow(unused_imports, reason = "public block subsystem API for VFS (#253)")]
pub use virtio_blk::{BlkEntry, MAX_BLK_DEVICES, VirtioBlkDevice};
pub use virtio_blk::{DRIVER, device, devices};

/// Usage string for the `blk` shell command (matching `Command::usage`).
const USAGE: &str = "usage: blk [read <id> <lba> [sectors] | write <id> <lba> <hex> [sectors] | flush <id>]";

/// Maximum sectors transferred in one `blk read` / `blk write` shell invocation.
const MAX_SHELL_SECTORS: usize = 16;

/// Maximum decoded pattern bytes accepted by `blk write` (fits comfortably inside `shell::MAX_LINE = 160`).
const MAX_WRITE_PATTERN_BYTES: usize = 64;

fn format_size(bytes: u64) -> String {
    const UNITS: [(u64, &str); 3] = [(1 << 30, "GiB"), (1 << 20, "MiB"), (1 << 10, "KiB")];
    match UNITS.into_iter().find(|&(s, _)| bytes >= s) {
        Some((scale, unit)) => {
            let tenths = (u128::from(bytes) * 10 + u128::from(scale / 2)) / u128::from(scale);
            format!("{}.{} {unit}", tenths / 10, tenths % 10)
        }
        None => format!("{bytes} B"),
    }
}

/// `blk`'s records table: one row per bound block device.
pub fn table() -> pipe::Table {
    const COLUMNS: [Column; 9] = [
        Column::new("id", Kind::Int, Align::Right, 2),
        Column::new("addr", Kind::Text, Align::Left, 12),
        Column::new("sectors", Kind::Int, Align::Right, 7),
        Column::new("size", Kind::Text, Align::Right, 8),
        Column::new("mode", Kind::Text, Align::Left, 4),
        Column::new("flush", Kind::Text, Align::Left, 5),
        Column::new("hits", Kind::Int, Align::Right, 4),
        Column::new("misses", Kind::Int, Align::Right, 6),
        Column::new("dirty", Kind::Int, Align::Right, 5),
    ];
    let mut t = pipe::Table::new(&COLUMNS);
    for entry in devices() {
        let info = entry.dev.info();
        let bytes = info.sector_count.saturating_mul(u64::from(info.sector_size));
        let stats = entry.stats();
        let mode = if entry.dev.is_failed() {
            "failed"
        } else if info.read_only {
            "ro"
        } else {
            "rw"
        };
        t.push(vec![
            sysinfo::int(entry.id),
            Value::Text(format!("{}", entry.dev.addr())),
            sysinfo::int(info.sector_count),
            Value::Text(format_size(bytes)),
            sysinfo::text(mode),
            sysinfo::text(if info.flush_supported { "yes" } else { "no" }),
            sysinfo::int(stats.hits),
            sysinfo::int(stats.misses),
            sysinfo::int(stats.dirty),
        ]);
    }
    t
}

/// Pipeline records provider for `blk`: yields [`table`] only when given no arguments.
pub fn records(args: &[String]) -> Option<Result<pipe::Table, String>> {
    args.is_empty().then(|| Ok(table()))
}

/// Note shown before an empty `blk` pipeline result when no devices are bound.
pub fn records_note() -> Option<String> {
    devices().is_empty().then(|| String::from("blk: no block devices"))
}

fn parse_sectors(arg: Option<&String>) -> Option<usize> {
    match arg {
        None => Some(1),
        Some(s) => s.parse::<usize>().ok().filter(|&n| (1..=MAX_SHELL_SECTORS).contains(&n)),
    }
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if s.is_empty() || !s.len().is_multiple_of(2) || s.len() > MAX_WRITE_PATTERN_BYTES * 2 {
        return None;
    }
    let bytes = s.as_bytes();
    let (pairs, _) = bytes.as_chunks::<2>();
    let mut out = Vec::with_capacity(pairs.len());
    for pair in pairs {
        let hi = hex_nibble(pair[0])?;
        let lo = hex_nibble(pair[1])?;
        out.push((hi << 4) | lo);
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// `blk` shell command handler.
pub fn command(ctx: &mut Ctx<'_>, args: &[String]) {
    match args {
        [] => {
            let entries = devices();
            if entries.is_empty() {
                let _ = ctx.out.write_str("blk: no block devices\n");
                return;
            }
            let mut out = String::new();
            let s = if entries.len() == 1 { "" } else { "s" };
            let _ = writeln!(out, "blk: {} device{s}", entries.len());
            table().render(&mut out);
            let _ = ctx.out.write_str(&out);
        }
        [sub, id_str, lba_str, rest @ ..] if sub == "read" && rest.len() <= 1 => {
            let (Some(id), Some(lba), Some(sectors)) =
                (id_str.parse::<usize>().ok(), lba_str.parse::<u64>().ok(), parse_sectors(rest.first()))
            else {
                return ctx.fail(USAGE);
            };
            let Some(dev) = device(id) else {
                return ctx.fail(format_args!("blk read: no device {id}"));
            };
            let mut buf = vec![0u8; sectors * SECTOR_SIZE];
            if let Err(e) = dev.read_sectors(lba, &mut buf) {
                return ctx.fail(format_args!("blk read: {e}"));
            }
            let mut out = String::with_capacity(buf.len() * 2 + sectors * 128);
            for (i, chunk) in buf.chunks(32).enumerate() {
                let _ = write!(out, "{:04x}: ", i * 32);
                for b in chunk {
                    let _ = write!(out, "{b:02x}");
                }
                out.push('\n');
            }
            write_in_chunks(ctx.out, &out);
        }
        [sub, id_str, lba_str, hex_str, rest @ ..] if sub == "write" && rest.len() <= 1 => {
            let (Some(id), Some(lba), Some(pattern), Some(sectors)) = (
                id_str.parse::<usize>().ok(),
                lba_str.parse::<u64>().ok(),
                decode_hex(hex_str),
                parse_sectors(rest.first()),
            ) else {
                return ctx.fail(USAGE);
            };
            let Some(dev) = device(id) else {
                return ctx.fail(format_args!("blk write: no device {id}"));
            };
            let total = sectors * SECTOR_SIZE;
            let mut buf = vec![0u8; total];
            for (i, dst) in buf.iter_mut().enumerate() {
                *dst = pattern[i % pattern.len()];
            }
            if let Err(e) = dev.write_sectors(lba, &buf) {
                return ctx.fail(format_args!("blk write: {e}"));
            }
            let _ = writeln!(ctx.out, "blk {id}: wrote {sectors} sector(s) at lba {lba}");
        }
        [sub, id_str] if sub == "flush" => {
            let Ok(id) = id_str.parse::<usize>() else {
                return ctx.fail(USAGE);
            };
            let Some(dev) = device(id) else {
                return ctx.fail(format_args!("blk flush: no device {id}"));
            };
            if let Err(e) = dev.flush() {
                return ctx.fail(format_args!("blk flush: {e}"));
            }
            let _ = writeln!(ctx.out, "blk {id}: flushed");
        }
        _ => ctx.fail(USAGE),
    }
}

crate::ktest!(
    fn virtio_blk_reads_writes_flushes_and_checks_bounds() {
        use crate::{kassert, kassert_eq};
        use lumen_bcache::BlockError;

        let dev = device(0).ok_or("no bound virtio-blk device 0")?;
        let info = dev.info();
        kassert_eq!(info.sector_size, SECTOR_SIZE as u32);
        kassert!(info.sector_count >= 2048);
        kassert!(!info.read_only);

        let before_runs = crate::virtio::outstanding_runs();

        // Bounds & alignment checks.
        let mut one = [0u8; SECTOR_SIZE];
        kassert_eq!(dev.read_sectors(info.sector_count, &mut one), Err(BlockError::OutOfRange));
        kassert_eq!(dev.read_sectors(u64::MAX, &mut one), Err(BlockError::OutOfRange));
        kassert_eq!(dev.read_sectors(0, &mut []), Err(BlockError::Unaligned));
        kassert_eq!(dev.read_sectors(0, &mut [0u8; 511]), Err(BlockError::Unaligned));
        kassert_eq!(dev.write_sectors(info.sector_count, &one), Err(BlockError::OutOfRange));
        kassert_eq!(dev.write_sectors(0, &[]), Err(BlockError::Unaligned));

        // Multi-sector (> 4 KiB) round-trip exercising <= 4 KiB chunking.
        let sectors = 10usize;
        let test_lba = 128;
        let mut orig = vec![0u8; sectors * SECTOR_SIZE];
        dev.read_sectors(test_lba, &mut orig).map_err(|_| "read orig failed")?;

        let mut pattern = vec![0u8; sectors * SECTOR_SIZE];
        for (i, b) in pattern.iter_mut().enumerate() {
            *b = ((i * 31) ^ (i >> 8) ^ 0x5a) as u8;
        }
        dev.write_sectors(test_lba, &pattern).map_err(|_| "multi-chunk write failed")?;
        dev.flush().map_err(|_| "flush failed")?;

        let mut read_back = vec![0u8; sectors * SECTOR_SIZE];
        dev.read_sectors(test_lba, &mut read_back).map_err(|_| "multi-chunk read failed")?;
        kassert_eq!(read_back, pattern);

        // Restore original disk contents cleanly.
        dev.write_sectors(test_lba, &orig).map_err(|_| "restore failed")?;
        dev.flush().map_err(|_| "restore flush failed")?;

        kassert_eq!(crate::virtio::outstanding_runs(), before_runs);
        Ok(())
    }
);

crate::ktest!(
    fn block_cache_over_virtio_blk_persists_through_sync() {
        use crate::{kassert, kassert_eq};
        use lumen_bcache::BlockCache;

        let dev = device(0).ok_or("no bound virtio-blk device 0")?;
        let before_runs = crate::virtio::outstanding_runs();

        let off = 200u64 * SECTOR_SIZE as u64 + 17;
        let mut payload = [0u8; 4100];
        for (i, b) in payload.iter_mut().enumerate() {
            *b = (i.wrapping_mul(17) ^ 0x3c) as u8;
        }

        {
            let cache = BlockCache::new(dev.clone(), 4).map_err(|_| "cache init")?;
            cache.write_at(off, &payload).map_err(|_| "cache write_at")?;
            cache.sync().map_err(|_| "cache sync")?;
        }

        let verify_cache = BlockCache::new(dev.clone(), 4).map_err(|_| "verify cache init")?;
        let mut got = [0u8; 4100];
        verify_cache.read_at(off, &mut got).map_err(|_| "cache read_at")?;
        kassert_eq!(&got[..], &payload[..]);

        // The registry's entry for the bound device reports its cache.
        kassert!(devices().iter().any(|e| e.id == 0 && e.stats().capacity > 0));

        kassert_eq!(crate::virtio::outstanding_runs(), before_runs);
        Ok(())
    }
);
