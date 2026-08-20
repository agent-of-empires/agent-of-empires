"""JSON-RPC 2.0 helpers for stdio communication with the AOE plugin host."""

from __future__ import annotations

import json
import sys
from dataclasses import dataclass, field
from typing import Any, TextIO


class EOFReached(Exception):
    """Raised when stdin reaches EOF."""


def read_message(stream: TextIO) -> dict[str, Any] | None:
    """Read one newline-delimited JSON-RPC message.

    Returns None on EOF. Raises EOFReached if the stream closes mid-line after
    non-whitespace content.
    """
    line = stream.readline()
    if not line:
        return None
    line = line.strip()
    if not line:
        return None
    try:
        return json.loads(line)
    except json.JSONDecodeError as exc:
        # Log to stderr so it shows up in the worker log but does not crash us.
        print(f"[kanban-lite] malformed JSON-RPC line: {exc}", file=sys.stderr)
        return None


def write_message(stream: TextIO, msg: dict[str, Any]) -> None:
    """Write one JSON-RPC message with a trailing newline."""
    stream.write(json.dumps(msg, separators=(",", ":")) + "\n")
    stream.flush()


def is_request(msg: dict[str, Any]) -> bool:
    return "id" in msg and "method" in msg


def is_notification(msg: dict[str, Any]) -> bool:
    return "id" not in msg and "method" in msg


def is_response(msg: dict[str, Any]) -> bool:
    return "id" in msg and ("result" in msg or "error" in msg)


def make_request(method: str, params: dict[str, Any], req_id: int) -> dict[str, Any]:
    return {"jsonrpc": "2.0", "id": req_id, "method": method, "params": params}


def make_notification(method: str, params: dict[str, Any]) -> dict[str, Any]:
    return {"jsonrpc": "2.0", "method": method, "params": params}


def make_response(req_id: int, result: Any) -> dict[str, Any]:
    return {"jsonrpc": "2.0", "id": req_id, "result": result}


def make_error(req_id: int, code: int, message: str) -> dict[str, Any]:
    return {"jsonrpc": "2.0", "id": req_id, "error": {"code": code, "message": message}}


@dataclass
class JsonRpcClient:
    """Synchronous JSON-RPC client that reads/writes a single stream pair.

    The AOE plugin host speaks newline-delimited JSON-RPC over the worker's
    stdin/stdout. This client is intentionally simple: one request in flight at
    a time, blocking reads. The stdin reader thread enqueues incoming messages;
    the main loop uses this client to pull responses.
    """

    in_stream: TextIO
    out_stream: TextIO
    next_id: int = field(default=1)

    def request(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
        """Send a request and block until the matching response arrives."""
        req_id = self.next_id
        self.next_id += 1
        write_message(self.out_stream, make_request(method, params, req_id))
        while True:
            msg = read_message(self.in_stream)
            if msg is None:
                raise EOFReached()
            if is_response(msg) and msg.get("id") == req_id:
                if "error" in msg:
                    raise RuntimeError(f"RPC error: {msg['error']}")
                return msg.get("result", {})

    def notify(self, method: str, params: dict[str, Any]) -> None:
        write_message(self.out_stream, make_notification(method, params))
