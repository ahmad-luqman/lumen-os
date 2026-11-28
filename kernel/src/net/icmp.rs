//! C6 protocol implementation boundary (#474 scaffold).

use super::inet::InetSocket;
use super::sock::{RecvDone, RecvIo, SendIo, ShutdownHow, SockOps};
use crate::linux::errno;
use alloc::sync::Arc;
use lumen_sockabi::{OptVal, SockAddr, SockOpt};

/// Owns protocol-specific interior mutability. Send/receive sequencing is
/// provided by InetSocket's Lock 0 locks; state leaf locks must not enclose
/// user copies, allocation-sensitive stack work, or blocking waits.
pub struct IcmpState {
    pub raw: bool,
}
impl IcmpState {
    pub(super) fn bind(&self, _socket: &InetSocket, _addr: &SockAddr) -> Result<(), u32> {
        Err(errno::EOPNOTSUPP)
    }
    pub(super) fn listen(&self, _socket: &InetSocket, _backlog: i32) -> Result<(), u32> {
        Err(errno::EOPNOTSUPP)
    }
    pub(super) fn accept(
        &self,
        _socket: &InetSocket,
        _nonblock: bool,
    ) -> Result<(Arc<dyn SockOps>, Option<SockAddr>), u32> {
        Err(errno::EOPNOTSUPP)
    }
    pub(super) fn connect(&self, _socket: &InetSocket, _addr: &SockAddr, _nonblock: bool) -> Result<(), u32> {
        Err(errno::EOPNOTSUPP)
    }
    pub(super) fn disconnect(&self, _socket: &InetSocket) -> Result<(), u32> {
        Err(errno::EOPNOTSUPP)
    }
    pub(super) fn send(&self, _socket: &InetSocket, _io: &mut SendIo<'_>) -> Result<usize, u32> {
        Err(errno::EOPNOTSUPP)
    }
    pub(super) fn recv(&self, _socket: &InetSocket, _io: &mut RecvIo<'_>) -> Result<RecvDone, u32> {
        Err(errno::EOPNOTSUPP)
    }
    pub(super) fn shutdown(&self, _socket: &InetSocket, _how: ShutdownHow) -> Result<(), u32> {
        Err(errno::EOPNOTSUPP)
    }
    pub(super) fn local_name(&self, _socket: &InetSocket) -> Result<SockAddr, u32> {
        Err(errno::EOPNOTSUPP)
    }
    pub(super) fn peer_name(&self, _socket: &InetSocket) -> Result<SockAddr, u32> {
        Err(errno::EOPNOTSUPP)
    }
    pub(super) fn set_opt(&self, _socket: &InetSocket, _opt: SockOpt, _val: &OptVal) -> Result<(), u32> {
        Err(errno::EOPNOTSUPP)
    }
    pub(super) fn get_opt(&self, _socket: &InetSocket, _opt: SockOpt) -> Result<OptVal, u32> {
        Err(errno::EOPNOTSUPP)
    }
    pub(super) fn fionread(&self, _socket: &InetSocket) -> Result<usize, u32> {
        Err(errno::EOPNOTSUPP)
    }
}
