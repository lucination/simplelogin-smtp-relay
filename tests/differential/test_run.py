#!/usr/bin/env python3
"""Focused regression tests for the differential harness fixtures."""
import importlib.util
import unittest
from pathlib import Path


RUN = Path(__file__).with_name("run.py")
spec = importlib.util.spec_from_file_location("differential_run", RUN)
if spec is None or spec.loader is None:
    raise RuntimeError(f"could not load {RUN}")
run = importlib.util.module_from_spec(spec)
spec.loader.exec_module(run)


class BrokenWriter:
    def write(self, _data):
        raise ConnectionResetError("client disconnected")

    def flush(self):
        raise AssertionError("flush must not run after a failed write")


class DifferentialFixtureTests(unittest.TestCase):
    def test_normal_parity_compares_recipients_and_headers_not_intentional_sender_difference(self):
        headers = {"to": "rev-a@simplelogin.test", "cc": None, "bcc": None, "subject": "one"}
        python = ((250, {}), [("alias@example.com", ["rev-a@simplelogin.test"], headers)])
        rust = ((250, {}), [("up", ["rev-a@simplelogin.test"], headers)])
        run.assert_normal_parity("example", python, rust, "alias@example.com", "up")

        changed_headers = dict(headers, subject="different")
        changed_rust = ((250, {}), [("up", ["rev-a@simplelogin.test"], changed_headers)])
        with self.assertRaises(AssertionError):
            run.assert_normal_parity("example", python, changed_rust, "alias@example.com", "up")

    def test_slow_upstream_fixture_exceeds_data_timeout_but_not_socket_timeout(self):
        scenario = run.slow_upstream_scenario()
        self.assertGreater(scenario["delay"], scenario["timeout"])
        self.assertGreater(scenario["upstream_timeout"], scenario["delay"])

    def test_mock_suppresses_client_disconnect_while_sending_response(self):
        handler = object.__new__(run.SMTPHandler)
        handler.wfile = BrokenWriter()
        self.assertFalse(handler.put("250 OK"))


if __name__ == "__main__":
    unittest.main()
