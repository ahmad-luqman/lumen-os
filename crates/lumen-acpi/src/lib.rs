//! ACPI tables for the Lumen kernel: the RSDP, the root table (the XSDT, or
//! the RSDT on ACPI 1.0 firmware), the MADT (interrupt controllers and CPUs),
//! the HPET, and what powering off takes: the FADT's PM1 control registers
//! and the DSDT's `\_S5_` sleep type values ([`find_s5`] searches the AML
//! bytes for the package: no AML interpreter builds on stable Rust).
//!
//! Firmware tables are packed and unaligned, so everything is parsed from
//! byte slices with bounds-checked little-endian reads, never pointer casts.
//! A table that is used has its length range-checked as soon as its header
//! is read, and its checksum before any field past the header is used (the
//! RSDP's revision and length come before its checksum: they say how much to
//! sum). A bad
//! table gives an [`Error`], never a panic. Physical memory is reached
//! through [`PhysMem`]: the kernel implements it with its page tables, the
//! tests with a fake memory.
//!
//! Nothing here allocates: tables are borrowed (`'a` is how long the memory
//! behind them stays readable) and MADT entries are decoded on the fly.

#![no_std]

use core::fmt;

/// Size of the header every system description table starts with.
pub const SDT_HEADER_LEN: usize = 36;
/// The longest table we accept. The kernel maps a table's whole length, so
/// a corrupt length field must not ask for gigabytes; real MADTs and HPETs
/// are a few KiB at most (even the DSDT rarely exceeds a few hundred KiB).
pub const MAX_TABLE_LEN: u32 = 1 << 20;
/// The longest RSDP we accept (ACPI 2.0+ defines 36 bytes).
const MAX_RSDP_LEN: u32 = 4096;
const RSDP_V1_LEN: usize = 20;
const RSDP_V2_LEN: usize = 36;
const MADT_FIXED_LEN: usize = SDT_HEADER_LEN + 8;
const HPET_LEN: usize = 56;
/// An ACPI 1.0 FADT, up to and including the fields we read from it.
const FADT_V1_LEN: usize = 116;

/// Physical memory as the parser sees it.
pub trait PhysMem<'a> {
    /// Exactly the `len` bytes at physical address `addr`, or `None` if they
    /// can't be read (a shorter slice counts as unreadable too).
    fn read(&mut self, addr: u64, len: usize) -> Option<&'a [u8]>;
}

impl<'a, M: PhysMem<'a> + ?Sized> PhysMem<'a> for &mut M {
    fn read(&mut self, addr: u64, len: usize) -> Option<&'a [u8]> {
        (**self).read(addr, len)
    }
}

/// A table's four-byte signature, such as `APIC` for the MADT.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Signature(pub [u8; 4]);

impl Signature {
    pub const XSDT: Signature = Signature(*b"XSDT");
    pub const RSDT: Signature = Signature(*b"RSDT");
    pub const MADT: Signature = Signature(*b"APIC");
    pub const HPET: Signature = Signature(*b"HPET");
    pub const FADT: Signature = Signature(*b"FACP");
    pub const DSDT: Signature = Signature(*b"DSDT");
    /// Not a table signature: names the RSDP in errors.
    pub const RSDP: Signature = Signature(*b"RSDP");
}

impl fmt::Display for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|&b| {
            use fmt::Write;
            f.write_char(if b.is_ascii_graphic() || b == b' ' { b as char } else { '?' })
        })
    }
}

impl fmt::Debug for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "\"{self}\"")
    }
}

/// Why a table was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The `len` bytes at physical address `addr` couldn't be read.
    Unreadable { addr: u64, len: usize },
    /// The RSDP doesn't start with `"RSD PTR "`.
    BadRsdpSignature,
    /// A table has another signature than the one asked for.
    WrongSignature { expected: Signature, found: Signature },
    /// A structure's bytes don't sum to zero.
    BadChecksum(Signature),
    /// Fewer bytes than the structure's fixed fields or its length field
    /// say, or a root table whose body isn't a whole number of entries.
    Truncated(Signature),
    /// A length field below the structure's header or above our cap.
    BadLength { sig: Signature, len: u32 },
    /// The RSDP points at no root table (both addresses are 0).
    NoRootTable,
    /// The root table lists no table with this signature. `skipped` is the
    /// address of the first listed table whose header couldn't be read, if
    /// any (it might have been the one).
    NotFound { sig: Signature, skipped: Option<u64> },
    /// The MADT entry at byte `offset` is shorter than 2 bytes or its type
    /// needs, or runs past the table's end (`len` is 0 if the table ends
    /// before the entry's length byte).
    BadMadtEntry { offset: usize, kind: u8, len: u8 },
    /// The HPET's registers aren't in memory space (`space` is the generic
    /// address structure's address space id).
    HpetNotInMemory { space: u8 },
    /// The HPET's register address is 0.
    HpetAddressZero,
    /// The FADT gives no PM1a control block.
    NoPm1aControl,
    /// A PM1 control block is outside I/O space (`space` is the generic
    /// address structure's address space id; memory-mapped PM1 blocks
    /// aren't supported).
    Pm1NotIo { space: u8 },
    /// A PM1 control block's I/O port doesn't fit 16 bits.
    BadPm1Port(u64),
    /// The FADT says the machine is hardware-reduced ACPI (no PM1 blocks;
    /// its sleep control register isn't supported).
    HardwareReduced,
    /// The FADT gives no DSDT address.
    NoDsdt,
    /// The AML holds no `_S5_` object we can decode (`bad` counts the
    /// `_S5_` names found that weren't a package of sleep type values).
    NoS5 { bad: usize },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Error::Unreadable { addr, len } => write!(f, "can't read {len} bytes at {addr:#x}"),
            Error::BadRsdpSignature => f.write_str("RSDP signature is not \"RSD PTR \""),
            Error::WrongSignature { expected, found } => write!(f, "expected table {expected}, found {found}"),
            Error::BadChecksum(sig) => write!(f, "{sig} checksum is wrong"),
            Error::Truncated(sig) => write!(f, "{sig} is truncated"),
            Error::BadLength { sig, len } => write!(f, "{sig} length {len} is out of range"),
            Error::NoRootTable => f.write_str("RSDP has neither an XSDT nor an RSDT address"),
            Error::NotFound { sig, skipped: None } => write!(f, "no {sig} table"),
            Error::NotFound { sig, skipped: Some(addr) } => {
                write!(f, "no {sig} table (skipped an unreadable table at {addr:#x})")
            }
            Error::BadMadtEntry { offset, kind, len } => {
                write!(f, "MADT entry at offset {offset} (type {kind}, length {len}) is malformed")
            }
            Error::HpetNotInMemory { space } => write!(f, "HPET registers in address space {space}, not memory"),
            Error::HpetAddressZero => f.write_str("HPET register address is 0"),
            Error::NoPm1aControl => f.write_str("the FADT gives no PM1a control block"),
            Error::Pm1NotIo { space } => write!(f, "a PM1 control block is in address space {space}, not I/O"),
            Error::BadPm1Port(port) => write!(f, "PM1 control block port {port:#x} doesn't fit 16 bits"),
            Error::HardwareReduced => f.write_str("hardware-reduced ACPI (no PM1 control blocks) isn't supported"),
            Error::NoDsdt => f.write_str("the FADT gives no DSDT address"),
            Error::NoS5 { bad: 0 } => f.write_str("the DSDT has no _S5_ object"),
            Error::NoS5 { bad } => write!(f, "the DSDT's _S5_ isn't a package of sleep types ({bad} found)"),
        }
    }
}

fn u8_at(b: &[u8], off: usize) -> Option<u8> {
    b.get(off).copied()
}

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(*b.get(off..)?.first_chunk()?))
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(*b.get(off..)?.first_chunk()?))
}

fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(*b.get(off..)?.first_chunk()?))
}

fn checksum_ok(b: &[u8]) -> bool {
    b.iter().fold(0u8, |s, &x| s.wrapping_add(x)) == 0
}

fn read<'a>(mem: &mut impl PhysMem<'a>, addr: u64, len: usize) -> Result<&'a [u8], Error> {
    match mem.read(addr, len) {
        Some(b) if b.len() == len => Ok(b),
        _ => Err(Error::Unreadable { addr, len }),
    }
}

/// The Root System Description Pointer: where the root table is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rsdp {
    /// 0 for ACPI 1.0, 2 for ACPI 2.0 and later. (Any value below 2 is
    /// read as 1.0, any other as 2.0+.)
    pub revision: u8,
    /// The RSDT's physical address (32-bit pointers); `None` if 0.
    pub rsdt: Option<u32>,
    /// The XSDT's physical address (64-bit pointers), from revision 2 on;
    /// `None` before that or if 0.
    pub xsdt: Option<u64>,
}

impl Rsdp {
    /// Parse an RSDP from `b`, which must hold all of it (20 bytes below
    /// revision 2, the length field's worth from revision 2 on). Checks the
    /// signature, the first 20 bytes' checksum and, from revision 2 on, that
    /// the length field is in range and the whole structure's checksum.
    pub fn parse(b: &[u8]) -> Result<Rsdp, Error> {
        let v1 = b.get(..RSDP_V1_LEN).ok_or(Error::Truncated(Signature::RSDP))?;
        if &v1[..8] != b"RSD PTR " {
            return Err(Error::BadRsdpSignature);
        }
        if !checksum_ok(v1) {
            return Err(Error::BadChecksum(Signature::RSDP));
        }
        let revision = v1[15];
        let rsdt = u32_at(v1, 16).filter(|&a| a != 0);
        if revision < 2 {
            return Ok(Rsdp { revision, rsdt, xsdt: None });
        }
        let full = b.get(..rsdp_v2_len(b)? as usize).ok_or(Error::Truncated(Signature::RSDP))?;
        if !checksum_ok(full) {
            return Err(Error::BadChecksum(Signature::RSDP));
        }
        let xsdt = u64_at(full, 24).filter(|&a| a != 0);
        Ok(Rsdp { revision, rsdt, xsdt })
    }

    /// Read and parse the RSDP at physical address `addr`, reading no more
    /// than its (range-checked) length field says.
    pub fn read<'a>(mem: &mut impl PhysMem<'a>, addr: u64) -> Result<Rsdp, Error> {
        let head = read(mem, addr, RSDP_V1_LEN)?;
        if head[15] < 2 {
            return Rsdp::parse(head);
        }
        let len = rsdp_v2_len(read(mem, addr, RSDP_V1_LEN + 4)?)?;
        Rsdp::parse(read(mem, addr, len as usize)?)
    }
}

/// The length field of a revision 2+ RSDP, range-checked.
fn rsdp_v2_len(b: &[u8]) -> Result<u32, Error> {
    let len = u32_at(b, 20).ok_or(Error::Truncated(Signature::RSDP))?;
    if !(RSDP_V2_LEN as u32..=MAX_RSDP_LEN).contains(&len) {
        return Err(Error::BadLength { sig: Signature::RSDP, len });
    }
    Ok(len)
}

/// A system description table whose length and checksum have been checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sdt<'a> {
    bytes: &'a [u8],
}

impl<'a> Sdt<'a> {
    /// Check the table at the start of `bytes` (which may run on past it):
    /// its length field must lie between the header size and
    /// [`MAX_TABLE_LEN`] and within `bytes`, and its bytes must sum to zero.
    pub fn parse(bytes: &'a [u8]) -> Result<Sdt<'a>, Error> {
        let (sig, len) = header(bytes)?;
        let bytes = bytes.get(..len as usize).ok_or(Error::Truncated(sig))?;
        if !checksum_ok(bytes) {
            return Err(Error::BadChecksum(sig));
        }
        Ok(Sdt { bytes })
    }

    /// Read and check the table at physical address `addr`.
    pub fn read(mem: &mut impl PhysMem<'a>, addr: u64) -> Result<Sdt<'a>, Error> {
        let (_, len) = header(read(mem, addr, SDT_HEADER_LEN)?)?;
        Sdt::parse(read(mem, addr, len as usize)?)
    }

    pub fn signature(&self) -> Signature {
        Signature(*self.bytes.first_chunk().expect("checked by parse: at least a header"))
    }

    pub fn revision(&self) -> u8 {
        self.bytes[8]
    }

    /// The whole table, header included.
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// The table after its header.
    pub fn body(&self) -> &'a [u8] {
        &self.bytes[SDT_HEADER_LEN..]
    }

    fn expect(self, expected: Signature) -> Result<Sdt<'a>, Error> {
        let found = self.signature();
        if found == expected { Ok(self) } else { Err(Error::WrongSignature { expected, found }) }
    }
}

/// The signature and the (range-checked) length field of a table header.
fn header(b: &[u8]) -> Result<(Signature, u32), Error> {
    let sig = Signature(*b.first_chunk().ok_or(Error::Truncated(Signature(*b"????")))?);
    if b.len() < SDT_HEADER_LEN {
        return Err(Error::Truncated(sig));
    }
    let len = u32_at(b, 4).expect("within the header");
    if !(SDT_HEADER_LEN as u32..=MAX_TABLE_LEN).contains(&len) {
        return Err(Error::BadLength { sig, len });
    }
    Ok((sig, len))
}

/// The ACPI tables reachable from an RSDP.
pub struct Acpi<'a, M> {
    mem: M,
    rsdp: Rsdp,
    /// The XSDT or the RSDT (which one says how wide its entries are).
    root: Sdt<'a>,
}

impl<'a, M: PhysMem<'a>> Acpi<'a, M> {
    /// Read the RSDP at `rsdp_addr` and the root table it points at: the
    /// XSDT when there is one, else the RSDT. (A broken XSDT is an error
    /// even if there is an RSDT.)
    pub fn new(mut mem: M, rsdp_addr: u64) -> Result<Self, Error> {
        let rsdp = Rsdp::read(&mut mem, rsdp_addr)?;
        let (addr, sig) = match (rsdp.xsdt, rsdp.rsdt) {
            (Some(xsdt), _) => (xsdt, Signature::XSDT),
            (None, Some(rsdt)) => (u64::from(rsdt), Signature::RSDT),
            (None, None) => return Err(Error::NoRootTable),
        };
        let root = Sdt::read(&mut mem, addr)?.expect(sig)?;
        let acpi = Acpi { mem, rsdp, root };
        if root.body().len() % acpi.entry_size() != 0 {
            return Err(Error::Truncated(sig));
        }
        Ok(acpi)
    }

    pub fn rsdp(&self) -> Rsdp {
        self.rsdp
    }

    /// The XSDT or RSDT.
    pub fn root(&self) -> Sdt<'a> {
        self.root
    }

    /// 8 for the XSDT's 64-bit pointers, 4 for the RSDT's.
    fn entry_size(&self) -> usize {
        if self.root.signature() == Signature::XSDT { 8 } else { 4 }
    }

    /// The physical addresses of the tables the root table lists (including
    /// any null entries).
    pub fn table_addresses(&self) -> impl Iterator<Item = u64> + use<'a, M> {
        let size = self.entry_size();
        self.root.body().chunks_exact(size).map(move |c| match size {
            8 => u64_at(c, 0).expect("8-byte chunk"),
            _ => u64::from(u32_at(c, 0).expect("4-byte chunk")),
        })
    }

    /// The first listed table with signature `sig`. Reads each listed
    /// table's header until one matches, skipping null entries and tables
    /// whose header can't be read (only the table returned has its length
    /// and checksum checked). If that table's length or checksum is bad, or
    /// it can't be read in full, that is the error: later tables with the
    /// same signature are not tried.
    pub fn find(&mut self, sig: Signature) -> Result<Sdt<'a>, Error> {
        let mut skipped = None;
        for addr in self.table_addresses().filter(|&a| a != 0) {
            match read(&mut self.mem, addr, SDT_HEADER_LEN) {
                Ok(head) if head.starts_with(&sig.0) => {
                    let (_, len) = header(head)?;
                    return Sdt::parse(read(&mut self.mem, addr, len as usize)?);
                }
                Ok(_) => {}
                Err(_) => {
                    skipped.get_or_insert(addr);
                }
            }
        }
        Err(Error::NotFound { sig, skipped })
    }

    /// The signatures of the listed tables, in order (null entries left out).
    pub fn signatures(&mut self) -> impl Iterator<Item = Result<Signature, Error>> + '_ {
        self.table_addresses().filter(|&a| a != 0).map(|addr| {
            let head = read(&mut self.mem, addr, SDT_HEADER_LEN)?;
            Ok(Signature(*head.first_chunk().expect("a whole header")))
        })
    }

    /// The memory reader, back.
    pub fn into_mem(self) -> M {
        self.mem
    }
}

/// The Multiple APIC Description Table: the local APIC address, the I/O
/// APICs, interrupt overrides and one entry per CPU. Every entry was checked
/// by [`Madt::parse`].
#[derive(Clone, Copy, Debug)]
pub struct Madt<'a> {
    local_apic: u32,
    flags: u32,
    entries: &'a [u8],
}

/// One MADT entry, as the table has it ([`Madt::cpus`] interprets the CPU
/// entries' flags).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MadtEntry {
    /// Type 0: a CPU with an 8-bit APIC id.
    LocalApic { uid: u8, apic_id: u8, flags: u32 },
    /// Type 1.
    IoApic(IoApic),
    /// Type 2: an ISA IRQ wired to another global system interrupt.
    InterruptOverride(InterruptOverride),
    /// Type 5: the local APIC's 64-bit address, which replaces the 32-bit
    /// one in the table's header.
    LocalApicAddress(u64),
    /// Type 9: a CPU with a 32-bit x2APIC id.
    LocalX2Apic { apic_id: u32, uid: u32, flags: u32 },
    /// Any other type (not decoded yet).
    Other { kind: u8, len: u8 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoApic {
    pub id: u8,
    pub address: u32,
    /// The first global system interrupt it handles.
    pub gsi_base: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InterruptOverride {
    pub bus: u8,
    pub irq: u8,
    pub gsi: u32,
    /// Polarity (bits 0-1) and trigger mode (bits 2-3).
    pub flags: u16,
}

/// Whether a CPU listed in the MADT can be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CpuState {
    /// Ready to use.
    Enabled,
    /// Not enabled, but the OS may enable it (hot-plug).
    OnlineCapable,
    /// Not usable.
    Disabled,
}

impl CpuState {
    /// From an MADT CPU entry's flags: bit 0 enabled, bit 1 online-capable
    /// (meaningful only when bit 0 is clear).
    fn from_flags(flags: u32) -> CpuState {
        match (flags & 1 != 0, flags & 2 != 0) {
            (true, _) => CpuState::Enabled,
            (false, true) => CpuState::OnlineCapable,
            (false, false) => CpuState::Disabled,
        }
    }
}

impl fmt::Display for CpuState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CpuState::Enabled => "enabled",
            CpuState::OnlineCapable => "online-capable",
            CpuState::Disabled => "disabled",
        })
    }
}

/// A CPU listed in the MADT. (Firmware may list one CPU in both a type 0
/// and a type 9 entry; the list is as the table has it.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cpu {
    pub apic_id: u32,
    /// The ACPI processor UID.
    pub uid: u32,
    pub state: CpuState,
}

impl<'a> Madt<'a> {
    /// Check that `t` is an MADT whose entries all fit, each long enough
    /// for its type.
    pub fn parse(t: Sdt<'a>) -> Result<Madt<'a>, Error> {
        let b = t.expect(Signature::MADT)?.bytes();
        let (local_apic, flags) =
            u32_at(b, SDT_HEADER_LEN).zip(u32_at(b, SDT_HEADER_LEN + 4)).ok_or(Error::Truncated(Signature::MADT))?;
        let madt = Madt { local_apic, flags, entries: &b[MADT_FIXED_LEN..] };
        let mut off = 0;
        while off < madt.entries.len() {
            off += decode(madt.entries, off)?.1;
        }
        Ok(madt)
    }

    /// The local APIC's physical address: a type 5 entry's if there is one,
    /// else the header's.
    pub fn local_apic_address(&self) -> u64 {
        self.entries()
            .find_map(|e| if let MadtEntry::LocalApicAddress(a) = e { Some(a) } else { None })
            .unwrap_or(u64::from(self.local_apic))
    }

    /// Whether the machine also has dual 8259 PICs (`PCAT_COMPAT`), which
    /// must be masked before using the APICs.
    pub fn has_8259_pics(&self) -> bool {
        self.flags & 1 != 0
    }

    pub fn entries(&self) -> impl Iterator<Item = MadtEntry> + use<'a> {
        let entries = self.entries;
        let mut off = 0;
        core::iter::from_fn(move || {
            if off >= entries.len() {
                return None;
            }
            let (entry, len) = decode(entries, off).expect("Madt::parse checked every entry");
            off += len;
            Some(entry)
        })
    }

    /// Every CPU listed, whatever its state.
    pub fn cpus(&self) -> impl Iterator<Item = Cpu> + use<'a> {
        self.entries().filter_map(|e| match e {
            MadtEntry::LocalApic { uid, apic_id, flags } => {
                Some(Cpu { apic_id: u32::from(apic_id), uid: u32::from(uid), state: CpuState::from_flags(flags) })
            }
            MadtEntry::LocalX2Apic { apic_id, uid, flags } => {
                Some(Cpu { apic_id, uid, state: CpuState::from_flags(flags) })
            }
            _ => None,
        })
    }

    pub fn io_apics(&self) -> impl Iterator<Item = IoApic> + use<'a> {
        self.entries().filter_map(|e| if let MadtEntry::IoApic(io) = e { Some(io) } else { None })
    }

    pub fn interrupt_overrides(&self) -> impl Iterator<Item = InterruptOverride> + use<'a> {
        self.entries().filter_map(|e| if let MadtEntry::InterruptOverride(o) = e { Some(o) } else { None })
    }
}

/// The entry at `off` in the MADT's entry area and its length.
fn decode(b: &[u8], off: usize) -> Result<(MadtEntry, usize), Error> {
    let (kind, len) = (u8_at(b, off).unwrap_or(0), u8_at(b, off + 1).unwrap_or(0));
    let bad = Error::BadMadtEntry { offset: MADT_FIXED_LEN + off, kind, len };
    // Every entry has its type and length bytes: a length of 0 would never
    // advance, and 1 can't hold the length byte itself.
    if len < 2 {
        return Err(bad);
    }
    let e = b.get(off..off + usize::from(len)).ok_or(bad)?;
    Ok((entry_fields(kind, len, e).ok_or(bad)?, usize::from(len)))
}

/// Decode entry `e` of type `kind`; `None` if it is shorter than its type's
/// fields.
fn entry_fields(kind: u8, len: u8, e: &[u8]) -> Option<MadtEntry> {
    Some(match kind {
        0 => MadtEntry::LocalApic { uid: u8_at(e, 2)?, apic_id: u8_at(e, 3)?, flags: u32_at(e, 4)? },
        1 => MadtEntry::IoApic(IoApic { id: u8_at(e, 2)?, address: u32_at(e, 4)?, gsi_base: u32_at(e, 8)? }),
        2 => MadtEntry::InterruptOverride(InterruptOverride {
            bus: u8_at(e, 2)?,
            irq: u8_at(e, 3)?,
            gsi: u32_at(e, 4)?,
            flags: u16_at(e, 8)?,
        }),
        5 => MadtEntry::LocalApicAddress(u64_at(e, 4)?),
        9 => MadtEntry::LocalX2Apic { apic_id: u32_at(e, 4)?, flags: u32_at(e, 8)?, uid: u32_at(e, 12)? },
        _ => MadtEntry::Other { kind, len },
    })
}

/// The High Precision Event Timer description.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hpet {
    /// Physical address of its registers (never 0).
    pub address: u64,
    /// Which HPET this is (0 for the first).
    pub number: u8,
    /// Minimum period, in counter ticks, for periodic mode without lost
    /// interrupts.
    pub min_tick: u16,
    /// Number of comparators (timers) in the first block.
    pub comparators: u8,
    /// The main counter is 64 bits wide.
    pub counter_64bit: bool,
    /// Can stand in for the PIT and RTC interrupts (legacy replacement).
    pub legacy_replacement: bool,
    pub pci_vendor: u16,
}

impl Hpet {
    pub fn parse(t: Sdt<'_>) -> Result<Hpet, Error> {
        let b = t.expect(Signature::HPET)?.bytes();
        if b.len() < HPET_LEN {
            return Err(Error::Truncated(Signature::HPET));
        }
        let space = b[40];
        if space != 0 {
            return Err(Error::HpetNotInMemory { space });
        }
        let parse = || {
            let id = u32_at(b, 36)?;
            Some(Hpet {
                address: u64_at(b, 44)?,
                number: b[52],
                min_tick: u16_at(b, 53)?,
                comparators: ((id >> 8) & 0x1f) as u8 + 1,
                counter_64bit: id & (1 << 13) != 0,
                legacy_replacement: id & (1 << 15) != 0,
                pci_vendor: (id >> 16) as u16,
            })
        };
        let hpet = parse().ok_or(Error::Truncated(Signature::HPET))?;
        if hpet.address == 0 {
            return Err(Error::HpetAddressZero);
        }
        Ok(hpet)
    }
}

/// What the FADT says about powering off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fadt {
    /// Physical address of the DSDT (`X_DSDT` when the table has it and it
    /// isn't 0, else `DSDT`; never 0).
    pub dsdt: u64,
    /// The PM1a control register (see [`Fadt::parse`] for which field).
    pub pm1a_control: Pm1Control,
    /// The PM1b control register, if there is one.
    pub pm1b_control: Option<Pm1Control>,
    /// How to ask the firmware for ACPI mode, if it can be asked.
    pub acpi_enable: Option<AcpiEnable>,
}

/// A PM1 control register's I/O port, as a FADT gives it: only
/// [`Fadt::parse`] makes one, so the kernel writes sleep values only to a
/// port the firmware named for them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pm1Control(u16);

impl Pm1Control {
    pub fn port(self) -> u16 {
        self.0
    }
}

/// The FADT's `SMI_CMD` port and the `ACPI_ENABLE` value written to it to
/// switch the chipset to ACPI mode. Only [`Fadt::parse`] makes one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AcpiEnable {
    port: u16,
    value: u8,
}

impl AcpiEnable {
    pub fn port(self) -> u16 {
        self.port
    }

    pub fn value(self) -> u8 {
        self.value
    }
}

/// Generic address structure address space ids.
const SPACE_IO: u8 = 1;
/// The FADT's flags (offset 112): bit 20, HW_REDUCED_ACPI.
const HW_REDUCED_ACPI: u32 = 1 << 20;

impl Fadt {
    /// Parse a FADT. Each PM1 control block comes from its 64-bit `X_`
    /// field (a generic address structure) when the table is long enough
    /// for it and its address isn't 0, and that one must be in I/O space
    /// (else [`Error::Pm1NotIo`]); otherwise from the 32-bit field. No PM1a
    /// block, or a hardware-reduced machine, is an error. `acpi_enable` is
    /// `None` when `SMI_CMD` or `ACPI_ENABLE` is 0 (the firmware is always
    /// in ACPI mode) or the port doesn't fit 16 bits.
    pub fn parse(t: Sdt<'_>) -> Result<Fadt, Error> {
        let b = t.expect(Signature::FADT)?.bytes();
        if b.len() < FADT_V1_LEN {
            return Err(Error::Truncated(Signature::FADT));
        }
        let field = |off| u32_at(b, off).expect("within the ACPI 1.0 FADT");
        // HW_REDUCED_ACPI is defined from ACPI 5.0; older tables have the
        // bit reserved (0).
        if field(112) & HW_REDUCED_ACPI != 0 {
            return Err(Error::HardwareReduced);
        }
        // A 64-bit field (ACPI 2.0+), if the table is long enough for it and
        // it isn't 0.
        let extended = |off| u64_at(b, off).filter(|&a| a != 0);
        let block = |gas: usize, legacy: usize| -> Result<Option<Pm1Control>, Error> {
            let addr = match (b.get(gas).copied(), extended(gas + 4)) {
                (Some(space), Some(_)) if space != SPACE_IO => return Err(Error::Pm1NotIo { space }),
                (Some(_), Some(addr)) => addr,
                _ => u64::from(field(legacy)),
            };
            if addr == 0 {
                return Ok(None);
            }
            u16::try_from(addr).map(|p| Some(Pm1Control(p))).map_err(|_| Error::BadPm1Port(addr))
        };
        let pm1a_control = block(172, 64)?.ok_or(Error::NoPm1aControl)?;
        let pm1b_control = block(184, 68)?;
        let dsdt = extended(140).unwrap_or(u64::from(field(40)));
        if dsdt == 0 {
            return Err(Error::NoDsdt);
        }
        let acpi_enable = match (u16::try_from(field(48)), b[52]) {
            (Ok(port), value) if port != 0 && value != 0 => Some(AcpiEnable { port, value }),
            _ => None,
        };
        Ok(Fadt { dsdt, pm1a_control, pm1b_control, acpi_enable })
    }
}

/// A sleep type value for a PM1 control register's 3-bit `SLP_TYP` field:
/// at most 7.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlpTyp(u8);

impl SlpTyp {
    /// `value`, if it fits the field.
    pub fn new(value: u8) -> Option<SlpTyp> {
        (value <= 7).then_some(SlpTyp(value))
    }

    pub fn get(self) -> u8 {
        self.0
    }
}

impl fmt::Display for SlpTyp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The values to write to the PM1a and PM1b control registers' `SLP_TYP`
/// field to enter a sleep state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SleepType {
    pub a: SlpTyp,
    pub b: SlpTyp,
}

impl SleepType {
    /// The S5 (soft off) values in the DSDT `t`: [`find_s5`] on its AML,
    /// once it is checked to be a DSDT.
    pub fn s5_from_dsdt(t: Sdt<'_>) -> Result<SleepType, Error> {
        find_s5(t.expect(Signature::DSDT)?.body())
    }
}

/// AML opcodes and prefixes [`find_s5`] knows.
mod aml {
    pub const ZERO: u8 = 0x00;
    pub const ONE: u8 = 0x01;
    pub const NAME: u8 = 0x08;
    pub const BYTE: u8 = 0x0a;
    pub const WORD: u8 = 0x0b;
    pub const DWORD: u8 = 0x0c;
    pub const QWORD: u8 = 0x0e;
    pub const PACKAGE: u8 = 0x12;
    pub const ROOT: u8 = b'\\';
    pub const PARENT: u8 = b'^';
}

/// Find `Name (_S5_, Package () { SLP_TYPa, SLP_TYPb, ... })` in the AML
/// `aml` (a DSDT's or SSDT's body) by searching its bytes, and decode the
/// first two values. A match must be a `NameOp` (with an optional `\` or
/// `^` prefixes) naming a `PackageOp` whose length fits the AML, holding
/// integer constants (`Zero`, `One`, or a byte, word, dword or qword
/// prefix) of at most 7. A package with a single value holds `SLP_TYPa`
/// in its low byte and `SLP_TYPb` in the next (as some firmware does;
/// nothing above them may be set); later values are ignored. The first
/// `_S5_` that decodes wins.
pub fn find_s5(aml: &[u8]) -> Result<SleepType, Error> {
    let mut bad = 0;
    for at in aml.windows(4).enumerate().filter(|(_, w)| *w == b"_S5_").map(|(i, _)| i) {
        if !named(aml, at) {
            continue;
        }
        match s5_package(aml, at + 4) {
            Some(t) => return Ok(t),
            None => bad += 1,
        }
    }
    Err(Error::NoS5 { bad })
}

/// Whether the name at `at` follows a `NameOp`, past one `\` or any `^`
/// prefixes.
fn named(aml: &[u8], at: usize) -> bool {
    let before = &aml[..at];
    let prefix = match before.last() {
        Some(&aml::ROOT) => 1,
        _ => before.iter().rev().take_while(|&&b| b == aml::PARENT).count(),
    };
    before.len() > prefix && before[before.len() - 1 - prefix] == aml::NAME
}

/// The sleep types in the package starting at `at` (its `PackageOp`).
fn s5_package(aml: &[u8], at: usize) -> Option<SleepType> {
    if aml.get(at) != Some(&aml::PACKAGE) {
        return None;
    }
    let (len, len_bytes) = pkg_length(aml.get(at + 1..)?)?;
    // The package's length counts from its PkgLength, through its end.
    let body = aml.get(at + 1..at + 1 + len)?.get(len_bytes..)?;
    let (&count, mut rest) = body.split_first()?;
    let mut value = || {
        let (v, n) = integer(rest)?;
        rest = &rest[n..];
        Some(v)
    };
    let (a, b) = match count {
        0 => return None,
        1 => {
            let v = value()?;
            if v >> 16 != 0 {
                return None;
            }
            (v & 0xff, v >> 8)
        }
        _ => (value()?, value()?),
    };
    let typ = |v: u64| SlpTyp::new(u8::try_from(v).ok()?);
    Some(SleepType { a: typ(a)?, b: typ(b)? })
}

/// An AML PkgLength at the start of `b`: the length it encodes and how
/// many bytes it took (1 to 4).
fn pkg_length(b: &[u8]) -> Option<(usize, usize)> {
    let lead = *b.first()?;
    let follow = usize::from(lead >> 6);
    if follow == 0 {
        return Some((usize::from(lead & 0x3f), 1));
    }
    // With following bytes, bits 4 and 5 of the lead byte must be 0.
    if lead & 0x30 != 0 {
        return None;
    }
    let mut len = usize::from(lead & 0x0f);
    for (i, &byte) in b.get(1..=follow)?.iter().enumerate() {
        len |= usize::from(byte) << (4 + 8 * i);
    }
    Some((len, 1 + follow))
}

/// An AML integer constant at the start of `b`: its value and its size in
/// bytes.
fn integer(b: &[u8]) -> Option<(u64, usize)> {
    let width = match *b.first()? {
        aml::ZERO => return Some((0, 1)),
        aml::ONE => return Some((1, 1)),
        aml::BYTE => 1,
        aml::WORD => 2,
        aml::DWORD => 4,
        aml::QWORD => 8,
        _ => return None,
    };
    let bytes = b.get(1..1 + width)?;
    let v = bytes.iter().rev().fold(0u64, |v, &byte| v << 8 | u64::from(byte));
    Some((v, 1 + width))
}

#[cfg(test)]
mod tests;
