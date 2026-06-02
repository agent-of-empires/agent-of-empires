#!/usr/bin/env python3
"""Local telemetry sink for trying out aoe's opt-in usage telemetry.

A throwaway HTTP server that accepts the telemetry POSTs aoe sends and
pretty-prints each payload to stdout. Use it to watch what the client emits
without standing up the real collection backend.

Usage:
    python3 scripts/telemetry_sink.py            # listens on 127.0.0.1:8899
    python3 scripts/telemetry_sink.py --port 9000

Then, in another shell, point aoe at it and opt in:
    export AOE_TELEMETRY_ENDPOINT=http://127.0.0.1:8899/ingest
    aoe telemetry enable
    aoe                  # TUI emits process_start + usage_snapshot
    # or: aoe serve      # daemon emits a serve snapshot on boot and hourly

Stdlib only, no dependencies. Ctrl-C to stop. Always replies 200 OK.
"""

import argparse
import datetime
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("content-length", 0))
        raw = self.rfile.read(length) if length else b""
        stamp = datetime.datetime.now().strftime("%H:%M:%S")
        try:
            parsed = json.loads(raw)
            body = json.dumps(parsed, indent=2, sort_keys=True)
            event = parsed.get("event", "?") if isinstance(parsed, dict) else "?"
        except (ValueError, UnicodeDecodeError):
            body = raw.decode("utf-8", "replace")
            event = "?"
        ua = self.headers.get("user-agent", "")
        # flush each line: stdout is block-buffered when piped to a file, and
        # this server is meant to be watched live.
        print(f"\n[{stamp}] POST {self.path}  event={event}  ({ua})", flush=True)
        print(body, flush=True)
        self.send_response(200)
        self.send_header("content-length", "0")
        self.end_headers()

    # Silence the default per-request access log; the handler prints its own.
    def log_message(self, *_args):
        pass


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8899)
    args = parser.parse_args()

    server = ThreadingHTTPServer((args.host, args.port), Handler)
    endpoint = f"http://{args.host}:{args.port}/ingest"
    print(f"telemetry sink listening on {args.host}:{args.port}")
    print(f"  export AOE_TELEMETRY_ENDPOINT={endpoint}")
    print("  (Ctrl-C to stop)")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nstopping")
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
