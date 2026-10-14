# Local transport is a Unix socket with peer credentials; the disk fallback is transitional

Status: proposed. Decides #4399 (part of #2734). Terms follow `CONTEXT.md`: **Daemon** is the `aoe serve` process, a **Client** is the TUI, CLI or Web dashboard that pilots it, a **Principal** is who a request to the Daemon acts as.

#3839 settled REST+WS over a Unix socket with kernel peer credentials and no bearer. #3838 settled a hard fail with reconnect guidance when no Daemon is reachable, with no on-disk cache. #4136 serves CLI reads over a Unix socket on Linux only and falls back to on-disk storage elsewhere. This ADR accepts #4136 as a bounded first wave, fixes the transport per platform, and says what removes the fallback.

## Decision

- **A Unix socket is the local channel.** #4136 ships as wave 1 of the Client cutover: its distinct Hello+Snapshot contract at `/api/runtime/ws` is an accepted exception to "same REST+WS routes over the Unix socket and TCP" (#3839). Converging onto the shared router and a typed local-owner Principal is an obligation of #3897.
- **macOS uses the same Unix socket with `LOCAL_PEERCRED`**, implemented in `src/process/macos.rs` and mirroring Linux `SO_PEERCRED`.
- **Windows has no local transport.** There is no Windows Daemon for now, so the Windows CLI is always a remote Client: no named pipe, no loopback TCP to a local Daemon. Revisit only if a Windows Daemon is planned.
- **The non-Linux disk fallback in #4136 is transitional.** It exists for read-only CLI commands only and is removed in the same change that ships the macOS listener. There is no fallback for mutations (#4400), the TUI or Windows. No calendar deadline is asserted.
- **The socket path derives from the app directory, not `XDG_RUNTIME_DIR`.** CityHall's process backend runs several per-user Daemons under one OS user with distinct `HOME`s, and they must not collide (constraint from #4394, ADR 0002). Peer credentials cannot tell such Daemons apart; that is accepted, because the same OS user is one trust boundary.
- **#4136 retained scope.**
  - Accepted: the SessionRead projection, with a ticketed obligation to converge on one projection later; the ACP selector fences; the strict noncreating Storage reads.
  - Split into its own PR: the notification helper replacement, which is independent lifecycle hardening and not a prerequisite for CLI reads.

## Considered options

- **Named pipe or loopback TCP with a token for the Windows CLI.** Rejected: both serve a local Daemon, and there is none on Windows.
- **Keep the non-Linux disk fallback permanently.** Rejected: it contradicts the hard fail of #3838 and leaves two read paths to maintain.
- **Hold #4136 until #3897 lands the shared router.** Rejected: CLI reads ship sooner as a bounded wave with the convergence obligation recorded here.

## Consequences

- Until the macOS listener lands, macOS Clients read the disk for read-only CLI commands. Landing it deletes that path.
- The macOS listener, the fallback removal and the projection convergence stay in the Not yet specified section of #2734 until their shape is fixed.
- Nothing here precludes a later multi-user layer (ADR 0002).
