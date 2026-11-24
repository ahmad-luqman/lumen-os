use linux_raw_sys::general as g;
use lumen_signal::altstack::*;

#[test]
fn sigaltstack_configuration_and_errno_rules() {
    let disabled = AltStack::DISABLED;
    assert!(!disabled.enabled());
    assert_eq!(disabled.status(0), g::SS_DISABLE);
    assert_eq!(disabled.configure(0, 0x10000, MINSIGSTKSZ - 1, 0), Err(StackError::TooSmall));
    assert_eq!(StackError::TooSmall.errno(), -12);
    let stack = disabled.configure(0, 0x10000, MINSIGSTKSZ, 0).unwrap();
    assert_eq!(stack.sp(), 0x10000);
    assert_eq!(stack.top(), 0x10800);
    assert_eq!(stack.size(), 2048);
    for flags in [4, g::SS_ONSTACK | g::SS_DISABLE, g::SS_AUTODISARM | 4] {
        assert_eq!(disabled.configure(0, 0x10000, 8192, flags), Err(StackError::Invalid));
    }
    for flags in [g::SS_DISABLE, g::SS_DISABLE | g::SS_AUTODISARM] {
        let result = stack.configure(0x30000, u64::MAX, u64::MAX, flags).unwrap();
        assert_eq!(result.flags(), flags);
        assert_eq!((result.sp(), result.size()), (0, 0));
        assert_eq!(result.status(0x30000), flags);
        assert_eq!(stack.configure(0x10001, 0, 0, flags), Err(StackError::OnStack));
    }
    assert_eq!(StackError::OnStack.errno(), -1);
    assert_eq!(StackError::Invalid.errno(), -22);
    assert_eq!(disabled.configure(0, u64::MAX - 10, 2048, 0), Err(StackError::Invalid));
    assert_eq!(disabled.configure(0, lumen_signal::frame::USER_MAX, 2048, 0), Err(StackError::Invalid));
    let reported = disabled.configure(0, 0x10000, 2048, g::SS_ONSTACK).unwrap();
    assert_eq!(reported.flags(), g::SS_ONSTACK);
    assert_eq!(reported.status(0), 0);
    assert_eq!(reported.status(0x10001), g::SS_ONSTACK);
    // EPERM wins even when the requested flags and size are invalid.
    assert_eq!(stack.configure(0x10001, 0, 0, 4), Err(StackError::OnStack));
}

#[test]
fn sigaltstack_membership_boundaries() {
    let stack = AltStack::DISABLED.configure(0, 0x10000, 4096, 0).unwrap();
    for (rsp, inside) in [
        (0, false),
        (0xffff, false),
        (0x10000, false),
        (0x10001, true),
        (0x10fff, true),
        (0x11000, true),
        (0x11001, false),
        (u64::MAX, false),
    ] {
        assert_eq!(stack.contains(rsp), inside);
        assert_eq!(stack.on_stack(rsp), inside);
        assert_eq!(stack.status(rsp), if inside { g::SS_ONSTACK } else { 0 });
        assert_eq!(stack.configure(rsp, 0x20000, 2048, 0).is_err(), inside);
    }
}

#[test]
fn sigaltstack_autodisarm_allows_handler_stack_switch() {
    let stack = AltStack::DISABLED.configure(0, 0x10000, 4096, g::SS_AUTODISARM).unwrap();
    assert!(stack.contains(0x10800));
    assert!(!stack.on_stack(0x10800));
    assert_eq!(stack.status(0x10800), g::SS_AUTODISARM);
    let during = stack.during_handler();
    assert_eq!(during, AltStack::DISABLED);
    assert_eq!(during.status(0x10800), g::SS_DISABLE);
    let replacement = during.configure(0x10800, 0x20000, 8192, 0).unwrap();
    assert_eq!(replacement.top(), 0x22000);
    assert_eq!(replacement.during_handler(), replacement);
    // Linux treats an armed AUTODISARM stack as not on-stack even before reset.
    assert!(stack.configure(0x10800, 0, 0, g::SS_DISABLE).is_ok());
}
