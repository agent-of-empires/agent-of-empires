"""JSON-RPC 2.0 helpers for stdio communication with the AOE plugin host."""

from __future__ import annotations

import json
import sys
from typing import Any, TextIO


class EOFReached(Exception):
    """Raised when stdin reaches EOF or closes mid-line."""


class ProtocolTimeout(Exception):
    """Raised when the host does not respond to a JSON-RPC request in time."""


def read_message(stream: TextIO) -> dict[str, Any] | None:
    """Read one newline-delimited JSON-RPC message.

    Blank and malformed frames are skipped so a single bad line cannot shut
    down the worker. Returns ``None`` only when the stream reaches a clean EOF.
    Raises ``EOFReached`` if the stream closes mid-line after non-whitespace
    content. Rejects JSON values that are not objects.
    """
    while True:
        raw_line = stream.readline()
        if raw_line == "":
            return None

        # A line that is not newline-terminated and contains non-whitespace
        # data means the stream closed mid-frame.
        if not raw_line.endswith("\n") and raw_line.strip():
            raise EOFReached()

        line = raw_line.strip()
        if not line:
            continue

        try:
            message = json.loads(line)
        except json.JSONDecodeError as exc:
            print(f"[kanban-lite] malformed JSON-RPC line: {exc}", file=sys.stderr)
            continue

        if not isinstance(message, dict):
            print("[kanban-lite] JSON-RPC frame must be an object", file=sys.stderr)
            continue

        return message


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
