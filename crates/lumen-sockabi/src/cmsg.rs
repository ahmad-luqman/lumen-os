//! `msghdr` ancillary data (`cmsg`) walking and building on Linux x86-64.
//!
//! Follows Linux v6.12 `include/linux/socket.h` (`CMSG_ALIGN`, `CMSG_LEN`,
//! `CMSG_SPACE`, `CMSG_OK`, `__cmsg_nxthdr`) and `net/core/scm.c`
//! (`__scm_send`, `scm_fp_copy`, `put_cmsg`, `scm_detach_fds`):
//! - **Decoding (`CmsgIter`, `Cmsg::rights_fds`, `count_rights_fds`,
//!   `Cmsg::decode_creds`):** enforces `CMSG_OK` (`EINVAL` for malformed
//!   `cmsg_len`), exact 12-byte `SCM_CREDENTIALS`, and `SCM_MAX_FD` (253) both
//!   per `SCM_RIGHTS` header and cumulatively across a message via
//!   [`count_rights_fds`].
//! - **Building (`CmsgWriter`):** on `recvmsg`, callers install `SCM_RIGHTS`
//!   in `scm_detach_fds` order:
//!   1. Query [`CmsgWriter::rights_capacity`] (`fdmax`) before installing any
//!      descriptors into the receiver's FD table.
//!   2. Install at most `min(total, capacity)` descriptors (stopping early on
//!      `EMFILE`, with `FD_CLOEXEC` when `MSG_CMSG_CLOEXEC` is set).
//!   3. Call [`CmsgWriter::push_rights`] with the installed fds and `total`;
//!      if fewer than `total` fds fit (including an early `EMFILE` stop),
//!      `MSG_CTRUNC` is set; the caller closes any unwritten `fds[ret..]` and
//!      drops any uninstalled pending files.

use core::slice::Iter;

use crate::Errno;
use crate::abi::{
    CMSG_ALIGN_SIZE, CMSG_HDRLEN, MSG_CTRUNC, SCM_CREDENTIALS, SCM_MAX_FD, SCM_RIGHTS, SOL_SOCKET, UCRED_LEN,
};

/// Peer credentials (`struct ucred`: 12 bytes, `pid_t` is signed `i32`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Ucred {
    pub pid: i32,
    pub uid: u32,
    pub gid: u32,
}

impl Ucred {
    /// Decode a 12-byte little-endian `struct ucred`.
    pub const fn from_bytes(bytes: &[u8; UCRED_LEN]) -> Self {
        Self {
            pid: i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            uid: u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            gid: u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
        }
    }

    /// Encode into a 12-byte little-endian `struct ucred`.
    pub fn to_bytes(&self) -> [u8; UCRED_LEN] {
        let mut out = [0u8; UCRED_LEN];
        out[0..4].copy_from_slice(&self.pid.to_le_bytes());
        out[4..8].copy_from_slice(&self.uid.to_le_bytes());
        out[8..12].copy_from_slice(&self.gid.to_le_bytes());
        out
    }
}

/// Align `len` up to x86-64's 8-byte `CMSG_ALIGN`, returning `None` on overflow.
pub const fn cmsg_align(len: usize) -> Option<usize> {
    match len.checked_add(CMSG_ALIGN_SIZE - 1) {
        Some(n) => Some(n & !(CMSG_ALIGN_SIZE - 1)),
        None => None,
    }
}

/// `CMSG_LEN(data_len)`: 16-byte header plus unpadded payload length.
pub const fn cmsg_len(data_len: usize) -> Option<usize> {
    CMSG_HDRLEN.checked_add(data_len)
}

/// `CMSG_SPACE(data_len)`: 16-byte header plus 8-byte-aligned payload length.
pub const fn cmsg_space(data_len: usize) -> Option<usize> {
    match cmsg_align(data_len) {
        Some(aligned) => CMSG_HDRLEN.checked_add(aligned),
        None => None,
    }
}

/// One parsed control message from a user `msg_control` buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cmsg<'a> {
    pub level: i32,
    pub cmsg_type: i32,
    pub data: &'a [u8],
}

impl<'a> Cmsg<'a> {
    /// Parse an `SCM_RIGHTS` payload's file descriptor numbers.
    ///
    /// Matches Linux `net/core/scm.c` `scm_fp_copy`: `count = data.len() / 4`,
    /// rejecting `count > SCM_MAX_FD` (253) with `EINVAL`. For a whole control
    /// buffer that may contain multiple `SCM_RIGHTS` headers, also check the
    /// cumulative total with [`count_rights_fds`].
    pub fn rights_fds(&self) -> Result<RightsIter<'a>, Errno> {
        if self.level != SOL_SOCKET || self.cmsg_type != SCM_RIGHTS {
            return Err(Errno::EINVAL);
        }
        let (chunks, _) = self.data.as_chunks::<4>();
        if chunks.len() > SCM_MAX_FD {
            return Err(Errno::EINVAL);
        }
        Ok(RightsIter { chunks: chunks.iter() })
    }

    /// Parse an `SCM_CREDENTIALS` payload.
    ///
    /// Matches Linux `net/core/scm.c` `__scm_send`: `cmsg_len` must equal
    /// `CMSG_LEN(sizeof(struct ucred))` (data length exactly 12), else `EINVAL`.
    pub fn decode_creds(&self) -> Result<Ucred, Errno> {
        if self.level != SOL_SOCKET || self.cmsg_type != SCM_CREDENTIALS {
            return Err(Errno::EINVAL);
        }
        let arr: &[u8; UCRED_LEN] = self.data.try_into().map_err(|_| Errno::EINVAL)?;
        Ok(Ucred::from_bytes(arr))
    }
}

/// Count the cumulative `SCM_RIGHTS` file descriptors across all control
/// messages in `buf`, validating every header and rejecting totals above
/// [`SCM_MAX_FD`] (253) with `EINVAL` (`net/core/scm.c` `scm_fp_copy`).
pub fn count_rights_fds(buf: &[u8]) -> Result<usize, Errno> {
    let mut total = 0usize;
    for item in CmsgIter::new(buf) {
        let cmsg = item?;
        if cmsg.level == SOL_SOCKET && cmsg.cmsg_type == SCM_RIGHTS {
            let count = cmsg.rights_fds()?.len();
            total += count;
            if total > SCM_MAX_FD {
                return Err(Errno::EINVAL);
            }
        }
    }
    Ok(total)
}

/// Iterator over little-endian `i32` file descriptors in an `SCM_RIGHTS` payload.
#[derive(Debug, Clone)]
pub struct RightsIter<'a> {
    chunks: Iter<'a, [u8; 4]>,
}

impl<'a> Iterator for RightsIter<'a> {
    type Item = i32;

    fn next(&mut self) -> Option<i32> {
        self.chunks.next().copied().map(i32::from_le_bytes)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.chunks.size_hint()
    }
}

impl ExactSizeIterator for RightsIter<'_> {}

/// Iterator over control messages in a user `msg_control` buffer.
///
/// Mirrors Linux's `for_each_cmsghdr` + `CMSG_OK`:
/// - Stops cleanly when fewer than `CMSG_HDRLEN` (16) bytes remain.
/// - Yields `Err(Errno::EINVAL)` and fuses if a header's `cmsg_len` is `< 16`
///   or exceeds the remaining bytes in the buffer.
/// - Advances by `CMSG_ALIGN(cmsg_len)` (8-byte boundary on x86-64).
#[derive(Debug, Clone)]
pub struct CmsgIter<'a> {
    buf: &'a [u8],
    offset: usize,
}

impl<'a> CmsgIter<'a> {
    /// Create a control-buffer iterator over `buf`.
    pub const fn new(buf: &'a [u8]) -> Self {
        Self { buf, offset: 0 }
    }
}

impl<'a> Iterator for CmsgIter<'a> {
    type Item = Result<Cmsg<'a>, Errno>;

    fn next(&mut self) -> Option<Self::Item> {
        let rest = &self.buf[self.offset..];
        let rem = rest.len();
        // `CMSG_FIRSTHDR` / `__cmsg_nxthdr`: stop when another cmsghdr cannot fit.
        let Some(hdr) = rest.first_chunk::<CMSG_HDRLEN>() else {
            self.offset = self.buf.len();
            return None;
        };
        let raw_len = u64::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3], hdr[4], hdr[5], hdr[6], hdr[7]]);
        let level = i32::from_le_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]);
        let cmsg_type = i32::from_le_bytes([hdr[12], hdr[13], hdr[14], hdr[15]]);

        // `CMSG_OK`: cmsg_len must be >= sizeof(struct cmsghdr) and <= remaining bytes.
        let Some(cmsg_len) = usize::try_from(raw_len).ok().filter(|&n| n >= CMSG_HDRLEN && n <= rem) else {
            self.offset = self.buf.len();
            return Some(Err(Errno::EINVAL));
        };

        let data = &rest[CMSG_HDRLEN..cmsg_len];

        match cmsg_align(cmsg_len).and_then(|step| self.offset.checked_add(step)) {
            Some(next) if next <= self.buf.len() => self.offset = next,
            _ => self.offset = self.buf.len(),
        }

        Some(Ok(Cmsg { level, cmsg_type, data }))
    }
}

/// Builder filling a user-sized `msg_control` buffer for `recvmsg`.
///
/// Tracks the byte length written (`msg_controllen`) and flags (`MSG_CTRUNC`).
/// Alignment padding bytes written into the buffer are always zeroed.
#[derive(Debug)]
pub struct CmsgWriter<'a> {
    buf: &'a mut [u8],
    offset: usize,
    flags: u32,
}

impl<'a> CmsgWriter<'a> {
    /// Start writing control messages into `buf`.
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, offset: 0, flags: 0 }
    }

    /// Bytes remaining in the control buffer.
    pub const fn remaining(&self) -> usize {
        self.buf.len() - self.offset
    }

    /// Maximum number of `SCM_RIGHTS` file descriptors that can fit in the
    /// remaining buffer (`remaining().saturating_sub(16) / 4`), matching
    /// Linux `net/core/scm.c` `scm_detach_fds`'s `fdmax` calculation.
    ///
    /// Callers compute `fdmax = writer.rights_capacity()`, install at most
    /// `min(total, fdmax)` fds (stopping early on `EMFILE`), and then pass the
    /// installed slice and `total` to [`Self::push_rights`].
    pub const fn rights_capacity(&self) -> usize {
        self.remaining().saturating_sub(CMSG_HDRLEN) / 4
    }

    /// Total bytes written so far (the `msg_controllen` to report back).
    pub const fn written(&self) -> usize {
        self.offset
    }

    /// Output `msg_flags` accumulated by this writer (`0` or `MSG_CTRUNC`).
    pub const fn flags(&self) -> u32 {
        self.flags
    }

    /// Whether any control message was truncated or dropped (`MSG_CTRUNC`).
    pub const fn truncated(&self) -> bool {
        self.flags & MSG_CTRUNC != 0
    }

    /// Mark the control buffer truncated (`MSG_CTRUNC`).
    pub fn mark_truncated(&mut self) {
        self.flags |= MSG_CTRUNC;
    }

    /// Finish writing and return `(written_len, msg_flags)`.
    pub const fn finish(self) -> (usize, u32) {
        (self.offset, self.flags)
    }

    /// Write `SCM_RIGHTS` descriptors into the remaining buffer, returning how
    /// many were written (`ret`).
    ///
    /// Callers must pre-cap installation with [`Self::rights_capacity`]
    /// (`debug_assert!(fds.len() <= self.rights_capacity())`), and close any
    /// unwritten `fds[ret..]` if `ret < fds.len()`. `total` is the total number
    /// of pending descriptors in the received message (which may exceed
    /// `fds.len()` if installation stopped early on `EMFILE` or was capped by
    /// `rights_capacity()`).
    ///
    /// Matches Linux `net/core/scm.c` `scm_detach_fds`:
    /// - If `fds.is_empty()` and `total == 0`, writes nothing and leaves
    ///   `MSG_CTRUNC` clear.
    /// - If `fit = min(fds.len(), self.rights_capacity()) == 0`, writes nothing
    ///   (no empty header), sets `MSG_CTRUNC`, and returns `0`.
    /// - Otherwise writes `fit` descriptors, zeroes any trailing alignment
    ///   padding up to `min(CMSG_SPACE(fit * 4), rem)`, sets `MSG_CTRUNC` when
    ///   `fit < total` or `fit < fds.len()`, and returns `fit`.
    pub fn push_rights(&mut self, fds: &[i32], total: usize) -> usize {
        debug_assert!(fds.len() <= self.rights_capacity(), "caller must cap installed fds by rights_capacity()");
        let required = total.max(fds.len());
        if required == 0 {
            return 0;
        }
        let fit = fds.len().min(self.rights_capacity());
        if fit == 0 {
            self.mark_truncated();
            return 0;
        }
        let data_len = fit * 4;
        let len = CMSG_HDRLEN + data_len;
        self.write_header(len, SOL_SOCKET, SCM_RIGHTS);
        let data_start = self.offset + CMSG_HDRLEN;
        for (i, &fd) in fds[..fit].iter().enumerate() {
            let slot = data_start + i * 4;
            self.buf[slot..slot + 4].copy_from_slice(&fd.to_le_bytes());
        }
        self.advance(len, data_len);
        if fit < required {
            self.mark_truncated();
        }
        fit
    }

    /// Write an `SCM_CREDENTIALS` control message following Linux `put_cmsg`.
    ///
    /// Returns `true` if the full 12-byte `ucred` fit, or `false` if truncated
    /// or dropped (in which case `MSG_CTRUNC` is set).
    pub fn push_creds(&mut self, creds: Ucred) -> bool {
        let bytes = creds.to_bytes();
        self.push_raw(SOL_SOCKET, SCM_CREDENTIALS, &bytes)
    }

    /// Write an arbitrary control message following Linux `net/core/scm.c` `put_cmsg`.
    ///
    /// - If fewer than `CMSG_HDRLEN` (16) bytes remain, writes nothing, sets
    ///   `MSG_CTRUNC`, and returns `false`.
    /// - If `CMSG_HDRLEN <= rem < CMSG_LEN(data.len())`, writes a header with
    ///   `cmsg_len = rem`, copies the prefix `data[..rem - 16]`, sets
    ///   `MSG_CTRUNC`, and returns `false`.
    /// - Otherwise writes the full message, zeroes alignment padding up to
    ///   `min(CMSG_SPACE(data.len()), rem)`, and returns `true`.
    pub fn push_raw(&mut self, level: i32, cmsg_type: i32, data: &[u8]) -> bool {
        let rem = self.remaining();
        if rem < CMSG_HDRLEN {
            self.mark_truncated();
            return false;
        }
        let Some(full_len) = cmsg_len(data.len()) else {
            self.mark_truncated();
            return false;
        };
        let (written_len, truncated) = if rem < full_len {
            self.mark_truncated();
            (rem, true)
        } else {
            (full_len, false)
        };
        let copy_data = written_len - CMSG_HDRLEN;
        self.write_header(written_len, level, cmsg_type);
        let data_start = self.offset + CMSG_HDRLEN;
        self.buf[data_start..data_start + copy_data].copy_from_slice(&data[..copy_data]);
        self.advance(written_len, data.len());
        !truncated
    }

    fn write_header(&mut self, len: usize, level: i32, cmsg_type: i32) {
        let hdr = &mut self.buf[self.offset..self.offset + CMSG_HDRLEN];
        hdr[0..8].copy_from_slice(&(len as u64).to_le_bytes());
        hdr[8..12].copy_from_slice(&level.to_le_bytes());
        hdr[12..16].copy_from_slice(&cmsg_type.to_le_bytes());
    }

    fn advance(&mut self, written_len: usize, data_len: usize) {
        let rem = self.remaining();
        let step = cmsg_space(data_len).map_or(rem, |space| space.min(rem));
        self.buf[self.offset + written_len..self.offset + step].fill(0);
        self.offset += step;
    }
}
