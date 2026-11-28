"""Safety and protocol contracts for the disposable VM harness."""
import importlib.util
import io
from pathlib import Path
import stat
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("fabric_vm_durability", Path(__file__).resolve().parents[1] / "fabric_vm_durability.py")
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)


class VmContract(unittest.TestCase):
    def test_guest_archive_is_aligned_and_contains_console(self):
        out = io.BytesIO()
        mod.newc_entry(out, "dev/console", stat.S_IFCHR | 0o600, b"", 5, 1)
        mod.newc_entry(out, "init", stat.S_IFREG | 0o755, b"abc")
        mod.newc_entry(out, "TRAILER!!!", 0, b"")
        data = out.getvalue()
        self.assertEqual(len(data) % 4, 0)
        self.assertEqual(data[:6], b"070701")
        self.assertEqual(int(data[78:86], 16), 5)
        self.assertEqual(int(data[86:94], 16), 1)
        self.assertIn(b"dev/console\0", data)
        self.assertIn(b"init\0", data)

    def test_vm_only_attaches_private_regular_disk(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            disk = root / "test.raw"
            disk.touch()
            args = mod.vm_command("qemu", Path("/boot/kernel"), root / "init.gz", disk, "write", "cross", "abc", "kvm")
            self.assertIn("-nic", args)
            self.assertEqual(args[args.index("-nic") + 1], "none")
            self.assertIn("cache=none", args[args.index("-drive") + 1])
            self.assertNotIn("-virtfs", args)
            for bad in (root, root / "missing", Path("/dev/null")):
                with self.assertRaises(ValueError):
                    mod.vm_command("qemu", Path("/boot/kernel"), root / "init.gz", bad, "write", "cross", "abc", "kvm")
            with self.assertRaises(ValueError):
                mod.vm_command("qemu", Path("/boot/kernel"), root / "init.gz", disk, "bad", "cross", "abc", "kvm")

    def test_exact_marker_not_kernel_command_line_or_partial_line(self):
        marker = "FABRIC_VM_PASS abc write cross"
        self.assertFalse(mod.marker_seen("kernel: " + marker + "\n", marker))
        self.assertFalse(mod.marker_seen(marker, marker))
        self.assertTrue(mod.marker_seen("boot\n" + marker + "\r\n", marker))


    def test_ext4_recovery_flag_requires_valid_superblock(self):
        with tempfile.TemporaryDirectory() as tmp:
            disk = Path(tmp) / "disk"
            disk.write_bytes(b"\0" * 2048)
            with self.assertRaisesRegex(RuntimeError, "invalid ext4"):
                mod.ext4_needs_recovery(disk)
            data = bytearray(2048)
            data[1080:1082] = b"\x53\xef"
            disk.write_bytes(data)
            self.assertFalse(mod.ext4_needs_recovery(disk))
            data[1120] = 4
            disk.write_bytes(data)
            self.assertTrue(mod.ext4_needs_recovery(disk))

    def test_report_rewrite_does_not_leave_stale_bytes(self):
        report = io.StringIO("old content" * 100)
        mod.write_report(report, {"cases": []})
        self.assertEqual(report.getvalue(), '{\n  "cases": []\n}\n')

    def test_link_aliases_and_nonprivate_directories_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            disk = root / "disk"
            disk.touch()
            alias = root / "alias"
            alias.symlink_to(disk)
            def command(path):
                return mod.vm_command("qemu", root / "kernel", root / "init", path, "write", "fault", "abc", "kvm")
            with self.assertRaises(ValueError):
                command(alias)
            alias.unlink()
            alias.hardlink_to(disk)
            with self.assertRaises(ValueError):
                command(disk)
            alias.unlink()
            root.chmod(0o755)
            with self.assertRaises(ValueError):
                command(disk)


if __name__ == "__main__":
    unittest.main()
