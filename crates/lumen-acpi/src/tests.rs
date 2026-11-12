//! Host tests, on handcrafted tables shaped like QEMU q35's.

extern crate std;

use std::vec;
use std::vec::Vec;

use super::*;

/// Fix a table's checksum byte (offset 9) so its bytes sum to zero.
fn fix_checksum(t: &mut [u8], at: usize) {
    t[at] = 0;
    let sum = t.iter().fold(0u8, |s, &x| s.wrapping_add(x));
    t[at] = sum.wrapping_neg();
}

/// A table with header `sig` and `body`, its length and checksum filled in.
fn table(sig: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut t = vec![0u8; SDT_HEADER_LEN];
    t[..4].copy_from_slice(sig);
    t[8] = 1; // revision
    t[10..16].copy_from_slice(b"LUMEN ");
    t.extend_from_slice(body);
    let len = t.len() as u32;
    t[4..8].copy_from_slice(&len.to_le_bytes());
    fix_checksum(&mut t, 9);
    t
}

/// Rewrite a table's length field to `len` (keeping `len` bytes) and fix its
/// checksum: an internally consistent table cut short.
fn cut(t: &[u8], len: usize) -> Vec<u8> {
    let mut t = t[..len].to_vec();
    t[4..8].copy_from_slice(&(len as u32).to_le_bytes());
    fix_checksum(&mut t, 9);
    t
}

fn rsdp_v1(rsdt: u32) -> Vec<u8> {
    let mut r = vec![0u8; 20];
    r[..8].copy_from_slice(b"RSD PTR ");
    r[9..15].copy_from_slice(b"LUMEN ");
    r[16..20].copy_from_slice(&rsdt.to_le_bytes());
    fix_checksum(&mut r, 8);
    r
}

fn rsdp_v2(rsdt: u32, xsdt: u64) -> Vec<u8> {
    let mut r = rsdp_v1(rsdt);
    r[15] = 2;
    fix_checksum(&mut r, 8);
    r.extend_from_slice(&36u32.to_le_bytes());
    r.extend_from_slice(&xsdt.to_le_bytes());
    r.extend_from_slice(&[0; 4]);
    fix_checksum(&mut r, 32);
    r
}

fn lapic(uid: u8, id: u8, flags: u32) -> Vec<u8> {
    let mut e = vec![0, 8, uid, id];
    e.extend_from_slice(&flags.to_le_bytes());
    e
}

fn ioapic(id: u8, addr: u32, gsi: u32) -> Vec<u8> {
    let mut e = vec![1, 12, id, 0];
    e.extend_from_slice(&addr.to_le_bytes());
    e.extend_from_slice(&gsi.to_le_bytes());
    e
}

fn iso(irq: u8, gsi: u32, flags: u16) -> Vec<u8> {
    let mut e = vec![2, 10, 0, irq];
    e.extend_from_slice(&gsi.to_le_bytes());
    e.extend_from_slice(&flags.to_le_bytes());
    e
}

fn x2apic(id: u32, uid: u32, flags: u32) -> Vec<u8> {
    let mut e = vec![9, 16, 0, 0];
    e.extend_from_slice(&id.to_le_bytes());
    e.extend_from_slice(&flags.to_le_bytes());
    e.extend_from_slice(&uid.to_le_bytes());
    e
}

fn lapic_override(addr: u64) -> Vec<u8> {
    let mut e = vec![5, 12, 0, 0];
    e.extend_from_slice(&addr.to_le_bytes());
    e
}

fn madt_with(entries: &[Vec<u8>]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&0xfee0_0000u32.to_le_bytes());
    body.extend_from_slice(&1u32.to_le_bytes()); // PCAT_COMPAT
    for e in entries {
        body.extend_from_slice(e);
    }
    table(b"APIC", &body)
}

/// Like QEMU q35 with `-smp 2,maxcpus=3`: two enabled CPUs, one hot-pluggable
/// slot, the I/O APIC, its ISA overrides and a local APIC NMI (type 4).
fn qemu_madt() -> Vec<u8> {
    madt_with(&[
        lapic(0, 0, 1),
        lapic(1, 1, 1),
        lapic(2, 2, 0),
        ioapic(0, 0xfec0_0000, 0),
        iso(0, 2, 0),
        iso(5, 5, 0xd),
        iso(9, 9, 0xd),
        vec![4, 6, 0xff, 0, 0, 1],
    ])
}

fn hpet_with(space: u8, id: u32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&id.to_le_bytes());
    body.extend_from_slice(&[space, 64, 0, 0]);
    body.extend_from_slice(&0xfed0_0000u64.to_le_bytes());
    body.push(0); // HPET number
    body.extend_from_slice(&128u16.to_le_bytes());
    body.push(0); // page protection
    table(b"HPET", &body)
}

/// QEMU's HPET: 3 comparators, 64-bit counter, legacy replacement, vendor 0x8086.
const QEMU_HPET_ID: u32 = 0x8086_a201;

/// Physical memory made of separate regions.
struct Mem<'a> {
    regions: &'a [(u64, Vec<u8>)],
    reads: usize,
}

impl<'a> Mem<'a> {
    fn new(regions: &'a [(u64, Vec<u8>)]) -> Self {
        Mem { regions, reads: 0 }
    }
}

impl<'a> PhysMem<'a> for Mem<'a> {
    fn read(&mut self, addr: u64, len: usize) -> Option<&'a [u8]> {
        self.reads += 1;
        self.regions.iter().find_map(|(base, bytes)| {
            let off = usize::try_from(addr.checked_sub(*base)?).ok()?;
            bytes.get(off..off.checked_add(len)?)
        })
    }
}

fn xsdt(addrs: &[u64]) -> Vec<u8> {
    table(b"XSDT", &addrs.iter().flat_map(|a| a.to_le_bytes()).collect::<Vec<_>>())
}

fn rsdt(addrs: &[u32]) -> Vec<u8> {
    table(b"RSDT", &addrs.iter().flat_map(|a| a.to_le_bytes()).collect::<Vec<_>>())
}

const RSDP_AT: u64 = 0x7fbf_d014;
const FACP: &[u8; 4] = b"FACP";

/// An ACPI 2.0 machine: RSDP → XSDT → FACP, MADT, HPET. The XSDT sits at an
/// odd address, so its 64-bit entries are unaligned.
fn machine() -> Vec<(u64, Vec<u8>)> {
    vec![
        // The RSDT is there too (ACPI 2.0 firmware keeps one for old
        // systems), but lists only FACP: the XSDT must win.
        (RSDP_AT, rsdp_v2(0x7fbd_0000, 0x7fbe_0001)),
        (0x7fbd_0000, rsdt(&[0x7fbe_1000])),
        (0x7fbe_0001, xsdt(&[0x7fbe_1000, 0x7fbe_2000, 0x7fbe_3000])),
        (0x7fbe_1000, table(FACP, &[0; 8])),
        (0x7fbe_2000, qemu_madt()),
        (0x7fbe_3000, hpet_with(0, QEMU_HPET_ID)),
    ]
}

#[test]
fn rsdp_revisions_parse() {
    assert_eq!(Rsdp::parse(&rsdp_v1(0x1234)), Ok(Rsdp { revision: 0, rsdt: 0x1234, xsdt: None }));
    assert_eq!(Rsdp::parse(&rsdp_v2(0x1234, 0x5678)), Ok(Rsdp { revision: 2, rsdt: 0x1234, xsdt: Some(0x5678) }));
    assert_eq!(Rsdp::parse(&rsdp_v2(0x1234, 0)).map(|r| r.xsdt), Ok(None));
}

#[test]
fn rsdp_checks_signature_both_checksums_and_length() {
    let mut sig = rsdp_v2(1, 2);
    sig[0] = b'X';
    assert_eq!(Rsdp::parse(&sig), Err(Error::BadRsdpSignature));
    let mut first = rsdp_v2(1, 2);
    first[16] ^= 1; // covered by both checksums
    assert_eq!(Rsdp::parse(&first), Err(Error::BadChecksum(Signature::RSDP)));
    let mut extended = rsdp_v2(1, 2);
    extended[24] ^= 1; // the XSDT address: only the extended checksum covers it
    assert_eq!(Rsdp::parse(&extended), Err(Error::BadChecksum(Signature::RSDP)));
    let mut short = rsdp_v2(1, 2);
    short[20] = 20;
    assert_eq!(Rsdp::parse(&short), Err(Error::BadLength { sig: Signature::RSDP, len: 20 }));
    for len in 0..36 {
        assert!(Rsdp::parse(&rsdp_v2(1, 2)[..len]).is_err(), "cut at {len}");
    }
    assert!(Rsdp::parse(&rsdp_v1(1)[..19]).is_err());
}

#[test]
fn tables_check_length_and_checksum() {
    let t = table(FACP, &[1, 2, 3]);
    let sdt = Sdt::parse(&t).unwrap();
    assert_eq!((sdt.signature(), sdt.revision(), sdt.body()), (Signature(*FACP), 1, &[1, 2, 3][..]));
    let mut longer = t.clone();
    longer.extend_from_slice(&[0xaa; 5]);
    assert_eq!(Sdt::parse(&longer).unwrap().bytes(), &t[..], "bytes past the length field are not the table's");
    let mut bad = t.clone();
    bad[37] ^= 1;
    assert_eq!(Sdt::parse(&bad), Err(Error::BadChecksum(Signature(*FACP))));
    for len in 0..t.len() {
        assert!(Sdt::parse(&t[..len]).is_err(), "cut at {len}");
    }
    let mut tiny = t.clone();
    tiny[4..8].copy_from_slice(&35u32.to_le_bytes());
    assert_eq!(Sdt::parse(&tiny), Err(Error::BadLength { sig: Signature(*FACP), len: 35 }));
    let mut huge = t;
    huge[4..8].copy_from_slice(&(MAX_TABLE_LEN + 1).to_le_bytes());
    assert_eq!(Sdt::parse(&huge), Err(Error::BadLength { sig: Signature(*FACP), len: MAX_TABLE_LEN + 1 }));
}

#[test]
fn walks_the_xsdt_to_the_madt_and_hpet() {
    let regions = machine();
    let mut acpi = Acpi::new(Mem::new(&regions), RSDP_AT).unwrap();
    assert_eq!(acpi.rsdp(), Rsdp { revision: 2, rsdt: 0x7fbd_0000, xsdt: Some(0x7fbe_0001) });
    assert_eq!(acpi.root().signature(), Signature::XSDT, "the XSDT wins over the RSDT");
    assert_eq!(acpi.table_addresses().collect::<Vec<_>>(), [0x7fbe_1000, 0x7fbe_2000, 0x7fbe_3000]);
    let sigs: Result<Vec<_>, _> = acpi.signatures().collect();
    assert_eq!(sigs.unwrap(), [Signature(*FACP), Signature::MADT, Signature::HPET]);
    assert_eq!(acpi.find(Signature::MADT).unwrap().bytes(), &qemu_madt()[..]);
    assert_eq!(Hpet::parse(acpi.find(Signature::HPET).unwrap()).unwrap().address, 0xfed0_0000);
    assert_eq!(acpi.find(Signature(*b"SSDT")), Err(Error::NotFound(Signature(*b"SSDT"))));
}

#[test]
fn acpi_1_uses_the_rsdt() {
    let regions = vec![
        (0xe0000, rsdp_v1(0x1000)),
        (0x1000, rsdt(&[0x2000, 0x3000])),
        (0x2000, table(FACP, &[])),
        (0x3000, qemu_madt()),
    ];
    let mut acpi = Acpi::new(Mem::new(&regions), 0xe0000).unwrap();
    assert_eq!(acpi.rsdp().revision, 0);
    assert_eq!(acpi.root().signature(), Signature::RSDT);
    assert_eq!(acpi.find(Signature::MADT).map(|t| t.signature()), Ok(Signature::MADT));
}

#[test]
fn a_zero_xsdt_address_falls_back_to_the_rsdt() {
    let regions = vec![(0x100, rsdp_v2(0x1000, 0)), (0x1000, rsdt(&[0x2000])), (0x2000, qemu_madt())];
    let mut acpi = Acpi::new(Mem::new(&regions), 0x100).unwrap();
    assert_eq!(acpi.root().signature(), Signature::RSDT);
    assert!(acpi.find(Signature::MADT).is_ok());
    let none = vec![(0x100, rsdp_v2(0, 0))];
    assert_eq!(Acpi::new(Mem::new(&none), 0x100).err(), Some(Error::NoRootTable));
}

#[test]
fn bad_roots_are_refused() {
    // The XSDT address points at something else.
    let mut regions = machine();
    regions[2].1 = table(b"RSDT", &[0; 8]);
    assert_eq!(
        Acpi::new(Mem::new(&regions), RSDP_AT).err(),
        Some(Error::WrongSignature { expected: Signature::XSDT, found: Signature::RSDT })
    );
    // A partial last entry.
    let mut regions = machine();
    regions[2].1 = table(b"XSDT", &[0; 12]);
    assert_eq!(Acpi::new(Mem::new(&regions), RSDP_AT).err(), Some(Error::Truncated(Signature::XSDT)));
    // Nothing at the XSDT address.
    let mut regions = machine();
    regions.remove(2);
    assert_eq!(
        Acpi::new(Mem::new(&regions), RSDP_AT).err(),
        Some(Error::Unreadable { addr: 0x7fbe_0001, len: SDT_HEADER_LEN })
    );
    // Nothing at the RSDP.
    assert_eq!(Acpi::new(Mem::new(&machine()), 0x1000).err(), Some(Error::Unreadable { addr: 0x1000, len: 20 }));
}

#[test]
fn find_reports_a_broken_table_on_the_way() {
    let mut regions = machine();
    regions[3].1[20] ^= 1; // FACP checksum: only matters once FACP is read in full
    let mut acpi = Acpi::new(Mem::new(&regions), RSDP_AT).unwrap();
    assert!(acpi.find(Signature::MADT).is_ok(), "only headers are read on the way");
    assert_eq!(acpi.find(Signature(*FACP)), Err(Error::BadChecksum(Signature(*FACP))));
    let mut regions = machine();
    regions.remove(3); // FACP unreadable
    let mut acpi = Acpi::new(Mem::new(&regions), RSDP_AT).unwrap();
    assert_eq!(acpi.find(Signature::MADT), Err(Error::Unreadable { addr: 0x7fbe_1000, len: SDT_HEADER_LEN }));
}

#[test]
fn madt_lists_cpus_ioapics_and_overrides() {
    let t = qemu_madt();
    let madt = Madt::parse(Sdt::parse(&t).unwrap()).unwrap();
    assert_eq!(madt.local_apic_address(), 0xfee0_0000);
    assert!(madt.has_8259_pics());
    let cpus: Vec<_> = madt.cpus().collect();
    assert_eq!(cpus.len(), 3);
    assert_eq!(cpus.iter().filter(|c| c.enabled).map(|c| c.apic_id).collect::<Vec<_>>(), [0, 1]);
    assert_eq!(cpus[2], Cpu { apic_id: 2, uid: 2, enabled: false, online_capable: false });
    assert_eq!(madt.io_apics().collect::<Vec<_>>(), [IoApic { id: 0, address: 0xfec0_0000, gsi_base: 0 }]);
    let isos: Vec<_> = madt.interrupt_overrides().collect();
    assert_eq!(isos.len(), 3);
    assert_eq!(isos[1], InterruptOverride { bus: 0, irq: 5, gsi: 5, flags: 0xd });
    assert_eq!(madt.entries().last(), Some(MadtEntry::Other { kind: 4, len: 6 }));
    assert_eq!(madt.entries().count(), 8);
}

#[test]
fn madt_reads_x2apic_cpus_and_the_address_override() {
    let t = madt_with(&[x2apic(0x1_0000, 7, 1), lapic_override(0x1_fee0_0000), x2apic(300, 8, 2), lapic(9, 3, 3)]);
    let madt = Madt::parse(Sdt::parse(&t).unwrap()).unwrap();
    assert_eq!(madt.local_apic_address(), 0x1_fee0_0000, "a type 5 entry wins over the header");
    assert_eq!(
        madt.cpus().collect::<Vec<_>>(),
        [
            Cpu { apic_id: 0x1_0000, uid: 7, enabled: true, online_capable: false },
            Cpu { apic_id: 300, uid: 8, enabled: false, online_capable: true },
            // Enabled wins: online-capable only means something when disabled.
            Cpu { apic_id: 3, uid: 9, enabled: true, online_capable: false },
        ]
    );
}

#[test]
fn malformed_madt_entries_are_refused() {
    let bad = |entries: &[Vec<u8>]| Madt::parse(Sdt::parse(&madt_with(entries)).unwrap()).map(|_| ()).unwrap_err();
    assert_eq!(bad(&[vec![4, 0]]), Error::BadMadtEntry { offset: 44, kind: 4, len: 0 });
    assert_eq!(bad(&[vec![4, 1]]), Error::BadMadtEntry { offset: 44, kind: 4, len: 1 });
    assert_eq!(bad(&[lapic(0, 0, 1), vec![0, 6, 0, 0, 0, 0]]), Error::BadMadtEntry { offset: 52, kind: 0, len: 6 });
    let mut short_x2 = x2apic(1, 1, 1);
    short_x2[1] = 12;
    short_x2.truncate(12);
    assert_eq!(bad(&[short_x2]), Error::BadMadtEntry { offset: 44, kind: 9, len: 12 });
    assert_eq!(bad(&[vec![1, 12, 0, 0]]), Error::BadMadtEntry { offset: 44, kind: 1, len: 12 }, "overruns the table");
    assert_eq!(bad(&[vec![0x7f]]), Error::BadMadtEntry { offset: 44, kind: 0x7f, len: 0 }, "one byte left");
    // Longer than its type needs (a later ACPI revision): fine.
    let mut long = lapic(0, 0, 1);
    long[1] = 10;
    long.extend_from_slice(&[0, 0]);
    let t = madt_with(&[long, lapic(1, 1, 1)]);
    let madt = Madt::parse(Sdt::parse(&t).unwrap()).unwrap();
    assert_eq!(madt.cpus().map(|c| c.apic_id).collect::<Vec<_>>(), [0, 1]);
    // Not an MADT at all.
    let hpet = hpet_with(0, QEMU_HPET_ID);
    assert_eq!(
        Madt::parse(Sdt::parse(&hpet).unwrap()).map(|_| ()),
        Err(Error::WrongSignature { expected: Signature::MADT, found: Signature::HPET })
    );
}

#[test]
fn a_cut_madt_is_refused_unless_cut_between_entries() {
    let t = qemu_madt();
    // Entry boundaries in the entry area: 8, 8, 8, 12, 10, 10, 10, 6.
    let mut boundaries = vec![MADT_FIXED_LEN];
    for len in [8, 8, 8, 12, 10, 10, 10, 6] {
        boundaries.push(boundaries.last().unwrap() + len);
    }
    assert_eq!(*boundaries.last().unwrap(), t.len());
    for len in 0..=t.len() {
        // Cut without fixing the header: the length field no longer fits.
        if len < t.len() {
            assert!(Sdt::parse(&t[..len]).is_err(), "raw cut at {len}");
        }
        // Cut with a consistent header: only the MADT's own checks remain.
        if len >= SDT_HEADER_LEN {
            let c = cut(&t, len);
            let madt = Madt::parse(Sdt::parse(&c).unwrap());
            assert_eq!(madt.is_ok(), boundaries.contains(&len), "consistent cut at {len}: {madt:?}");
        }
    }
}

#[test]
fn hpet_parses_and_checks_its_address_space() {
    let t = hpet_with(0, QEMU_HPET_ID);
    assert_eq!(
        Hpet::parse(Sdt::parse(&t).unwrap()),
        Ok(Hpet {
            address: 0xfed0_0000,
            number: 0,
            min_tick: 128,
            comparators: 3,
            counter_64bit: true,
            legacy_replacement: true,
            pci_vendor: 0x8086,
        })
    );
    let io = hpet_with(1, QEMU_HPET_ID);
    assert_eq!(Hpet::parse(Sdt::parse(&io).unwrap()), Err(Error::HpetNotInMemory { space: 1 }));
    for len in SDT_HEADER_LEN..t.len() {
        assert_eq!(Hpet::parse(Sdt::parse(&cut(&t, len)).unwrap()), Err(Error::Truncated(Signature::HPET)), "{len}");
    }
    let plain = hpet_with(0, 0x0000_0001);
    let h = Hpet::parse(Sdt::parse(&plain).unwrap()).unwrap();
    assert_eq!((h.comparators, h.counter_64bit, h.legacy_replacement, h.pci_vendor), (1, false, false, 0));
}

#[test]
fn errors_name_what_went_wrong() {
    use std::string::ToString;
    assert_eq!(Error::NotFound(Signature::HPET).to_string(), "no HPET table");
    assert_eq!(Signature(*b"A\0\xffB").to_string(), "A??B");
    assert_eq!(
        Error::BadMadtEntry { offset: 52, kind: 0, len: 6 }.to_string(),
        "MADT entry at offset 52 (type 0, length 6) is malformed"
    );
}

#[test]
fn reads_only_what_it_needs() {
    // Header, then the whole table: two reads per table found, one per header skipped.
    let regions = machine();
    let mut acpi = Acpi::new(Mem::new(&regions), RSDP_AT).unwrap();
    acpi.find(Signature::HPET).unwrap();
    let mem = acpi.into_mem();
    // RSDP: 20, 24, 36 bytes; XSDT: header + table; FACP and MADT headers; HPET header + table.
    assert_eq!(mem.reads, 3 + 2 + 2 + 2);
}

#[test]
fn a_short_read_is_unreadable() {
    // A reader that hands back fewer bytes than asked for.
    struct Short<'a>(&'a [u8]);
    impl<'a> PhysMem<'a> for Short<'a> {
        fn read(&mut self, _addr: u64, len: usize) -> Option<&'a [u8]> {
            Some(&self.0[..len.min(self.0.len())])
        }
    }
    let rsdp = rsdp_v2(0, 0x1000);
    assert_eq!(Acpi::new(Short(&rsdp[..30]), 0x100).err(), Some(Error::Unreadable { addr: 0x100, len: 36 }));
}
