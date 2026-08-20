"""Kanban Lite worker main loop."""

from __future__ import annotations

import queue
import sys
import threading
from typing import Any

from .board import group_by_repo, group_by_status, parse_excluded_statuses
from .rpc import EOFReached, ProtocolTimeout, is_notification, is_response, read_message, write_message
from .session_model import session_from_json
from .uistate import (
    build_filter_facet_payload,
    build_row_column_payload,
    build_settings_page_payload,
    build_sort_key_payload,
    ui_state_remove,
    ui_state_set,
)

DEFAULT_SETTINGS: dict[str, Any] = {
    "default_grouping": "status",
    "refresh_secs": 30,
    "show_row_column": True,
    "excluded_statuses": "stopped,unknown",
}

SETTING_KEYS = list(DEFAULT_SETTINGS.keys())

# How long the worker waits for a single JSON-RPC response before giving up.
PROTOCOL_TIMEOUT_SECS = 10.0


class KanbanWorker:
    """AOE plugin worker that pushes a Kanban board view of active sessions."""

    def __init__(self, in_stream: Any = sys.stdin, out_stream: Any = sys.stdout) -> None:
        self.in_stream = in_stream
        self.out_stream = out_stream
        self.inbound_queue: queue.Queue[dict[str, Any] | None] = queue.Queue()
        self.pending_notifications: list[dict[str, Any]] = []
        self.next_id = 1
        self.settings = dict(DEFAULT_SETTINGS)
        self.pushed_session_ids: set[str] = set()
        self.refresh_due = True
        self.settings_need_refresh = False
        self.running = True
        self.reader_thread: threading.Thread | None = None

    def start(self) -> None:
        print("[kanban-lite] worker starting", file=sys.stderr, flush=True)
        self.reader_thread = threading.Thread(target=self._stdin_reader, daemon=True)
        self.reader_thread.start()
        try:
            self._bootstrap_settings()
            self._run_loop()
        except EOFReached:
            print("[kanban-lite] EOF reached; exiting", file=sys.stderr, flush=True)
        except Exception as exc:
            print(f"[kanban-lite] fatal error: {exc}", file=sys.stderr, flush=True)
            raise
        finally:
            self.running = False
            print("[kanban-lite] worker exiting", file=sys.stderr, flush=True)

    def _stdin_reader(self) -> None:
        """Read JSON-RPC messages from stdin and enqueue them."""
        try:
            while True:
                msg = read_message(self.in_stream)
                if msg is None:
                    print("[kanban-lite] stdin EOF", file=sys.stderr, flush=True)
                    break
                self.inbound_queue.put(msg)
        except Exception as exc:
            print(f"[kanban-lite] stdin reader error: {exc}", file=sys.stderr, flush=True)
        finally:
            self.inbound_queue.put(None)

    def _bootstrap_settings(self) -> None:
        """Fetch initial settings at worker startup."""
        for key in SETTING_KEYS:
            try:
                result = self._request("config.get", {"key": key})
                self.settings[key] = result.get("value") if result.get("value") is not None else DEFAULT_SETTINGS[key]
            except Exception as exc:
                print(f"[kanban-lite] failed to read setting {key}: {exc}", file=sys.stderr)

    def _request(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
        """Send a JSON-RPC request and block until the matching response.

        Notifications that arrive while waiting are queued for the main loop;
        they are not handled inline to avoid re-entrancy.
        """
        req_id = self.next_id
        self.next_id += 1
        write_message(
            self.out_stream,
            {"jsonrpc": "2.0", "id": req_id, "method": method, "params": params},
        )
        while True:
            try:
                msg = self.inbound_queue.get(timeout=PROTOCOL_TIMEOUT_SECS)
            except queue.Empty as exc:
                raise ProtocolTimeout(
                    f"no response to {method} after {PROTOCOL_TIMEOUT_SECS}s"
                ) from exc
            if msg is None:
                raise EOFReached()
            if is_response(msg):
                if msg.get("id") == req_id:
                    if "error" in msg:
                        raise RuntimeError(f"RPC error: {msg['error']}")
                    return msg.get("result", {})
                print(f"[kanban-lite] unexpected response: {msg}", file=sys.stderr)
                continue
            if is_notification(msg):
                self.pending_notifications.append(msg)
                continue
            print(f"[kanban-lite] unhandled message: {msg}", file=sys.stderr)

    def _handle_notification(self, msg: dict[str, Any]) -> None:
        method = msg.get("method", "")
        if method == "plugin.settings.changed":
            changed_keys = msg.get("params", {}).get("changed_keys", [])
            if not changed_keys or any(k in SETTING_KEYS for k in changed_keys):
                self.settings_need_refresh = True
                self.refresh_due = True

    def _rebuild_settings(self) -> None:
        """Re-read all settings from the host."""
        self.settings_need_refresh = False
        for key in SETTING_KEYS:
            try:
                result = self._request("config.get", {"key": key})
                self.settings[key] = result.get("value") if result.get("value") is not None else DEFAULT_SETTINGS[key]
            except Exception as exc:
                print(f"[kanban-lite] failed to re-read setting {key}: {exc}", file=sys.stderr)

    def _drain_notifications(self) -> None:
        """Process any notifications collected during request waits."""
        while self.pending_notifications:
            msg = self.pending_notifications.pop(0)
            self._handle_notification(msg)

    def _run_loop(self) -> None:
        """Main loop: refresh on schedule or when woken by a notification."""
        while self.running:
            self._drain_notifications()

            if self.settings_need_refresh:
                self._rebuild_settings()

            if self.refresh_due:
                self.refresh_due = False
                try:
                    self._refresh()
                except Exception as exc:
                    print(f"[kanban-lite] refresh error: {exc}", file=sys.stderr)

            refresh_secs = int(self.settings.get("refresh_secs", DEFAULT_SETTINGS["refresh_secs"]))
            if refresh_secs <= 0:
                refresh_secs = 30

            try:
                msg = self.inbound_queue.get(timeout=refresh_secs)
            except queue.Empty:
                self.refresh_due = True
                continue

            if msg is None:
                break

            if is_notification(msg):
                self._handle_notification(msg)
            elif is_response(msg):
                print(f"[kanban-lite] spurious response: {msg}", file=sys.stderr)

    def _refresh(self) -> None:
        """Fetch sessions and push all UI state."""
        result = self._request(
            "sessions.list",
            {"exclude": ["archived", "snoozed", "trashed"]},
        )
        raw_sessions = result.get("sessions", [])
        sessions = [session_from_json(s) for s in raw_sessions]

        grouping = str(self.settings.get("default_grouping", DEFAULT_SETTINGS["default_grouping"]))
        excluded = parse_excluded_statuses(
            str(self.settings.get("excluded_statuses", DEFAULT_SETTINGS["excluded_statuses"]))
        )

        if grouping == "repo":
            columns = group_by_repo(sessions)
        else:
            columns = group_by_status(sessions, excluded)

        # Push global UI state.
        settings_payload = build_settings_page_payload(grouping, columns)
        write_message(
            self.out_stream,
            ui_state_set("settings-page", "kanban_board", settings_payload),
        )
        write_message(
            self.out_stream,
            ui_state_set("sort-key", "kanban_status_sort", build_sort_key_payload()),
        )
        write_message(
            self.out_stream,
            ui_state_set("filter-facet", "kanban_status_filter", build_filter_facet_payload()),
        )

        # Push or remove per-session row-column entries.
        show_row_column = bool(self.settings.get("show_row_column", DEFAULT_SETTINGS["show_row_column"]))
        visible_session_ids: set[str] = set()
        for session in sessions:
            visible_session_ids.add(session.id)
            if show_row_column:
                payload = build_row_column_payload(session)
                write_message(
                    self.out_stream,
                    ui_state_set("row-column", "kanban_status", payload, session_id=session.id),
                )
            else:
                write_message(
                    self.out_stream,
                    ui_state_remove("row-column", "kanban_status", session_id=session.id),
                )

        # Remove row-column entries for sessions that disappeared.
        for old_id in self.pushed_session_ids - visible_session_ids:
            write_message(
                self.out_stream,
                ui_state_remove("row-column", "kanban_status", session_id=old_id),
            )

        self.pushed_session_ids = visible_session_ids


def main() -> None:
    worker = KanbanWorker()
    try:
        worker.start()
    except KeyboardInterrupt:
        pass
    except EOFReached:
        pass
    finally:
        worker.running = False


if __name__ == "__main__":
    main()
