use lumen_vfs::DevId;

#[test]
fn named_filesystem_device_ids_are_pairwise_distinct() {
    let devices =
        [DevId::ROOT, DevId::TMPFS, DevId::DEVFS, DevId::PROCFS, DevId::DEVPTS, DevId::PIPEFS, DevId::DEV_SHM];
    for (i, device) in devices.iter().enumerate() {
        for other in &devices[i + 1..] {
            assert_ne!(device, other, "filesystem device identifiers must not alias");
        }
    }
}
