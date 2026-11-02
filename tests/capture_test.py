import importlib.util
from contextlib import redirect_stdout
import io
from pathlib import Path
import tempfile
import unittest


spec = importlib.util.spec_from_file_location(
    "capture_p2p", Path(__file__).resolve().parents[1] / "tools/capture_p2p.py")
capture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(capture)


class CaptureTests(unittest.TestCase):
    def test_only_expected_p2p_group_names(self):
        self.assertTrue(capture.p2p_name("p2p-wlp0s20-12", "p2p-wlp0s20-"))
        for name in ("wlp0s20f3", "any", "p2p-dev-wlp0s20f3", "p2p-wlp0s20-12;id"):
            self.assertFalse(capture.p2p_name(name, "p2p-wlp0s20-"))

    def test_privilege_and_capture_are_bounded_without_payload_dump(self):
        command = capture.capture_command("p2p-wlp0s20-12", "astro67", "/usr/bin/tcpdump")
        self.assertIn("65s", command)
        self.assertIn("200", command)
        self.assertIn("-p", command)
        self.assertIn("-Z", command)
        for forbidden in ("any", "-I", "-w", "-A", "-X", "-XX"):
            self.assertNotIn(forbidden, command)
        self.assertEqual(command[-1], capture.FILTER)

    def test_lan_interface_rejected(self):
        for name in ("any", "wlp0s20f3", "lo", "enp0s31f6"):
            with self.assertRaises(ValueError):
                capture.capture_command(name, "astro67", "/usr/bin/tcpdump")

    def test_early_failures_are_recorded_privately(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            path = Path(directory) / "preparacion.txt"
            with redirect_stdout(io.StringIO()):
                with self.assertRaisesRegex(RuntimeError, "fallo simulado"):
                    with capture.diagnostic_log(path) as report:
                        report("Preparando")
                        raise RuntimeError("fallo simulado")
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            self.assertIn("RuntimeError: fallo simulado", path.read_text())

    def test_old_down_interfaces_do_not_block_capture_preparation(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            root = Path(directory)
            for name in ("p2p-wlp0s20-4", "p2p-wlp0s20-6"):
                (root / name).mkdir()
                (root / name / "flags").write_text("0x1002\n")
            self.assertEqual(capture.candidates("p2p-wlp0s20-", True, root), [])
            active = root / "p2p-wlp0s20-18"
            active.mkdir()
            (active / "flags").write_text("0x1003\n")
            self.assertEqual(capture.candidates("p2p-wlp0s20-", True, root), [active.name])


if __name__ == "__main__":
    unittest.main()
