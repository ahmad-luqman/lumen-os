//! Networking and socket subsystem (M9; ARCHITECTURE sections 7, 8 and 10).
//!
//! `sock` owns the generic socket file object ([`sock::SockOps`], `sockfs` on
//! dev `0:8`, [`sock::InFlightFile`], [`sock::block_on`]) shared by every
//! socket family (C1 `AF_UNIX`, C3 UDP, C4 TCP, C6 ICMP).

pub mod sock;

/// Initialize the networking subsystem at boot, after `vfs::init` and
/// `linux::init`.
pub fn init() {
    log::debug!(target: "net", "net subsystem initialized");
}
