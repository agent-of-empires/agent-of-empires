# GitHub Integration

Agent of Empires talks to GitHub through a single backend client (`src/github/`).
Every call to `api.github.com` goes through it. Only unauthenticated public
reads are wired up today (the update checker hitting the releases endpoint).
This page documents the typed failures that surface.

## When a request fails

Request failures are typed so the surface (a TUI toast or a web error banner)
can show the right next step:

- **401 Unauthorized**: the token is missing, invalid, or expired. Re-authenticate.
- **403 with a missing scope**: AoE names the required scope from GitHub's
  `X-Accepted-OAuth-Scopes` response header, for example `repo` or `workflow`,
  so you know exactly what to re-authorize.
- **403 or 429 rate limited**: wait for the limit to reset. Authenticating raises
  the limit, so an unauthenticated user is pointed at setting a token.
- **404 Not Found**: the resource does not exist or is not visible to your token.
- **Network unreachable**: distinguished from auth, so a GitHub outage never
  tells you to re-login.
