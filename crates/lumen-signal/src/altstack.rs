//! Downward-growing x86-64 alternate signal stacks (kernel/signal.c).

use linux_raw_sys::{errno, general};

/// x86-64 UAPI asm/signal.h; unlike the host's value on e.g. aarch64.
pub const MINSIGSTKSZ: u64 = 2048;
#[cfg(target_arch = "x86_64")]
const _: () = assert!(MINSIGSTKSZ == general::MINSIGSTKSZ as u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StackError {
    OnStack,
    TooSmall,
    Invalid,
}

impl StackError {
    pub const fn errno(self) -> i32 {
        -(match self {
            Self::OnStack => errno::EPERM,
            Self::TooSmall => errno::ENOMEM,
            Self::Invalid => errno::EINVAL,
        } as i32)
    }
}

/// A validated configured alternate stack retaining the supplied flags.
/// SS_AUTODISARM makes on-stack reporting false, as Linux
/// on_sig_stack does, and disables the configuration during handler execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AltStack {
    sp: u64,
    size: u64,
    flags: u32,
}

impl Default for AltStack {
    fn default() -> Self {
        Self::DISABLED
    }
}

impl AltStack {
    pub const DISABLED: Self = Self { sp: 0, size: 0, flags: general::SS_DISABLE };

    pub const fn sp(self) -> u64 {
        self.sp
    }
    pub const fn size(self) -> u64 {
        self.size
    }
    pub const fn flags(self) -> u32 {
        self.flags
    }
    pub const fn enabled(self) -> bool {
        self.size != 0
    }
    pub const fn top(self) -> u64 {
        self.sp + self.size
    }

    /// Physical membership for a downward-growing stack: base < RSP <= top.
    /// This remains true for an armed AUTODISARM stack, unlike on_stack.
    pub const fn contains(self, rsp: u64) -> bool {
        self.enabled() && rsp > self.sp && rsp - self.sp <= self.size
    }

    pub const fn on_stack(self, rsp: u64) -> bool {
        self.flags & general::SS_AUTODISARM == 0 && self.contains(rsp)
    }

    /// sigaltstack old-value flags: AUTODISARM plus computed current status.
    /// Frames instead save the raw configured flags returned by flags().
    pub const fn status(self, rsp: u64) -> u32 {
        (self.flags & general::SS_AUTODISARM)
            | if !self.enabled() {
                general::SS_DISABLE
            } else if self.on_stack(rsp) {
                general::SS_ONSTACK
            } else {
                0
            }
    }

    /// Change the configuration using sigaltstack rules. SS_ONSTACK is accepted
    /// as an input status bit and stored (Linux do_sigaltstack). Address wrap
    /// or an interval beyond Lumen's user range is refused, stricter than Linux.
    /// The on-stack -EPERM check precedes flag validation (-EINVAL).
    pub fn configure(self, rsp: u64, sp: u64, size: u64, flags: u32) -> Result<Self, StackError> {
        if self.on_stack(rsp) {
            return Err(StackError::OnStack);
        }
        let mode = flags & !general::SS_AUTODISARM;
        if !matches!(mode, 0 | general::SS_ONSTACK | general::SS_DISABLE) {
            return Err(StackError::Invalid);
        }
        if mode == general::SS_DISABLE {
            return Ok(Self { sp: 0, size: 0, flags });
        }
        if size < MINSIGSTKSZ {
            return Err(StackError::TooSmall);
        }
        if sp.checked_add(size).is_none_or(|top| top > crate::frame::USER_MAX) {
            return Err(StackError::Invalid);
        }
        Ok(Self { sp, size, flags })
    }

    #[must_use]
    pub const fn during_handler(self) -> Self {
        if self.flags & general::SS_AUTODISARM != 0 { Self::DISABLED } else { self }
    }
}
