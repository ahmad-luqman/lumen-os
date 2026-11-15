//! Host tests against the fake configuration space, with functions shaped
//! like QEMU q35's.

extern crate std;

use std::vec::Vec;

use crate::device::read_header;
use crate::fake::{Fake, Function, at};
use crate::*;

const VIRTIO: u16 = names::VIRTIO_VENDOR;

/// A modern virtio rng as QEMU lays it out: a 64-bit prefetchable BAR 4 of
/// 16 KiB above 4 GiB (the virtio structures), a 4 KiB BAR 1 (the MSI-X
/// table), and its capabilities: MSI-X, then five virtio ones.
fn virtio_rng() -> Function {
    Function::endpoint(VIRTIO, 0x1044, 0x00, 0xff)
        .subsystem(VIRTIO, 0x1100)
        .irq_pin(1)
        .mem32(1, 0xc100_0000, 0x1000, false)
        .mem64(4, 0x80_0000_4000, 0x4000, true)
        .caps(
            0x98,
            &[
                (0x98, cap::id::MSIX, 0x84),
                (0x84, 9, 0x70),
                (0x70, 9, 0x60),
                (0x60, 9, 0x50),
                (0x50, 9, 0x40),
                (0x40, 9, 0),
            ],
        )
}

#[test]
fn pci_addresses_check_their_ranges_and_print_like_lspci() {
    assert_eq!(PciAddr::new(0, 0, 32, 0), None);
    assert_eq!(PciAddr::new(0, 0, 0, 8), None);
    let a = at(1, 0xab, 31, 7);
    assert_eq!((a.segment(), a.bus(), a.device(), a.function()), (1, 0xab, 31, 7));
    assert_eq!(std::format!("{a}"), "0001:ab:1f.7");
    assert_eq!(std::format!("{}", at(0, 0, 2, 0)), "0000:00:02.0");
    assert_eq!(a.ecam_offset(), 31 << 15 | 7 << 12);
    assert!(at(0, 1, 0, 0) > at(0, 0, 31, 7), "ordered by bus, then device, then function");
}

#[test]
fn narrow_reads_take_their_part_of_the_dword() {
    let mut cs = Fake::default().with(at(0, 0, 1, 0), virtio_rng());
    let a = at(0, 0, 1, 0);
    assert_eq!(cs.read16(a, 0), VIRTIO);
    assert_eq!(cs.read16(a, 2), 0x1044);
    assert_eq!(cs.read8(a, 0x0b), 0x00);
    assert_eq!(cs.read8(a, 0x0a), 0xff);
    assert_eq!(cs.read8(a, 0x3d), 1);
}

#[test]
fn device_info_has_the_header_fields() {
    let a = at(0, 0, 3, 0);
    let mut cs = Fake::default().with(a, virtio_rng().prog_if(0x5a));
    let d = DeviceInfo::read(&mut cs, a).unwrap();
    assert_eq!((d.addr, d.vendor, d.device), (a, VIRTIO, 0x1044));
    assert_eq!((d.class, d.subclass, d.prog_if, d.revision), (0x00, 0xff, 0x5a, 1));
    assert_eq!(d.subsystem, (VIRTIO, 0x1100));
    assert_eq!(d.irq_pin, 1);
    assert_eq!(DeviceInfo::read(&mut cs, at(0, 0, 4, 0)), None, "nothing there reads as all ones");
}

#[test]
fn a_vendor_of_0_or_ffff_is_no_function() {
    let a = at(0, 0, 0, 0);
    for vendor in [0, 0xffff] {
        let mut cs = Fake::default().with(a, Function::endpoint(vendor, 0x1234, 2, 0));
        assert_eq!(read_header(&mut cs, a), None, "{vendor:#x}");
    }
}

#[test]
fn bars_of_every_kind_are_sized_and_keep_their_addresses() {
    let a = at(0, 0, 3, 0);
    let f = virtio_rng().io(0, 0xc040, 0x20, 32).mem32(2, 0xfebf_0000, 0x1_0000, true);
    let before: Vec<u32> = (0..6).map(|i| f.bar(i)).collect();
    let mut cs = Fake::default().with(a, f);
    let d = DeviceInfo::read(&mut cs, a).unwrap();
    assert_eq!(
        d.bars,
        [
            Some(Bar::Io { port: 0xc040, size: 0x20 }),
            Some(Bar::Memory { phys: 0xc100_0000, size: 0x1000, prefetchable: false, is_64: false }),
            Some(Bar::Memory { phys: 0xfebf_0000, size: 0x1_0000, prefetchable: true, is_64: false }),
            None,
            Some(Bar::Memory { phys: 0x80_0000_4000, size: 0x4000, prefetchable: true, is_64: true }),
            None, // BAR 4's upper half
        ]
    );
    let f = cs.get(a);
    let after: Vec<u32> = (0..6).map(|i| f.bar(i)).collect();
    assert_eq!(after, before, "every BAR gets its value back");
}

#[test]
fn bars_are_sized_with_decoding_off_and_the_command_restored() {
    let a = at(0, 0, 3, 0);
    let mut cs = Fake::default().with(a, virtio_rng());
    let before = cs.get(a).command();
    DeviceInfo::read(&mut cs, a).unwrap();
    let f = cs.get(a);
    assert!(!f.sized_while_decoding);
    let off = before & !(command::IO_SPACE | command::MEMORY_SPACE);
    assert_eq!(f.commands, [off, before]);
    assert_eq!(f.command(), before);
}

#[test]
fn a_host_bridge_is_sized_with_decoding_on() {
    // As Linux does (mmio_always_on): its command register isn't touched.
    let a = at(0, 0, 0, 0);
    let mut cs =
        Fake::default().with(a, Function::endpoint(0x8086, 0x29c0, 0x06, 0x00).mem32(0, 0xe000_0000, 0x1000, false));
    let d = DeviceInfo::read(&mut cs, a).unwrap();
    assert_eq!(d.bars[0], Some(Bar::Memory { phys: 0xe000_0000, size: 0x1000, prefetchable: false, is_64: false }));
    assert!(cs.get(a).commands.is_empty());
}

#[test]
fn a_16_bit_io_decoder_is_sized_from_its_low_half() {
    let a = at(0, 0, 3, 0);
    let mut cs = Fake::default()
        .with(a, Function::endpoint(0x8086, 0x2922, 1, 6).io(4, 0xc0e0, 0x20, 16).io(5, 0x1000, 0x100, 32));
    let d = DeviceInfo::read(&mut cs, a).unwrap();
    assert_eq!(d.bars[4], Some(Bar::Io { port: 0xc0e0, size: 0x20 }));
    assert_eq!(d.bars[5], Some(Bar::Io { port: 0x1000, size: 0x100 }));
}

#[test]
fn a_big_64_bit_bar_is_sized_across_both_halves() {
    let a = at(0, 0, 3, 0);
    let mut cs =
        Fake::default().with(a, Function::endpoint(0x1234, 0x1111, 3, 0).mem64(0, 0x10_0000_0000, 8 << 30, true));
    let d = DeviceInfo::read(&mut cs, a).unwrap();
    assert_eq!(d.bars[0], Some(Bar::Memory { phys: 0x10_0000_0000, size: 8 << 30, prefetchable: true, is_64: true }));
    assert_eq!(d.bars[1], None);
}

#[test]
fn unimplemented_bars_are_none_and_unassigned_or_malformed_ones_say_so() {
    let a = at(0, 0, 3, 0);
    let f = Function::endpoint(0x8086, 0x10d3, 2, 0)
        // Implemented, but at 0: unassigned, its size kept.
        .mem32(0, 0, 0x2_0000, false)
        // No address bit sticks: not implemented.
        .raw_bar(1, 0, 0)
        // An I/O BAR at port 0: unassigned.
        .io(2, 0, 0x20, 32)
        // The reserved memory type (0b11).
        .raw_bar(3, 0xfe00_0006, 0xffff_f000)
        // Read-only address bits set below the writable ones: sized by
        // the lowest bit that sticks, not past 4 GiB.
        .raw_bar(4, 0xffff_8000, 0xffff_0000)
        // A 64-bit BAR in the last slot: no room for its upper half.
        .raw_bar(5, 0xfd00_0004, 0xffff_f000);
    let mut cs = Fake::default().with(a, f);
    assert_eq!(
        DeviceInfo::read(&mut cs, a).unwrap().bars,
        [
            Some(Bar::Unassigned { io: false, size: 0x2_0000, is_64: false }),
            None,
            Some(Bar::Unassigned { io: true, size: 0x20, is_64: false }),
            Some(Bar::Malformed(BarProblem::ReservedType)),
            Some(Bar::Memory { phys: 0xffff_8000, size: 0x8000, prefetchable: false, is_64: false }),
            Some(Bar::Malformed(BarProblem::NoUpperHalf)),
        ]
    );
    // A 64-bit BAR whose range would end at 2^64.
    let top = Function::endpoint(1, 2, 3, 4).raw_bar(0, 0xffff_f004, 0xffff_f000).raw_bar(1, 0xffff_ffff, 0xffff_ffff);
    let mut cs = Fake::default().with(a, top);
    let bars = DeviceInfo::read(&mut cs, a).unwrap().bars;
    let past = BarProblem::PastAddressSpace { phys: 0xffff_ffff_ffff_f000, size: 0x1000 };
    assert_eq!(bars[..2], [Some(Bar::Malformed(past)), None]);
}

#[test]
fn a_64_bit_bar_at_0_is_unassigned_and_keeps_its_upper_slot() {
    let a = at(0, 0, 3, 0);
    let mut cs = Fake::default().with(a, Function::endpoint(VIRTIO, 0x1044, 0, 0xff).mem64(4, 0, 0x4000, true));
    let bars = DeviceInfo::read(&mut cs, a).unwrap().bars;
    assert_eq!(bars[4..], [Some(Bar::Unassigned { io: false, size: 0x4000, is_64: true }), None]);
    assert!(bars[4].unwrap().is_64() && !bars[4].unwrap().is_memory());
}

#[test]
fn an_io_bar_past_the_16_bit_port_space_is_malformed() {
    let a = at(0, 0, 3, 0);
    let f = Function::endpoint(1, 2, 3, 4).io(0, 0xffe0, 0x20, 32).io(1, 0x1_0000, 0x100, 32).io(2, 0xff00, 0x100, 32);
    let mut cs = Fake::default().with(a, f);
    let bars = DeviceInfo::read(&mut cs, a).unwrap().bars;
    assert_eq!(bars[0], Some(Bar::Io { port: 0xffe0, size: 0x20 }), "ends exactly at 0x10000");
    assert_eq!(bars[1], Some(Bar::Malformed(BarProblem::IoPastPortSpace { port: 0x1_0000, size: 0x100 })));
    assert_eq!(bars[2], Some(Bar::Io { port: 0xff00, size: 0x100 }));
}

#[test]
fn a_bridge_has_two_bars_and_no_subsystem() {
    let a = at(0, 0, 0x1e, 0);
    let f = Function::endpoint(0x1b36, 0x0001, 6, 4)
        .header_type(1)
        .mem32(0, 0xfe80_0000, 0x100, false)
        // The bridge's bus numbers live where an endpoint's BAR 2 would.
        .raw_bar(2, 0x0002_0100, 0xffff_ffff);
    let mut cs = Fake::default().with(a, f);
    let d = DeviceInfo::read(&mut cs, a).unwrap();
    assert_eq!(d.bars[0], Some(Bar::Memory { phys: 0xfe80_0000, size: 0x100, prefetchable: false, is_64: false }));
    assert_eq!(d.bars[1..], [None; 5]);
    assert_eq!(cs.get(a).bar(2), 0x0002_0100, "bus numbers untouched");
    assert_eq!(d.subsystem, (0, 0));
    assert_eq!(d.buses, Some(BusNumbers { primary: 0, secondary: 1, subordinate: 2 }));
}

#[test]
fn a_bridge_reads_its_capabilities_and_not_a_subsystem() {
    // 0x2c of a type 1 header is the prefetchable limit's upper half, not
    // a subsystem id.
    let a = at(0, 0, 0x1c, 0);
    let mut f =
        Function::endpoint(0x1b36, 0x000c, 6, 4).header_type(1).caps(0x40, &[(0x40, 0x10, 0x60), (0x60, 0x11, 0)]);
    f.regs[0x2c / 4] = 0x1234_5678;
    let mut cs = Fake::default().with(a, f);
    let d = DeviceInfo::read(&mut cs, a).unwrap();
    assert_eq!(d.subsystem, (0, 0));
    assert_eq!(d.caps.iter().map(|c| (c.id, c.offset)).collect::<Vec<_>>(), [(0x10, 0x40), (0x11, 0x60)]);
    assert!(d.caps.is_complete());
}

#[test]
fn bridges_and_isa_bridges_are_sized_with_decoding_off() {
    for (subclass, header_type) in [(1, 0), (4, 1)] {
        let a = at(0, 0, 0x1f, 0);
        let f = Function::endpoint(0x8086, 0x2918, 6, subclass).header_type(header_type).mem32(
            0,
            0xfe00_0000,
            0x1000,
            false,
        );
        let mut cs = Fake::default().with(a, f);
        let before = cs.get(a).command();
        DeviceInfo::read(&mut cs, a).unwrap();
        let f = cs.get(a);
        assert_eq!(f.commands, [before & !(command::IO_SPACE | command::MEMORY_SPACE), before], "subclass {subclass}");
        assert!(!f.sized_while_decoding);
    }
}

#[test]
fn sizing_leaves_the_status_register_alone() {
    // Bit 13, received master abort, is write-one-to-clear: writing the
    // command register back must not clear it.
    let a = at(0, 0, 3, 0);
    let mut cs = Fake::default().with(a, virtio_rng().status(1 << 13));
    let before = cs.get(a).status_bits();
    DeviceInfo::read(&mut cs, a).unwrap();
    assert_eq!(cs.get(a).status_bits(), before);
    assert_ne!(before & 1 << 13, 0);
    // The fake does clear what a 1 is written to.
    cs.write(a, reg::COMMAND, 1 << 29);
    assert_eq!(cs.get(a).status_bits() & 1 << 13, 0);
}

#[test]
fn a_cardbus_bridge_has_no_bars_or_capabilities() {
    let a = at(0, 0, 5, 0);
    let f = Function::endpoint(0x104c, 0xac56, 6, 7)
        .header_type(2)
        .mem32(0, 0xfe00_0000, 0x1000, false)
        .caps(0x40, &[(0x40, 1, 0)]);
    let mut cs = Fake::default().with(a, f);
    let d = DeviceInfo::read(&mut cs, a).unwrap();
    assert_eq!((d.bars, d.caps.len(), d.caps.is_complete()), ([None; 6], 0, true));
    assert!(cs.get(a).commands.is_empty());
}

#[test]
fn capabilities_are_listed_in_order() {
    let a = at(0, 0, 3, 0);
    let mut cs = Fake::default().with(a, virtio_rng());
    let caps = DeviceInfo::read(&mut cs, a).unwrap().caps;
    let found: Vec<(u8, u8)> = caps.iter().map(|c| (c.id, c.offset)).collect();
    assert_eq!(found, [(0x11, 0x98), (9, 0x84), (9, 0x70), (9, 0x60), (9, 0x50), (9, 0x40)]);
    assert!(caps.is_complete());
    assert_eq!(caps.end(), CapEnd::Complete);
    assert_eq!(caps.find(cap::id::MSIX), Some(Cap { id: 0x11, offset: 0x98 }));
    assert_eq!(caps.find(cap::id::VENDOR).map(|c| c.offset), Some(0x84));
    assert_eq!(caps.find(cap::id::MSI), None);
}

#[test]
fn without_the_status_bit_there_are_no_capabilities() {
    let a = at(0, 0, 3, 0);
    let mut f = virtio_rng();
    f.regs[1] &= !(u32::from(STATUS_CAP_LIST) << 16);
    let mut cs = Fake::default().with(a, f);
    let caps = DeviceInfo::read(&mut cs, a).unwrap().caps;
    assert!(caps.is_empty() && caps.is_complete());
}

#[test]
fn pointers_have_their_reserved_low_bits_masked() {
    let a = at(0, 0, 3, 0);
    let mut cs =
        Fake::default().with(a, Function::endpoint(1, 2, 3, 4).caps(0x43, &[(0x40, 5, 0x52), (0x50, 0x11, 0x03)]));
    let found: Vec<(u8, u8)> = DeviceInfo::read(&mut cs, a).unwrap().caps.iter().map(|c| (c.id, c.offset)).collect();
    assert_eq!(found, [(5, 0x40), (0x11, 0x50)], "0x03 masks to 0: the end");
}

#[test]
fn a_looping_capability_chain_is_cut_off_and_marked() {
    let a = at(0, 0, 3, 0);
    // 0x40 -> 0x50 -> 0x40 -> ...
    let mut cs =
        Fake::default().with(a, Function::endpoint(1, 2, 3, 4).caps(0x40, &[(0x40, 5, 0x50), (0x50, 9, 0x40)]));
    let caps = DeviceInfo::read(&mut cs, a).unwrap().caps;
    assert_eq!(caps.end(), CapEnd::TooLong);
    assert!(!caps.is_complete());
    assert_eq!(caps.len(), MAX_CAPS);
    // A capability pointing at itself.
    let mut cs = Fake::default().with(a, Function::endpoint(1, 2, 3, 4).caps(0xfc, &[(0xfc, 9, 0xfc)]));
    let caps = DeviceInfo::read(&mut cs, a).unwrap().caps;
    assert_eq!((caps.end(), caps.len()), (CapEnd::TooLong, MAX_CAPS));
    // The walk stops reading once it is cut off.
    assert!(cs.reads < 2 * MAX_CAPS + 32, "{} reads", cs.reads);
}

#[test]
fn the_longest_clean_list_is_complete() {
    // One capability in every dword from 0x40 to 0xfc: 48, ending cleanly.
    let a = at(0, 0, 3, 0);
    let caps: Vec<(u8, u8, u8)> =
        (0x40..=0xfcu8).step_by(4).map(|o| (o, 9, if o == 0xfc { 0 } else { o + 4 })).collect();
    let mut cs = Fake::default().with(a, Function::endpoint(1, 2, 3, 4).caps(0x40, &caps));
    let list = DeviceInfo::read(&mut cs, a).unwrap().caps;
    assert_eq!((list.len(), list.end()), (MAX_CAPS, CapEnd::Complete));
}

#[test]
fn a_pointer_into_the_header_is_cut_off_and_marked() {
    let a = at(0, 0, 3, 0);
    let mut cs = Fake::default().with(a, Function::endpoint(1, 2, 3, 4).caps(0x40, &[(0x40, 5, 0x34)]));
    let caps = DeviceInfo::read(&mut cs, a).unwrap().caps;
    assert_eq!(caps.end(), CapEnd::IntoHeader { from: 0x40, to: 0x34 });
    assert_eq!(caps.iter().map(|c| c.offset).collect::<Vec<_>>(), [0x40], "what came before is kept");
    // The header's own pointer.
    let mut cs = Fake::default().with(a, Function::endpoint(1, 2, 3, 4).caps(0x10, &[]));
    let caps = DeviceInfo::read(&mut cs, a).unwrap().caps;
    assert_eq!((caps.end(), caps.len()), (CapEnd::IntoHeader { from: 0, to: 0x10 }, 0));
    // Just below the first allowed offset.
    let mut cs = Fake::default().with(a, Function::endpoint(1, 2, 3, 4).caps(0x40, &[(0x40, 5, 0x3c)]));
    assert_eq!(DeviceInfo::read(&mut cs, a).unwrap().caps.end(), CapEnd::IntoHeader { from: 0x40, to: 0x3c });
}

#[test]
fn an_unknown_header_type_isnt_read_and_isnt_probeable() {
    let a = at(0, 0, 3, 0);
    let f = Function::endpoint(1, 2, 3, 4)
        .header_type(0x05)
        .mem32(0, 0xfe00_0000, 0x1000, false)
        .caps(0x40, &[(0x40, 5, 0)]);
    let mut cs = Fake::default().with(a, f);
    let d = DeviceInfo::read(&mut cs, a).unwrap();
    assert_eq!((d.bars, d.caps.len(), d.caps.end()), ([None; 6], 0, CapEnd::UnknownHeader(5)));
    assert!(!d.probeable());
    assert!(cs.get(a).commands.is_empty());
    // The multi-function bit isn't part of the type.
    let mut cs = Fake::default().with(a, Function::endpoint(1, 2, 3, 4).header_type(0x85));
    assert_eq!(DeviceInfo::read(&mut cs, a).unwrap().caps.end(), CapEnd::UnknownHeader(5));
}

/// Every function `enumerate` finds in `buses` of `segment`.
fn scan(cs: &mut Fake, segment: u16, buses: core::ops::RangeInclusive<u8>) -> Vec<PciAddr> {
    let mut found = Vec::new();
    enumerate(cs, segment, buses, |d| found.push(d.addr));
    found
}

#[test]
fn multi_function_devices_are_scanned_past_function_0() {
    let lpc = Function::endpoint(0x8086, 0x2918, 6, 1).multi();
    let mut cs = Fake::default()
        // q35's 00:1f: LPC, SATA and SMBus, function 0 multi-function.
        .with(at(0, 0, 0x1f, 0), lpc)
        .with(at(0, 0, 0x1f, 2), Function::endpoint(0x8086, 0x2922, 1, 6))
        .with(at(0, 0, 0x1f, 3), Function::endpoint(0x8086, 0x2930, 0x0c, 5))
        // A single-function device: function 3 answering is not looked at.
        .with(at(0, 0, 2, 0), Function::endpoint(0x8086, 0x10d3, 2, 0))
        .with(at(0, 0, 2, 3), Function::endpoint(0x8086, 0x10d3, 2, 0))
        // No function 0: the device isn't there, whatever answers at 1.
        .with(at(0, 0, 4, 1), Function::endpoint(VIRTIO, 0x1044, 0, 0xff));
    assert_eq!(scan(&mut cs, 0, 0..=0), [at(0, 0, 2, 0), at(0, 0, 0x1f, 0), at(0, 0, 0x1f, 2), at(0, 0, 0x1f, 3)]);
}

#[test]
fn functions_1_and_7_are_scanned() {
    let mut cs = Fake::default()
        .with(at(0, 0, 0x1f, 0), Function::endpoint(0x8086, 0x2918, 6, 1).multi())
        .with(at(0, 0, 0x1f, 1), Function::endpoint(0x8086, 0x2922, 1, 6))
        .with(at(0, 0, 0x1f, 7), Function::endpoint(0x8086, 0x2930, 0x0c, 5));
    assert_eq!(scan(&mut cs, 0, 0..=0), [at(0, 0, 0x1f, 0), at(0, 0, 0x1f, 1), at(0, 0, 0x1f, 7)]);
}

#[test]
fn a_multi_function_function_0_decodes_like_a_single_function_one() {
    let (a, b) = (at(0, 0, 3, 0), at(0, 0, 4, 0));
    let mut cs = Fake::default().with(a, virtio_rng()).with(b, virtio_rng().multi());
    let mut found = Vec::new();
    enumerate(&mut cs, 0, 0..=0, |d| found.push(d));
    let [single, multi] = found[..] else { panic!("{found:?}") };
    assert_eq!((multi.bars, multi.caps, multi.subsystem), (single.bars, single.caps, single.subsystem));
    assert!(multi.caps.is_complete() && multi.bars[4].is_some());
}

#[test]
fn every_bus_in_the_range_is_scanned_and_nothing_outside_it() {
    let mut cs = Fake::default()
        .with(at(0, 0, 0, 0), Function::endpoint(0x8086, 0x29c0, 6, 0))
        // Behind a bridge: reached without following it.
        .with(at(0, 1, 0, 0), virtio_rng())
        .with(at(0, 255, 31, 0), virtio_rng())
        .with(at(1, 0, 0, 0), virtio_rng());
    assert_eq!(scan(&mut cs, 0, 0..=255), [at(0, 0, 0, 0), at(0, 1, 0, 0), at(0, 255, 31, 0)]);
    assert_eq!(scan(&mut cs, 0, 1..=254), [at(0, 1, 0, 0)]);
    assert_eq!(scan(&mut cs, 1, 0..=0), [at(1, 0, 0, 0)]);
}

#[test]
fn an_empty_bus_costs_one_read_per_device() {
    let mut cs = Fake::default();
    assert!(scan(&mut cs, 0, 0..=0).is_empty());
    assert_eq!(cs.reads, 32);
}

#[test]
fn virtio_devices_are_named_by_type_modern_or_transitional() {
    assert_eq!(names::device(VIRTIO, 0x1044, 0x1100), Some("virtio rng"));
    assert_eq!(names::device(VIRTIO, 0x1052, 0x1100), Some("virtio input"));
    assert_eq!(names::device(VIRTIO, 0x1059, 0x1100), Some("virtio sound"));
    // Transitional ids: the type is the subsystem id.
    assert_eq!(names::device(VIRTIO, 0x1005, 4), Some("virtio rng"));
    assert_eq!(names::device(VIRTIO, 0x1001, 2), Some("virtio block"));
    assert_eq!(names::device(VIRTIO, 0x1000, 1), Some("virtio network"));
    assert_eq!(names::device(VIRTIO, 0x1009, 9), Some("virtio 9p"));
    assert_eq!(names::virtio_type(VIRTIO, 0x1040 + 25, 0), Some(25));
    assert_eq!(names::virtio_type(0x8086, 0x1044, 4), None);
    assert_eq!(names::device(VIRTIO, 0x10ff, 0), None);
    assert_eq!(names::device(VIRTIO, 0x1040 + 99, 0), None);
}

#[test]
fn q35s_devices_and_classes_have_names() {
    assert_eq!(names::device(0x8086, 0x29c0, 0), Some("Q35 host bridge"));
    assert_eq!(names::device(0x1234, 0x1111, 0), Some("standard VGA"));
    assert_eq!(names::device(0x8086, 0x10d3, 0), Some("82574L ethernet (e1000e)"));
    assert_eq!(names::device(0xdead, 0xbeef, 0), None);
    assert_eq!(names::class(3, 0), "VGA controller");
    assert_eq!(names::class(6, 1), "ISA bridge");
    assert_eq!(names::class(4, 1), "audio device");
    // The class alone where the subclass isn't known.
    assert_eq!(names::class(9, 0x80), "input device");
    assert_eq!(names::class(0, 0xff), "unclassified device");
    assert_eq!(names::class(0x40, 0), "unknown class");
}

#[test]
fn ecam_offsets_count_from_the_regions_first_bus() {
    let addr = |bus, device, function| at(0, bus, device, function);
    // A region of buses 0x10-0x1f: bus 0x12 is 2 MiB in.
    assert_eq!(ecam_offset(0, 0x10..=0x1f, addr(0x12, 0, 0), 0x40), Some(2 << 20 | 0x40));
    assert_eq!(ecam_offset(0, 0x10..=0x1f, addr(0x10, 0, 0), 0), Some(0));
    assert_eq!(ecam_offset(0, 0x10..=0x1f, addr(0x1f, 31, 7), 0xffc), Some(0xf << 20 | 31 << 15 | 7 << 12 | 0xffc));
    // Outside its buses, its segment, or not a dword below 4096.
    assert_eq!(ecam_offset(0, 0x10..=0x1f, addr(0x0f, 0, 0), 0), None);
    assert_eq!(ecam_offset(0, 0x10..=0x1f, addr(0x20, 0, 0), 0), None);
    assert_eq!(ecam_offset(1, 0x10..=0x1f, addr(0x12, 0, 0), 0), None);
    assert_eq!(ecam_offset(0, 0x10..=0x1f, addr(0x12, 0, 0), 0x41), None);
    assert_eq!(ecam_offset(0, 0x10..=0x1f, addr(0x12, 0, 0), 0x1000), None);
}

#[test]
fn problems_name_each_thing_wrong() {
    let a = at(0, 0, 3, 0);
    let everywhere = |_: u16, _: u8| true;
    let mut cs = Fake::default().with(a, virtio_rng());
    assert_eq!(DeviceInfo::read(&mut cs, a).unwrap().problems(everywhere).count(), 0);
    let f = virtio_rng().mem64(4, 0, 0x4000, true).raw_bar(0, 0xfe00_0006, 0xffff_f000).caps(0x40, &[(0x40, 5, 0x40)]);
    let mut cs = Fake::default().with(a, f);
    let problems: Vec<Problem> = DeviceInfo::read(&mut cs, a).unwrap().problems(everywhere).collect();
    assert_eq!(
        problems,
        [
            Problem::Caps(CapEnd::TooLong),
            Problem::MalformedBar { index: 0, why: BarProblem::ReservedType },
            Problem::UnassignedBar { index: 4, io: false, size: 0x4000 },
        ]
    );
    let text: Vec<std::string::String> = problems.iter().map(|p| std::format!("{p}")).collect();
    assert_eq!(
        text,
        [
            "capability list incomplete: more than 48 capabilities (the list loops); drivers won't be given it",
            "BAR 0 is unusable: it uses the reserved memory type",
            "BAR 4 (memory, 0x4000 bytes) has no address: the firmware didn't assign it",
        ]
    );
}

#[test]
fn a_bridge_to_buses_outside_ecam_is_a_problem() {
    let bridge = |secondary, subordinate| {
        let a = at(0, 0, 0x1c, 0);
        let f = Function::endpoint(0x1b36, 0x000c, 6, 4).header_type(1).bridge_buses(0, secondary, subordinate);
        let mut cs = Fake::default().with(a, f);
        DeviceInfo::read(&mut cs, a).unwrap()
    };
    // ECAM covers segment 0's buses 0-15.
    let covered = |segment: u16, bus: u8| segment == 0 && bus < 16;
    assert_eq!(bridge(1, 15).problems(covered).count(), 0);
    assert_eq!(bridge(0, 0).problems(covered).count(), 0, "not configured");
    for (secondary, subordinate) in [(1, 16), (16, 16), (5, 4)] {
        assert_eq!(
            bridge(secondary, subordinate).problems(covered).collect::<Vec<_>>(),
            [Problem::BusesOutsideEcam { secondary, subordinate }],
            "{secondary}-{subordinate}"
        );
    }
    assert_eq!(
        std::format!("{}", Problem::BusesOutsideEcam { secondary: 1, subordinate: 16 }),
        "the buses behind this bridge (1-16) aren't all in mapped configuration space: devices there aren't listed"
    );
}

#[test]
fn every_problem_and_cap_end_reads_as_a_sentence() {
    let cases = [
        (
            std::format!("{}", CapEnd::IntoHeader { from: 0, to: 0x10 }),
            "the capabilities pointer points into the header (0x10)",
        ),
        (
            std::format!("{}", CapEnd::IntoHeader { from: 0x40, to: 0x3c }),
            "the capability at 0x40 points into the header (0x3c)",
        ),
        (std::format!("{}", CapEnd::UnknownHeader(5)), "header type 5 is unknown"),
        (std::format!("{}", CapEnd::Complete), "complete"),
        (std::format!("{}", BarProblem::NoUpperHalf), "it is 64-bit in the last slot (no upper half)"),
        (
            std::format!("{}", BarProblem::IoPastPortSpace { port: 0x1_0000, size: 0x100 }),
            "its I/O range 0x10000 (+0x100) ends past port 0xffff",
        ),
        (
            std::format!("{}", BarProblem::PastAddressSpace { phys: 0xffff_ffff_ffff_f000, size: 0x1000 }),
            "its range 0xfffffffffffff000 (+0x1000) doesn't end below 2^64",
        ),
        (
            std::format!("{}", Problem::UnassignedBar { index: 2, io: true, size: 0x20 }),
            "BAR 2 (I/O, 0x20 bytes) has no address: the firmware didn't assign it",
        ),
    ];
    for (got, want) in cases {
        assert_eq!(got, want);
    }
}

#[test]
fn the_boot_log_line_names_the_function() {
    let a = at(0, 0, 0, 0);
    let mut cs = Fake::default().with(a, Function::endpoint(0x8086, 0x29c0, 6, 0));
    let d = DeviceInfo::read(&mut cs, a).unwrap();
    assert_eq!(std::format!("{d}"), "0000:00:00.0 8086:29c0 host bridge (class 060000) Q35 host bridge");
    let mut cs = Fake::default().with(a, Function::endpoint(0xdead, 0x000c, 0x40, 1).prog_if(2));
    let d = DeviceInfo::read(&mut cs, a).unwrap();
    assert_eq!(std::format!("{d}"), "0000:00:00.0 dead:000c unknown class (class 400102)");
    assert_eq!((d.name(), d.class_name()), (None, "unknown class"));
}
