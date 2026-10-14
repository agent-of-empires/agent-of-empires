# The Daemon stays single-user; multi-user lives in CityHall

Status: proposed. Decides #4394 (part of #2734). Terms follow `CONTEXT.md`: **Daemon** is the `aoe serve` process, a **Client** is the TUI, CLI or Web dashboard that pilots it, a **Principal** is who a request to the Daemon acts as.

The "software development factory" scope (a user base with local, remote and SSO accounts, an authentication service, several humans running agents through authenticated Clients, agents handed between users) is real but not targeted first. [CityHall](https://github.com/agent-of-empires/cityhall) already provides users, OIDC SSO, RBAC, per-user secrets and one aoe workspace per user behind its proxy. We keep that split: one Daemon serves exactly one human, and every multi-user concern lives in CityHall. The Daemon only gains a seam so a later layer plugs in without a redesign.

## Decision

- **One Daemon, one human.** The Daemon never models several users. A factory runs one Daemon per user, orchestrated and authenticated by CityHall.
- **CityHall owns** the user base, SSO and any OIDC integration, per-user authorization, human audit, and moving agents between users (agent-of-empires/cityhall#99).
- **A typed Principal is the only seam added now.** The "authentication principal" from #2734 section D becomes one type with four variants, and every current auth path maps to one:
  - **Local owner**: a Unix socket peer. Loopback TCP trust (`is_local_trusted` in `src/server/auth.rs`) also maps here, transitionally: it ends on each platform when that platform's Unix socket listener lands (#3897).
  - **Device**: a passphrase login bound to a browser (`src/server/login.rs`).
  - **Token holder**: a caller presenting the `serve.token` bearer. `--auth none` skips the token check and admits every caller as a Token holder.
  - **Proxy**: a caller on a configured proxy ingress (#3839), such as CityHall's proxy with its per-workspace bearer. Never the local owner.

  Every authorization decision goes through one function of principal and route. The scattered `read_only` checks move behind it as their code is touched, not as a separate project.
- **No owner on Sessions, no human audit log, no user header.** Sessions carry no owner field. `--auth=trusted-proxy` with a user header (#2735 tier 3) is out of scope: a single-user Daemon has no use for the user name.
- **CityHall reaches its Daemons with a per-workspace bearer.** CityHall mints the token and its proxy injects it; the Daemon runs in token mode and sees a Proxy principal. CityHall stops running its Daemons with `--auth none`; this ADR does not remove the `aoe serve --auth none` option, and whether it survives on non-loopback binds is #3839's call. The remote TLS rule of #3839 holds unchanged: the proxy to Daemon hop is HTTPS, loopback, or inside the workspace container (agent-of-empires/cityhall#100).
- **Same OS user is one trust boundary.** Daemons sharing an OS user, as CityHall's process backend runs them with distinct `HOME`s, are isolated for data, not for security. The Unix socket path derives from the app directory, not `XDG_RUNTIME_DIR`, so such Daemons do not collide (constraint passed to #4407).
- **CityHall adapts to the split.** Its current deployment shape is not a constraint on the Daemon and Client work.

## Considered options

- **One shared Daemon serving many humans.** Rejected: it duplicates CityHall's identity, roles and audit inside the Daemon, and agents run as the Daemon's OS user, so humans sharing a Daemon share its OS privileges. Per-user authorization would reach every route, the Session store, tmux and the Sandbox.
- **Keep CityHall on `--auth none` behind a trusted network.** Rejected: it adds a standing exception to the #3839 transport rules for one deployment.
- **CityHall talks to each Daemon over its Unix socket.** Rejected: the proxy would be authorized as the local owner, which it is not.

## Consequences

- CityHall calling a Daemon on its own behalf (activity for admins, cityhall#15; settings approvals, cityhall#13) needs a distinct principal with its own permissions. It is left in the Not yet specified section of #2734; the Principal seam is what makes it additive.
- Remote access for a solo user stays passphrase, plus passkeys (#4348). There is no aoe-native SSO.
