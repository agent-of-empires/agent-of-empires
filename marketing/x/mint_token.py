#!/usr/bin/env python3
"""One-time: mint an OAuth 1.0a access token for @agentofempires.

Path B setup. The X app (its API key/secret) is owned by njbrake's developer
account, which keeps billing under njbrake. But the access token is what decides
WHICH account a post comes from, so we run a one-time 3-legged OAuth flow: you
authorize the app while logged in as @agentofempires, and this prints that
account's access token + secret as `export` lines to add to your shell.

Credentials live in your shell environment (e.g. ~/.zshrc), never in the repo.

Prereqs:
  * X_API_KEY and X_API_SECRET exported in your shell (the app's consumer
    key/secret from the developer portal "Keys and tokens" tab).
  * The app has OAuth 1.0a user authentication enabled, Read and Write
    permission, and a callback URL configured.
  * tweepy installed: python3 -m pip install -r marketing/x/requirements.txt

Run it in your own terminal (you need a browser for the authorize step):
  python3 marketing/x/mint_token.py
Then add the two printed export lines to ~/.zshrc and reload your shell.
"""

from __future__ import annotations

import os
import sys
from urllib.parse import parse_qs, urlparse


def import_tweepy():
    try:
        import tweepy
    except ModuleNotFoundError:
        sys.exit(
            "tweepy not installed.\n"
            "Run: python3 -m pip install -r marketing/x/requirements.txt"
        )
    return tweepy


def get_consumer() -> tuple[str, str]:
    consumer_key = os.environ.get("X_API_KEY")
    consumer_secret = os.environ.get("X_API_SECRET")
    if not consumer_key or not consumer_secret:
        sys.exit(
            "X_API_KEY / X_API_SECRET are not in your environment.\n"
            "Export them first (the app's consumer key/secret), e.g. add\n"
            "  export X_API_KEY=...\n  export X_API_SECRET=...\n"
            "to ~/.zshrc, then `source ~/.zshrc` and re-run."
        )
    return consumer_key, consumer_secret


def main() -> None:
    tweepy = import_tweepy()
    consumer_key, consumer_secret = get_consumer()

    handler = tweepy.OAuth1UserHandler(consumer_key, consumer_secret, callback="oob")
    try:
        url = handler.get_authorization_url()
    except Exception as exc:  # noqa: BLE001 - surface the real cause
        sys.exit(
            f"could not start the OAuth flow: {exc}\n"
            "Check the app has OAuth 1.0a enabled, Read+Write permission, and a "
            "callback URL set in User authentication settings."
        )

    print("\n1. Open this URL in a browser LOGGED IN AS @agentofempires")
    print("   (an incognito/private window is the easy way):\n")
    print("   " + url + "\n")
    print("2. Approve the app. X shows a 7-digit PIN (or redirects you).\n")
    raw = input("3. Paste the PIN here (or the full redirected URL): ").strip()
    if "oauth_verifier=" in raw:
        raw = parse_qs(urlparse(raw).query)["oauth_verifier"][0]

    try:
        access_token, access_secret = handler.get_access_token(raw)
    except Exception as exc:  # noqa: BLE001
        sys.exit(f"failed to exchange PIN for an access token: {exc}")

    who = None
    verify_error = None
    try:
        client = tweepy.Client(
            consumer_key=consumer_key,
            consumer_secret=consumer_secret,
            access_token=access_token,
            access_token_secret=access_secret,
        )
        me = client.get_me()
        if me and me.data:
            who = me.data.username
    except Exception as exc:  # noqa: BLE001 - report it, do not mask it
        verify_error = exc

    if who:
        print(f"\nThis token posts as: @{who}")
        if who.lower() != "agentofempires":
            print(
                "WARNING: that is not @agentofempires. You likely authorized while "
                "logged in as the wrong account.\nRe-run and approve in a window "
                "logged in as @agentofempires.\n"
            )
    else:
        # Verification failed (network/API), which is NOT the same as a
        # wrong-account result. Say so plainly instead of crying wrong-account.
        detail = f": {verify_error}" if verify_error else "."
        print(f"\nCould not verify which account this token posts as{detail}")
        print("Confirm you authorized as @agentofempires before relying on it.")

    print("\nAdd these two lines to ~/.zshrc, then `source ~/.zshrc`:\n")
    print(f'export X_ACCESS_TOKEN="{access_token}"')
    print(f'export X_ACCESS_SECRET="{access_secret}"')


if __name__ == "__main__":
    main()
