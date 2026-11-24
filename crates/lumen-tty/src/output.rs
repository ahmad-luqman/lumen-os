use linux_raw_sys::general::*;

/// A serialized output queue. `room` guarantees that the next `push` accepts
/// that many bytes in full. `push` must not partially accept a slice; the
/// caller must hold the same device lock across room/push/flush. Any device
/// backpressure beyond this queue belongs to its owner.
pub trait Output {
    fn room(&self) -> usize;
    fn push(&mut self, bytes: &[u8]);
    fn flush(&mut self);
}

#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Written {
    /// Input bytes accepted, including bytes suppressed by ONOCR. Retry the
    /// remaining suffix when the sink has room; no transformed byte is split.
    pub consumed: usize,
}

/// Facts for the writer's job-control check. All fields must be supplied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WriterFacts {
    /// This terminal controls the caller's session.
    pub controlling_terminal: bool,
    /// The caller's process group differs from the foreground group.
    pub background: bool,
    /// SIGTTOU is blocked in the calling thread.
    pub signal_blocked: bool,
    /// The caller ignores SIGTTOU.
    pub signal_ignored: bool,
    /// The caller's process group is orphaned.
    pub orphaned_group: bool,
}

#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackgroundWrite {
    Allow,
    /// Send SIGTTOU to the writer's own process group; fail with -ERESTARTSYS.
    SendSigttou,
    Eio,
}

/// Job-control decision for writes gated by TOSTOP. Linux applies the same
/// check before TCSETS* regardless of TOSTOP: pass true for those operations.
#[must_use = "the kernel must apply the job-control decision"]
pub fn background_write_signal(tostop: bool, facts: WriterFacts) -> BackgroundWrite {
    if !tostop || !facts.controlling_terminal || !facts.background || facts.signal_blocked || facts.signal_ignored {
        BackgroundWrite::Allow
    } else if facts.orphaned_group {
        BackgroundWrite::Eio
    } else {
        BackgroundWrite::SendSigttou
    }
}

pub(crate) fn is_continuation(byte: u8) -> bool {
    byte & 0xc0 == 0x80
}

fn process(byte: u8, flags: u32, utf8: bool, column: &mut usize) -> ([u8; 2], usize) {
    // Even without postprocessing, echo tab erasure needs the actual column.
    let flags = if flags & OPOST != 0 { flags } else { 0 };
    match byte {
        b'\n' => {
            if flags & (ONLRET | ONLCR) != 0 {
                *column = 0;
            }
            if flags & ONLCR != 0 {
                return (*b"\r\n", 2);
            }
        }
        b'\r' => {
            if flags & ONOCR != 0 && *column == 0 {
                return ([0; 2], 0);
            }
            if flags & OCRNL != 0 {
                if flags & ONLRET != 0 {
                    *column = 0;
                }
                return ([b'\n', 0], 1);
            }
            *column = 0;
        }
        b'\t' => *column = column.saturating_add(8 - *column % 8),
        super::BS => *column = column.saturating_sub(1),
        32..=255 if byte != 127 && !(utf8 && is_continuation(byte)) => *column = column.saturating_add(1),
        _ => {}
    }
    ([byte, 0], 1)
}

/// A source unit is at most three bytes (erase's BS SPACE BS). Transform it
/// speculatively, then commit the entire unit and its column together.
pub(crate) fn push_unit(out: &mut dyn Output, source: &[u8], flags: u32, utf8: bool, column: &mut usize) -> bool {
    let mut bytes = [0; 6];
    let mut len = 0;
    let mut next_column = *column;
    for &byte in source {
        let (processed, n) = process(byte, flags, utf8, &mut next_column);
        bytes[len..len + n].copy_from_slice(&processed[..n]);
        len += n;
    }
    if out.room() < len {
        return false;
    }
    if len != 0 {
        out.push(&bytes[..len]);
    }
    *column = next_column;
    true
}
