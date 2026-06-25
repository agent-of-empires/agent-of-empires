#!/usr/bin/env python3
"""Post to X (Twitter) for the @agentofempires account.

This is the "pipe": it composes a post (optionally with media and a link reply)
and sends it through the X API v2. It is deliberately dumb. The judgment about
what to say lives in the `aoe-build-in-public` Claude skill and in your own head.

Safety by design:
  * Dry-run is the DEFAULT. Nothing is sent unless you pass --send.
  * The dry-run path has zero dependencies and never touches the network, so you
    can preview a post anywhere.
  * Credentials are read from the environment only (export them in your shell,
    e.g. ~/.zshrc), never stored in the repo, hardcoded, or printed.

Cost note (X moved to pay-per-use on 2026-02-06):
  * ~$0.015 per post
  * ~$0.20 per post that contains a link
So put the GitHub link in a REPLY, not the main post. It is ~13x cheaper and X's
ranking also suppresses reach on posts with bare external links. Same move, two
wins. The --reply flag exists for exactly this.

Usage:
  # Preview (no network, no creds needed):
  python post_to_x.py --text "9 PRs merged this week across 4 parallel agents."

  # Preview with media + a link reply:
  python post_to_x.py \
      --text "Watch 5 agents work at once. Stuck vs waiting vs idle, at a glance." \
      --media ../../docs/assets/demo.gif \
      --reply "Run your own fleet: https://github.com/agent-of-empires/agent-of-empires"

  # Actually send (requires the four X_* vars exported in your shell):
  python post_to_x.py --text "..." --media demo.gif --reply "..." --send
"""

from __future__ import annotations

import argparse
import os
import re
import sys
import time
from pathlib import Path

MAX_LEN = 280
ENV_KEYS = ("X_API_KEY", "X_API_SECRET", "X_ACCESS_TOKEN", "X_ACCESS_SECRET")
LINK_RE = re.compile(r"https?://", re.IGNORECASE)

# Media upload must use the v2 endpoint: the v1.1 upload endpoint (what
# tweepy's API.media_upload calls) 401s on the current non-elevated access
# level, while v2 create_tweet works. So we do the v2 chunked flow by hand.
MEDIA_UPLOAD_URL = "https://api.x.com/2/media/upload"
CHUNK_SIZE = 4 * 1024 * 1024  # 4 MB per APPEND segment
REQUEST_TIMEOUT = (10, 60)  # (connect, read) seconds, so a hang fails instead of stalling
PROCESSING_TIMEOUT_SECS = 300  # hard cap on waiting for async media processing
EXPECTED_USERNAME = "agentofempires"  # refuse to post from any other account

MEDIA_TYPES = {
    ".gif": "image/gif",
    ".png": "image/png",
    ".jpg": "image/jpeg",
    ".jpeg": "image/jpeg",
    ".webp": "image/webp",
    ".mp4": "video/mp4",
    ".m4v": "video/mp4",
    ".mov": "video/quicktime",
}


def has_link(text: str) -> bool:
    return bool(LINK_RE.search(text))


def estimate_cost(text: str, reply: str | None) -> float:
    """Rough pay-per-use estimate: $0.015/post, $0.20 if it contains a link."""
    cost = 0.20 if has_link(text) else 0.015
    if reply is not None:
        cost += 0.20 if has_link(reply) else 0.015
    return cost


def preview(text: str, media: list[str], reply: str | None) -> None:
    bar = "=" * 60
    print(bar)
    print("DRY RUN - nothing sent. Pass --send to actually post.")
    print(bar)
    print("MAIN POST:")
    print(text)
    n = len(text)
    flag = "  <-- OVER LIMIT" if n > MAX_LEN else ""
    print(f"\n  chars: {n}/{MAX_LEN}{flag}")
    if has_link(text):
        print("  note: main post contains a link. Move it to --reply to cut cost")
        print("        ~13x and avoid X's link-reach penalty.")
    if media:
        print("\nMEDIA:")
        for m in media:
            exists = "ok" if Path(m).is_file() else "MISSING"
            print(f"  [{exists}] {m}")
    if reply is not None:
        print("\nREPLY (posted as a thread under the main post):")
        print(reply)
        rn = len(reply)
        rflag = "  <-- OVER LIMIT" if rn > MAX_LEN else ""
        print(f"\n  chars: {rn}/{MAX_LEN}{rflag}")
    print(f"\nestimated cost: ${estimate_cost(text, reply):.3f}")
    print(bar)


def require_credentials() -> dict[str, str]:
    missing = [k for k in ENV_KEYS if not os.environ.get(k)]
    if missing:
        sys.exit(
            "Cannot --send: missing credentials "
            + ", ".join(missing)
            + ".\nExport them in your shell (see marketing/x/README.md), e.g. add"
            "\n  export X_API_KEY=...   (and the other three) to ~/.zshrc,"
            "\nthen `source ~/.zshrc` or open a new terminal."
        )
    return {k: os.environ[k] for k in ENV_KEYS}


def import_tweepy():
    """Import tweepy with a friendly message instead of a raw traceback."""
    try:
        import tweepy
    except ModuleNotFoundError:
        sys.exit(
            "Cannot --send: tweepy is not installed.\n"
            "Install it with: python3 -m pip install -r marketing/x/requirements.txt"
        )
    return tweepy


def media_category(path: Path) -> str:
    ext = path.suffix.lower()
    if ext == ".gif":
        return "tweet_gif"
    if ext in (".mp4", ".m4v", ".mov"):
        return "tweet_video"
    return "tweet_image"


def _check(resp, step: str) -> None:
    if resp.status_code >= 300:
        sys.exit(f"media upload {step} failed: {resp.status_code} {resp.text}")


def upload_media(creds: dict[str, str], paths: list[str]) -> list[str]:
    import requests
    from requests_oauthlib import OAuth1

    auth = OAuth1(
        creds["X_API_KEY"],
        creds["X_API_SECRET"],
        creds["X_ACCESS_TOKEN"],
        creds["X_ACCESS_SECRET"],
    )
    return [_upload_one(requests, auth, Path(p)) for p in paths]


def _upload_one(requests, auth, path: Path) -> str:
    if not path.is_file():
        sys.exit(f"media file not found: {path}")
    total = path.stat().st_size
    media_type = MEDIA_TYPES.get(path.suffix.lower(), "application/octet-stream")

    # v2 chunked upload uses dedicated sub-endpoints (initialize / append /
    # finalize), not v1.1-style command= params. STATUS is the one exception:
    # GET on the base with command=STATUS.
    init = requests.post(
        f"{MEDIA_UPLOAD_URL}/initialize",
        auth=auth,
        json={
            "media_type": media_type,
            "total_bytes": total,
            "media_category": media_category(path),
        },
        timeout=REQUEST_TIMEOUT,
    )
    _check(init, "INIT")
    media_id = init.json()["data"]["id"]

    with path.open("rb") as fh:
        segment = 0
        while True:
            chunk = fh.read(CHUNK_SIZE)
            if not chunk:
                break
            append = requests.post(
                f"{MEDIA_UPLOAD_URL}/{media_id}/append",
                auth=auth,
                data={"segment_index": str(segment)},
                files={"media": ("chunk", chunk, "application/octet-stream")},
                timeout=REQUEST_TIMEOUT,
            )
            _check(append, f"APPEND segment {segment}")
            segment += 1

    final = requests.post(
        f"{MEDIA_UPLOAD_URL}/{media_id}/finalize",
        auth=auth,
        timeout=REQUEST_TIMEOUT,
    )
    _check(final, "FINALIZE")

    info = (final.json().get("data") or {}).get("processing_info")
    deadline = time.monotonic() + PROCESSING_TIMEOUT_SECS
    while info and info.get("state") in ("pending", "in_progress"):
        if time.monotonic() > deadline:
            sys.exit(f"media processing timed out after {PROCESSING_TIMEOUT_SECS}s: {info}")
        time.sleep(info.get("check_after_secs", 1))
        status = requests.get(
            MEDIA_UPLOAD_URL,
            auth=auth,
            params={"command": "STATUS", "media_id": media_id},
            timeout=REQUEST_TIMEOUT,
        )
        _check(status, "STATUS")
        info = (status.json().get("data") or {}).get("processing_info")
    if info and info.get("state") == "failed":
        sys.exit(f"media processing failed: {info}")

    return media_id


def require_brand_account(client) -> None:
    """Refuse to post unless the credentials authenticate as EXPECTED_USERNAME.

    A terminal can hold tokens for the wrong handle; verify before anything goes
    out so we never post from the wrong account.
    """
    try:
        me = client.get_me()
    except Exception as exc:  # noqa: BLE001 - surface the real cause
        sys.exit(f"refusing to send: could not verify the X account: {exc}")
    username = getattr(me.data, "username", None) if me else None
    if not username:
        sys.exit("refusing to send: could not read the authenticated username.")
    if username.lower() != EXPECTED_USERNAME:
        sys.exit(
            f"refusing to send: credentials authenticate as @{username}, "
            f"expected @{EXPECTED_USERNAME}."
        )


def send(text: str, media: list[str], reply: str | None) -> None:
    creds = require_credentials()
    tweepy = import_tweepy()
    client = tweepy.Client(
        consumer_key=creds["X_API_KEY"],
        consumer_secret=creds["X_API_SECRET"],
        access_token=creds["X_ACCESS_TOKEN"],
        access_token_secret=creds["X_ACCESS_SECRET"],
    )
    require_brand_account(client)

    media_ids = upload_media(creds, media) if media else None
    resp = client.create_tweet(text=text, media_ids=media_ids)
    tweet_id = resp.data["id"]
    print(f"posted: https://x.com/agentofempires/status/{tweet_id}")

    if reply is not None:
        reply_resp = client.create_tweet(text=reply, in_reply_to_tweet_id=tweet_id)
        reply_id = reply_resp.data["id"]
        print(f"reply:  https://x.com/agentofempires/status/{reply_id}")


def validate_media(paths: list[str]) -> None:
    """Enforce X's attachment limits locally before uploading anything."""
    if not paths:
        return
    categories = []
    for raw in paths:
        path = Path(raw)
        if not path.is_file():
            sys.exit(f"media file not found: {path}")
        if path.suffix.lower() not in MEDIA_TYPES:
            sys.exit(f"unsupported media type: {path.suffix or path}")
        categories.append(media_category(path))
    animated = any(c in ("tweet_video", "tweet_gif") for c in categories)
    if animated and len(paths) > 1:
        sys.exit("refusing to send: a gif or video post can attach only one file.")
    if not animated and len(paths) > 4:
        sys.exit("refusing to send: an image post can attach at most 4 files.")


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Compose and (optionally) post to X for @agentofempires.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("--text", required=True, help="Main post body.")
    parser.add_argument(
        "--media",
        action="append",
        default=[],
        metavar="PATH",
        help="Image/gif/video to attach. Repeatable (max 4 images, or 1 video).",
    )
    parser.add_argument(
        "--reply",
        default=None,
        help="Text posted as a reply under the main post. Put your link here.",
    )
    parser.add_argument(
        "--send",
        action="store_true",
        help="Actually post. Without this flag the script only previews.",
    )
    args = parser.parse_args()

    if not args.send:
        preview(args.text, args.media, args.reply)
        return

    if len(args.text) > MAX_LEN or (args.reply and len(args.reply) > MAX_LEN):
        sys.exit("refusing to send: a post exceeds 280 chars. Trim it and retry.")

    validate_media(args.media)
    send(args.text, args.media, args.reply)


if __name__ == "__main__":
    main()
