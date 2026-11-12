"""Where each scenario keeps its artifacts."""

from pathlib import Path

from lumen_harness import artifact_dir

ROOT = Path("/a")


def test_artifacts_are_keyed_by_module_and_test():
    # Same test name in two files: two directories (they may run at once).
    boot = artifact_dir(ROOT, "tests/scenarios/test_boot.py::test_x")
    mm = artifact_dir(ROOT, "tests/scenarios/test_mm.py::test_x")
    assert (boot, mm) == (ROOT / "test_boot/test_x", ROOT / "test_mm/test_x")
    # Parameters stay distinct, made safe for a file name.
    assert artifact_dir(ROOT, "tests/scenarios/test_acpi.py::test_cpu[2,maxcpus=4-2 CPUs]") == (
        ROOT / "test_acpi/test_cpu_2_maxcpus_4-2_CPUs_"
    )
