# X posting tooling for @agentofempires

`post_to_x.py` previews or sends an approved post with optional media and a link
reply. Drafting belongs to the `aoe-build-in-public` skill.

## Safety model

- Preview is the default and does not use the network. Only `--send` posts.
- Keep the four `X_*` credentials in your shell environment, never the repo.
- A person must approve the draft and run the send command.

## Credentials

Four variables, exported in your shell:

| Variable | What it is |
| --- | --- |
| `X_API_KEY` | App consumer key (developer portal -> app -> Keys and tokens) |
| `X_API_SECRET` | App consumer secret |
| `X_ACCESS_TOKEN` | @agentofempires user token (minted by `mint_token.py`) |
| `X_ACCESS_SECRET` | @agentofempires user token secret |

Use `exports.example.sh` as the shell configuration template.

## One-time setup

1. Create a production X developer app with OAuth 1.0a read and write access.
2. Export its API key and secret in your shell.
3. Install the send dependency:
   ```bash
   python3 -m pip install -r marketing/x/requirements.txt
   ```
4. Mint the `@agentofempires` access token:
   ```bash
   python3 marketing/x/mint_token.py
   ```
   Authorize while logged in as `@agentofempires`, enter the PIN, and export the
   two values it prints.

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

Links in the main post cost more and reduce reach. Put the GitHub URL in
`--reply`; the script warns when the main text contains a link. Run
`post_to_x.py --help` for all flags and limits.
