//! Readable names for the devices and classes Lumen meets: QEMU q35's
//! chipset, its display and NICs, its bridges, and the virtio devices.
//! Anything else has no device name, and a class name from the class code
//! alone.

/// Red Hat's vendor id, which every virtio PCI device uses.
pub const VIRTIO_VENDOR: u16 = 0x1af4;

/// The virtio device type of a virtio PCI device: modern ones encode it in
/// the device id (`0x1040 + type`), transitional ones (`0x1000-0x103f`) in
/// the subsystem id. `None` for other vendors and ids.
pub fn virtio_type(vendor: u16, device: u16, subsystem_id: u16) -> Option<u16> {
    match (vendor, device) {
        (VIRTIO_VENDOR, 0x1000..=0x103f) => Some(subsystem_id),
        (VIRTIO_VENDOR, 0x1040..=0x107f) => Some(device - 0x1040),
        _ => None,
    }
}

/// The name of a virtio device type (virtio 1.2, section 5).
fn virtio_name(kind: u16) -> Option<&'static str> {
    Some(match kind {
        1 => "virtio network",
        2 => "virtio block",
        3 => "virtio console",
        4 => "virtio rng",
        5 => "virtio balloon",
        8 => "virtio SCSI",
        9 => "virtio 9p",
        16 => "virtio GPU",
        18 => "virtio input",
        19 => "virtio vsock",
        25 => "virtio sound",
        _ => return None,
    })
}

/// The device's name, if Lumen knows it (`subsystem_id` tells
/// transitional virtio devices apart).
pub fn device(vendor: u16, device: u16, subsystem_id: u16) -> Option<&'static str> {
    if let Some(kind) = virtio_type(vendor, device, subsystem_id) {
        return virtio_name(kind);
    }
    Some(match (vendor, device) {
        (0x8086, 0x29c0) => "Q35 host bridge",
        (0x8086, 0x2918) => "ICH9 LPC bridge",
        (0x8086, 0x2922) => "ICH9 SATA (AHCI)",
        (0x8086, 0x2930) => "ICH9 SMBus",
        (0x8086, 0x10d3) => "82574L ethernet (e1000e)",
        (0x8086, 0x100e) => "82540EM ethernet (e1000)",
        (0x1234, 0x1111) => "standard VGA",
        (0x1b36, 0x0001) => "PCI-to-PCI bridge",
        (0x1b36, 0x000c) => "PCIe root port",
        (0x1b36, 0x000d) => "xHCI USB controller",
        _ => return None,
    })
}

/// A name for the class code: the subclass's where Lumen knows it, else the
/// class's, else `"unknown class"`.
pub fn class(class: u8, subclass: u8) -> &'static str {
    let specific = match (class, subclass) {
        (0x01, 0x00) => Some("SCSI controller"),
        (0x01, 0x01) => Some("IDE controller"),
        (0x01, 0x06) => Some("SATA controller"),
        (0x01, 0x08) => Some("NVM controller"),
        (0x02, 0x00) => Some("ethernet controller"),
        (0x03, 0x00) => Some("VGA controller"),
        (0x04, 0x01) => Some("audio device"),
        (0x04, 0x03) => Some("audio device"),
        (0x06, 0x00) => Some("host bridge"),
        (0x06, 0x01) => Some("ISA bridge"),
        (0x06, 0x04) => Some("PCI bridge"),
        (0x09, 0x00) => Some("keyboard controller"),
        (0x09, 0x02) => Some("mouse controller"),
        (0x0c, 0x03) => Some("USB controller"),
        (0x0c, 0x05) => Some("SMBus controller"),
        _ => None,
    };
    specific.unwrap_or(match class {
        0x00 => "unclassified device",
        0x01 => "storage controller",
        0x02 => "network controller",
        0x03 => "display controller",
        0x04 => "multimedia controller",
        0x05 => "memory controller",
        0x06 => "bridge",
        0x07 => "communication controller",
        0x08 => "system peripheral",
        0x09 => "input device",
        0x0a => "docking station",
        0x0b => "processor",
        0x0c => "serial bus controller",
        0x0d => "wireless controller",
        0x0e => "intelligent controller",
        0x0f => "satellite controller",
        0x10 => "encryption controller",
        0x11 => "signal processing controller",
        0x12 => "processing accelerator",
        0x13 => "non-essential instrumentation",
        0xff => "vendor-specific device",
        _ => "unknown class",
    })
}
