"""Tests for the Python client against a stand-in daemon.

    python -m unittest clients/python/test_agent_tracker.py -v
"""

import atexit
import json
import os
import sys
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from agent_tracker import Tracker  # noqa: E402


def _reject_constant(name: str):
    raise ValueError(f"{name} is not JSON")


class FakeDaemon:
    """Records every POSTed batch. Parses strictly, like the real daemon: NaN/Infinity get a 400."""

    def __init__(self) -> None:
        self.batches: list[list[dict]] = []
        self.rejected: list[bytes] = []
        self.received = threading.Event()  # set when a POST arrives
        self.release = threading.Event()  # POSTs wait on this before answering
        self.release.set()
        daemon = self

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self) -> None:
                body = self.rfile.read(int(self.headers["content-length"]))
                daemon.received.set()
                daemon.release.wait(10)
                try:
                    batch = json.loads(body, parse_constant=_reject_constant)
                except ValueError:
                    daemon.rejected.append(body)
                    self.send_response(400)
                else:
                    daemon.batches.append(batch)
                    self.send_response(200)
                self.send_header("content-length", "0")
                self.end_headers()

            def log_message(self, *args) -> None:
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.server.server_port}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def updates(self, span_id: str) -> tuple[dict | None, dict | None]:
        """The (start, end) updates received for one span."""
        mine = [u for b in self.batches for u in b if u.get("span_id") == span_id]
        start = next((u for u in mine if "start" in u), None)
        end = next((u for u in mine if "end" in u), None)
        return start, end

    def close(self) -> None:
        self.release.set()
        self.server.shutdown()
        self.server.server_close()


class Ambiguous:
    """Behaves like a numpy array: comparing it or asking its truth value raises."""

    def __eq__(self, other):
        raise ValueError("The truth value of an array with more than one element is ambiguous")

    def __bool__(self):
        raise ValueError("The truth value of an array with more than one element is ambiguous")

    def __str__(self) -> str:
        return "[0.1 0.2 0.3]"


class TrackerTest(unittest.TestCase):
    def setUp(self) -> None:
        self.daemon = FakeDaemon()
        self.t = Tracker(url=self.daemon.url)
        atexit.unregister(self.t.flush)  # a stuck queue must not hang interpreter exit
        self.addCleanup(self.daemon.close)

    def flush(self) -> None:
        done = threading.Thread(target=self.t.flush, daemon=True)
        done.start()
        done.join(10)
        self.assertFalse(done.is_alive(), "flush() did not return")

    def test_input_whose_eq_raises_does_not_raise_into_the_agent(self) -> None:
        ran = False
        try:
            with self.t.tool("embed", input=Ambiguous()) as s:
                ran = True
        except ValueError as e:
            self.fail(f"span raised into the agent: {e!r}")
        self.assertTrue(ran, "span body did not run")
        self.flush()
        start, end = self.daemon.updates(s.id)
        self.assertIsNotNone(start, "start update missing")
        self.assertIsNotNone(end, "end update missing")
        self.assertEqual(start["input"], "[0.1 0.2 0.3]")
        self.assertEqual(end["status"], "ok")

    def test_nan_becomes_null_and_does_not_lose_the_batch(self) -> None:
        # Hold the drain thread on its first POST so everything after it is sent as one batch.
        self.daemon.release.clear()
        with self.t.trace("gate"):
            pass
        self.assertTrue(self.daemon.received.wait(5), "first POST never arrived")
        with self.t.tool("score") as bad:
            bad.output({"confidence": float("nan"), "upper": float("inf"), "n": 3})
        with self.t.tool("unrelated") as other:
            other.output("fine")
        self.daemon.release.set()
        self.flush()

        self.assertEqual(self.daemon.rejected, [], "daemon rejected a batch as invalid JSON")
        _, bad_end = self.daemon.updates(bad.id)
        _, other_end = self.daemon.updates(other.id)
        self.assertIsNotNone(bad_end, "span with nan output was lost")
        self.assertIsNotNone(other_end, "unrelated span in the same batch was lost")
        self.assertEqual(bad_end["output"], {"confidence": None, "upper": None, "n": 3})
        self.assertEqual(other_end["output"], "fine")
        batch = next(b for b in self.daemon.batches if other_end in b)
        self.assertIn(bad_end, batch, "test setup: spans were not sent in the same batch")

    def test_trace_agent_tool_nesting_links_parents(self) -> None:
        with self.t.trace("Plan a trip", input="kyoto") as root:
            with self.t.agent("planner") as agent:
                with self.t.tool("search", input={"q": "kyoto"}) as tool:
                    tool.output(["a", "b"])
        self.flush()

        (root_start, _), (agent_start, agent_end), (tool_start, tool_end) = (
            self.daemon.updates(s.id) for s in (root, agent, tool)
        )
        self.assertNotIn("parent_id", root_start)
        self.assertEqual(root_start["kind"], "message")
        self.assertEqual(root_start["attrs"], {"source": "sdk:python"})
        self.assertEqual(agent_start["parent_id"], root.id)
        self.assertEqual(agent_start["agent"], "planner")
        self.assertNotIn("input", agent_start)  # None is dropped
        self.assertNotIn("links", agent_start)  # [] is dropped
        self.assertEqual(tool_start["parent_id"], agent.id)
        self.assertEqual(tool_start["input"], {"q": "kyoto"})
        self.assertEqual(tool_end["output"], ["a", "b"])
        self.assertEqual({u["trace_id"] for u in (root_start, agent_start, tool_start)}, {root.trace_id})
        self.assertEqual((agent_end["status"], tool_end["status"]), ("ok", "ok"))


if __name__ == "__main__":
    unittest.main()
