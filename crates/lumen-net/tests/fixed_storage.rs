use lumen_net::*;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct Checked;
thread_local! {
    static FAIL: Cell<Option<usize>> = const { Cell::new(None) };
    static MEASURE: Cell<Option<(usize, usize)>> = const { Cell::new(None) };
}
fn reject(size: usize) -> bool {
    MEASURE.with(|m| {
        if let Some((n, bytes)) = m.get() {
            m.set(Some((n + 1, bytes + size)));
        }
    });
    FAIL.with(|f| match f.get() {
        Some(0) => {
            f.set(None);
            true
        }
        Some(n) => {
            f.set(Some(n - 1));
            false
        }
        None => false,
    })
}
// SAFETY: accepted operations delegate unchanged to System; a rejected
// allocation returns null, which GlobalAlloc permits. TLS cells allocate nothing.
unsafe impl GlobalAlloc for Checked {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if reject(layout.size()) {
            return std::ptr::null_mut();
        }
        // SAFETY: caller supplies a valid allocation layout.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if reject(layout.size()) {
            return std::ptr::null_mut();
        }
        // SAFETY: caller supplies a valid allocation layout.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if reject(size) {
            return std::ptr::null_mut();
        }
        // SAFETY: the original pointer/layout and new size come from GlobalAlloc's caller.
        unsafe { System.realloc(ptr, layout, size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: caller returns a live allocation with its original layout.
        unsafe { System.dealloc(ptr, layout) }
    }
}
#[global_allocator]
static ALLOCATOR: Checked = Checked;
struct Device;
impl NetDevice for Device {
    fn info(&self) -> NetInfo {
        NetInfo { mac: [2, 0, 0, 0, 0, 1], mtu: 1500, link_up: true }
    }
    fn tx_ready(&self) -> bool {
        false
    }
    fn receive(&mut self, _: &mut [u8; ETH_FRAME_MAX]) -> Option<usize> {
        None
    }
    fn transmit<R>(&mut self, _: usize, _: impl FnOnce(&mut [u8]) -> R) -> Result<R, TxError> {
        Err(TxError::Full)
    }
    fn reclaim(&mut self) -> usize {
        0
    }
}
fn fixed() -> Result<Stack<Device>, NetError> {
    Stack::try_new(Loopback::try_new(65535, 16)?, Instant::ZERO, 474)
}
#[test]
fn fixed_storage_allocation_failures_return_nomem() {
    MEASURE.with(|m| m.set(Some((0, 0))));
    let stack = fixed().unwrap();
    let (allocations, bytes) = MEASURE.with(|m| m.take().unwrap());
    drop(stack);
    assert!(bytes < 7 * 1024 * 1024, "fixed payload footprint: {bytes}");
    assert!(allocations > 10);
    for failed in 0..allocations {
        FAIL.with(|f| f.set(Some(failed)));
        let result = fixed();
        FAIL.with(|f| f.set(None));
        assert!(matches!(result, Err(NetError::NoMem)), "allocation {failed}");
    }
}
