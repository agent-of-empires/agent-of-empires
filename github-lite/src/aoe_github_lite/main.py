"""GitHub Lite worker main loop."""

from __future__ import annotations

import queue
import sys
import threading
from typing import Any

from .github import list_open_prs, parse_overrides, parse_repo_slug
from .pane import build_pane_payload, ui_state_remove, ui_state_set
from .rpc import EOFReached, ProtocolTimeout, is_notification, is_response, read_message, write_message
from .session_model import Session, session_from_json

DEFAULT_SETTINGS: dict[str, Any] = {
    "default_owner": "",
    "repo_overrides": "",
    "github_token": "",
    "refresh_secs": 60,
}

SETTING_KEYS = list(DEFAULT_SETTINGS.keys())
PROTOCOL_TIMEOUT_SECS = 10.0


class GitHubLiteWorker:
    """AOE plugin worker that pushes a GitHub PR pane for each session."""

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
        # Cache PRs per slug to avoid hammering the API across refreshes.
        self._pr_cache: dict[str, list] = {}

    def start(self) -> None:
        print("[github-lite] worker starting", file=sys.stderr, flush=True)
        self.reader_thread = threading.Thread(target=self._stdin_reader, daemon=True)
        self.reader_thread.start()
        try:
            self._bootstrap_settings()
            self._run_loop()
        except EOFReached:
            print("[github-lite] EOF reached; exiting", file=sys.stderr, flush=True)
        except Exception as exc:
            print(f"[github-lite] fatal error: {exc}", file=sys.stderr, flush=True)
            raise
        finally:
            self.running = False
            print("[github-lite] worker exiting", file=sys.stderr, flush=True)

    def _stdin_reader(self) -> None:
        try:
            while True:
                msg = read_message(self.in_stream)
                if msg is None:
                    print("[github-lite] stdin EOF", file=sys.stderr, flush=True)
                    break
                self.inbound_queue.put(msg)
        except Exception as exc:
            print(f"[github-lite] stdin reader error: {exc}", file=sys.stderr, flush=True)
        finally:
            self.inbound_queue.put(None)

    def _bootstrap_settings(self) -> None:
        for key in SETTING_KEYS:
            try:
                result = self._request("config.get", {"key": key})
                value = result.get("value")
                self.settings[key] = value if value is not None else DEFAULT_SETTINGS[key]
            except Exception as exc:
                print(f"[github-lite] failed to read setting {key}: {exc}", file=sys.stderr)

    def _request(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
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
                print(f"[github-lite] unexpected response: {msg}", file=sys.stderr)
                continue
            if is_notification(msg):
                self.pending_notifications.append(msg)
                continue
            print(f"[github-lite] unhandled message: {msg}", file=sys.stderr)

    def _handle_notification(self, msg: dict[str, Any]) -> None:
        method = msg.get("method", "")
        if method == "plugin.settings.changed":
            changed_keys = msg.get("params", {}).get("changed_keys", [])
            if not changed_keys or any(k in SETTING_KEYS for k in changed_keys):
                self.settings_need_refresh = True
                self.refresh_due = True
                # Clear PR cache when settings change so slug/token updates apply.
                self._pr_cache.clear()

    def _rebuild_settings(self) -> None:
        self.settings_need_refresh = False
        for key in SETTING_KEYS:
            try:
                result = self._request("config.get", {"key": key})
                value = result.get("value")
                self.settings[key] = value if value is not None else DEFAULT_SETTINGS[key]
            except Exception as exc:
                print(f"[github-lite] failed to re-read setting {key}: {exc}", file=sys.stderr)

    def _drain_notifications(self) -> None:
        while self.pending_notifications:
            msg = self.pending_notifications.pop(0)
            self._handle_notification(msg)

    def _run_loop(self) -> None:
        while self.running:
            self._drain_notifications()

            if self.settings_need_refresh:
                self._rebuild_settings()

            if self.refresh_due:
                self.refresh_due = False
                try:
                    self._refresh()
                except Exception as exc:
                    print(f"[github-lite] refresh error: {exc}", file=sys.stderr)

            refresh_secs = int(self.settings.get("refresh_secs", DEFAULT_SETTINGS["refresh_secs"]))
            if refresh_secs <= 0:
                refresh_secs = 60

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
                print(f"[github-lite] spurious response: {msg}", file=sys.stderr)

    def _refresh(self) -> None:
        result = self._request(
            "sessions.list",
            {"exclude": ["archived", "snoozed", "trashed"]},
        )
        raw_sessions = result.get("sessions", [])
        sessions = [session_from_json(s) for s in raw_sessions]

        default_owner = str(self.settings.get("default_owner", DEFAULT_SETTINGS["default_owner"]))
        overrides = parse_overrides(
            str(self.settings.get("repo_overrides", DEFAULT_SETTINGS["repo_overrides"]))
        )
        token = str(self.settings.get("github_token", DEFAULT_SETTINGS["github_token"]))

        visible_session_ids: set[str] = set()
        for session in sessions:
            visible_session_ids.add(session.id)
            slug = parse_repo_slug(session.project_path, default_owner, overrides)
            prs = self._fetch_prs(slug, token)
            write_message(
                self.out_stream,
                ui_state_set(session.id, build_pane_payload(session, slug, prs)),
            )

        for old_id in self.pushed_session_ids - visible_session_ids:
            write_message(self.out_stream, ui_state_remove(old_id))

        self.pushed_session_ids = visible_session_ids

    def _fetch_prs(self, slug: str | None, token: str) -> list:
        if slug is None:
            return []
        if slug in self._pr_cache:
            return self._pr_cache[slug]
        try:
            prs = list_open_prs(slug, token)
        except Exception as exc:
            print(f"[github-lite] failed to fetch PRs for {slug}: {exc}", file=sys.stderr)
            prs = []
        self._pr_cache[slug] = prs
        return prs


def main() -> None:
    worker = GitHubLiteWorker()
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
