# GitHub Integration

Agent of Empires talks to GitHub through a single backend client (`src/github/`).
Every call to `api.github.com` goes through it, and it never shells out to `gh`
for individual requests. This page documents how AoE finds a GitHub token, what
happens when it cannot, and what is intentionally deferred.

## How AoE finds your token

`gh` is an optional token source, never a hard dependency. AoE resolves a token
once, in this fixed order, and the first hit wins:

1. The `GITHUB_TOKEN` environment variable.
2. The `GH_TOKEN` environment variable.
3. `gh auth token`, but only when the GitHub CLI is installed and authenticated.
   AoE captures the token `gh` prints and sends it as `Authorization: Bearer
   <token>`; it does not run `gh` per request.

If you already use the GitHub CLI on your machine (the common case on a dev
laptop), steps 1 and 2 miss, step 3 returns your existing token, and everything
works with no prompt and no extra login.

Empty or whitespace-only environment variables are ignored, so an exported but
blank `GITHUB_TOKEN` falls through to `gh` rather than failing.

## When no token is available

Each failure produces its own hint, never a generic "auth required". The hint
always matches the actual cause:

| Situation | What AoE tells you |
| --- | --- |
| No env token and `gh` is not installed | Set `GITHUB_TOKEN` (or `GH_TOKEN`), or install the GitHub CLI and run `gh auth login`. |
| `gh` is installed but not authenticated | Run `gh auth login`. AoE does not tell you to install `gh`, because it is already installed. |
| `gh` returns an empty token | Re-authenticate with `gh auth login`, or set a token directly. |
| Running `gh` fails | The underlying error, plus a note that you can set `GITHUB_TOKEN` to bypass `gh`. |

## When a request fails

Once a token is resolved, request failures are also typed so the surface (a TUI
toast or a web error banner) can show the right next step:

- **401 Unauthorized**: the token is missing, invalid, or expired. Re-authenticate.
- **403 with a missing scope**: AoE names the required scope from GitHub's
  `X-Accepted-OAuth-Scopes` response header, for example `repo` or `workflow`,
  so you know exactly what to re-authorize.
- **403 or 429 rate limited**: wait for the limit to reset. Authenticating raises
  the limit, so an unauthenticated user is pointed at setting a token.
- **404 Not Found**: the resource does not exist or is not visible to your token.
- **Network unreachable**: distinguished from auth, so a GitHub outage never
  tells you to re-login.

## Deferred to follow-ups

This foundation deliberately stops at token resolution, the typed errors above,
and the read endpoints the update checker needs. The rest is tracked separately:

- Device-flow login as the no-`gh`, no-env-token fallback: #1678.
- GraphQL, ETag conditional caching, and rate-limit backoff in the client: #1679.
- A guided scope-elevation re-auth flow on write failures: #1680.
- GitHub Enterprise host derivation from the git remote: #1668.
