# Local transport is a Unix socket with peer credentials

Status: accepted (#4409). Decides #4399 (part of #2734). Terms follow `CONTEXT.md`: **Daemon** is the `aoe serve` process, a **Client** is the TUI, CLI or Web dashboard that pilots it, a **Principal** is who a request to the Daemon acts as.

Issue #3839 settled the same REST+WS routes over a local Unix socket and remote TCP, with kernel peer credentials and no bearer on the Unix socket. #3838 settled a hard fail with reconnect guidance when no Daemon is reachable, with no on-disk cache. #4136 introduced Linux-only CLI reads over a Unix socket with transitional disk reads elsewhere. This ADR accepts that bounded wave, fixes the transport per platform, and requires removal of the fallback.

## Decision

- **A Unix socket is the local channel.** #4136 ships as a bounded wave 1: its distinct Hello+Snapshot contract is an accepted exception to "same REST+WS routes over the local Unix socket and remote TCP" (#3839). #3897 removed that exception: the one-shot read is `/api/runtime/read/ws` on the shared router, reached over the same Unix socket as every other route, and the separate runtime socket with its publication markers is gone. Requests carry a typed `Principal`, and a Unix socket peer is `LocalPeer`. The path stays distinct from `/api/runtime/ws`, which #4009's TUI stream uses at protocol v2.
- **macOS uses the same Unix socket with `LOCAL_PEERCRED`**, implemented in `src/process/macos.rs` and mirroring Linux `SO_PEERCRED` (#4431).
- **The Windows CLI has no local transport.** There is no Windows Daemon for now, so the Windows CLI is always a remote Client: no named pipe, no loopback TCP to a local Daemon. Revisit only if a Windows Daemon is planned.
- **#4136's transitional CLI disk-read exception is superseded by #4470.** It allowed direct reads of authoritative storage, not a Client-side cache, on supported non-Linux hosts, on Linux without a published Daemon, and when `AOE_DAEMON_URL` got no answer. [Consequences](#consequences) specifies the current behavior and auto-start prerequisite. Mutations (#4400), the TUI and Windows never use a disk fallback.
- **The socket path derives from the app directory (`HOME`-based), not `XDG_RUNTIME_DIR`.** The path selects the Daemon; peer credentials only prove the caller has the same UID. CityHall's process backend runs several per-user Daemons under one OS user with distinct `HOME`s, and they must not collide (constraint from #4394, ADR 0002). Peer credentials cannot tell such Daemons apart; that is accepted, because the same OS user is one trust boundary.
- **#4136 retained scope.**
  - Accepted: the SessionRead projection, with an obligation to converge on one projection later (#4432); the ACP selector fences; the generation, status and Idle pairing in `src/server/reload.rs`; the strict noncreating Storage reads.
  - Split into its own PR: the notification helper replacement, which is independent lifecycle hardening and not a prerequisite for CLI reads.

## Considered options

- **Named pipe or loopback TCP with a token for the Windows CLI.** Rejected: both serve a local Daemon, and there is none on Windows.
- **Keep the non-Linux disk fallback permanently.** Rejected: it contradicts the hard fail of #3838 and leaves two read paths to maintain.
- **Derive the socket path from `XDG_RUNTIME_DIR`.** Rejected: Daemons that share an OS user but have distinct `HOME`s would collide.
- **Hold #4136 until #3897 lands the shared router.** Rejected: CLI reads ship sooner as a bounded wave with the convergence obligation recorded here.

## Consequences

- CLI auto-start landed in #4778 for #4465. #4470 removes all three transitional disk read paths together. Covered CLI reads now use the Daemon or hard fail with reconnect guidance, following ADR 0004's local-only startup policy.
- Tracking: router and `LocalPeer` convergence is #3897 (done), the macOS UDS listener is #4431, projection convergence is #4432.
- Nothing here precludes a later multi-user layer (ADR 0002).
