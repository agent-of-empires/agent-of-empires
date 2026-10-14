# Local transport is a Unix socket with peer credentials; the disk fallback is transitional

Status: proposed. Decides #4399 (part of #2734). Terms follow `CONTEXT.md`: **Daemon** is the `aoe serve` process, a **Client** is the TUI, CLI or Web dashboard that pilots it, a **Principal** is who a request to the Daemon acts as.

Issue #3839 settled the same REST+WS routes over a local Unix socket and remote TCP, with kernel peer credentials and no bearer on the Unix socket. #3838 settled a hard fail with reconnect guidance when no Daemon is reachable, with no on-disk cache. #4136 serves CLI reads over a Unix socket on Linux only and falls back to on-disk storage elsewhere. This ADR accepts #4136 as a bounded wave 1, fixes the transport per platform, and says what removes the fallback.

## Decision

- **A Unix socket is the local channel.** #4136 ships as a bounded wave 1: its distinct Hello+Snapshot contract at `/api/runtime/ws` is an accepted exception to "same REST+WS routes over the local Unix socket and remote TCP" (#3839). Converging onto the shared router and a typed `LocalPeer` Principal is an explicit obligation of #3897.
- **macOS uses the same Unix socket with `LOCAL_PEERCRED`**, implemented in `src/process/macos.rs` and mirroring Linux `SO_PEERCRED`.
- **The Windows CLI has no local transport.** There is no Windows Daemon for now, so the Windows CLI is always a remote Client: no named pipe, no loopback TCP to a local Daemon. Revisit only if a Windows Daemon is planned.
- **The non-Linux disk fallback in #4136 is a transitional exception to #3838.** It is direct read access to authoritative storage, not a Client-side cache, and exists for read-only CLI commands only. It is removed in the same change that ships the macOS UDS listener; afterwards a CLI that cannot reach the Daemon hard fails with reconnect guidance, as #3838 requires. There is no fallback for mutations (#4400), the TUI or Windows. No calendar deadline is asserted.
- **The socket path derives from the app directory (`HOME`-based), not `XDG_RUNTIME_DIR`.** The path selects the Daemon; peer credentials only prove the caller has the same UID. CityHall's process backend runs several per-user Daemons under one OS user with distinct `HOME`s, and they must not collide (constraint from #4394, ADR 0002). Peer credentials cannot tell such Daemons apart; that is accepted, because the same OS user is one trust boundary.
- **#4136 retained scope.**
  - Accepted: the SessionRead projection, with a ticketed obligation to converge on one projection later; the ACP selector fences; the strict noncreating Storage reads.
  - Split into its own PR: the notification helper replacement, which is independent lifecycle hardening and not a prerequisite for CLI reads.

## Considered options

- **Named pipe or loopback TCP with a token for the Windows CLI.** Rejected: both serve a local Daemon, and there is none on Windows.
- **Keep the non-Linux disk fallback permanently.** Rejected: it contradicts the hard fail of #3838 and leaves two read paths to maintain.
- **Derive the socket path from `XDG_RUNTIME_DIR`.** Rejected: Daemons that share an OS user but have distinct `HOME`s would collide.
- **Hold #4136 until #3897 lands the shared router.** Rejected: CLI reads ship sooner as a bounded wave with the convergence obligation recorded here.

## Consequences

- Until the macOS UDS listener lands, macOS read-only CLI commands may read storage directly. The TUI and mutations never do. Landing the listener deletes that path.
- Router and `LocalPeer` convergence is tracked by #3897. The macOS listener with the fallback removal, and the projection convergence, stay in the Not yet specified section of #2734 until their shape is fixed.
- Nothing here precludes a later multi-user layer (ADR 0002).
