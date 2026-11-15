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
    assert_eq!(a.function_offset(), 31 << 15 | 7 << 12);
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
    assert!(bars[4].unwrap().slots() == 2 && !bars[4].unwrap().is_memory());
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

/// A bridge at `addr` with these bus numbers, read.
fn bridge_at(addr: PciAddr, primary: u8, secondary: u8, subordinate: u8) -> DeviceInfo {
    let f = Function::endpoint(0x1b36, 0x000c, 6, 4).header_type(1).bridge_buses(primary, secondary, subordinate);
    let mut cs = Fake::default().with(addr, f);
    DeviceInfo::read(&mut cs, addr).unwrap()
}

#[test]
fn a_bridges_bus_numbers_are_checked() {
    let on_bus_0 = |secondary, subordinate| bridge_at(at(0, 0, 0x1c, 0), 0, secondary, subordinate);
    // ECAM covers segment 0's buses 0-15.
    let covered = |segment: u16, bus: u8| segment == 0 && bus < 16;
    let problems = |d: DeviceInfo| d.problems(covered).collect::<Vec<_>>();
    assert_eq!(problems(on_bus_0(1, 15)), []);
    assert_eq!(problems(on_bus_0(0, 0)), [Problem::BridgeUnconfigured]);
    assert_eq!(problems(on_bus_0(0, 5)), [Problem::BridgeUnconfigured]);
    for (secondary, subordinate) in [(1, 16), (16, 16)] {
        assert_eq!(
            problems(on_bus_0(secondary, subordinate)),
            [Problem::BusesOutsideEcam { secondary, subordinate }],
            "{secondary}-{subordinate}"
        );
    }
    assert_eq!(problems(on_bus_0(5, 4)), [Problem::BridgeBusesBackwards { secondary: 5, subordinate: 4 }]);
    // On bus 2: its primary bus must be 2, its secondary above 2.
    let on_bus_2 = |primary, secondary| bridge_at(at(0, 2, 0, 0), primary, secondary, 9);
    assert_eq!(problems(on_bus_2(2, 3)), []);
    assert_eq!(problems(on_bus_2(0, 3)), [Problem::BridgeMisnumbered { primary: 0, secondary: 3, subordinate: 9 }]);
    assert_eq!(problems(on_bus_2(2, 2)), [Problem::BridgeMisnumbered { primary: 2, secondary: 2, subordinate: 9 }]);
    // `covered` is asked about the bridge's own segment.
    let in_segment_1 = bridge_at(at(1, 0, 0x1c, 0), 0, 1, 15);
    assert_eq!(in_segment_1.problems(|segment, bus| segment == 1 && bus < 16).count(), 0);
    assert_eq!(
        in_segment_1.problems(covered).collect::<Vec<_>>(),
        [Problem::BusesOutsideEcam { secondary: 1, subordinate: 15 }]
    );
    assert_eq!(
        std::format!("{}", Problem::BusesOutsideEcam { secondary: 1, subordinate: 16 }),
        "the buses behind this bridge (1-16) aren't all in mapped configuration space: devices there aren't listed"
    );
}

#[test]
fn bar_slots_say_why_they_hold_no_bar() {
    let a = at(0, 0, 3, 0);
    let read = |f: Function| {
        let mut cs = Fake::default().with(a, f);
        DeviceInfo::read(&mut cs, a).unwrap()
    };
    let rng = read(virtio_rng());
    assert_eq!(
        rng.bar_slot(4),
        Ok(Bar::Memory { phys: 0x80_0000_4000, size: 0x4000, prefetchable: true, is_64: true })
    );
    assert_eq!(rng.bar_slot(5), Err(NoBar::UpperHalf));
    assert_eq!(rng.bar_slot(0), Err(NoBar::NotImplemented));
    assert_eq!(rng.bar_slot(6), Err(NoBar::OutOfRange));
    let unassigned = read(virtio_rng().mem64(4, 0, 0x4000, true));
    assert_eq!(unassigned.bar_slot(4), Ok(Bar::Unassigned { io: false, size: 0x4000, is_64: true }));
    assert_eq!(unassigned.bar_slot(5), Err(NoBar::UpperHalf));
    // A 64-bit range ending at 2^64 still takes both slots.
    let top =
        read(Function::endpoint(1, 2, 3, 4).raw_bar(0, 0xffff_f004, 0xffff_f000).raw_bar(1, 0xffff_ffff, 0xffff_ffff));
    let past = BarProblem::PastAddressSpace { phys: 0xffff_ffff_ffff_f000, size: 0x1000 };
    assert_eq!((top.bar_slot(0), top.bar_slot(1)), (Ok(Bar::Malformed(past)), Err(NoBar::UpperHalf)));
    // A 64-bit BAR in the last slot takes only it.
    let last = read(Function::endpoint(1, 2, 3, 4).raw_bar(5, 0xfd00_0004, 0xffff_f000));
    assert_eq!(last.bar_slot(5), Ok(Bar::Malformed(BarProblem::NoUpperHalf)));
    assert_eq!(Bar::Malformed(BarProblem::NoUpperHalf).slots(), 1);
    // A bridge's slots past its 2 BARs.
    let bridge = bridge_at(at(0, 0, 0x1c, 0), 0, 1, 1);
    assert_eq!(bridge.bar_slot(2), Err(NoBar::NotImplemented));
}

#[test]
fn a_below_1_mib_memory_bar_is_decoded_as_32_bit() {
    // Type 0b01, a PCI 2.x leftover.
    let a = at(0, 0, 3, 0);
    let mut cs = Fake::default().with(a, Function::endpoint(1, 2, 3, 4).raw_bar(0, 0x000d_0002, 0xffff_0000));
    let bars = DeviceInfo::read(&mut cs, a).unwrap().bars;
    assert_eq!(bars[0], Some(Bar::Memory { phys: 0x000d_0000, size: 0x1_0000, prefetchable: false, is_64: false }));
}

#[test]
fn io_bars_without_writable_bits_or_with_the_reserved_bit_set() {
    let a = at(0, 0, 3, 0);
    // An I/O BAR none of whose address bits stick: not implemented.
    // Bit 1 of an I/O BAR is reserved: it isn't part of the port.
    let f = Function::endpoint(1, 2, 3, 4).raw_bar(0, 0x1, 0).raw_bar(1, 0xc043, 0xffff_ffe0);
    let mut cs = Fake::default().with(a, f);
    let bars = DeviceInfo::read(&mut cs, a).unwrap().bars;
    assert_eq!(bars[..2], [None, Some(Bar::Io { port: 0xc040, size: 0x20 })]);
}

#[test]
fn virtio_ids_are_transitional_up_to_0x103f_and_modern_to_0x107f() {
    assert_eq!(names::virtio_type(VIRTIO, 0x103f, 4), Some(4));
    assert_eq!(names::virtio_type(VIRTIO, 0x1040, 4), Some(0));
    assert_eq!(names::virtio_type(VIRTIO, 0x107f, 4), Some(0x3f));
    assert_eq!(names::virtio_type(VIRTIO, 0x1080, 4), None);
    assert_eq!(names::virtio_type(VIRTIO, 0x0fff, 4), None);
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
        (
            std::format!("{}", Problem::BridgeUnconfigured),
            "this bridge has no secondary bus (not configured): devices behind it aren't listed",
        ),
        (
            std::format!("{}", Problem::BridgeMisnumbered { primary: 0, secondary: 2, subordinate: 9 }),
            "this bridge's bus numbers (primary 0, secondary 2, subordinate 9) don't fit the bus it is on: \
             devices behind it may not be listed",
        ),
        (
            std::format!("{}", Problem::BridgeBusesBackwards { secondary: 5, subordinate: 4 }),
            "this bridge's secondary bus 5 is above its subordinate bus 4: devices behind it may not be listed",
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
    assert_eq!(d.virtio_type(), None);
    let mut cs = Fake::default().with(a, virtio_rng());
    assert_eq!(DeviceInfo::read(&mut cs, a).unwrap().virtio_type(), Some(4));
}

/// [`virtio_rng`]'s virtio capabilities as QEMU lays them out, in list
/// order: the PCI-configuration-access one (type 5, BAR 0, length 0) at
/// 0x84, notify at 0x70 (length field 20, its multiplier after), device at
/// 0x60, ISR at 0x50 and common at 0x40, each 4 KiB of BAR 4.
const RNG_CAPS: [virtio::VirtioCap; 5] = [
    vcap(0x84, 20, 5, 0, 0, 0),
    vcap(0x70, 20, virtio::NOTIFY_CFG, 4, 0x3000, 0x1000),
    vcap(0x60, 16, virtio::DEVICE_CFG, 4, 0x2000, 0x1000),
    vcap(0x50, 16, virtio::ISR_CFG, 4, 0x1000, 0x1000),
    vcap(0x40, 16, virtio::COMMON_CFG, 4, 0, 0x1000),
];

/// [`virtio_rng`] with [`RNG_CAPS`] filled in.
fn virtio_rng_caps() -> Function {
    RNG_CAPS.iter().fold(virtio_rng(), |f, c| f.virtio_cap(c.at, c.cap_len, c.cfg_type, c.bar, c.offset, c.length))
}

fn virtio_caps(f: Function) -> Vec<virtio::VirtioCap> {
    let a = at(0, 0, 3, 0);
    let mut cs = Fake::default().with(a, f);
    let d = DeviceInfo::read(&mut cs, a).unwrap();
    d.caps.iter().filter(|c| c.id == cap::id::VENDOR).map(|c| virtio::VirtioCap::read(&mut cs, a, c)).collect()
}

const fn vcap(at: u8, cap_len: u8, cfg_type: u8, bar: u8, offset: u32, length: u32) -> virtio::VirtioCap {
    virtio::VirtioCap { at, cap_len, cfg_type, bar, offset, length }
}

#[test]
fn virtio_capabilities_are_read_in_list_order() {
    assert_eq!(virtio_caps(virtio_rng_caps()), RNG_CAPS);
    // Fields past the 256-byte header aren't read: a capability at 0xfc
    // has only its header dword.
    let near_end = virtio_caps(virtio_rng().caps(0xfc, &[(0xfc, 9, 0)]).virtio_cap(0xfc, 16, 1, 4, 0, 0));
    assert_eq!(near_end, [vcap(0xfc, 16, 1, 0, 0, 0)]);
}

#[test]
fn used_keeps_what_pci_transport_new_keeps() {
    let u = virtio::used(RNG_CAPS);
    assert_eq!((u.common, u.notify), (Some(RNG_CAPS[4]), Some(RNG_CAPS[1])));
    assert_eq!((u.isr, u.device), (Some(RNG_CAPS[3]), Some(RNG_CAPS[2])));
    assert_eq!(u.iter().map(|c| c.cfg_type).collect::<Vec<_>>(), [1, 2, 3, 4]);
    // The first of each type counts; a length field below 16 is skipped,
    // and a notify capability needs 20 (its multiplier follows).
    let u = virtio::used([
        vcap(0x40, 15, virtio::COMMON_CFG, 0, 0, 0),
        vcap(0x50, 16, virtio::COMMON_CFG, 1, 0, 0),
        vcap(0x60, 16, virtio::COMMON_CFG, 2, 0, 0),
        vcap(0x70, 16, virtio::NOTIFY_CFG, 3, 0, 0),
        vcap(0x80, 20, virtio::NOTIFY_CFG, 4, 0, 0),
        vcap(0x90, 20, virtio::NOTIFY_CFG, 5, 0, 0),
        vcap(0xa0, 16, 5, 0, 0, 0),
        vcap(0xb0, 16, 9, 0, 0, 0),
    ]);
    assert_eq!((u.common.map(|c| c.bar), u.notify.map(|c| c.bar)), (Some(1), Some(4)));
    assert_eq!((u.isr, u.device), (None, None));
}

#[test]
fn check_wants_each_kept_structure_inside_a_mapped_bar() {
    use virtio::CapProblem;
    let caps = RNG_CAPS;
    let bar4 = |size| move |bar| (bar == 4).then_some(size);
    // QEMU's layout in its 16 KiB BAR 4 passes.
    assert_eq!(virtio::check(&caps, bar4(0x4000)), Ok(virtio::used(caps.iter().copied())));
    let common = caps[4];
    // BAR 4 not mapped (or missing): the first kept structure, common,
    // is refused.
    assert_eq!(virtio::check(&caps, |_| None), Err(CapProblem::BarNotMapped { cap: common }));
    // A BAR too small for the notify structure at 0x3000.
    assert_eq!(virtio::check(&caps, bar4(0x3fff)), Err(CapProblem::PastBar { cap: caps[1], bar_size: 0x3fff }));
    // A BAR index above 5 is refused even if the lookup answered.
    let bad = [vcap(0x40, 16, virtio::COMMON_CFG, 6, 0, 0x10)];
    assert_eq!(virtio::check(&bad, |_| Some(0x4000)), Err(CapProblem::BarNotMapped { cap: bad[0] }));
    // Offset plus length past 4 GiB (the crate adds them in a u32).
    let wraps = [vcap(0x40, 16, virtio::COMMON_CFG, 4, 0xffff_f000, 0x2000)];
    assert_eq!(
        virtio::check(&wraps, |_| Some(u64::MAX)),
        Err(CapProblem::PastBar { cap: wraps[0], bar_size: u64::MAX })
    );
    // A structure ending exactly at the BAR's end fits.
    let exact = [vcap(0x40, 16, virtio::COMMON_CFG, 4, 0x3000, 0x1000)];
    assert!(virtio::check(&exact, bar4(0x4000)).is_ok());
    // What the crate won't keep isn't checked: a second common capability
    // in an unmapped BAR, the configuration-access one (type 5, BAR 0).
    let second = [vcap(0x40, 16, virtio::COMMON_CFG, 4, 0, 0x38), vcap(0x50, 16, virtio::COMMON_CFG, 2, 0, 0x10)];
    assert!(virtio::check(&second, bar4(0x4000)).is_ok());
}

#[test]
fn check_wants_each_kept_structure_as_long_as_what_the_crate_maps() {
    use virtio::CapProblem;
    // The common configuration is 56 bytes (up to queue_device at 48).
    assert_eq!(virtio::common::LEN, 56);
    assert_eq!(virtio::common::DEVICE_STATUS, 20);
    assert_eq!((virtio::common::MSIX_CONFIG, virtio::common::QUEUE_SELECT), (16, 22));
    assert_eq!((virtio::common::QUEUE_MSIX_VECTOR, virtio::common::QUEUE_NOTIFY_OFF), (26, 30));
    let one = |cfg_type, length| [vcap(0x40, 20, cfg_type, 4, 0, length)];
    let bar = |_| Some(0x1000);
    for (cfg_type, need) in
        [(virtio::COMMON_CFG, 56), (virtio::NOTIFY_CFG, 2), (virtio::ISR_CFG, 1), (virtio::DEVICE_CFG, 4)]
    {
        assert_eq!(virtio::min_length(cfg_type), need);
        assert!(virtio::check(&one(cfg_type, need), bar).is_ok(), "type {cfg_type}");
        let short = one(cfg_type, need - 1);
        assert_eq!(virtio::check(&short, bar), Err(CapProblem::TooShort { cap: short[0], need }), "type {cfg_type}");
    }
    // A notify capability with a length field of 19 is skipped (no
    // multiplier), so a short notify structure behind it isn't checked.
    let skipped = [vcap(0x40, 19, virtio::NOTIFY_CFG, 4, 0, 0)];
    assert_eq!(virtio::used(skipped).notify, None);
    assert!(virtio::check(&skipped, bar).is_ok());
}

#[test]
fn a_queues_notify_register_must_be_inside_the_notify_structure() {
    use virtio::CapProblem;
    // QEMU's: 4 KiB, multiplier 4.
    let notify = vcap(0x70, 20, virtio::NOTIFY_CFG, 4, 0x3000, 0x1000);
    assert_eq!(virtio::notify_fits(notify, 4, 0, 0), Ok(()));
    assert_eq!(virtio::notify_fits(notify, 4, 3, 1023), Ok(()));
    assert_eq!(
        virtio::notify_fits(notify, 4, 3, 1024),
        Err(CapProblem::NotifyPastEnd { cap: notify, queue: 3, notify_off: 1024, multiplier: 4 })
    );
    // A 2-byte structure holds exactly one register, at 0.
    let tiny = vcap(0x70, 20, virtio::NOTIFY_CFG, 4, 0, 2);
    assert_eq!(virtio::notify_fits(tiny, 0, 5, 77), Ok(()));
    assert_eq!(virtio::notify_fits(tiny, 0, 0, 1), Ok(()));
    assert!(virtio::notify_fits(tiny, 2, 0, 1).is_err());
    // An odd length: the crate sees length / 2 registers (one here).
    let odd = vcap(0x70, 20, virtio::NOTIFY_CFG, 4, 0, 3);
    assert_eq!(virtio::notify_fits(odd, 1, 0, 1), Ok(()));
    assert!(virtio::notify_fits(odd, 2, 0, 1).is_err());
    // No overflow at the extremes.
    assert!(virtio::notify_fits(notify, u32::MAX, 0, u16::MAX).is_err());
}

#[test]
fn writes_to_the_msi_and_msix_capabilities_are_found() {
    use crate::cap::{MSIX_LEN, msi_len};
    assert_eq!(MSIX_LEN, 12);
    assert_eq!([msi_len(0), msi_len(1 << 7), msi_len(1 << 8), msi_len(1 << 7 | 1 << 8)], [12, 16, 20, 24]);
    // virtio_rng: MSI-X at 0x98 (12 bytes); a 64-bit MSI with masking at
    // 0xa8 (24 bytes) added after it.
    let a = at(0, 0, 3, 0);
    let mut f = virtio_rng_caps().caps(0x98, &[(0x98, cap::id::MSIX, 0xa8), (0xa8, cap::id::MSI, 0x84)]);
    f.regs[0xa8 / 4] |= (1 << 7 | 1 << 8) << 16;
    let mut cs = Fake::default().with(a, f);
    let d = DeviceInfo::read(&mut cs, a).unwrap();
    assert!(d.caps.is_complete());
    let hit = |cs: &mut Fake, off| d.caps.interrupt_cap_at(cs, a, off).map(|c| c.id);
    assert_eq!(hit(&mut cs, 0x94), None);
    assert_eq!(hit(&mut cs, 0x98), Some(cap::id::MSIX));
    assert_eq!(hit(&mut cs, 0xa0), Some(cap::id::MSIX));
    assert_eq!(hit(&mut cs, 0xa4), None);
    assert_eq!(hit(&mut cs, 0xa8), Some(cap::id::MSI));
    assert_eq!(hit(&mut cs, 0xbc), Some(cap::id::MSI));
    assert_eq!(hit(&mut cs, 0xc0), None);
    // The virtio capabilities and the header aren't.
    assert_eq!(hit(&mut cs, 0x40), None);
    assert_eq!(hit(&mut cs, 0x04), None);
}

#[test]
fn check_refuses_capabilities_the_crate_cant_read_inside_the_header() {
    use virtio::CapProblem;
    // The crate reads the dwords at +4, +8 and +12 of every capability
    // with a length field of 16 or more, adding the offsets in a u8.
    let at_f0 = [vcap(0xf0, 16, virtio::COMMON_CFG, 4, 0, 0x38)];
    assert!(virtio::check(&at_f0, |_| Some(0x1000)).is_ok());
    let at_f4 = [vcap(0xf4, 16, 9, 0, 0, 0)];
    assert_eq!(virtio::check(&at_f4, |_| Some(0x1000)), Err(CapProblem::PastHeader { cap: at_f4[0] }));
    // ... and +16 of the notify one it keeps.
    let notify_ec = [vcap(0xec, 20, virtio::NOTIFY_CFG, 4, 0, 0x10)];
    assert!(virtio::check(&notify_ec, |_| Some(0x1000)).is_ok());
    let notify_f0 = [vcap(0xf0, 20, virtio::NOTIFY_CFG, 4, 0, 0x10)];
    assert_eq!(virtio::check(&notify_f0, |_| Some(0x1000)), Err(CapProblem::PastHeader { cap: notify_f0[0] }));
    // A capability it skips (length field below 16) may sit anywhere.
    let short = [vcap(0xfc, 4, 9, 0, 0, 0)];
    assert!(virtio::check(&short, |_| None).is_ok());
}

#[test]
fn virtio_cap_problems_say_which_structure_and_why() {
    use virtio::CapProblem;
    let cases = [
        (
            CapProblem::PastHeader { cap: vcap(0xf4, 16, virtio::COMMON_CFG, 4, 0, 0) },
            "the common configuration capability at 0xf4 runs past the 256-byte header",
        ),
        (
            CapProblem::BarNotMapped { cap: vcap(0x70, 20, virtio::NOTIFY_CFG, 2, 0, 0) },
            "the notification structure is in BAR 2, which isn't a mapped memory BAR",
        ),
        (
            CapProblem::PastBar { cap: vcap(0x50, 16, virtio::ISR_CFG, 4, 0x1000, 0x1000), bar_size: 0x1800 },
            "the ISR status structure (0x1000 bytes at 0x1000) runs past BAR 4 (0x1800 bytes)",
        ),
        (
            CapProblem::BarNotMapped { cap: vcap(0x60, 16, virtio::DEVICE_CFG, 7, 0, 0) },
            "the device configuration structure is in BAR 7, which isn't a mapped memory BAR",
        ),
        (
            CapProblem::TooShort { cap: vcap(0x40, 16, virtio::COMMON_CFG, 4, 0, 0x30), need: 0x38 },
            "the common configuration structure is 0x30 bytes, shorter than 0x38",
        ),
        (
            CapProblem::NotifyPastEnd {
                cap: vcap(0x70, 20, virtio::NOTIFY_CFG, 4, 0, 0x1000),
                queue: 2,
                notify_off: 0x400,
                multiplier: 4,
            },
            "queue 2's notify register at 0x1000 (queue_notify_off 1024 times 4) is outside the notification \
             structure (0x1000 bytes)",
        ),
        (
            CapProblem::NotifyPastEnd {
                cap: vcap(0x70, 20, virtio::NOTIFY_CFG, 4, 0, 0x1000),
                queue: 0,
                notify_off: u16::MAX,
                multiplier: u32::MAX,
            },
            "queue 0's notify register at 0xfffeffff0001 (queue_notify_off 65535 times 4294967295) is outside the \
             notification structure (0x1000 bytes): both read all ones, the device isn't responding",
        ),
    ];
    for (problem, want) in cases {
        assert_eq!(std::format!("{problem}"), want);
    }
}

/// `virtio_rng()` with its MSI-X capability's registers as QEMU sets them:
/// 2 entries (table size field 1), the table at BAR 1 offset 0, the PBA at
/// BAR 1 offset 0x800, Message Control otherwise clear.
fn virtio_rng_with_msix(control: u16, table: u32, pba: u32) -> Function {
    let mut f = virtio_rng();
    // The header (id 0x11, next 0x84) stays as `caps` laid it out.
    f.regs[0x98 / 4] = f.regs[0x98 / 4] & 0xffff | u32::from(control) << 16;
    f.regs[0x9c / 4] = table;
    f.regs[0xa0 / 4] = pba;
    f
}

#[test]
fn the_msix_capability_says_where_its_table_is_and_how_big() {
    use crate::msix::{Capability, Location};
    let a = at(0, 0, 3, 0);
    let mut cs = Fake::default().with(a, virtio_rng_with_msix(1, 1, 0x801));
    let caps = DeviceInfo::read(&mut cs, a).expect("present").caps;
    let cap = caps.find(cap::id::MSIX).expect("an MSI-X capability");
    let m = Capability::read(&mut cs, a, cap).expect("usable");
    assert_eq!(m.offset, 0x98);
    assert_eq!(m.table, Location { bir: 1, offset: 0 });
    assert_eq!(m.pba, Location { bir: 1, offset: 0x800 });
    assert_eq!((m.table_size(), m.table_len()), (2, 32));
    assert_eq!([m.entry_offset(0), m.entry_offset(1), m.entry_offset(2)], [Some(0), Some(16), None]);
    // The offset's low 3 bits are the BIR, not part of it; the table size
    // field's top (2047) means 2048 entries; the control bits above it
    // don't count.
    let mut cs = Fake::default().with(a, virtio_rng_with_msix(0xc7ff, 0x2004, 5));
    let m = Capability::read(&mut cs, a, cap).expect("usable");
    assert_eq!(m.table, Location { bir: 4, offset: 0x2000 });
    assert_eq!((m.table_size(), m.table_len()), (2048, 2048 * 16));
    assert_eq!(m.control, 0xc7ff);
}

#[test]
fn a_reserved_msix_bar_indicator_is_refused() {
    use crate::msix::{Capability, MsixProblem};
    let a = at(0, 0, 3, 0);
    let cap = Cap { id: cap::id::MSIX, offset: 0x98 };
    for (table, pba, want) in
        [(6, 1, MsixProblem::TableBir(6)), (0x1007, 1, MsixProblem::TableBir(7)), (1, 6, MsixProblem::PbaBir(6))]
    {
        let mut cs = Fake::default().with(a, virtio_rng_with_msix(0, table, pba));
        assert_eq!(Capability::read(&mut cs, a, cap), Err(want));
    }
    assert_eq!(std::format!("{}", MsixProblem::TableBir(6)), "the MSI-X table's BAR indicator is 6 (reserved)");
    assert_eq!(
        std::format!("{}", MsixProblem::PbaBir(7)),
        "the MSI-X pending bit array's BAR indicator is 7 (reserved)"
    );
}

#[test]
fn msix_enable_clears_the_function_mask_and_keeps_the_rest() {
    use crate::msix::{CONTROL_ENABLE, CONTROL_FUNCTION_MASK, enabled, with_control};
    assert_eq!((CONTROL_ENABLE, CONTROL_FUNCTION_MASK), (0x8000, 0x4000));
    assert_eq!(enabled(0x0001), 0x8001);
    assert_eq!(enabled(0x4001), 0x8001);
    assert_eq!(enabled(0xc7ff), 0x87ff);
    // The header's id and next pointer stay as read.
    assert_eq!(with_control(0x4001_8411, 0x8001), 0x8001_8411);
    assert_eq!(with_control(0xffff_0011, 0), 0x0000_0011);
}

#[test]
fn an_msix_entry_encodes_fixed_delivery_to_one_local_apic() {
    use crate::msix::{
        ENTRY_ADDR_HI, ENTRY_ADDR_LO, ENTRY_DATA, ENTRY_LEN, ENTRY_VECTOR_CONTROL, Message, VECTOR_MASKED,
    };
    // The layout (PCI 3.0 §6.8.2.6-9) and the mask bit.
    assert_eq!((ENTRY_ADDR_LO, ENTRY_ADDR_HI, ENTRY_DATA, ENTRY_VECTOR_CONTROL, ENTRY_LEN), (0, 4, 8, 12, 16));
    assert_eq!(VECTOR_MASKED, 1);
    // To APIC 0, vector 0x90: 0xfee00000, physical mode, no redirection
    // hint; data = the vector, fixed delivery, edge triggered.
    let m = Message::fixed(0, 0x90);
    assert_eq!(m, Message { address: 0xfee0_0000, data: 0x90 });
    assert_eq!(m.entry(true), [0xfee0_0000, 0, 0x90, 1]);
    assert_eq!(m.entry(false), [0xfee0_0000, 0, 0x90, 0]);
    // The destination id goes in address bits 12-19.
    let m = Message::fixed(0xab, 0xef);
    assert_eq!(m.address, 0xfeea_b000);
    assert_eq!(m.entry(false), [0xfeea_b000, 0, 0xef, 0]);
    assert_eq!(Message::fixed(0xff, 0x91).address >> 12 & 0xff, 0xff);
}
