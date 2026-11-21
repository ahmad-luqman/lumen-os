//! Limine boot module discovery for static Linux ELF64 binaries (M6).
//!
//! Boot modules are attached to the ISO by `tools/mkiso.sh` (`module_path` /
//! `module_string` in `limine.conf`) and mapped read-only by Limine in the
//! HHDM. Keeping userspace binaries (including GPL `busybox`) as external boot
//! modules keeps the MIT-licensed kernel ELF completely free of third-party
//! binaries.

use alloc::vec::Vec;

use crate::boot;

/// A read-only Linux binary loaded into memory by Limine as a boot module.
#[derive(Clone, Copy)]
pub struct BootModule {
    pub name: &'static str,
    pub bytes: &'static [u8],
}

/// List all boot modules provided by Limine.
pub fn list() -> Vec<BootModule> {
    let Some(resp) = boot::MODULES.get_response() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for file in resp.modules() {
        let name = file
            .string()
            .to_str()
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| file.path().to_str().ok().and_then(|p| p.rsplit('/').next()).filter(|s| !s.is_empty()))
            .unwrap_or("module");
        // SAFETY: Limine maps boot modules in the HHDM (`EXECUTABLE_AND_MODULES`
        // or `BOOTLOADER_RECLAIMABLE`), both of which `mm::paging` maps for the
        // kernel's entire lifetime and `mm::frame` reserves (non-usable).
        let bytes = unsafe { core::slice::from_raw_parts(file.addr(), file.size() as usize) };
        out.push(BootModule { name, bytes });
    }
    out
}

/// Find a Limine boot module by its configured name (`module_string` or path basename).
pub fn find(name: &str) -> Option<BootModule> {
    list().into_iter().find(|m| m.name == name)
}
