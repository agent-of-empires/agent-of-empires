import type { RepoGroup, SessionResponse, Workspace } from "./types";
import { safeGetItem, safeSetItem } from "./safeStorage";

export type SidebarSortMode = "manual" | "lastActivity" | "attention";

export const SIDEBAR_SORT_MODE_KEY = "aoe-sidebar-sort-mode";

const VALID_MODES: readonly SidebarSortMode[] = ["manual", "lastActivity", "attention"];

export function loadSidebarSortMode(): SidebarSortMode {
  const raw = safeGetItem(SIDEBAR_SORT_MODE_KEY);
  if (raw && (VALID_MODES as readonly string[]).includes(raw)) {
    return raw as SidebarSortMode;
  }
  return "manual";
}

export function saveSidebarSortMode(mode: SidebarSortMode): void {
  safeSetItem(SIDEBAR_SORT_MODE_KEY, mode);
}

function epochOr(ts: string | null | undefined): number {
  if (!ts) return Number.NEGATIVE_INFINITY;
  const n = Date.parse(ts);
  return Number.isFinite(n) ? n : Number.NEGATIVE_INFINITY;
}

/** Most-recent activity timestamp across a workspace's sessions, in epoch ms.
 *  Considers `last_accessed_at`, `idle_entered_at`, and `created_at`; nulls
 *  and unparseable strings are skipped. Returns `Number.NEGATIVE_INFINITY`
 *  when no usable timestamp exists. */
export function workspaceLastActivityMs(ws: Workspace): number {
  let best = Number.NEGATIVE_INFINITY;
  for (const s of ws.sessions) {
    const m = Math.max(epochOr(s.last_accessed_at), epochOr(s.idle_entered_at), epochOr(s.created_at));
    if (m > best) best = m;
  }
  return best;
}

/** Group-level activity key: max across the group's workspaces. */
export function repoGroupLastActivityMs(workspaces: readonly Workspace[]): number {
  let best = Number.NEGATIVE_INFINITY;
  for (const ws of workspaces) {
    const m = workspaceLastActivityMs(ws);
    if (m > best) best = m;
  }
  return best;
}

/** True when at least one of the workspace's sessions has been
 *  web-pinned. Mirrors the aggregator shape used for `isFavorited` in
 *  `WorkspaceSidebar.tsx`. See #1581. */
export function workspaceIsPinned(ws: Workspace): boolean {
  return ws.sessions.some((s) => s.pinned_at != null);
}

/** True when every one of the workspace's sessions is in a sink state
 *  (archived or currently snoozed). Uses an "all sessions sunk"
 *  aggregator on purpose: a multi-session workspace with one running
 *  session must not disappear into the collapsible footer just because
 *  a sibling session was archived. See #1581. */
export function workspaceIsSunk(ws: Workspace): boolean {
  if (ws.sessions.length === 0) return false;
  return ws.sessions.every((s) => s.archived_at != null || s.snoozed_until != null);
}

/** True when a repo group still has at least one workspace that is
 *  not sunk (archived or actively snoozed across all sessions). The
 *  sidebar uses this to hide the group's header when every workspace
 *  has dropped into the global "Snoozed & archived" footer, so the
 *  live list does not show an orphan header with no rows. The footer
 *  itself scans the unfiltered group list, so sunk sessions are not
 *  lost. See #1600. */
export function repoGroupHasLiveWorkspace(group: RepoGroup): boolean {
  return group.workspaces.some((ws) => !workspaceIsSunk(ws));
}

/** Triage tier for a workspace: 0 = pinned (top of every sort), 1 =
 *  live (default), 2 = sunk (bottom of every sort, target of the
 *  collapsible "Snoozed & archived" section). A workspace cannot be
 *  both pinned and sunk because `Instance::pin()` clears the sink
 *  fields server-side, so any pinned session keeps the whole workspace
 *  in tier 0 even if a sibling session is archived. See #1581. */
export function workspaceTriageTier(ws: Workspace): 0 | 1 | 2 {
  if (workspaceIsPinned(ws)) return 0;
  if (workspaceIsSunk(ws)) return 2;
  return 1;
}

/** Whether two RFC3339 snooze timestamps are close enough to count
 *  as the same snooze deadline, given some unavoidable skew between
 *  the client's `Date.now()` (used to mint the optimistic preview)
 *  and the server's `Utc::now()` (used by `Instance::snooze`). A 2
 *  minute tolerance covers serialization rounding, daemon RTT, and
 *  small clock drift without letting a brand-new snooze get swapped
 *  back to a stale one. Unparseable strings fall back to literal
 *  equality so the helper is defensive. See #1581. */
export function snoozeTimestampCloseEnough(aIso: string, bIso: string): boolean {
  const a = Date.parse(aIso);
  const b = Date.parse(bIso);
  if (!Number.isFinite(a) || !Number.isFinite(b)) return aIso === bIso;
  return Math.abs(a - b) <= 2 * 60_000;
}

/** Resolve the "effective" snoozed_until value the row should render
 *  with, given a server-derived prop and an optimistic local
 *  override. `undefined` on the optimistic side means "no override,
 *  fall through"; `null` means "pretend the server already
 *  unsnoozed"; a string means "pretend the server already snoozed
 *  until then." Extracted as a pure helper so the optimistic
 *  resolution is unit-testable without mounting the whole sidebar.
 *  See #1581 CodeRabbit review. */
export function resolveEffectiveSnoozedUntil(
  optimistic: string | null | undefined,
  serverValue: string | null | undefined,
): string | null | undefined {
  if (optimistic === undefined) return serverValue;
  return optimistic;
}

/** Triage state of a single session row, used by the sidebar context
 *  menu to decide which actions to show. The state machine is
 *  mutually exclusive: only one of pinned/archived/snoozed can be the
 *  active state at a time (the server's XOR rules in
 *  `Instance::pin/archive/snooze` enforce this), so the menu only
 *  offers the corresponding "Un…" toggle plus Rename / Notifications
 *  / Delete. Live rows get the full Pin / Archive / Snooze… set.
 *  See #1581. */
export type TriageState = "live" | "pinned" | "archived" | "snoozed";

/** Action visibility from a triage state. The state machine assumes
 *  the server has already enforced mutual exclusion, so a row that is
 *  archived simply cannot also be pinned: the menu would show two
 *  contradictory toggles. Priority for the (impossible-but-defensive)
 *  case where a workspace aggregator surfaces more than one tier:
 *  pinned > archived > snoozed > live. */
export interface TriageMenuShape {
  showPin: boolean;
  showUnpin: boolean;
  showArchive: boolean;
  showUnarchive: boolean;
  showSnooze: boolean;
  showUnsnooze: boolean;
}

export function triageStateOf(input: { isPinned: boolean; isArchived: boolean; isSnoozed: boolean }): TriageState {
  if (input.isPinned) return "pinned";
  if (input.isArchived) return "archived";
  if (input.isSnoozed) return "snoozed";
  return "live";
}

export function triageMenuShape(state: TriageState): TriageMenuShape {
  switch (state) {
    case "pinned":
      return {
        showPin: false,
        showUnpin: true,
        showArchive: false,
        showUnarchive: false,
        showSnooze: false,
        showUnsnooze: false,
      };
    case "archived":
      return {
        showPin: false,
        showUnpin: false,
        showArchive: false,
        showUnarchive: true,
        showSnooze: false,
        showUnsnooze: false,
      };
    case "snoozed":
      return {
        showPin: false,
        showUnpin: false,
        showArchive: false,
        showUnarchive: false,
        showSnooze: false,
        showUnsnooze: true,
      };
    case "live":
      return {
        showPin: true,
        showUnpin: false,
        showArchive: true,
        showUnarchive: false,
        showSnooze: true,
        showUnsnooze: false,
      };
  }
}

/** Stable, deterministic comparator. Triage tier wins first (pinned at
 *  the top, sunk at the bottom, regardless of sort mode); within tier
 *  the comparator falls back to last-activity descending, with id
 *  ascending as the tie-break so equal timestamps never flake the
 *  render order. The two activity keys are compared with `<` / `>`
 *  rather than subtraction because workspaces with no usable timestamp
 *  return `Number.NEGATIVE_INFINITY`; `-Infinity - -Infinity` is
 *  `NaN`, which `Array.prototype.sort` treats like `0` (equal) and
 *  would silently skip the id tie-break, leaving ordering at the mercy
 *  of input order. */
export function compareWorkspacesByLastActivityDesc(a: Workspace, b: Workspace): number {
  const aTier = workspaceTriageTier(a);
  const bTier = workspaceTriageTier(b);
  if (aTier !== bTier) return aTier - bTier;
  const aMs = workspaceLastActivityMs(a);
  const bMs = workspaceLastActivityMs(b);
  if (aMs < bMs) return 1;
  if (aMs > bMs) return -1;
  return a.id.localeCompare(b.id);
}

/** Sink rank for the Attention sort, mirroring the TUI's tier-99 bucket
 *  (`attention_tier`, src/session/groups.rs). Archived and snoozed sessions
 *  get this rank so they never lift their workspace toward the top, even
 *  when their last live status was Waiting or Error. */
const ATTENTION_SINK_RANK = 99;

/** Priority rank for a single session under the Attention sort. Lower =
 *  higher priority = closer to the top. Mirrors the TUI `attention_tier`
 *  status taxonomy: Waiting needs a human (top), then Error, then the rest
 *  of the live states, with transient lifecycle states at the bottom.
 *  Archived / snoozed sessions short-circuit to the sink rank so a snoozed
 *  Waiting session cannot make its workspace look urgent. The `urgent`
 *  hook flag is handled separately as a cross-rank promoter in
 *  `compareWorkspacesByAttention`, matching the TUI's `attention_session_key`
 *  where urgent is the primary term and status tier is secondary. */
export function sessionAttentionRank(s: SessionResponse): number {
  if (s.archived_at != null || s.snoozed_until != null) {
    return ATTENTION_SINK_RANK;
  }
  switch (s.status) {
    case "Waiting":
      return 0;
    case "Error":
      return 1;
    case "Idle":
      return 2;
    case "Unknown":
      return 3;
    case "Running":
      return 4;
    case "Stopped":
      return 5;
    case "Starting":
    case "Creating":
    case "Deleting":
      return 6;
    default:
      // Defensive: an unknown status string from a newer server reads as
      // "glance warranted", matching the Unknown rank rather than sinking.
      return 3;
  }
}

/** Best (lowest) attention rank across a workspace's sessions. A workspace
 *  is as urgent as its most-urgent session. */
export function workspaceAttentionRank(ws: Workspace): number {
  let best = ATTENTION_SINK_RANK;
  for (const s of ws.sessions) {
    const rank = sessionAttentionRank(s);
    if (rank < best) best = rank;
  }
  return best;
}

/** True when any of the workspace's sessions is a user favorite. */
export function workspaceIsFavorited(ws: Workspace): boolean {
  return ws.sessions.some((s) => s.favorited);
}

/** True when any of the workspace's sessions carries the agent-raised
 *  `urgent` hook flag. The server clears urgent for archived / snoozed
 *  sessions (`Instance::is_urgent()`), so a sunk workspace never reports
 *  urgent and cannot claw back above live rows. See #1640. */
export function workspaceIsUrgent(ws: Workspace): boolean {
  return ws.sessions.some((s) => s.urgent === true);
}

/** Attention-sort comparator. Key chain, all deterministic with an id
 *  tie-break so equal keys never flake the render order:
 *    1. triage tier (pinned floats, sunk sinks, same web invariant as
 *       last-activity sort);
 *    2. urgent first (cross-rank promoter, mirrors the TUI urgent-bias);
 *    3. attention rank ascending (Waiting above Error above Idle ...);
 *    4. favorited first within a rank;
 *    5. last activity descending (newest-first, matching the existing web
 *       feel; the TUI's longest-aging-first is deferred until the server
 *       exposes a status-entry timestamp, see #1640);
 *    6. id ascending.
 *  Activity keys use `<` / `>` rather than subtraction because
 *  `workspaceLastActivityMs` can return `Number.NEGATIVE_INFINITY`, and
 *  `-Infinity - -Infinity` is `NaN`, which `Array.prototype.sort` treats as
 *  equal and would silently skip the id tie-break. */
export function compareWorkspacesByAttention(a: Workspace, b: Workspace): number {
  const aTier = workspaceTriageTier(a);
  const bTier = workspaceTriageTier(b);
  if (aTier !== bTier) return aTier - bTier;

  const aUrgent = workspaceIsUrgent(a);
  const bUrgent = workspaceIsUrgent(b);
  if (aUrgent !== bUrgent) return aUrgent ? -1 : 1;

  const aRank = workspaceAttentionRank(a);
  const bRank = workspaceAttentionRank(b);
  if (aRank !== bRank) return aRank - bRank;

  const aFav = workspaceIsFavorited(a);
  const bFav = workspaceIsFavorited(b);
  if (aFav !== bFav) return aFav ? -1 : 1;

  const aMs = workspaceLastActivityMs(a);
  const bMs = workspaceLastActivityMs(b);
  if (aMs < bMs) return 1;
  if (aMs > bMs) return -1;
  return a.id.localeCompare(b.id);
}

/** Comparator for the axes that compute their own row order (the user-group
 *  and nested subgroup axes), which have no manual drag order. `manual`
 *  falls back to last-activity there, preserving the pre-#1640 behavior
 *  where those axes always sorted by last activity; `lastActivity` and
 *  `attention` are honored when selected. The repo axis does NOT use this
 *  (it special-cases `manual` with the persisted workspace rank). */
export function compareWorkspacesForComputedSortMode(mode: SidebarSortMode): (a: Workspace, b: Workspace) => number {
  if (mode === "attention") return compareWorkspacesByAttention;
  return compareWorkspacesByLastActivityDesc;
}

/** Best (lowest) attention rank across a repo group's workspaces, so a
 *  group holding a Waiting session floats above one whose best session is
 *  merely Running. */
export function repoGroupAttentionRank(workspaces: readonly Workspace[]): number {
  let best = ATTENTION_SINK_RANK;
  for (const ws of workspaces) {
    const rank = workspaceAttentionRank(ws);
    if (rank < best) best = rank;
  }
  return best;
}

/** True when any workspace in the group carries the urgent hook flag. */
export function repoGroupIsUrgent(workspaces: readonly Workspace[]): boolean {
  return workspaces.some(workspaceIsUrgent);
}

/** True when any workspace in the group is favorited. */
export function repoGroupIsFavorited(workspaces: readonly Workspace[]): boolean {
  return workspaces.some(workspaceIsFavorited);
}
