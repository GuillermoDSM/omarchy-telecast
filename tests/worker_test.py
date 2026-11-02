"""Read-only process/IPC checks. Never scans, captures or activates Wi-Fi P2P."""

import json
import os
from pathlib import Path
import selectors
import signal
import subprocess
import time
import unittest

ENGINE = Path(os.environ.get("MIRACAST_ENGINE", Path(__file__).resolve().parents[1] / "engine/target/debug/omarchy-miracast"))


class WorkerTests(unittest.TestCase):
    def setUp(self):
        self.process = subprocess.Popen([str(ENGINE), "serve"], stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.process.stdout, selectors.EVENT_READ)
        self.buffer = b""
        self.wait_event(lambda event: event.get("event") == "ready")

    def tearDown(self):
        if self.process.poll() is None:
            self.process.terminate()
            self.process.wait(timeout=20)
        self.selector.close()
        for stream in (self.process.stdin, self.process.stdout, self.process.stderr):
            stream.close()

    def send(self, value):
        self.process.stdin.write(json.dumps(value).encode() + b"\n")
        self.process.stdin.flush()

    def wait_event(self, predicate):
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            while b"\n" in self.buffer:
                line, self.buffer = self.buffer.split(b"\n", 1)
                event = json.loads(line)
                if predicate(event):
                    return event
            if not self.selector.select(timeout=max(0, deadline - time.monotonic())):
                break
            data = os.read(self.process.stdout.fileno(), 65536)
            if not data:
                self.fail("Worker exited before the expected event")
            self.buffer += data
        self.fail("Timed out waiting for worker event")

    def test_diagnostics_invalid_connect_and_quit(self):
        self.send({"command": "doctor"})
        event = self.wait_event(lambda e: e.get("event") == "diagnostics")
        self.assertTrue(event["experimental"])
        self.assertFalse(event["firewall_changed"])
        self.send({"command": "connect", "mac": "not-a-mac", "test": True})
        event = self.wait_event(lambda e: e.get("phase") == "error")
        self.assertIn("inválida", event["detail"])
        self.assertFalse(event["busy"])
        self.send({"command": "quit"})
        self.assertEqual(self.process.wait(timeout=5), 0)

    def test_eof_and_sigterm(self):
        self.process.stdin.close()
        self.assertEqual(self.process.wait(timeout=5), 0)

    def test_sigterm(self):
        self.process.send_signal(signal.SIGTERM)
        self.assertEqual(self.process.wait(timeout=5), 0)


if __name__ == "__main__":
    unittest.main()
