"""Tests for JSON-RPC helpers."""

from __future__ import annotations

import io

import pytest

from aoe_kanban_lite.rpc import EOFReached, read_message


def _stream(lines: list[str]) -> io.TextIOWrapper:
    return io.TextIOWrapper(io.BytesIO("".join(lines).encode("utf-8")), encoding="utf-8")


def test_read_message_skips_blank_lines():
    stream = _stream(["\n", "\n", '{"jsonrpc":"2.0"}\n'])
    msg = read_message(stream)
    assert msg == {"jsonrpc": "2.0"}


def test_read_message_skips_malformed_json():
    stream = _stream(["not json\n", '{"jsonrpc":"2.0"}\n'])
    msg = read_message(stream)
    assert msg == {"jsonrpc": "2.0"}


def test_read_message_skips_non_object_json():
    stream = _stream(["42\n", '{"jsonrpc":"2.0"}\n'])
    msg = read_message(stream)
    assert msg == {"jsonrpc": "2.0"}


def test_read_message_returns_none_on_eof():
    stream = _stream([])
    assert read_message(stream) is None


def test_read_message_raises_eof_on_midline():
    stream = _stream(["partial"])
    with pytest.raises(EOFReached):
        read_message(stream)
