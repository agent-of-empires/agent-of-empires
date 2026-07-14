# Session & Worktree Internals

Contributor reference for the session layer: Claude conversation resume, worktree creation, scratch-session cleanup, and MCP forwarding. Users want the corresponding guides ([Session Resume](../../guides/session-resume.md), [Git Worktrees](../../guides/worktrees.md), [Scratch Sessions](../../guides/scratch-sessions.md), [MCP Servers](../../guides/mcp-servers.md)).

## Claude conversation resume

aoe persists each Claude Code session's conversation id so a session resumes the same transcript across restarts. The flow: aoe generates a UUID, launches `claude --session-id <uuid>`, records it, and relaunches with `claude --resume <uuid>`.

Two mechanisms keep the recorded id current as Claude rotates it (on `/clear`, `--fork-session`, `--continue`):

- **Hook sidecar (primary).** aoe installs `SessionStart` and `UserPromptSubmit` hooks in `~/.claude/settings.json`. They extract `session_id` from Claude's stdin and write it atomically to `/tmp/aoe-hooks-<euid>/<instance-id>/session_id` (per-user host base, issue #1844). The poller reads this before scanning, so rotations are caught within ~1 poll tick (~2s). The sidecar is host-only.
- **Filesystem-scan fallback.** If the sidecar is absent, stale (>5 min), or invalid, the poller scans `~/.claude/projects/<project>/` for the most recent `.jsonl`. Siblings sharing a project path are disambiguated via the tmux env `AOE_CAPTURED_SESSION_ID`. For Docker sessions the scan runs in-container via `docker exec` (5s cap).

`resume_intent` is decoupled from the poller's observed id so a peer CLI write isn't undone and a daemon restart can't resurrect a cleared value. The post-launch persist of the new id plus the one-shot `Cleared` auto-promote land in a single atomic flock, preserving a concurrent peer write during the launch window.

## Worktree creation

`git worktree add` only checks out tracked files; it does not copy `node_modules`, `.venv`, or `target/`, so creation is cheap and network IO (`git fetch`, `git submodule update`) dominates almost every slow run. For [multi-repo workspaces](../../guides/multi-repo-workspaces.md), the per-repo `create_worktree` calls run concurrently via `std::thread::scope`. See [Git Worktrees](../../guides/worktrees.md) for the bare-repo layout aoe auto-detects.

## Scratch-session cleanup

Scratch sessions store their working dir under the app data dir (not `/tmp`) so it survives reboots and stays inside the namespace the daemon sweeps. Delete-time cleanup runs only when the session's `scratch` flag is true AND the path lives under the scratch root: a tampered `project_path` pointing at, say, `/etc` is left alone. This invariant is what makes the orphan sweep safe to run unattended.

## MCP server forwarding

Native agent MCP configs are re-read live at session start, so edits apply on the next session; aoe only reads these files, never writes them. Precedence is per-server, not whole-file, and overrides are logged. Project-local `.mcp.json` from a repo must sit behind the same repo-trust gate as lifecycle hooks (an untrusted clone could otherwise launch commands on session open); per-profile and project-local source layers are tracked as higher layers.

## Passive-status pipeline

The passive-status pipeline is the daemon's + TUI's `status_poll_loop` shape that detects agent status transitions (Running / Idle / Waiting / Error / ...) via background polling, without any user action. The fix behind #2690 / #2697 / #2729 hardened its invariants; this section is the canonical entry point for a contributor arriving at any of the six touched files (`src/session/instance.rs`, `src/server/mod.rs`, `src/tui/status_poller.rs`, `src/tui/attached_status_hooks.rs`, `src/tui/home/mod.rs`, `src/tui/home/tests.rs`).

The two user-observable behavior contracts this fix introduced (a blank activity column for fresh, never-touched Running sessions, and a briefly stale idle time on structured/ACP sessions after a daemon restart) are written up for users in the [next-release notes](../../releases/vNext.md#session-activity-column).

The canonical glossary lives on `PassiveStatusPatch` in [`src/session/instance.rs`](../../../src/session/instance.rs) under the "Poller vocabulary" heading: `passive status`, `passive status patch`, `live status baseline`, `detected status`, `poller-authoritative status`. That rustdoc block is under a `pub(crate)` item and does not appear in `cargo doc --no-deps` output, so link from here rather than duplicate.

Authority rule:

- **Plain tmux sessions**: the poller is authoritative on `status` / `idle_entered_at` / `last_accessed_at`. `Instance::update_status_with_metadata` reads pane metadata + tmux state, compares against `live_status_baseline`, and mutates in place. `merge_passive_status_patch` on disk applies the same fields per patch.
- **Structured / ACP sessions**: `apply_acp_overlay_inplace` in `src/server/mod.rs` is the sole authority. `decide_passive_transition` returns `patch: None` for `is_structured()` rows, so the passive-status writers deliberately skip them. Post-daemon-restart the row's `idle_entered_at` resets to the last durable value (either creation-time, or the last explicit user action); ACP event handlers re-emit as they observe new state.

Writer shapes:

- **Daemon**: batches transitions per profile per tick. One `Storage::update` per profile via `api::persist_session_update` at `src/server/api/sessions.rs`. Bundle type is `PassiveTransitionWrites` in `src/server/mod.rs`.
- **TUI**: writes one transition at a time. `HomeView::persist_passive_status_transition` in `src/tui/home/mod.rs`.

Both writers funnel through `Instance::merge_passive_status_patch`, whose field semantics are:

- `last_accessed_at`: monotone non-decreasing. The `>=` guard drops an older-or-equal incoming value, keyed on wall-clock `chrono::Utc::now()` (not monotonic; best-effort under NTP rewinds).
- `status` and `idle_entered_at`: unconditional writes (last-writer-wins).

Safety of the two-writer interleave relies on (a) the poller being the sole authority on `status`/`idle_entered_at`, and (b) both writers reading the same live source, so they converge within one poll interval of the slower cadence (daemon 2s, TUI ~500ms). A future field added to `PassiveStatusPatch` that is neither monotone nor single-authority would diverge silently between the two paths; adding one requires either unifying the writers first or explicitly documenting why the two-writer shape stays safe. The invariant is captured in the `PassiveTransitionWrites` docstring at `src/server/mod.rs` and enforced by reviewer diligence.

`live_status_baseline` is `#[serde(skip)]` on `Instance` and seeded `None` in `Instance::new`. On every fresh disk load (TUI relaunch, daemon tick reload), the field is `None` because of the `serde(skip)`, and the first `update_status_with_metadata` seeds it without restamping. The construction-ordering safety (the field's `None` seed relies on `Instance::new` running before the instance enters shared state, not on synchronization) is documented on the field itself in `src/session/instance.rs`.
