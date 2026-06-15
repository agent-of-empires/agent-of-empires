# GitHub Integration

Agent of Empires talks to GitHub through a single backend client (`src/github/`).
Every call to `api.github.com` goes through it. Only unauthenticated public
reads are wired up today (the update checker hitting the releases endpoint).
This page documents the typed failures that surface.

## When a request fails

Request failures are typed so the surface (a TUI toast or a web error banner)
can show the right next step:

- **401 Unauthorized**: AoE only makes unauthenticated public requests, so this
  usually means the resource is private or the endpoint requires sign-in.
- **403 with a missing scope**: AoE names the required scope from GitHub's
  `X-Accepted-OAuth-Scopes` response header, for example `repo` or `workflow`.
  AoE makes unauthenticated requests, so the resource needs a signed-in client.
- **403 or 429 rate limited**: wait for the limit to reset.
- **404 Not Found**: the resource does not exist or is not publicly visible.
- **Network unreachable**: distinguished from auth, so a GitHub outage never
  tells you to re-login.
