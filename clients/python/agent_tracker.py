"""Agent Tracker client for Python. Standard library only.

    t = Tracker()
    with t.trace("Plan a trip", input=prompt):
        with t.agent("planner", input=prompt):
            with t.llm("plan", model="claude-sonnet-5-5") as s:
                s.output(call_model()).tokens(412, 58)
            with t.tool("search", input={"q": "kyoto"}) as s:
                s.output(search("kyoto"))
    t.flush()

Spans nest through contextvars, so tasks started with asyncio.gather inside a
fork or split each become their own child span. Updates are sent from a
background thread and never block or raise into your agent.
"""

from __future__ import annotations

import atexit
import contextvars
import json
import os
import queue
import secrets
import sys
import threading
import time
import urllib.request
import uuid
from contextlib import contextmanager
from typing import Any, Iterator

_current: contextvars.ContextVar[Span | None] = contextvars.ContextVar("agent_tracker_span", default=None)


def _now() -> float:
    return time.time() * 1000


class Span:
    def __init__(self, tracker: Tracker, trace_id: str, span_id: str) -> None:
        self.trace_id = trace_id
        self.id = span_id
        self._tracker = tracker
        self._fields: dict[str, Any] = {}
        self._ended = False

    def output(self, value: Any) -> Span:
        self._fields["output"] = value
        return self

    def tokens(self, input: int, output: int) -> Span:
        self._fields["tokens_in"] = input
        self._fields["tokens_out"] = output
        return self

    def model(self, name: str) -> Span:
        self._fields["model"] = name
        return self

    def attr(self, key: str, value: Any) -> Span:
        self._fields.setdefault("attrs", {})[key] = value
        return self

    def link(self, *spans: Span | str) -> Span:
        self._fields.setdefault("links", []).extend(s if isinstance(s, str) else s.id for s in spans)
        return self

    def _end(self, error: BaseException | None) -> None:
        if self._ended:
            return
        self._ended = True
        update ={**self._fields, "trace_id": self.trace_id, "span_id": self.id, "end": _now()}
        update["status"] = "error" if error else "ok"
        if error:
            update["error"] = str(error) or type(error).__name__
        self._tracker._send(update)


class Tracker:
    def __init__(self, url: str | None = None, source: str = "sdk:python") -> None:
        self.url = (url or os.environ.get("AGENT_TRACKER_URL") or "http://127.0.0.1:4318").rstrip("/")
        self.source = source
        self._queue: queue.Queue[dict[str, Any]] = queue.Queue()
        self._warned = False
        threading.Thread(target=self._drain, name="agent-tracker", daemon=True).start()
        atexit.register(self.flush)

    @contextmanager
    def span(
        self,
        kind: str,
        name: str,
        *,
        agent: str | None = None,
        input: Any = None,
        model: str | None = None,
        attrs: dict[str, Any] | None = None,
        links: list[Span | str] | None = None,
    ) -> Iterator[Span]:
        parent = _current.get()
        span = Span(self, parent.trace_id if parent else str(uuid.uuid4()), secrets.token_hex(8))
        update: dict[str, Any] = {
            "trace_id": span.trace_id,
            "span_id": span.id,
            "parent_id": parent.id if parent else None,
            "kind": kind,
            "name": name,
            "agent": agent or (name if kind == "agent" else None),
            "start": _now(),
            "input": input,
            "model": model,
            "attrs": attrs if parent else {"source": self.source, **(attrs or {})},
            "links": [l if isinstance(l, str) else l.id for l in links or []],
        }
        self._send({k: v for k, v in update.items() if v not in (None, [], {})})
        token = _current.set(span)
        try:
            yield span
        except BaseException as e:
            span._end(e)
            raise
        else:
            span._end(None)
        finally:
            _current.reset(token)

    @contextmanager
    def trace(self, name: str, input: Any = None) -> Iterator[Span]:
        """A new trace rooted at the message you sent."""
        token = _current.set(None)
        try:
            with self.span("message", name, input=input) as msg:
                msg._end(None)
                yield msg
        finally:
            _current.reset(token)

    def agent(self, name: str, **kw: Any):
        return self.span("agent", name, **kw)

    def llm(self, name: str, **kw: Any):
        return self.span("llm", name, **kw)

    def tool(self, name: str, **kw: Any):
        return self.span("tool", name, **kw)

    def split(self, name: str, **kw: Any):
        """Children run as parallel branches, each handling one part of the input."""
        return self.span("split", name, **kw)

    def fork(self, name: str, **kw: Any):
        """Children run as parallel branches over the same input."""
        return self.span("fork", name, **kw)

    def join(self, name: str, **kw: Any):
        return self.span("join", name, **kw)

    def flush(self) -> None:
        """Blocks until every update sent so far has been delivered (or dropped)."""
        self._queue.join()

    def _send(self, update: dict[str, Any]) -> None:
        self._queue.put(update)

    def _drain(self) -> None:
        while True:
            batch = [self._queue.get()]
            while len(batch) < 200:
                try:
                    batch.append(self._queue.get_nowait())
                except queue.Empty:
                    break
            try:
                req = urllib.request.Request(
                    f"{self.url}/api/spans",
                    data=json.dumps(batch, default=str).encode(),
                    headers={"content-type": "application/json"},
                )
                urllib.request.urlopen(req, timeout=5).close()
            except Exception as e:
                if not self._warned:
                    print(f"[agent-tracker] cannot reach {self.url}: {e}", file=sys.stderr)
                    self._warned = True
            finally:
                for _ in batch:
                    self._queue.task_done()
