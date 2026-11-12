//! ACPI tables for the Lumen kernel: the RSDP, the root table (the XSDT, or
//! the RSDT on ACPI 1.0 firmware), the MADT (interrupt controllers and CPUs)
//! and the HPET.
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

#[cfg(test)]
mod tests;
