//! Networking and socket subsystem (M9; ARCHITECTURE sections 7, 8 and 10).
//!
//! `sock` owns the generic socket file object ([`sock::SockOps`], `sockfs` on
//! dev `0:8`, [`sock::InFlightFile`], [`sock::block_on`]) shared by every
//! socket family (C1 `AF_UNIX`, C3 UDP, C4 TCP, C6 ICMP).
//! `virtio_net` owns the NIC and ARP diagnostics; `stack` owns startup,
//! serialized stack access and the polling worker. `inet` owns identities,
//! readiness and common protocol dispatch. `tcp`, `udp` and `icmp` own
//! protocol state, constructors and the delegated SockOps implementations.

pub mod sock;

/// Initialize the networking subsystem at boot, after `vfs::init` and
/// `linux::init`. Populated by B3 (`NetStack` and the `net` thread) and B4
/// (address configuration).
pub fn init() {
    stack::init();
}

pub mod virtio_net;

pub mod inet;
pub mod stack;

pub mod icmp;
pub mod tcp;
pub mod udp;
