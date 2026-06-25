# X posting tooling for @agentofempires

The "pipe" for getting build-in-public posts onto X. It is intentionally dumb:
it composes a post, optionally attaches media, optionally adds a link reply, and
sends it. What to say is decided by the `aoe-build-in-public` Claude skill (see
`.claude/skills/aoe-build-in-public/`) and by you.

## Why this exists

aoe's happiest user is a multi-agent power user who lives in X dev circles. That
person adopts tools they see respected builders *using*, not tools that get
advertised at them. So the play is build-in-public: show aoe shipping aoe. This
script is the last 5% that turns an approved draft into a posted tweet.

## Safety model

- **Dry-run is the default.** Without `--send`, the script only previews. The
  preview path has zero dependencies and never hits the network.
- **Credentials never live in the repo.** The scripts read four `X_*` variables
  straight from your shell environment. Keep them in `~/.zshrc` (or your shell's
  rc), not in any tracked file.
- **The human stays the trigger.** The script can post, but it only posts when a
  person runs it with `--send`. The skill enforces a draft -> approve -> send
  gate on top of that.

## Credentials

Four variables, exported in your shell:

| Variable | What it is |
| --- | --- |
| `X_API_KEY` | App consumer key (developer portal -> app -> Keys and tokens) |
| `X_API_SECRET` | App consumer secret |
| `X_ACCESS_TOKEN` | @agentofempires user token (minted by `mint_token.py`) |
| `X_ACCESS_SECRET` | @agentofempires user token secret |

See `exports.example.sh` for a copy-paste block. Add the filled-in lines to
`~/.zshrc`, then `source ~/.zshrc` or open a new terminal. Verify with:

```bash
for v in X_API_KEY X_API_SECRET X_ACCESS_TOKEN X_ACCESS_SECRET; do
  eval "val=\$$v"
  [ -n "$val" ] && echo "$v: set" || echo "$v: MISSING or empty"
done
```

It must report all four as `set`. If any shows `MISSING or empty`, you likely
sourced the template (`exports.example.sh`) without filling in the values.

## One-time setup

1. Create an X developer app (Production environment) under your developer
   account. Enable OAuth 1.0a with **Read and Write** permission and set a
   callback URL (any valid URL; the PIN flow does not use it).
2. Copy the app's API Key + Secret into `~/.zshrc` as `export X_API_KEY=...` and
   `export X_API_SECRET=...`, then `source ~/.zshrc`.
3. Install the send-path dependency (the dry-run path needs nothing):
   ```bash
   python3 -m pip install -r marketing/x/requirements.txt
   ```
4. Mint the @agentofempires access token. Run this in your own terminal (you
   need a browser for the authorize step):
   ```bash
   python3 marketing/x/mint_token.py
   ```
   Open the printed URL in a browser logged in as **@agentofempires**, approve,
   paste back the PIN. It prints two `export` lines. Add them to `~/.zshrc` and
   `source ~/.zshrc`.

## Usage

Preview a post (no creds, no network):
```bash
python3 marketing/x/post_to_x.py \
  --text "9 PRs merged this week across 4 parallel agents in aoe."
```

Preview with media and a link reply (the link goes in the reply, not the post):
```bash
python3 marketing/x/post_to_x.py \
  --text "Watch 5 agents work at once. Stuck, waiting, idle, all at a glance." \
  --media docs/assets/demo.gif \
  --reply "Run your own fleet: https://github.com/agent-of-empires/agent-of-empires"
```

Actually send it (needs the four `X_*` vars in your environment):
```bash
python3 marketing/x/post_to_x.py --text "..." --media demo.gif --reply "..." --send
```

## The link-in-reply rule

X moved to pay-per-use on 2026-02-06: roughly **$0.015 per post**, but
**$0.20 if the post contains a link**. A bare link in the main post also gets
reach-suppressed by X's ranking. So keep the main post link-free and put the
GitHub URL in `--reply`. Cheaper and better reach. The script warns you if a
link shows up in the main post.

## Flags

| Flag | Meaning |
| --- | --- |
| `--text` | Main post body (required). Warns over 280 chars; refuses to send over. |
| `--media PATH` | Attach an image/gif/video. Repeatable (max 4 images, or 1 video). |
| `--reply TEXT` | Post TEXT as a reply under the main post. Put your link here. |
| `--send` | Actually post. Omit to preview only. |

## Voice rules (for whatever drafts these)

- No em dashes, and no double-hyphen separators in prose. Use commas, periods, or rephrase.
- No hashtag spam. One tasteful tag at most, usually zero.
- Show, do not tell: a 15-second clip of the dashboard beats three sentences of
  adjectives.
- Lead with the user's pain, not aoe's features.
