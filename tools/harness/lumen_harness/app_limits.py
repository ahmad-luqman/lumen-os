"""The app runtime's slot limits as the scenarios see them (#278): copies of
`lumen_app_api::limits`'s `MAX_APPS`, `KERNEL_RESERVE` and `APP_WORST`
(tests/harness/test_app_limits.py checks them against limits.rs), and
`slots`, the count `run` works out from what the frame allocator owns."""

KIB = 1024
MIB = 1024 * KIB

MAX_APPS = 4
KERNEL_RESERVE = 32 * MIB
# 16 MiB of memory, 1 of value stack, 8 of code, 2 of overhead and three
# 640 x 480 surfaces (limits.rs's test pins it so).
APP_WORST = 27 * MIB + 3 * 640 * 480 * 4
# What `MAX_APPS` slots need the frame allocator to own.
SLOTS_NEED = KERNEL_RESERVE + MAX_APPS * APP_WORST


def slots(owned: int) -> int:
    """`lumen_app_api::limits::slots`: how many apps may run at once on a
    machine whose frame allocator owns `owned` bytes."""
    return min(max(owned - KERNEL_RESERVE, 0) // APP_WORST, MAX_APPS)
