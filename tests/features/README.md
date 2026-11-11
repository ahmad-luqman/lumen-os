# Feature manifest

`docs/STATUS.md` is generated from these files plus the latest test results.
Nothing counts as working unless a test listed here checks it (hard rule 1).

One file per area (`boot.toml`, `crash.toml`, `mm.toml`, ...), so parallel PRs
touch different files. Add the feature in the same PR that adds its tests:

```toml
[[feature]]
name = "Kernel heap with working free"
milestone = "M1"
tests = ["ktest:lumen_kernel::mm::heap::*", "cargo:lumen-heap"]
```

Test IDs:

| Prefix | Source |
|---|---|
| `scenario:<file>::<test>` | `tests/scenarios` (QEMU) |
| `harness:<file>::<test>` | `tests/harness` (host-side harness self-tests) |
| `ktest:<path>` | in-kernel test, by TAP name |
| `audit:flags` | `make audit` |
| `cargo:<crate>` | `cargo test` of a crate in `crates/` |

`*` is the only wildcard; brackets are literal (`test_x[pf]`).

CI (`make status-check`) fails if any listed feature has a failing test, or if
a pattern matches no test that ran (a typo or a deleted test).
