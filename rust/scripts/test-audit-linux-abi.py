"""Offline parser contracts, not real target OS execution."""
import importlib.util
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location("abi", Path(__file__).with_name("audit-linux-abi.py"))
abi = importlib.util.module_from_spec(spec)
spec.loader.exec_module(abi)
HEADER = "Class: ELF64\nData: 2's complement, little endian\nType: DYN (PIE)\nMachine: Advanced Micro Devices X86-64\n"
VERSIONS = "Version needs section '.gnu.version_r' contains 1 entry:\n Name: GLIBC_2.34 Flags: none Version: 2\n Name: GLIBC_2.39 Flags: WEAK Version: 3\n"
DYNAMIC = " 0x0000000000000001 (NEEDED) Shared library: [libc.so.6]\n"
PROGRAM = " [Requesting program interpreter: /lib64/ld-linux-x86-64.so.2]\n"


class AbiContracts(unittest.TestCase):
    def test_ceiling_checks_strong_not_weak(self):
        result = abi.parse(HEADER, VERSIONS, DYNAMIC, PROGRAM, "2.34.0")
        self.assertEqual(result["ceiling_status"], "PASS")
        self.assertEqual(result["weak_glibc"], ["2.39"])
        self.assertEqual(abi.parse(HEADER, VERSIONS, DYNAMIC, PROGRAM, "2.28")["ceiling_status"], "FAIL")
        self.assertEqual(abi.parse(HEADER, VERSIONS, DYNAMIC, PROGRAM)["ceiling_status"], "NOT_REQUESTED")

    def test_reject_private_and_unknown_versions(self):
        for tag in ("GLIBC_PRIVATE", "GLIBC_ABI_DT_RELR"):
            with self.assertRaises(ValueError):
                abi.parse(HEADER, VERSIONS + f" Name: {tag} Flags: none Version: 5\n", DYNAMIC, PROGRAM)

    def test_reject_wrong_arch_and_missing_dynamic_requirements(self):
        with self.assertRaises(ValueError):
            abi.parse(HEADER.replace("Advanced Micro Devices X86-64", "AArch64"), VERSIONS, DYNAMIC, PROGRAM)
        with self.assertRaises(ValueError):
            abi.parse(HEADER, "No version information found", DYNAMIC, PROGRAM)

    def test_arm64_checks_machine_and_interpreter(self):
        header = HEADER.replace("Advanced Micro Devices X86-64", "AArch64")
        program = PROGRAM.replace("/lib64/ld-linux-x86-64.so.2", "/lib/ld-linux-aarch64.so.1")
        result = abi.parse(header, VERSIONS, DYNAMIC, program, "2.34", "arm64")
        self.assertEqual(result["architecture"], "arm64")
        self.assertEqual(result["ceiling_status"], "PASS")
        with self.assertRaises(ValueError):
            abi.parse(header, VERSIONS, DYNAMIC, PROGRAM, "2.34", "arm64")

    def test_ignore_exported_versions(self):
        export = "Version definition section:\n Name: GLIBC_9.99 Flags: none Version: 1\n"
        self.assertEqual(abi.parse(HEADER, export + VERSIONS, DYNAMIC, PROGRAM)["max_strong_glibc"], "2.34")


if __name__ == "__main__":
    unittest.main(verbosity=2)
