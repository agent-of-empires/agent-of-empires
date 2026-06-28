/* eslint-disable react-refresh/only-export-components */
import {
  createContext,
  memo,
  useCallback,
  useContext,
  useEffect,
  useLayoutEffect,
  useMemo,
  useReducer,
  useRef,
  useState,
  type MutableRefObject,
} from "react";
import { createPortal } from "react-dom";
import {
  Archive,
  ArrowLeftRight,
  CircleDot,
  CircleStop,
  Folder,
  Hourglass,
  Layers,
  ListFilter,
  Moon,
  Pencil,
  Pin,
  Play,
  Plus,
  RotateCcw,
  Sparkles,
  Trash2,
  X,
} from "lucide-react";
import { usePluginUiEntries } from "../lib/pluginUiContext";
import {
  type ActiveFacet,
  pluginFacetSpecs,
  pluginSortSpecs,
  sessionMatchesFacets,
  toneTextClass,
} from "../lib/pluginUi";
import {
  DndContext,
  MouseSensor,
  TouchSensor,
  useSensor,
  useSensors,
  closestCenter,
  type CollisionDetection,
  type DragEndEvent,
} from "@dnd-kit/core";
import { SortableContext, useSortable, verticalListSortingStrategy, arrayMove } from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";
import type { ProjectInfo, RepoGroup, SessionResponse, SessionStatus, Workspace } from "../lib/types";
import { ProjectsSection } from "./ProjectsSection";
import type { SidebarAxis } from "../lib/sidebarAxis";
import {
  archivableWorkspaces,
  nestedSidebarGroupShouldRender,
  sidebarGroupHasLiveWorkspace,
  sidebarGroupShouldRender,
  type NestedSidebarGroup,
  type SidebarGroup,
  type SidebarWorkspaceView,
} from "../lib/sidebarGroups";
import { safeGetItem, safeSetItem } from "../lib/safeStorage";
import { menuBus, closeOtherContextMenus } from "../lib/menuBus";
import { REPO_COLOR_OPTIONS, repoColorStyle, repoSwatchStyle, type RepoAppearanceUpdate } from "../lib/repoAppearance";
import { STATUS_DOT_CLASS, getStatusTextClass, isSessionActive } from "../lib/session";
import { useIdleDecayWindowMs } from "../lib/idleDecay";
import { exceedsTouchSlop } from "../lib/longPress";
import { useUnreadIndicatorEnabled } from "../lib/unreadIndicator";
import { TOUR_ANCHORS, tourAnchor } from "../lib/tourSteps";
import {
  renameSession,
  setSessionNotifications,
  setWorktreeName,
  smartRenameSession,
  updateSessionGroup,
} from "../lib/api";
import { useServerDown, OFFLINE_TITLE } from "../lib/connectionState";
import { requestOpenSession } from "../lib/sessionRoute";
import { requestSwitchAgent } from "../lib/switchAgentTrigger";
import { useClampedMenuPosition } from "../lib/menuPosition";
import { useHasDraftForSessions } from "../lib/acpDrafts";
import { useQueuedCountForSessions } from "../hooks/useAcpQueueCount";
import { useRateLimitedForSessions } from "../hooks/useAcpRateLimit";
import {
  triageMenuShape,
  triageStateOf,
  workspaceIsPinned,
  workspaceIsSunk,
  workspaceIsTrashed,
  type SidebarSortMode,
} from "../lib/sidebarSort";
import {
  effectiveArchivedOf,
  effectivePinnedOf,
  effectiveSnoozedUntilOf,
  effectiveUnreadOf,
  type OptimisticTriage,
} from "../lib/sidebarOptimistic";
import { useSidebarTriage } from "../hooks/useSidebarTriage";
import { EMPTY_SELECTION, classifyClick, selectionReducer } from "../lib/sidebarSelection";
import { bucketSelectionForBulk, summarizeBulkResults, type BulkTriageBuckets } from "../lib/sidebarBulk";
import { reportError, reportInfo } from "../lib/toastBus";
// Re-exported for back-compat with `SnoozeModal.test.tsx`, which imports it
// from this module; the definition now lives in `sidebarOptimistic.ts`.
export { makeOptimisticSnoozedUntil } from "../lib/sidebarOptimistic";
import { StatusGlyph } from "./StatusGlyph";
import { OwnerAvatar } from "./OwnerAvatar";
import { SessionGroupModal } from "./SessionGroupModal";
import { SidebarSortPicker } from "./SidebarSortPicker";
import { Tooltip } from "./Tooltip";
import { PluginRowLine } from "./plugin/PluginSlots";

const SIDEBAR_WIDTH_KEY = "aoe-sidebar-width";
const SUNK_EXPANDED_KEY = "aoe-sidebar-sunk-expanded";
const DEFAULT_WIDTH = 280;
const MIN_WIDTH = 200;
const MAX_WIDTH = 480;

/** Snooze duration presets surfaced by the sidebar context menu. Order
 *  and values mirror the TUI dialog presets at
 *  `src/tui/dialogs/snooze_duration.rs`, so the two surfaces describe
 *  the same set of choices. The TUI extends past these via a manual
 *  numeric entry; the web sidebar omits that path in v1 (the menu
 *  stays flat). See #1581. */
export const SNOOZE_PRESETS: readonly { label: string; minutes: number }[] = [
  { label: "1 hour", minutes: 60 },
  { label: "2 hours", minutes: 120 },
  { label: "3 hours", minutes: 180 },
  { label: "4 hours", minutes: 240 },
  { label: "5 hours", minutes: 300 },
  { label: "6 hours", minutes: 360 },
  { label: "1 day", minutes: 1440 },
  { label: "1 week", minutes: 10080 },
];

/** Whether a row's right-click context menu acts on just that row or on the
 *  whole multi-selection. `prepareScope` decides at right-click time. See
 *  #2312. */
type RowContextScope = { kind: "single" } | { kind: "bulk"; count: number; buckets: BulkTriageBuckets };

/** Stable bridge passed to every SessionRow so the in-row context menu can
 *  drive bulk triage over the selection without prop-drilling the selection
 *  arrays (which would defeat React.memo). The parent keeps the live selection
 *  in a ref; this object's identity never changes. See #2312. */
export interface RowBulkApi {
  prepareScope: (ws: Workspace) => RowContextScope;
  pin: (workspaces: Workspace[], pinned: boolean) => void;
  archive: (workspaces: Workspace[], archived: boolean) => void;
  snooze: (workspaces: Workspace[], minutes: number | null) => void;
}

const CTX_ITEM =
  "w-full text-left px-3 py-2 md:py-2 max-md:py-3 text-sm text-text-secondary hover:bg-surface-700/50 cursor-pointer transition-colors flex items-center gap-2";

/** Triage actions for the right-click menu when more than one row is selected.
 *  Reuses the same eligibility buckets as the old bulk bar so a mixed
 *  selection shows count-labelled actions ("Pin 3" / "Unpin 2"). Single-only
 *  actions (rename, switch agent, stop/start) are intentionally absent in this
 *  branch. See #2312. */
function BulkTriageMenuItems({
  count,
  buckets,
  api,
  onDone,
}: {
  count: number;
  buckets: BulkTriageBuckets;
  api: RowBulkApi;
  onDone: () => void;
}) {
  const act = (run: () => void) => {
    onDone();
    run();
  };
  return (
    <>
      <div className="px-3 py-1 text-[11px] font-mono uppercase tracking-widest text-text-muted">{count} selected</div>
      {buckets.pinnable.length > 0 && (
        <button
          data-testid="sidebar-context-menu-bulk-pin"
          className={CTX_ITEM}
          onClick={() => act(() => api.pin(buckets.pinnable, true))}
        >
          <Pin className="h-3.5 w-3.5 shrink-0 -rotate-45" />
          Pin {buckets.pinnable.length}
        </button>
      )}
      {buckets.unpinnable.length > 0 && (
        <button
          data-testid="sidebar-context-menu-bulk-unpin"
          className={CTX_ITEM}
          onClick={() => act(() => api.pin(buckets.unpinnable, false))}
        >
          <Pin className="h-3.5 w-3.5 shrink-0 -rotate-45" />
          Unpin {buckets.unpinnable.length}
        </button>
      )}
      {buckets.archivable.length > 0 && (
        <button
          data-testid="sidebar-context-menu-bulk-archive"
          className={CTX_ITEM}
          onClick={() => act(() => api.archive(buckets.archivable, true))}
        >
          <Archive className="h-3.5 w-3.5 shrink-0" />
          Archive {buckets.archivable.length}
        </button>
      )}
      {buckets.unarchivable.length > 0 && (
        <button
          data-testid="sidebar-context-menu-bulk-unarchive"
          className={CTX_ITEM}
          onClick={() => act(() => api.archive(buckets.unarchivable, false))}
        >
          <Archive className="h-3.5 w-3.5 shrink-0" />
          Unarchive {buckets.unarchivable.length}
        </button>
      )}
      {buckets.snoozable.length > 0 && (
        <>
          <div className="px-3 py-1 text-[11px] font-mono uppercase tracking-widest text-text-muted">
            Snooze {buckets.snoozable.length}
          </div>
          {SNOOZE_PRESETS.map((preset) => (
            <button
              key={preset.minutes}
              data-testid="sidebar-context-menu-bulk-snooze"
              className={`${CTX_ITEM} pl-6`}
              onClick={() => act(() => api.snooze(buckets.snoozable, preset.minutes))}
            >
              <Moon className="h-3.5 w-3.5 shrink-0" />
              {preset.label}
            </button>
          ))}
        </>
      )}
      {buckets.unsnoozable.length > 0 && (
        <button
          data-testid="sidebar-context-menu-bulk-unsnooze"
          className={CTX_ITEM}
          onClick={() => act(() => api.snooze(buckets.unsnoozable, null))}
        >
          <Moon className="h-3.5 w-3.5 shrink-0" />
          Unsnooze {buckets.unsnoozable.length}
        </button>
      )}
    </>
  );
}

// Group headers and session rows are both sortable inside the one
// sidebar DndContext, so a header drag must not collide with a session
// droppable (or vice versa). dnd-kit registers every sortable in a
// single droppable registry per context; without filtering, closestCenter
// would happily report a workspace row as the drop target while dragging a
// group. Restrict candidates to droppables whose `data.type` matches the
// dragged item, then defer to closestCenter within that subset. See #1644.
const typedClosestCenter: CollisionDetection = (args) => {
  const activeType = args.active.data.current?.type;
  return closestCenter({
    ...args,
    droppableContainers: args.droppableContainers.filter((container) => container.data.current?.type === activeType),
  });
};

interface Props {
  groups: SidebarGroup[];
  // The nested `repo+group` axis model (#1720). Only consumed when
  // `axis === "repo+group"`; the flat `groups` list drives the other axes.
  nestedGroups: NestedSidebarGroup[];
  // Fully-trashed workspaces, computed by the parent from the authoritative
  // unsliced workspace list (`workspaces.filter(workspaceIsTrashed)`), NOT from
  // the per-`group_path` slice views in `groups`/`nestedGroups`. Trash
  // membership and Restore scope are a whole-workspace concern: a workspace
  // split across groups is in Trash only when every one of its sessions is
  // trashed, and Restore must cover all of them. See #2533. Optional with an
  // empty default so callers that never trash (and render-only tests) need not
  // thread it.
  trashedWorkspaces?: Workspace[];
  onToggleSubgroup: (repoId: string, groupPath: string) => void;
  onReorderWorkspaces: (newOrder: string[]) => void;
  onReorderGroups: (orderedGroupIds: string[]) => void;
  activeId: string | null;
  open: boolean;
  onToggle: () => void;
  onSelect: (workspaceId: string) => void;
  onToggleGroup: (groupId: string) => void;
  onUpdateRepoAppearance: (repoId: string, update: RepoAppearanceUpdate) => void;
  onNew: () => void;
  onCreateSession: (repoPath: string) => void;
  /** Pin a repo (register it) so it persists with zero sessions. See #2047. */
  onPinProject?: (repoPath: string) => void;
  /** Unpin a repo: remove every registry entry for its path. See #2047. */
  onUnpinProject?: (group: SidebarGroup) => void;
  /** Saved projects with no live session, for the dedicated Projects section
   *  (#2212). Pinned projects render above as headers, not here. */
  savedProjects: RepoGroup[];
  /** Open the add-project form (directory browser + scope + base branch). */
  onAddProject: () => void;
  /** Open the edit form for one registration (default base branch). */
  onEditProject: (project: ProjectInfo) => void;
  /** Remove a project: delete every registration for its path. */
  onRemoveProject: (group: RepoGroup) => void;
  onSettings: () => void;
  onDeleteSession?: (workspaceId: string) => void;
  /** Restore a trashed workspace from the Trash section: receives every
   *  session id in the workspace, since a workspace only lands in Trash when
   *  all of its sessions are trashed (#2489). */
  onRestoreSession?: (sessionIds: string[]) => void;
  onStopSession?: (workspaceId: string) => void;
  onStartSession?: (workspaceId: string) => void;
  readOnly?: boolean;
  sortMode: SidebarSortMode;
  onSortModeChange: (mode: SidebarSortMode) => void;
  pluginSortRef: { pluginId: string; entryId: string } | null;
  onPluginSortChange: (ref: { pluginId: string; entryId: string }) => void;
  axis: SidebarAxis;
  onAxisChange: (axis: SidebarAxis) => void;
}

function bestSession(
  ws: Workspace,
  idleDecayWindowMs: number,
): {
  status: SessionStatus;
  createdAt: string | null;
  idleEnteredAt: string | null;
} {
  const running = ws.sessions.find((s) => isSessionActive(s, idleDecayWindowMs));
  if (running)
    return {
      status: running.status,
      createdAt: running.created_at,
      idleEnteredAt: running.idle_entered_at ?? null,
    };
  const error = ws.sessions.find((s) => s.status === "Error");
  if (error)
    return {
      status: "Error",
      createdAt: error.created_at,
      idleEnteredAt: null,
    };
  const first = ws.sessions[0];
  return {
    status: first?.status ?? "Unknown",
    createdAt: first?.created_at ?? null,
    idleEnteredAt: first?.idle_entered_at ?? null,
  };
}

/** Derive which of the three context-menu presets best describes a
 *  session's current per-event notification overrides. If the three
 *  overrides aren't all the same value, the session is in a "custom"
 *  mixed state, which the context menu renders as "Default" too
 *  (selecting "Default" then resets it cleanly). */
type NotifyPreset = "off" | "default" | "all";
function detectNotifyPreset(
  waiting: boolean | null | undefined,
  idle: boolean | null | undefined,
  error: boolean | null | undefined,
): NotifyPreset {
  if (waiting === false && idle === false && error === false) return "off";
  if (waiting === true && idle === true && error === true) return "all";
  return "default";
}

function loadSavedWidth(): number {
  const saved = safeGetItem(SIDEBAR_WIDTH_KEY);
  if (saved) {
    const w = parseInt(saved, 10);
    if (w >= MIN_WIDTH && w <= MAX_WIDTH) return w;
  }
  return DEFAULT_WIDTH;
}

/** Hydrate the single global "Snoozed & archived" footer expanded
 *  state from localStorage. Defaults to collapsed (TUI parity with
 *  the `toggle_archived_section` keybind starting collapsed). An
 *  earlier iteration kept a per-group dict here; any leftover dict
 *  is treated as collapsed. */
function loadSunkExpanded(): boolean {
  const raw = safeGetItem(SUNK_EXPANDED_KEY);
  if (raw === "true") return true;
  return false;
}

/** One-line sidebar affordance showing plan progress for structured view
 *  sessions that have emitted a Plan. Quiet by default (renders only
 *  when `summary.total > 0`); mirrors the top-of-structured view PlanStrip's
 *  visual language so the sidebar and main view stay consistent. See
 *  #1061. */
function PlanProgressMini({ summary }: { summary: NonNullable<SessionResponse["plan_summary"]> }) {
  const pct = summary.total > 0 ? Math.min(100, Math.round((summary.completed / summary.total) * 100)) : 0;
  const title = summary.current_step_title ?? "plan in progress";
  const ariaLabel = summary.current_step_title
    ? `Plan progress: ${summary.completed} of ${summary.total} steps; current step ${summary.current_step_title}`
    : `Plan progress: ${summary.completed} of ${summary.total} steps`;
  return (
    <div className="mt-1 flex items-center gap-2" title={title}>
      <div
        role="progressbar"
        aria-valuenow={summary.completed}
        aria-valuemin={0}
        aria-valuemax={summary.total}
        aria-label={ariaLabel}
        className="h-1 flex-1 rounded-full bg-surface-800 overflow-hidden"
      >
        <div className="h-full bg-brand-400 transition-all" style={{ width: `${pct}%` }} />
      </div>
      <span className="text-[10px] font-mono tabular-nums text-text-dim shrink-0">
        {summary.completed}/{summary.total}
      </span>
    </div>
  );
}

/** Sidebar chip that ticks down to a `ScheduleWakeup` fire time. Self-
 *  destructs when the wake passes (sets local count to 0 and renders
 *  "waking…"; the next sessions-endpoint refresh removes the underlying
 *  field). 1Hz timer is local to the row so we don't fan out a global
 *  tick across the sidebar. See #1091. */
function WakeupCountdown({ wakeAt, reason }: { wakeAt: string; reason: string | null | undefined }) {
  const targetMs = Date.parse(wakeAt);
  const [now, setNow] = useState(() => Date.now());
  const elapsed = !Number.isFinite(targetMs) || targetMs <= now;
  useEffect(() => {
    if (elapsed) return;
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, [elapsed]);
  if (!Number.isFinite(targetMs)) return null;
  const remaining = Math.max(0, Math.floor((targetMs - now) / 1000));
  const label = elapsed ? "waking…" : `in ${formatDurationSecondsShort(remaining)}`;
  const title = reason ? `Scheduled wakeup: ${reason}` : "Scheduled wakeup";
  return (
    <span
      title={title}
      aria-label={`Scheduled wakeup ${label}`}
      className="inline-flex shrink-0 items-center gap-0.5 rounded border border-sky-700/40 bg-sky-950/30 px-1 py-0 text-[10px] font-medium text-sky-300"
    >
      <span aria-hidden="true">⏰</span>
      {label}
    </span>
  );
}

/** Sidebar chip shown while the agent has an armed `Monitor` (a
 *  background watch). Unlike the wakeup chip there is no fire time, so this
 *  is a static "monitoring" badge with no countdown. It persists across the
 *  monitor's re-fires (those resume the agent without a user prompt) and
 *  the underlying `monitor_active` field clears on the next user prompt. */
function MonitorBadge({ description }: { description: string | null | undefined }) {
  const title = description ? `Monitoring: ${description}` : "Monitoring a background job";
  return (
    <span
      title={title}
      aria-label={`Monitoring${description ? ` ${description}` : ""}`}
      className="inline-flex shrink-0 items-center gap-0.5 rounded border border-violet-700/40 bg-violet-950/30 px-1 py-0 text-[10px] font-medium text-violet-300"
    >
      <span aria-hidden="true">👁</span>
      monitoring
    </span>
  );
}

/** Compact duration formatting used by the wakeup chip: `45s`, `3m`,
 *  `1h 7m`. Drops sub-minute resolution above one minute since the chip
 *  is read at a glance. */
function formatDurationSecondsShort(seconds: number): string {
  if (seconds < 60) return `${seconds}s`;
  const m = Math.floor(seconds / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  const remM = m % 60;
  return remM === 0 ? `${h}h` : `${h}h ${remM}m`;
}

/** Compact "time remaining" label for the snooze chip computed once at
 *  render time (no per-second timer, by design: snooze rows poll the
 *  sessions API at the existing cadence and the static label is more
 *  battery-friendly than a 1s ticker on phones, see #1581 design
 *  discussion). Bucket sizes:
 *   - < 1 minute : "<1m"
 *   - < 1 hour   : "Nm"
 *   - < 1 day    : "Nh" (rounded down)
 *   - else       : "Nd" (rounded down)
 *  Past timestamps return "soon" since the wake-up has expired but the
 *  next poll has not yet cleared the row. */
export function formatSnoozeRemainingShort(snoozedUntilIso: string): string {
  const target = Date.parse(snoozedUntilIso);
  if (!Number.isFinite(target)) return "snoozed";
  const remainingMs = target - Date.now();
  if (remainingMs <= 0) return "soon";
  const minutes = Math.floor(remainingMs / 60_000);
  if (minutes < 1) return "<1m";
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h`;
  return `${Math.floor(hours / 24)}d`;
}

/** Trash control in the sidebar footer, next to Settings (#2512). Trash is a
 *  terminal, rarely-touched state, so it lives as an icon with an upward
 *  popover rather than a full scrolling section. Outside click and Escape
 *  close it, mirroring SidebarSortPicker. Only rendered when something is
 *  trashed. */
function TrashMenu({
  trashedWorkspaces,
  readOnly,
  onOpen,
  onRestore,
  onDelete,
}: {
  trashedWorkspaces: Workspace[];
  readOnly?: boolean;
  onOpen: (workspaceId: string, e: { metaKey: boolean; ctrlKey: boolean; shiftKey: boolean }) => void;
  onRestore: (sessionIds: string[]) => void;
  onDelete: (workspaceId: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onDocClick = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    const onKeydown = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    document.addEventListener("mousedown", onDocClick);
    document.addEventListener("keydown", onKeydown);
    return () => {
      document.removeEventListener("mousedown", onDocClick);
      document.removeEventListener("keydown", onKeydown);
    };
  }, [open]);

  const count = trashedWorkspaces.length;

  return (
    <div ref={ref} className="relative">
      <button
        onClick={() => setOpen((o) => !o)}
        aria-haspopup="menu"
        aria-expanded={open}
        data-testid="sidebar-trash-toggle"
        className="w-8 h-8 flex items-center justify-center text-text-secondary hover:text-text-primary hover:bg-surface-800/50 cursor-pointer rounded-md transition-colors"
        title={`Trash (${count})`}
        aria-label={`Trash (${count})`}
      >
        <Trash2 className="h-4 w-4" />
      </button>
      {open && (
        <div
          role="menu"
          data-testid="sidebar-trash-menu"
          className="absolute bottom-full mb-1 left-0 min-w-[220px] max-h-[50vh] overflow-y-auto bg-surface-800 border border-surface-700/50 rounded-md shadow-xl py-1 z-50 animate-fade-in"
        >
          <div className="px-3 py-1.5 text-[11px] font-mono uppercase tracking-widest text-text-muted">
            Trash ({count})
          </div>
          {trashedWorkspaces.map((ws) => (
            <div
              key={ws.id}
              data-testid="sidebar-trash-row"
              className="flex items-center gap-2 px-3 py-1.5 text-[13px] text-text-secondary"
            >
              <button
                type="button"
                onClick={(e) => onOpen(ws.id, e)}
                title={ws.displayName}
                data-testid="sidebar-trash-open"
                className="flex-1 truncate text-left hover:text-text-primary cursor-pointer"
              >
                {ws.displayName}
              </button>
              {!readOnly && (
                <>
                  <button
                    onClick={() => {
                      const ids = ws.sessions.map((s) => s.id);
                      if (ids.length > 0) onRestore(ids);
                    }}
                    data-testid="sidebar-trash-restore"
                    title="Restore"
                    aria-label="Restore"
                    className="text-accent-500 hover:text-accent-600 cursor-pointer"
                  >
                    <RotateCcw className="h-3.5 w-3.5" />
                  </button>
                  <button
                    onClick={() => onDelete(ws.id)}
                    data-testid="sidebar-trash-purge"
                    title="Delete permanently"
                    aria-label="Delete permanently"
                    className="text-status-error/80 hover:text-status-error cursor-pointer"
                  >
                    <X className="h-3.5 w-3.5" />
                  </button>
                </>
              )}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

// Wraps a SessionRow with @dnd-kit sortable plumbing. The row itself
// is the drag handle: a short tap/click navigates as before, but a
// press-and-hold (sensor delay) lifts the row so the user can reorder.
// See #1169.

// "Drag just ended" timestamp shared by every sortable row and the
// document-level click suppressor. Lives as a ref on the sidebar so
// HMR resets don't leave it in a weird state and so siblings can't
// see each other through a module-scoped global. The document
// listener checks `ref.current` on every click; rows write to it
// while dragging and on release.
export const DragSuppressContext = createContext<MutableRefObject<number> | null>(null);
function useDragSuppressRef(): MutableRefObject<number> {
  const ref = useContext(DragSuppressContext);
  if (!ref) {
    throw new Error("DragSuppressContext used outside provider");
  }
  return ref;
}

function useSuppressClickAfterDrag(ref: MutableRefObject<number>) {
  useEffect(() => {
    const handler = (e: MouseEvent) => {
      if (Date.now() < ref.current) {
        e.preventDefault();
        e.stopPropagation();
        e.stopImmediatePropagation();
      }
    };
    // The click Chromium dispatches after a drag-release can bypass
    // React's event delegation and land on the inner row without firing
    // any wrapping capture handler. A document-level capture listener
    // catches it before row activation kicks in.
    document.addEventListener("click", handler, true);
    return () => document.removeEventListener("click", handler, true);
  }, [ref]);
}

function SortableSessionRow({
  rowKey,
  ...props
}: {
  rowKey?: string;
  workspace: Workspace;
  isActive: boolean;
  isSelected: boolean;
  onActivate: (e: { metaKey: boolean; ctrlKey: boolean; shiftKey: boolean }) => void;
  onDelete?: (workspaceId: string) => void;
  onStop?: (workspaceId: string) => void;
  onStart?: (workspaceId: string) => void;
  onCreateSession?: (repoPath: string) => void;
  readOnly?: boolean;
  dragDisabled?: boolean;
  optimistic: OptimisticTriage;
  onPinToggle: (ws: Workspace, pinned: boolean) => void;
  onArchiveToggle: (ws: Workspace, archived: boolean) => void;
  onSnooze: (ws: Workspace, minutes: number | null) => void;
  onUnreadToggle: (ws: Workspace, markUnread: boolean) => void;
  bulkApi: RowBulkApi;
}) {
  const dragSuppressRef = useDragSuppressRef();
  // `disabled` no-ops the sensor listeners. `readOnly` covers viewers
  // who can't write, `dragDisabled` covers modes where the visible order
  // is computed (e.g. last-activity sort), so a drag would have no
  // meaning. Skipping the sortable wiring entirely would also drop the
  // click suppressor; that's harmless in either case since nothing else
  // triggers a drag.
  const dragOff = !!props.readOnly || !!props.dragDisabled;
  const { listeners, setNodeRef, transform, transition, isDragging } = useSortable({
    id: rowKey ?? props.workspace.id,
    disabled: dragOff,
    data: { type: "workspace" },
  });
  useEffect(() => {
    if (isDragging) {
      // Keep extending the window while dragging so a slow drag still
      // suppresses the trailing click on release.
      dragSuppressRef.current = Date.now() + 1000;
    } else if (dragSuppressRef.current > Date.now()) {
      // Drag just ended; the click is on its way. Hold the suppression
      // for ~250ms after release (enough to swallow the synthetic click,
      // short enough that a real tap right after still navigates).
      dragSuppressRef.current = Date.now() + 250;
    }
  }, [isDragging, dragSuppressRef]);
  const style = {
    transform: CSS.Transform.toString(transform),
    transition,
    touchAction: "manipulation",
    // Lift the active row above its siblings so the ring/shadow aren't
    // clipped by the next row in the list.
    zIndex: isDragging ? 10 : "auto",
    position: "relative",
  } as const;
  return (
    // We intentionally spread only `listeners` (pointer-down etc.) and
    // not dnd-kit's `attributes`. The latter inject role="button" and a
    // tabIndex which would duplicate the inner Link as a focusable,
    // button-styled affordance for assistive tech. Keyboard drag isn't
    // supported here, so the omitted attributes don't cost anything.
    <div
      ref={setNodeRef}
      style={style}
      {...(dragOff ? {} : listeners)}
      aria-roledescription={dragOff ? undefined : "Press and hold to reorder"}
      // While dragging, the row gets an amber ring (matches the active
      // session accent) and a soft shadow so it reads as elevated above
      // the rest of the list. ring-inset keeps the highlight tight to
      // the row rectangle; the transition runs in both directions so
      // the lift and the drop both feel intentional. The inner
      // SessionRow keeps its own background, so we only style the
      // outline here.
      className={"transition-shadow duration-150 " + (isDragging ? "ring-2 ring-inset ring-brand-500 shadow-lg" : "")}
    >
      <SessionRow {...props} indented />
    </div>
  );
}

// Props handed to the grip element so RepoGroupHeader can wire a dedicated
// drag handle. The whole header is NOT draggable: it already owns an
// expand/collapse click, a context menu, a rename input, and a new-session
// button, so a header-wide drag would smother them. The grip is the sole
// activator. See #1644.
type DragHandleProps = {
  setActivatorNodeRef: (el: HTMLElement | null) => void;
  attributes: ReturnType<typeof useSortable>["attributes"];
  listeners: ReturnType<typeof useSortable>["listeners"];
  isDragging: boolean;
};

// Sortable wrapper around an entire repo-group block (header + its rows),
// so the group moves as a unit. Only real repo groups are wrapped;
// synthetic Multi-repo/Scratch groups render plainly and stay pinned.
function SortableRepoGroup({
  groupId,
  disabled,
  children,
}: {
  groupId: string;
  disabled: boolean;
  children: (handle: DragHandleProps) => React.ReactNode;
}) {
  const { attributes, listeners, setNodeRef, setActivatorNodeRef, transform, transition, isDragging } = useSortable({
    id: groupId,
    disabled,
    data: { type: "group" },
  });
  const style = {
    transform: CSS.Transform.toString(transform),
    transition,
    position: "relative",
    zIndex: isDragging ? 20 : "auto",
  } as const;
  return (
    <div
      ref={setNodeRef}
      style={style}
      className={"transition-shadow duration-150 " + (isDragging ? "ring-2 ring-inset ring-brand-500 shadow-lg" : "")}
    >
      {children({ setActivatorNodeRef, attributes, listeners, isDragging })}
    </div>
  );
}

export const SessionRow = memo(function SessionRow({
  workspace,
  isActive,
  isSelected,
  onActivate,
  onDelete,
  onStop,
  onStart,
  onCreateSession,
  readOnly,
  indented,
  optimistic,
  onPinToggle,
  onArchiveToggle,
  onSnooze,
  onUnreadToggle,
  bulkApi,
}: {
  workspace: Workspace;
  isActive: boolean;
  // Whether this row is part of the sidebar multi-select. See #1724.
  isSelected: boolean;
  // Row click. The parent interprets the modifier keys (plain navigates,
  // Cmd/Ctrl toggles, Shift ranges), so the row forwards the event up rather
  // than navigating directly. See #1724.
  onActivate: (e: { metaKey: boolean; ctrlKey: boolean; shiftKey: boolean }) => void;
  onDelete?: (workspaceId: string) => void;
  onStop?: (workspaceId: string) => void;
  onStart?: (workspaceId: string) => void;
  // Open the session wizard prefilled from this row's project (path, agent,
  // and the latest session's options), mirroring the per-project "+" button.
  onCreateSession?: (repoPath: string) => void;
  readOnly?: boolean;
  indented?: boolean;
  // Optimistic triage overlay for this row plus the parent-owned mutation
  // callbacks. Triage state used to live in the row as three `useState`s;
  // it now lives in the sidebar so bulk actions can drive many rows from
  // one place. See #1724.
  optimistic: OptimisticTriage;
  onPinToggle: (ws: Workspace, pinned: boolean) => void;
  onArchiveToggle: (ws: Workspace, archived: boolean) => void;
  onSnooze: (ws: Workspace, minutes: number | null) => void;
  onUnreadToggle: (ws: Workspace, markUnread: boolean) => void;
  // Stable bridge for bulk triage from the right-click menu. See #2312.
  bulkApi: RowBulkApi;
}) {
  const idleDecayWindowMs = useIdleDecayWindowMs();
  const unreadIndicatorEnabled = useUnreadIndicatorEnabled();
  const { status: sessionStatus, createdAt, idleEnteredAt } = bestSession(workspace, idleDecayWindowMs);
  const textClass = getStatusTextClass(
    {
      status: sessionStatus,
      idle_entered_at: idleEnteredAt,
    },
    idleDecayWindowMs,
  );
  const firstSession = workspace.sessions[0];
  // Repo path used to prefill a "New Session" launched from this row, matching
  // the per-project "+" button (handleCreateSession keys off this same path).
  const newSessionRepoPath = firstSession?.main_repo_path || firstSession?.project_path || null;
  // The structured view session backing this row, if any. Drives the "Switch
  // agent" context-menu item, which only makes sense for an ACP structured view
  // session (tmux rows have no agent to hand off). Multi-session rows are
  // rare; pick the first structured view session in the workspace.
  const acpSession = workspace.sessions.find((s) => s.view === "structured");
  const runningSession = workspace.sessions.find((s) => isSessionActive(s, idleDecayWindowMs));
  const singleSession = workspace.sessions.length === 1;
  const sessionTitle = firstSession?.title.trim() ?? "";
  const branchLabel = workspace.branch ?? null;
  const baseBranch = firstSession?.base_branch ?? null;
  const label = singleSession ? sessionTitle || branchLabel || "default" : branchLabel || sessionTitle || "default";
  const subtitle = singleSession && sessionTitle && branchLabel && sessionTitle !== branchLabel ? branchLabel : null;
  const subtitleTitle = subtitle && baseBranch ? `${subtitle} (based on ${baseBranch})` : subtitle;
  // Workspace renders as favorited when any of its sessions are
  // favorited. Mirrors the TUI's within-tier pin: the star promotes the
  // row visually so the user can find their starred work fast. Toggled
  // via TUI `f`/`F` or `aoe session favorite|unfavorite`.
  const isFavorited = workspace.sessions.some((s) => s.favorited);
  // Web-only triage signals. `pinned` floats the workspace to the top
  // of every sort mode; `archived` and `snoozedUntil` mark the row as
  // sunk (the parent splits sunk workspaces into a separate collapsible
  // section). Aggregators mirror the matching helpers in
  // `lib/sidebarSort.ts` to keep render and sort in sync. See #1581.
  const isPinned = workspace.sessions.some((s) => s.pinned_at != null);
  const isArchived = workspace.sessions.some((s) => s.archived_at != null);
  const snoozedUntil = workspace.sessions.find((s) => s.snoozed_until)?.snoozed_until ?? null;
  // Unread marker for the row, server value plus optimistic overlay. The
  // active (open) row is always suppressed: opening reads it and the App
  // clears it a beat later, so hiding it avoids a flash in the poll window.
  const serverUnread = workspace.sessions.some((s) => s.unread === true);
  const effectiveUnread = effectiveUnreadOf(optimistic, serverUnread);
  const isUnread = unreadIndicatorEnabled && effectiveUnread && !isActive;
  // Like the TUI, the unread marker *replaces* the resting status glyph with a
  // solid dot (rather than sitting beside the title). Only for resting states:
  // a live spinner (Running/Waiting/...) stays, since live status outranks it
  // and the auto-mark only ever lands on Idle anyway.
  const showUnreadGlyph = isUnread && (sessionStatus === "Idle" || sessionStatus === "Unknown");
  const sessionId = firstSession?.id;
  const navigationSessionId = runningSession?.id ?? firstSession?.id ?? null;
  const sessionPath = navigationSessionId ? `/session/${encodeURIComponent(navigationSessionId)}` : "/";
  const isDeleting = sessionStatus === "Deleting";
  const notifyPreset = detectNotifyPreset(
    firstSession?.notify_on_waiting,
    firstSession?.notify_on_idle,
    firstSession?.notify_on_error,
  );
  // Surface an unsent acp-composer draft on this workspace's row.
  // Drafts live in localStorage under `acp:draft:<session_id>`; we
  // check every session id in the workspace so multi-session rows
  // (rare today) still light up if any of them has pending text.
  const sessionIds = useMemo(() => workspace.sessions.map((s) => s.id), [workspace.sessions]);
  const hasDraft = useHasDraftForSessions(sessionIds);
  // Queued structured view follow-up prompts waiting to fire when the current
  // turn ends. Summed across the workspace's sessions, mirroring how
  // `hasDraft` ORs the same set. Lets a user juggling sessions see at a
  // glance which rows have prompts pending without opening the structured view.
  const queuedCount = useQueuedCountForSessions(sessionIds);
  // Rate-limit park visibility parity with the structured view notice (#1715).
  // The server maps rate-limited stops to Idle, so the status glyph can't
  // distinguish a parked session from a normal idle one; surface it here
  // from the same acp-state mirror the queued badge reads.
  const rateLimited = useRateLimitedForSessions(sessionIds);
  const rateLimitResetLabel = useMemo(() => {
    if (!rateLimited?.resetsAt) return null;
    const reset = new Date(rateLimited.resetsAt);
    return Number.isNaN(reset.getTime()) ? null : reset.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  }, [rateLimited]);
  const rateLimitTitle = rateLimited
    ? `Rate-limited${rateLimited.count > 1 ? ` (${rateLimited.count} sessions)` : ""}${rateLimitResetLabel ? `; resets at ${rateLimitResetLabel}` : ""}`
    : "";

  const setNotifyPreset = async (preset: NotifyPreset) => {
    setContextMenu(null);
    if (!sessionId || preset === notifyPreset) return;
    await setSessionNotifications(sessionId, preset);
  };

  // Triage actions (pin / archive / snooze). The optimistic overlay and the
  // network calls live in the sidebar parent now (keyed by workspace id) so
  // a bulk action can drive many rows at once; the row just closes its own
  // menu/modal and delegates the mutation. See #1724. The optimistic snap
  // still clears itself once the next sessions-poll reflects the same value,
  // so a successful round-trip is invisible to the user (just feels fast).
  // Snooze duration picker. Lives in its own portal-rendered modal,
  // independent of the context menu's lifecycle so the parent-menu
  // dismissal listener cannot close the picker out from under us.
  const [snoozeModalOpen, setSnoozeModalOpen] = useState(false);
  // Edit-workdir-name picker, also in its own portal-rendered modal so the
  // context-menu dismissal listener does not close it. See #1723.
  const [workdirModalOpen, setWorkdirModalOpen] = useState(false);

  const togglePin = () => {
    setContextMenu(null);
    onPinToggle(workspace, !effectivePinnedOf(optimistic, isPinned));
  };

  const toggleArchive = () => {
    setContextMenu(null);
    onArchiveToggle(workspace, !effectiveArchivedOf(optimistic, isArchived));
  };

  const applySnooze = (minutes: number | null) => {
    setContextMenu(null);
    setSnoozeModalOpen(false);
    onSnooze(workspace, minutes);
  };

  const toggleUnread = () => {
    setContextMenu(null);
    // Mark unread when currently read, mark read when currently unread,
    // mirroring the TUI `u` toggle.
    onUnreadToggle(workspace, !effectiveUnread);
  };

  // Close the context menu first, then open the modal in the next
  // tick so the menu's document-click dismiss listener does not race
  // with the modal's mount.
  const openSnoozeModal = () => {
    setContextMenu(null);
    setSnoozeModalOpen(true);
  };

  // Open the switch-agent dialog for this row's structured view session. The
  // dialog lives in that session's Composer (it prefills the composer on
  // confirm), so we navigate to the session first, then request the open.
  // When the row is already the active session the navigation is a no-op
  // and the dispatched event opens the dialog immediately; otherwise the
  // Composer consumes the pending latch once it mounts.
  const handleSwitchAgent = () => {
    setContextMenu(null);
    if (!acpSession) return;
    requestOpenSession(acpSession.id);
    requestSwitchAgent(acpSession.id);
  };

  // Re-run smart rename ("Auto-name now") for a structured session whose
  // automatic rename never landed. Best-effort and async: a success just means
  // the one-shot was re-triggered; the title updates over the live session
  // stream when it completes. Only offered while the session is still
  // default-named (see the menu gate), so it never overwrites a chosen title.
  const handleAutoNameNow = async () => {
    setContextMenu(null);
    if (!acpSession) return;
    const result = await smartRenameSession(acpSession.id);
    if (!result.ok) {
      reportError(result.message ?? "Could not start auto-name. Please try again.");
    }
  };

  // Effective state for rendering: optimistic overrides win until the
  // sidebar's overlay reconciler drops them once the prop catches up.
  const effectivePinned = effectivePinnedOf(optimistic, isPinned);
  const effectiveArchived = effectiveArchivedOf(optimistic, isArchived);
  const effectiveSnoozedUntil = effectiveSnoozedUntilOf(optimistic, snoozedUntil);
  const effectiveSnoozed = effectiveSnoozedUntil != null;

  const [contextMenu, setContextMenu] = useState<{
    x: number;
    y: number;
    // Single-row menu, or bulk over the active multi-selection. See #2312.
    scope: RowContextScope;
  } | null>(null);
  const [renaming, setRenaming] = useState(false);
  const [renameValue, setRenameValue] = useState(label);
  const renameRef = useRef<HTMLInputElement>(null);
  const sessionGroup = firstSession?.group_path ?? "";
  const [editingGroup, setEditingGroup] = useState(false);
  const longPressTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const longPressFired = useRef(false);
  const touchOpenedAt = useRef(0);
  const touchStart = useRef<{ x: number; y: number } | null>(null);
  const menuRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    return () => {
      if (longPressTimer.current) clearTimeout(longPressTimer.current);
    };
  }, []);

  useClampedMenuPosition(contextMenu, menuRef, setContextMenu);

  useEffect(() => {
    if (!contextMenu) return;
    const close = () => setContextMenu(null);
    const onDocClick = (e: MouseEvent) => {
      // Clicks inside the menu should be handled by item onClick
      // handlers, not by this dismiss listener.
      if (menuRef.current?.contains(e.target as Node)) return;
      // On mobile, lifting the finger after a long-press dispatches a
      // synthetic click even when touchend called preventDefault().
      // Ignore clicks that arrive shortly after a touch-triggered open.
      if (Date.now() - touchOpenedAt.current < 500) return;
      close();
    };
    // Defer so the event that opened the menu finishes bubbling first
    const id = requestAnimationFrame(() => {
      document.addEventListener("click", onDocClick);
      document.addEventListener("contextmenu", close);
    });
    // Listen for the "close" broadcast from any sibling SessionRow
    // that is opening its own menu.
    menuBus.addEventListener("close", close);
    return () => {
      cancelAnimationFrame(id);
      document.removeEventListener("click", onDocClick);
      document.removeEventListener("contextmenu", close);
      menuBus.removeEventListener("close", close);
    };
  }, [contextMenu]);

  const handleContextMenu = (e: React.MouseEvent) => {
    if (isDeleting) return;
    e.preventDefault();
    closeOtherContextMenus();
    setContextMenu({ x: e.clientX, y: e.clientY, scope: bulkApi.prepareScope(workspace) });
  };

  const clearLongPress = () => {
    if (longPressTimer.current) {
      clearTimeout(longPressTimer.current);
      longPressTimer.current = null;
    }
  };

  const handleTouchStart = (e: React.TouchEvent) => {
    clearLongPress();
    longPressFired.current = false;
    if (!sessionId || isDeleting) return;
    const touch = e.touches[0];
    if (!touch) return;
    const tx = touch.clientX;
    const ty = touch.clientY;
    touchStart.current = { x: tx, y: ty };
    longPressTimer.current = setTimeout(() => {
      longPressFired.current = true;
      touchOpenedAt.current = Date.now();
      closeOtherContextMenus();
      setContextMenu({ x: tx, y: ty, scope: bulkApi.prepareScope(workspace) });
    }, 500);
  };

  // Cancel the pending long-press only once the finger moves past the slop, so
  // a normal jittery hold still opens the menu while a deliberate drag
  // (scroll/reorder) cancels it. See exceedsTouchSlop (#2232).
  const handleTouchMove = (e: React.TouchEvent) => {
    const touch = e.touches[0];
    if (!touch || !touchStart.current) return;
    if (exceedsTouchSlop(touchStart.current, { x: touch.clientX, y: touch.clientY })) {
      clearLongPress();
    }
  };

  const handleTouchEnd = (e: React.TouchEvent) => {
    clearLongPress();
    if (longPressFired.current) {
      e.preventDefault();
    }
  };

  const startRename = () => {
    if (renaming) return;
    setContextMenu(null);
    setRenameValue(sessionTitle || label);
    setRenaming(true);
    requestAnimationFrame(() => renameRef.current?.select());
  };

  const commitRename = async () => {
    setRenaming(false);
    const trimmed = renameValue.trim();
    // Compare against the current title, not the displayed label: when a
    // single session has no title yet, label is the branch and accepting
    // the prefilled value should still set the title.
    if (!trimmed || trimmed === sessionTitle || !sessionId) return;
    // For a tied worktree session the rename also moves the directory and can
    // fail (e.g. 409 while running); surface the server message. See #1927.
    const result = await renameSession(sessionId, trimmed);
    if (!result.ok && result.message) {
      reportError(result.message);
    }
  };

  // Editing the workdir name moves the worktree directory, so it is only
  // offered for an aoe-managed worktree session that is not running. When the
  // session is tied (#1927) naming collapses into the rename action, so the
  // standalone workdir edit is hidden.
  const canEditWorkdir =
    !!firstSession?.has_managed_worktree && !firstSession?.tie_workdir_to_name && !runningSession && !!sessionId;

  const openWorkdirModal = () => {
    setContextMenu(null);
    setWorkdirModalOpen(true);
  };

  const startGroupEdit = () => {
    setContextMenu(null);
    setEditingGroup(true);
  };

  const saveGroup = async (group: string): Promise<boolean> => {
    if (!sessionId) return false;
    return updateSessionGroup(sessionId, group);
  };

  const handleDelete = () => {
    setContextMenu(null);
    onDelete?.(workspace.id);
  };

  const handleStop = () => {
    setContextMenu(null);
    onStop?.(workspace.id);
  };
  // Mirror the TUI's `x` guard: a session that is already stopped or
  // mid-lifecycle has nothing to stop, so hide the action for those.
  const canStop = !["Stopped", "Deleting", "Creating"].includes(sessionStatus);

  const handleStart = () => {
    setContextMenu(null);
    onStart?.(workspace.id);
  };
  // Start is the inverse of Stop: only offered for a stopped session.
  const canStart = sessionStatus === "Stopped";

  if (renaming) {
    return (
      <div className={`py-1 ${indented ? "pl-6 pr-3" : "px-3"}`}>
        <input
          ref={renameRef}
          type="text"
          value={renameValue}
          onChange={(e) => setRenameValue(e.target.value)}
          onBlur={commitRename}
          onKeyDown={(e) => {
            if (e.key === "Enter") commitRename();
            if (e.key === "Escape") setRenaming(false);
          }}
          data-testid="sidebar-rename-input"
          className="w-full bg-surface-900 border border-brand-600 rounded px-2 py-1 text-[13px] md:text-[14px] font-mono text-text-primary focus:outline-none"
        />
      </div>
    );
  }

  return (
    <>
      <a
        href={sessionPath}
        tabIndex={isDeleting ? -1 : undefined}
        aria-disabled={isDeleting || undefined}
        data-testid="sidebar-session-row"
        draggable={false}
        onClick={(e) => {
          // Let the browser handle non-primary clicks (middle-click still
          // opens the session href in a new tab) and Alt+click.
          if (e.button !== 0 || e.altKey) {
            return;
          }
          if (isDeleting) {
            e.preventDefault();
            return;
          }
          if (longPressFired.current) {
            e.preventDefault();
            return;
          }
          // Primary click (plain or with Shift / Cmd / Ctrl): the parent
          // decides navigate vs. select. Always preventDefault so a modifier
          // click builds the selection instead of following the href.
          e.preventDefault();
          onActivate(e);
        }}
        onContextMenu={handleContextMenu}
        onTouchStart={handleTouchStart}
        onTouchEnd={handleTouchEnd}
        onTouchMove={handleTouchMove}
        onTouchCancel={clearLongPress}
        data-selected={isSelected || undefined}
        className={`block w-full text-left py-2 cursor-pointer select-none [-webkit-touch-callout:none] transition-colors duration-75 ${
          indented ? "pl-6 pr-3" : "px-3"
        } ${
          isActive
            ? "bg-surface-850 border-l-2 border-brand-600"
            : "border-l-2 border-transparent hover:bg-surface-700/40"
        } ${
          isSelected ? "ring-1 ring-inset ring-brand-500/60 bg-brand-500/10" : ""
        } ${isDeleting ? "opacity-50 pointer-events-none" : ""}`}
      >
        {isSelected && <span className="sr-only">Selected</span>}
        <div className="flex items-center gap-2">
          <span
            className={`text-sm shrink-0 leading-none font-mono ${showUnreadGlyph ? "text-status-unread font-semibold" : textClass}`}
          >
            {showUnreadGlyph ? (
              <span title="Unread" aria-label="Unread" data-testid="sidebar-unread-dot">
                ●
              </span>
            ) : (
              <StatusGlyph status={sessionStatus} createdAt={createdAt} idleEnteredAt={idleEnteredAt} />
            )}
          </span>
          <div className="min-w-0 flex-1">
            <span
              className={`flex items-center gap-1.5 text-[13px] md:text-[14px] ${showUnreadGlyph ? "text-status-unread font-semibold" : isSessionActive({ status: sessionStatus, idle_entered_at: idleEnteredAt }, idleDecayWindowMs) ? textClass : isActive ? "text-text-primary" : "text-text-secondary"} ${isFavorited || effectivePinned ? "font-semibold" : ""} ${effectiveArchived || effectiveSnoozed ? "italic opacity-70" : ""}`}
            >
              {effectivePinned && (
                <span title="Pinned" aria-label="Pinned" className="shrink-0 inline-flex text-brand-400">
                  <Pin className="h-3 w-3 -rotate-45" />
                </span>
              )}
              {isFavorited && (
                <span title="Favorited" aria-label="Favorited" className="shrink-0 text-amber-300">
                  *
                </span>
              )}
              <span className="truncate" title={label}>
                {label}
              </span>
              {hasDraft && (
                <span title="Unsent draft" aria-label="Unsent draft" className="inline-flex shrink-0">
                  <Pencil className="h-3 w-3 text-amber-400/90" />
                </span>
              )}
              {queuedCount > 0 && (
                <span
                  title={`${queuedCount} queued prompt${queuedCount === 1 ? "" : "s"}`}
                  aria-label={`${queuedCount} queued`}
                  className="inline-flex shrink-0 items-center rounded border border-sky-700/40 bg-sky-950/30 px-1 text-[10px] font-mono font-medium tabular-nums text-sky-300"
                >
                  {queuedCount}
                </span>
              )}
              {rateLimited && (
                <span
                  title={rateLimitTitle}
                  aria-label={rateLimitTitle}
                  className="inline-flex shrink-0 items-center gap-0.5 rounded border border-orange-700/40 bg-orange-950/30 px-1 text-[10px] font-mono font-medium text-orange-300"
                >
                  <Hourglass className="h-3 w-3" />
                  {rateLimited.count > 1 && <span className="tabular-nums">{rateLimited.count}</span>}
                  {rateLimitResetLabel && <span>{rateLimitResetLabel}</span>}
                </span>
              )}
              {effectiveArchived && (
                <span
                  title="Archived"
                  aria-label="Archived"
                  className="shrink-0 inline-flex items-center gap-0.5 rounded border border-surface-700/40 bg-surface-800/40 px-1 py-0 text-[10px] font-mono font-medium text-text-dim"
                >
                  <Archive className="h-3 w-3" />
                  <span className="hidden sm:inline">archived</span>
                </span>
              )}
              {!effectiveArchived && effectiveSnoozed && effectiveSnoozedUntil && (
                <span
                  title={`Snoozed until ${new Date(effectiveSnoozedUntil).toLocaleString()}`}
                  aria-label="Snoozed"
                  className="shrink-0 inline-flex items-center gap-0.5 rounded border border-surface-700/40 bg-surface-800/40 px-1 py-0 text-[10px] font-mono font-medium text-text-dim"
                >
                  <Moon className="h-3 w-3" />
                  <span>{formatSnoozeRemainingShort(effectiveSnoozedUntil)}</span>
                </span>
              )}
              {firstSession?.view === "structured" && firstSession.acp_worker_state === "resuming" && (
                <span
                  title="Structured view worker is resuming"
                  aria-label="Resuming"
                  className="inline-flex shrink-0 items-center gap-0.5 rounded border border-amber-700/40 bg-amber-950/30 px-1 py-0 text-[10px] font-medium text-amber-300"
                >
                  <span className="inline-block h-1.5 w-1.5 animate-pulse rounded-full bg-amber-400/80" />
                  Resuming
                </span>
              )}
              {firstSession?.smart_rename === "pending" && (
                <span
                  title="Will auto-name this session from your first message"
                  aria-label="Will auto-name"
                  className="inline-flex shrink-0 items-center gap-0.5 rounded border border-surface-700/40 bg-surface-800/40 px-1 py-0 text-[10px] font-mono font-medium text-text-dim"
                >
                  <Sparkles className="h-3 w-3" />
                  <span className="hidden sm:inline">Auto-name</span>
                </span>
              )}
              {firstSession?.smart_rename === "running" && (
                <span
                  title="Generating a name from your first message"
                  aria-label="Naming"
                  className="inline-flex shrink-0 items-center gap-0.5 rounded border border-amber-700/40 bg-amber-950/30 px-1 py-0 text-[10px] font-medium text-amber-300"
                >
                  <span className="inline-block h-1.5 w-1.5 animate-pulse rounded-full bg-amber-400/80" />
                  Naming…
                </span>
              )}
              {firstSession?.next_wakeup_at && (
                <WakeupCountdown wakeAt={firstSession.next_wakeup_at} reason={firstSession.next_wakeup_reason} />
              )}
              {firstSession?.monitor_active && <MonitorBadge description={firstSession.monitor_description} />}
            </span>
            {firstSession && <PluginRowLine sessionId={firstSession.id} />}
            {subtitle && (
              <span className="block text-[11px] font-mono text-text-dim truncate" title={subtitleTitle ?? subtitle}>
                {subtitle}
                {baseBranch && <span className="ml-1 text-text-dim/70">← {baseBranch}</span>}
              </span>
            )}
            {firstSession?.plan_summary &&
              firstSession.plan_summary.total > 0 &&
              // Hide the completed-plan bar when the session is also
              // sitting idle waiting for the next prompt: at that
              // point the bar is a static "100% 5/5" line that adds
              // clutter without conveying anything actionable. The
              // bar reappears on the next prompt because the agent
              // either emits a new plan (resetting completed) or
              // stays on the old one but flips status back to Running.
              !(
                firstSession.plan_summary.completed >= firstSession.plan_summary.total && firstSession.status === "Idle"
              ) && <PlanProgressMini summary={firstSession.plan_summary} />}
            {firstSession && (firstSession.workspace_repos?.length ?? 0) > 1 && (
              <span
                className="mt-0.5 flex flex-wrap gap-1 text-[10px] font-mono text-text-dim"
                title={firstSession.workspace_repos.map((r) => r.source_path).join("\n")}
              >
                {firstSession.workspace_repos.map((r) => (
                  <span
                    key={r.source_path}
                    className="px-1 py-px bg-surface-800/50 border border-surface-700/40 rounded text-text-secondary"
                  >
                    {r.name}
                  </span>
                ))}
              </span>
            )}
          </div>
        </div>
      </a>
      {contextMenu &&
        createPortal(
          <div
            ref={menuRef}
            data-testid="sidebar-context-menu"
            className="fixed z-50 bg-surface-800 border border-surface-700 rounded-lg shadow-lg py-1 min-w-[180px] overflow-y-auto"
            style={{
              left: contextMenu.x,
              top: contextMenu.y,
              maxHeight: "calc(100vh - 16px)",
            }}
          >
            {contextMenu.scope.kind === "bulk" ? (
              <BulkTriageMenuItems
                count={contextMenu.scope.count}
                buckets={contextMenu.scope.buckets}
                api={bulkApi}
                onDone={() => setContextMenu(null)}
              />
            ) : (
              <>
                {!readOnly && onCreateSession && newSessionRepoPath && (
                  <button
                    onClick={() => {
                      setContextMenu(null);
                      onCreateSession(newSessionRepoPath);
                    }}
                    data-testid="sidebar-context-menu-new-session"
                    className="w-full text-left px-3 py-2 md:py-2 max-md:py-3 text-sm text-text-secondary hover:bg-surface-700/50 cursor-pointer transition-colors flex items-center gap-2"
                  >
                    <Plus className="h-3.5 w-3.5 shrink-0" />
                    New Session
                  </button>
                )}
                <button
                  onClick={startRename}
                  data-testid="sidebar-context-menu-rename"
                  className="w-full text-left px-3 py-2 md:py-2 max-md:py-3 text-sm text-text-secondary hover:bg-surface-700/50 cursor-pointer transition-colors"
                >
                  Rename
                </button>
                {!readOnly && canEditWorkdir && (
                  <button
                    onClick={openWorkdirModal}
                    data-testid="sidebar-context-menu-edit-workdir"
                    className="w-full text-left px-3 py-2 md:py-2 max-md:py-3 text-sm text-text-secondary hover:bg-surface-700/50 cursor-pointer transition-colors"
                  >
                    Edit workdir name
                  </button>
                )}
                {!readOnly && (
                  <button
                    onClick={startGroupEdit}
                    data-testid="sidebar-context-menu-edit-group"
                    className="w-full text-left px-3 py-2 md:py-2 max-md:py-3 text-sm text-text-secondary hover:bg-surface-700/50 cursor-pointer transition-colors"
                  >
                    Edit group
                  </button>
                )}
                {!readOnly && acpSession && (
                  <button
                    onClick={handleSwitchAgent}
                    data-testid="sidebar-context-menu-switch-agent"
                    className="w-full text-left px-3 py-2 md:py-2 max-md:py-3 text-sm text-text-secondary hover:bg-surface-700/50 cursor-pointer transition-colors flex items-center gap-2"
                  >
                    <ArrowLeftRight className="h-3.5 w-3.5 shrink-0" />
                    Switch agent
                  </button>
                )}
                {!readOnly && acpSession?.default_name && (
                  <button
                    onClick={() => void handleAutoNameNow()}
                    data-testid="sidebar-context-menu-auto-name"
                    className="w-full text-left px-3 py-2 md:py-2 max-md:py-3 text-sm text-text-secondary hover:bg-surface-700/50 cursor-pointer transition-colors flex items-center gap-2"
                  >
                    <Sparkles className="h-3.5 w-3.5 shrink-0" />
                    Auto-name now
                  </button>
                )}
                {!readOnly && canStop && (
                  <button
                    onClick={handleStop}
                    data-testid="sidebar-context-menu-stop"
                    className="w-full text-left px-3 py-2 md:py-2 max-md:py-3 text-sm text-text-secondary hover:bg-surface-700/50 cursor-pointer transition-colors flex items-center gap-2"
                  >
                    <CircleStop className="h-3.5 w-3.5 shrink-0" />
                    Stop
                  </button>
                )}
                {!readOnly && canStart && (
                  <button
                    onClick={handleStart}
                    data-testid="sidebar-context-menu-start"
                    className="w-full text-left px-3 py-2 md:py-2 max-md:py-3 text-sm text-text-secondary hover:bg-surface-700/50 cursor-pointer transition-colors flex items-center gap-2"
                  >
                    <Play className="h-3.5 w-3.5 shrink-0" />
                    Start
                  </button>
                )}
                <div className="border-t border-surface-700/20 my-1" />
                <div className="px-3 py-1 text-[11px] font-mono uppercase tracking-widest text-text-muted">
                  Notifications
                </div>
                {(["off", "default", "all"] as const).map((preset) => {
                  const label = preset === "off" ? "Off" : preset === "default" ? "Default" : "All events";
                  const selected = notifyPreset === preset;
                  return (
                    <button
                      key={preset}
                      onClick={() => void setNotifyPreset(preset)}
                      className={`w-full text-left pl-6 pr-3 py-2 md:py-2 max-md:py-3 text-sm hover:bg-surface-700/50 cursor-pointer transition-colors flex items-center gap-2 ${
                        selected ? "text-text-primary" : "text-text-secondary"
                      }`}
                    >
                      <span className="w-3 text-brand-500">{selected ? "✓" : ""}</span>
                      {label}
                    </button>
                  );
                })}
                {!readOnly && (
                  <>
                    <div className="border-t border-surface-700/20 my-1" />
                    <div className="px-3 py-1 text-[11px] font-mono uppercase tracking-widest text-text-muted">
                      Triage
                    </div>
                    {(() => {
                      // Menu actions are gated by the row's current triage
                      // state so contradictory toggles never appear in the
                      // UI: an archived row only offers Unarchive, a snoozed
                      // row only offers Unsnooze. A pinned row also offers
                      // Archive/Snooze, since those are valid transitions
                      // (the backend clears the pin) and match the TUI. The
                      // shape helper lives in `lib/sidebarSort.ts` so it can
                      // be unit tested. See #1581.
                      const shape = triageMenuShape(
                        triageStateOf({
                          isPinned: effectivePinned,
                          isArchived: effectiveArchived,
                          isSnoozed: effectiveSnoozed,
                        }),
                      );
                      return (
                        <>
                          {shape.showPin && (
                            <button
                              onClick={() => void togglePin()}
                              data-testid="sidebar-context-menu-pin"
                              className="w-full text-left pl-6 pr-3 py-2 md:py-2 max-md:py-3 text-sm text-text-secondary hover:bg-surface-700/50 cursor-pointer transition-colors flex items-center gap-2"
                            >
                              <Pin className="h-3.5 w-3.5 shrink-0 -rotate-45" />
                              Pin
                            </button>
                          )}
                          {shape.showUnpin && (
                            <button
                              onClick={() => void togglePin()}
                              data-testid="sidebar-context-menu-pin"
                              className="w-full text-left pl-6 pr-3 py-2 md:py-2 max-md:py-3 text-sm text-text-secondary hover:bg-surface-700/50 cursor-pointer transition-colors flex items-center gap-2"
                            >
                              <Pin className="h-3.5 w-3.5 shrink-0 -rotate-45" />
                              Unpin
                            </button>
                          )}
                          {shape.showArchive && (
                            <button
                              onClick={() => void toggleArchive()}
                              data-testid="sidebar-context-menu-archive"
                              className="w-full text-left pl-6 pr-3 py-2 md:py-2 max-md:py-3 text-sm text-text-secondary hover:bg-surface-700/50 cursor-pointer transition-colors flex items-center gap-2"
                            >
                              <Archive className="h-3.5 w-3.5 shrink-0" />
                              Archive
                            </button>
                          )}
                          {shape.showUnarchive && (
                            <button
                              onClick={() => void toggleArchive()}
                              data-testid="sidebar-context-menu-archive"
                              className="w-full text-left pl-6 pr-3 py-2 md:py-2 max-md:py-3 text-sm text-text-secondary hover:bg-surface-700/50 cursor-pointer transition-colors flex items-center gap-2"
                            >
                              <Archive className="h-3.5 w-3.5 shrink-0" />
                              Unarchive
                            </button>
                          )}
                          {shape.showSnooze && (
                            <button
                              onClick={openSnoozeModal}
                              data-testid="sidebar-context-menu-snooze"
                              className="w-full text-left pl-6 pr-3 py-2 md:py-2 max-md:py-3 text-sm text-text-secondary hover:bg-surface-700/50 cursor-pointer transition-colors flex items-center gap-2"
                            >
                              <Moon className="h-3.5 w-3.5 shrink-0" />
                              Snooze…
                            </button>
                          )}
                          {shape.showUnsnooze && (
                            <button
                              onClick={() => void applySnooze(null)}
                              data-testid="sidebar-context-menu-unsnooze"
                              className="w-full text-left pl-6 pr-3 py-2 md:py-2 max-md:py-3 text-sm text-text-secondary hover:bg-surface-700/50 cursor-pointer transition-colors flex items-center gap-2"
                            >
                              <Moon className="h-3.5 w-3.5 shrink-0" />
                              Unsnooze
                            </button>
                          )}
                          {/* Unlike the others, the unread toggle is always
                          offered (any sort), gated only on the feature
                          flag; the label flips to "Mark as read" when the
                          row already carries a marker. */}
                          {unreadIndicatorEnabled && (
                            <button
                              onClick={() => void toggleUnread()}
                              data-testid="sidebar-context-menu-unread"
                              className="w-full text-left pl-6 pr-3 py-2 md:py-2 max-md:py-3 text-sm text-text-secondary hover:bg-surface-700/50 cursor-pointer transition-colors flex items-center gap-2"
                            >
                              <CircleDot className="h-3.5 w-3.5 shrink-0" />
                              {effectiveUnread ? "Mark as read" : "Mark as unread"}
                            </button>
                          )}
                        </>
                      );
                    })()}
                    <div className="border-t border-surface-700/20 my-1" />
                    <button
                      onClick={handleDelete}
                      data-testid="sidebar-context-menu-delete"
                      className="w-full text-left px-3 py-2 md:py-2 max-md:py-3 text-sm text-status-error hover:bg-status-error/10 cursor-pointer transition-colors"
                    >
                      Delete
                    </button>
                  </>
                )}
              </>
            )}
          </div>,
          document.body,
        )}
      {snoozeModalOpen &&
        createPortal(
          <SnoozeModal
            title={label}
            onCancel={() => setSnoozeModalOpen(false)}
            onPick={(minutes) => void applySnooze(minutes)}
          />,
          document.body,
        )}
      {workdirModalOpen &&
        sessionId &&
        createPortal(
          <WorkdirNameModal
            title={label}
            currentBranch={branchLabel}
            onCancel={() => setWorkdirModalOpen(false)}
            onSubmit={async (name, renameBranch) => {
              const res = await setWorktreeName(sessionId, name, renameBranch);
              if (res.ok) setWorkdirModalOpen(false);
              return res;
            }}
          />,
          document.body,
        )}
      {editingGroup &&
        createPortal(
          <SessionGroupModal
            sessionTitle={sessionTitle || label}
            currentGroup={sessionGroup}
            onSave={saveGroup}
            onClose={() => setEditingGroup(false)}
          />,
          document.body,
        )}
    </>
  );
});

/** Edit-workdir-name modal. Renamed the worktree directory and, when the
 *  user opts in, the git branch. Rendered as its own portal so it is
 *  independent of the row's context menu. See #1723. */
export function WorkdirNameModal({
  title,
  currentBranch,
  onCancel,
  onSubmit,
}: {
  title: string;
  currentBranch: string | null;
  onCancel: () => void;
  onSubmit: (name: string, renameBranch: boolean) => Promise<{ ok: boolean; message?: string }>;
}) {
  const [name, setName] = useState("");
  const [renameBranch, setRenameBranch] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onCancel();
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [onCancel]);

  const submit = async () => {
    if (busy) return;
    const trimmed = name.trim();
    if (!trimmed) {
      setError("Enter a new workdir name.");
      return;
    }
    setBusy(true);
    setError(null);
    const res = await onSubmit(trimmed, renameBranch);
    setBusy(false);
    if (!res.ok) {
      setError(res.message ?? "Failed to edit the workdir name.");
    }
  };

  return (
    <div
      data-testid="workdir-modal-backdrop"
      onClick={(e) => {
        if (e.target === e.currentTarget) onCancel();
      }}
      className="fixed inset-0 z-[60] flex items-center justify-center bg-black/60 px-4 py-8 overflow-y-auto"
      role="dialog"
      aria-modal="true"
      aria-label="Edit workdir name"
    >
      <div
        data-testid="workdir-modal"
        className="w-full max-w-sm rounded-lg border border-surface-700 bg-surface-800 shadow-xl"
      >
        <div className="px-4 py-3 border-b border-surface-700/40">
          <div className="text-sm font-mono text-text-primary truncate" title={title}>
            Edit workdir name
            <span className="text-text-muted"> · {title}</span>
          </div>
          {currentBranch && (
            <div className="mt-1 text-[11px] text-text-dim font-mono">Current branch: {currentBranch}</div>
          )}
        </div>
        <div className="px-4 py-3 flex flex-col gap-3">
          <input
            type="text"
            autoFocus
            aria-label="New workdir name"
            disabled={busy}
            value={name}
            onChange={(e) => {
              setName(e.target.value);
              setError(null);
            }}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                e.preventDefault();
                void submit();
              }
            }}
            placeholder="new-workdir-name"
            data-testid="workdir-modal-name"
            className="w-full bg-surface-900 border border-surface-700 rounded px-2 py-1 text-[13px] md:text-[14px] font-mono text-text-primary focus:outline-none focus:border-brand-600 disabled:opacity-50"
          />
          <label className="flex items-center gap-2 text-sm text-text-secondary cursor-pointer">
            <input
              type="checkbox"
              disabled={busy}
              checked={renameBranch}
              onChange={(e) => setRenameBranch(e.target.checked)}
              data-testid="workdir-modal-rename-branch"
            />
            Also rename git branch
          </label>
          {error && (
            <div data-testid="workdir-modal-error" className="text-[11px] text-status-error">
              {error}
            </div>
          )}
        </div>
        <div className="px-4 py-3 border-t border-surface-700/40 flex justify-end gap-2">
          <button
            onClick={onCancel}
            className="px-3 py-1 text-sm text-text-secondary hover:bg-surface-700/50 rounded cursor-pointer transition-colors"
          >
            Cancel
          </button>
          <button
            onClick={() => void submit()}
            disabled={busy}
            data-testid="workdir-modal-save"
            className="px-3 py-1 text-sm text-text-primary bg-brand-600 hover:bg-brand-500 rounded cursor-pointer transition-colors disabled:opacity-50"
          >
            {busy ? "Saving…" : "Save"}
          </button>
        </div>
      </div>
    </div>
  );
}

/** Bounds for `validate_snooze_duration` on the server. Mirrored
 *  client-side so the modal can pre-validate and disable the submit
 *  button rather than round-trip a 400. See
 *  `src/session/config.rs::SNOOZE_MAX_MINUTES`. */
const SNOOZE_MIN_MINUTES = 1;
const SNOOZE_MAX_MINUTES = 30 * 24 * 60;

type SnoozeUnit = "m" | "h" | "d" | "w";

const SNOOZE_UNIT_LABELS: Record<SnoozeUnit, string> = {
  m: "minutes",
  h: "hours",
  d: "days",
  w: "weeks",
};

function snoozeUnitToMinutes(value: number, unit: SnoozeUnit): number {
  switch (unit) {
    case "m":
      return value;
    case "h":
      return value * 60;
    case "d":
      return value * 60 * 24;
    case "w":
      return value * 60 * 24 * 7;
  }
}

/** Centered modal duration picker rendered as a separate portal so it
 *  is independent of the row's context menu. Three submit paths:
 *   - 8 TUI presets (matching `src/tui/dialogs/snooze_duration.rs`).
 *   - Custom duration: number + unit (m/h/d/w).
 *   - Until a specific date+time (HTML5 datetime-local input).
 *  Backdrop click and Escape both dismiss. See #1581. */
export function SnoozeModal({
  title,
  onCancel,
  onPick,
}: {
  title: string;
  onCancel: () => void;
  onPick: (minutes: number) => void;
}) {
  const [customValue, setCustomValue] = useState("");
  const [customUnit, setCustomUnit] = useState<SnoozeUnit>("h");
  const [untilValue, setUntilValue] = useState("");
  const [customError, setCustomError] = useState<string | null>(null);
  const [untilError, setUntilError] = useState<string | null>(null);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onCancel();
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [onCancel]);

  const submitCustom = () => {
    setCustomError(null);
    const n = Number.parseInt(customValue, 10);
    if (!Number.isFinite(n) || n <= 0) {
      setCustomError("Enter a positive whole number.");
      return;
    }
    const minutes = snoozeUnitToMinutes(n, customUnit);
    if (minutes < SNOOZE_MIN_MINUTES || minutes > SNOOZE_MAX_MINUTES) {
      setCustomError(`Must be between 1 minute and 30 days (got ${minutes} minutes).`);
      return;
    }
    onPick(minutes);
  };

  const submitUntil = () => {
    setUntilError(null);
    if (!untilValue) {
      setUntilError("Pick a date and time.");
      return;
    }
    // datetime-local values are wall-clock (no zone). Date.parse
    // interprets them as local time, which matches user expectation
    // (snooze "until 9am tomorrow" means 9am in the user's TZ).
    const target = Date.parse(untilValue);
    if (!Number.isFinite(target)) {
      setUntilError("Invalid date.");
      return;
    }
    const deltaMs = target - Date.now();
    if (deltaMs <= 0) {
      setUntilError("Pick a time in the future.");
      return;
    }
    const minutes = Math.max(1, Math.round(deltaMs / 60_000));
    if (minutes > SNOOZE_MAX_MINUTES) {
      setUntilError("Maximum snooze is 30 days from now.");
      return;
    }
    onPick(minutes);
  };

  return (
    <div
      data-testid="snooze-modal-backdrop"
      onClick={(e) => {
        if (e.target === e.currentTarget) onCancel();
      }}
      className="fixed inset-0 z-[60] flex items-center justify-center bg-black/60 px-4 py-8 overflow-y-auto"
      role="dialog"
      aria-modal="true"
      aria-label="Snooze session"
    >
      <div
        data-testid="snooze-modal"
        className="w-full max-w-sm rounded-lg border border-surface-700 bg-surface-800 shadow-xl"
      >
        <div className="px-4 py-3 border-b border-surface-700/40">
          <div className="text-sm font-mono text-text-primary truncate" title={title}>
            Snooze
            <span className="text-text-muted"> · {title}</span>
          </div>
          <div className="mt-1 text-[11px] text-text-dim">How long should this session sit out?</div>
        </div>
        <div className="flex flex-col py-2">
          {SNOOZE_PRESETS.map((preset) => (
            <button
              key={preset.minutes}
              onClick={() => onPick(preset.minutes)}
              data-testid={`snooze-modal-preset-${preset.minutes}`}
              className="w-full text-left px-4 py-2 text-sm text-text-secondary hover:bg-surface-700/50 cursor-pointer transition-colors"
            >
              {preset.label}
            </button>
          ))}
        </div>
        <div className="px-4 py-3 border-t border-surface-700/40">
          <div className="text-[11px] font-mono uppercase tracking-widest text-text-muted mb-2">Custom duration</div>
          <div className="flex items-center gap-2">
            <input
              type="number"
              inputMode="numeric"
              min={1}
              value={customValue}
              onChange={(e) => setCustomValue(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") submitCustom();
              }}
              placeholder="3"
              data-testid="snooze-modal-custom-value"
              aria-label="Custom snooze duration"
              className="w-20 rounded border border-surface-700 bg-surface-900 px-2 py-1 text-sm text-text-primary focus:border-brand-600 focus:outline-none"
            />
            <select
              value={customUnit}
              onChange={(e) => setCustomUnit(e.target.value as SnoozeUnit)}
              data-testid="snooze-modal-custom-unit"
              aria-label="Custom snooze unit"
              className="rounded border border-surface-700 bg-surface-900 px-2 py-1 text-sm text-text-primary focus:border-brand-600 focus:outline-none"
            >
              {(Object.keys(SNOOZE_UNIT_LABELS) as SnoozeUnit[]).map((u) => (
                <option key={u} value={u}>
                  {SNOOZE_UNIT_LABELS[u]}
                </option>
              ))}
            </select>
            <button
              onClick={submitCustom}
              data-testid="snooze-modal-custom-submit"
              className="ml-auto rounded bg-brand-600 px-3 py-1 text-sm font-medium text-text-primary hover:bg-brand-500 cursor-pointer transition-colors"
            >
              Snooze
            </button>
          </div>
          {customError && (
            <div role="alert" data-testid="snooze-modal-custom-error" className="mt-1 text-[11px] text-status-error">
              {customError}
            </div>
          )}
        </div>
        <div className="px-4 py-3 border-t border-surface-700/40">
          <div className="text-[11px] font-mono uppercase tracking-widest text-text-muted mb-2">Until</div>
          <div className="flex items-center gap-2">
            <input
              type="datetime-local"
              value={untilValue}
              onChange={(e) => setUntilValue(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") submitUntil();
              }}
              data-testid="snooze-modal-until-value"
              aria-label="Snooze until"
              className="flex-1 min-w-0 rounded border border-surface-700 bg-surface-900 px-2 py-1 text-sm text-text-primary focus:border-brand-600 focus:outline-none"
            />
            <button
              onClick={submitUntil}
              data-testid="snooze-modal-until-submit"
              className="rounded bg-brand-600 px-3 py-1 text-sm font-medium text-text-primary hover:bg-brand-500 cursor-pointer transition-colors"
            >
              Snooze
            </button>
          </div>
          {untilError && (
            <div role="alert" data-testid="snooze-modal-until-error" className="mt-1 text-[11px] text-status-error">
              {untilError}
            </div>
          )}
        </div>
        <div className="px-4 py-3 border-t border-surface-700/40 flex justify-end">
          <button
            onClick={onCancel}
            data-testid="snooze-modal-cancel"
            className="text-sm text-text-dim hover:text-text-primary cursor-pointer transition-colors"
          >
            Cancel
          </button>
        </div>
      </div>
    </div>
  );
}

export const SidebarGroupHeader = memo(function SidebarGroupHeader({
  group,
  hasActiveChild,
  onClick,
  onNewSession,
  onUpdateAppearance,
  onArchiveAll,
  onPin,
  onUnpin,
  offline,
  dragHandle,
}: {
  group: SidebarGroup;
  hasActiveChild: boolean;
  onClick: () => void;
  onNewSession: () => void;
  onUpdateAppearance: (repoId: string, update: RepoAppearanceUpdate) => void;
  /** Archive every active session under this group. Omitted (read-only /
   *  offline) hides the action; the parent owns the confirmation. */
  onArchiveAll?: () => void;
  /** Register this repo in the pin registry so it persists with zero
   *  sessions. Repo axis only; omitted (read-only / offline) hides it. */
  onPin?: (repoPath: string) => void;
  /** Remove every registry entry for this repo path (unpin). Omitted
   *  (read-only / offline) hides it. */
  onUnpin?: (group: SidebarGroup) => void;
  offline: boolean;
  dragHandle?: DragHandleProps;
}) {
  // Appearance (rename/alias/color) and its context menu are repo-axis
  // only. The user-group axis has no per-group appearance in v1, so the
  // menu trigger and rename input are gated off rather than rendered inert.
  const canAppearance = group.capabilities.appearance;
  // "Archive all in group" works on every axis (a project or a manual
  // group), so it can light up the context menu even where appearance is
  // off. Count only the still-active members so the label is honest and the
  // action hides once everything is already archived.
  const archivableCount = onArchiveAll ? archivableWorkspaces(group).length : 0;
  const canArchiveAll = archivableCount > 0;
  // Pin/unpin is repo-axis only and needs a concrete repo path. Pin shows
  // when the repo is not yet registered; unpin when it is. See #2047.
  const canPin = !!onPin && group.capabilities.create === "repo" && !!group.repoPath && !group.pinned;
  const canUnpin = !!onUnpin && group.kind === "repo" && group.pinned;
  const hasMenu = canAppearance || canArchiveAll || canPin || canUnpin;
  const headerTitle = group.groupPath ?? group.repoPath;
  const [contextMenu, setContextMenu] = useState<{
    x: number;
    y: number;
  } | null>(null);
  const [renaming, setRenaming] = useState(false);
  const [renameValue, setRenameValue] = useState(group.alias ?? group.displayName);
  const renameRef = useRef<HTMLInputElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const dotClass = STATUS_DOT_CLASS[group.status === "active" ? "Running" : "Idle"] ?? "bg-status-idle";
  const headerStyle = repoColorStyle(group.color);
  const headerHoverClass = group.color ? "" : "hover:bg-surface-800/50";
  // Count live rows only, matching the list rendered below (which drops
  // workspaces where every session is archived or snoozed via
  // workspaceIsSunk). Summing raw sessions inflated the badge above the
  // visible row count. See #2372.
  const sessionCount = group.workspaces.filter((v) => !workspaceIsSunk(v.workspace)).length;

  // The whole header row is the drag activator now (no grip handle), so a
  // drag ends with the pointer over one of the row's controls. Suppress the
  // trailing click for a beat after a drag so reordering a group doesn't also
  // collapse it or fire the New Session button. The window stays open for the
  // whole drag (Infinity), so even a multi-second drag can't leak the click,
  // then closes 250ms after release. Enforced row-wide via onClickCapture so
  // every control is covered, not just the toggle.
  const dragSuppressRef = useRef(0);
  const isDragging = dragHandle?.isDragging ?? false;
  useEffect(() => {
    if (isDragging) {
      dragSuppressRef.current = Number.POSITIVE_INFINITY;
    } else if (dragSuppressRef.current > Date.now()) {
      dragSuppressRef.current = Date.now() + 250;
    }
  }, [isDragging]);
  const suppressClickAfterDrag = (e: React.MouseEvent) => {
    if (dragSuppressRef.current > Date.now()) {
      e.preventDefault();
      e.stopPropagation();
    }
  };

  const openMenuAt = useCallback((x: number, y: number) => {
    closeOtherContextMenus();
    setContextMenu({ x, y });
  }, []);

  const handleHeaderKeyDown = (e: React.KeyboardEvent<HTMLDivElement>) => {
    if (e.target !== e.currentTarget) return;
    if (e.key !== "Enter" && e.key !== " " && e.key !== "ContextMenu" && !(e.shiftKey && e.key === "F10")) {
      return;
    }
    e.preventDefault();
    const rect = e.currentTarget.getBoundingClientRect();
    openMenuAt(rect.left + 12, rect.bottom + 4);
  };

  useClampedMenuPosition(contextMenu, menuRef, setContextMenu);

  useEffect(() => {
    if (!contextMenu) return;
    const close = () => setContextMenu(null);
    const onDocClick = (e: MouseEvent) => {
      if (menuRef.current?.contains(e.target as Node)) return;
      close();
    };
    const id = requestAnimationFrame(() => {
      document.addEventListener("click", onDocClick);
      document.addEventListener("contextmenu", close);
    });
    menuBus.addEventListener("close", close);
    return () => {
      cancelAnimationFrame(id);
      document.removeEventListener("click", onDocClick);
      document.removeEventListener("contextmenu", close);
      menuBus.removeEventListener("close", close);
    };
  }, [contextMenu]);

  const commitRename = () => {
    setRenaming(false);
    const trimmed = renameValue.trim();
    onUpdateAppearance(group.id, { alias: trimmed || null });
  };

  if (renaming) {
    return (
      <div
        data-testid="sidebar-group-header"
        data-group-id={group.id}
        className={`flex items-center gap-2 px-3 py-2 transition-colors duration-75 text-text-secondary ${headerHoverClass} ${
          hasActiveChild ? "border-l-2 border-brand-600" : ""
        }`}
        style={headerStyle}
      >
        <span className={`w-2 h-2 rounded-full shrink-0 ${dotClass}`} />
        <input
          ref={renameRef}
          type="text"
          value={renameValue}
          onChange={(e) => setRenameValue(e.target.value)}
          onBlur={commitRename}
          onKeyDown={(e) => {
            if (e.key === "Enter") commitRename();
            if (e.key === "Escape") setRenaming(false);
          }}
          data-testid="sidebar-group-rename-input"
          className="min-w-0 flex-1 rounded border border-brand-600 bg-surface-900 px-2 py-1 text-[13px] md:text-[14px] font-mono text-text-primary focus:outline-none"
        />
      </div>
    );
  }

  return (
    <>
      <div
        data-testid="sidebar-group-header"
        data-group-id={group.id}
        data-draggable={dragHandle ? "true" : undefined}
        tabIndex={hasMenu ? 0 : undefined}
        aria-haspopup={hasMenu ? "menu" : undefined}
        aria-label={
          hasMenu ? `${group.kind === "repo" ? "Project" : "Group"} actions for ${group.displayName}` : undefined
        }
        onContextMenu={
          hasMenu
            ? (e) => {
                e.preventDefault();
                openMenuAt(e.clientX, e.clientY);
              }
            : undefined
        }
        onKeyDown={hasMenu ? handleHeaderKeyDown : undefined}
        onClickCapture={suppressClickAfterDrag}
        className={`group flex items-center gap-2 px-3 py-2 transition-colors duration-75 text-text-secondary focus:outline-none focus:ring-2 focus:ring-brand-600 ${headerHoverClass} ${
          hasActiveChild ? "border-l-2 border-brand-600" : ""
        }`}
        style={headerStyle}
        // The whole row is the drag activator (no grip). Mirrors SessionRow:
        // spread only `listeners` (pointer-down), not dnd-kit's `attributes`,
        // which would inject a role/tabIndex that collide with the context
        // menu wiring. Keyboard drag isn't supported, matching session rows.
        ref={dragHandle?.setActivatorNodeRef}
        {...dragHandle?.listeners}
      >
        <span className={`w-2 h-2 rounded-full shrink-0 ${dotClass}`} />
        <button
          onClick={onClick}
          aria-expanded={!group.collapsed}
          className="flex items-center gap-2 flex-1 min-w-0 text-left cursor-pointer"
        >
          <span className="relative h-4 w-4 shrink-0">
            <span
              data-testid="sidebar-group-icon"
              className="absolute inset-0 flex items-center justify-center transition-opacity duration-75 group-hover:opacity-0 group-focus-within:opacity-0"
            >
              {group.remoteOwner ? (
                <OwnerAvatar owner={group.remoteOwner} size={16} />
              ) : (
                <Folder className="h-3.5 w-3.5 text-text-dim" />
              )}
            </span>
            <span
              data-testid="sidebar-group-fold-chevron"
              className="absolute inset-0 flex items-center justify-center opacity-0 transition-opacity duration-75 group-hover:opacity-100 group-focus-within:opacity-100"
            >
              <svg
                width="10"
                height="10"
                viewBox="0 0 10 10"
                fill="currentColor"
                aria-hidden="true"
                className={`text-text-dim transition-transform duration-75 ${group.collapsed ? "-rotate-90" : ""}`}
              >
                <path
                  d="M2 3 L5 6.5 L8 3"
                  fill="none"
                  stroke="currentColor"
                  strokeWidth="1.5"
                  strokeLinecap="round"
                  strokeLinejoin="round"
                />
              </svg>
            </span>
          </span>
          {group.pinned && (
            <span
              className="shrink-0 text-[10px] leading-none text-text-dim"
              data-testid="sidebar-group-pinned-marker"
              title="Pinned project (persists without sessions)"
              aria-label="Pinned project"
            >
              ◆
            </span>
          )}
          <span className="text-[13px] md:text-[14px] font-medium truncate flex-1" title={headerTitle}>
            {group.displayName}
          </span>
          <span className="shrink-0 text-[12px] tabular-nums text-text-dim" data-testid="sidebar-group-session-count">
            ({sessionCount})
          </span>
        </button>
        <Tooltip text={offline ? OFFLINE_TITLE : "New session"}>
          <button
            onClick={onNewSession}
            disabled={offline}
            className="w-8 h-8 flex items-center justify-center shrink-0 rounded-md transition-colors text-text-muted hover:text-text-secondary hover:bg-surface-700/50 cursor-pointer disabled:opacity-40 disabled:cursor-not-allowed disabled:hover:text-text-muted disabled:hover:bg-transparent"
            aria-label={`New session in ${group.displayName}`}
          >
            <svg
              width="14"
              height="14"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2.5"
              strokeLinecap="round"
            >
              <line x1="12" y1="5" x2="12" y2="19" />
              <line x1="5" y1="12" x2="19" y2="12" />
            </svg>
          </button>
        </Tooltip>
      </div>
      {hasMenu &&
        contextMenu &&
        createPortal(
          <div
            ref={menuRef}
            data-testid="sidebar-group-context-menu"
            className="fixed z-50 bg-surface-800 border border-surface-700 rounded-lg shadow-lg py-1 min-w-[190px] overflow-y-auto"
            style={{
              left: contextMenu.x,
              top: contextMenu.y,
              maxHeight: "calc(100vh - 16px)",
            }}
          >
            {canPin && (
              <button
                onClick={() => {
                  setContextMenu(null);
                  if (group.repoPath) onPin?.(group.repoPath);
                }}
                data-testid="sidebar-group-context-menu-pin"
                className="w-full text-left px-3 py-2 md:py-2 max-md:py-3 text-sm text-text-secondary hover:bg-surface-700/50 cursor-pointer transition-colors"
              >
                Pin project
              </button>
            )}
            {canUnpin && (
              <button
                onClick={() => {
                  setContextMenu(null);
                  onUnpin?.(group);
                }}
                data-testid="sidebar-group-context-menu-unpin"
                className="w-full text-left px-3 py-2 md:py-2 max-md:py-3 text-sm text-text-secondary hover:bg-surface-700/50 cursor-pointer transition-colors"
              >
                Unpin project
              </button>
            )}
            {(canPin || canUnpin) && (canArchiveAll || canAppearance) && (
              <div className="border-t border-surface-700/20 my-1" />
            )}
            {canArchiveAll && (
              <button
                onClick={() => {
                  setContextMenu(null);
                  onArchiveAll?.();
                }}
                data-testid="sidebar-group-context-menu-archive-all"
                className="w-full text-left px-3 py-2 md:py-2 max-md:py-3 text-sm text-text-secondary hover:bg-surface-700/50 cursor-pointer transition-colors"
              >
                {`Archive all (${archivableCount})`}
              </button>
            )}
            {canArchiveAll && canAppearance && <div className="border-t border-surface-700/20 my-1" />}
            {canAppearance && (
              <>
                <button
                  onClick={() => {
                    setContextMenu(null);
                    setRenameValue(group.alias ?? group.defaultDisplayName);
                    setRenaming(true);
                    requestAnimationFrame(() => renameRef.current?.select());
                  }}
                  data-testid="sidebar-group-context-menu-rename"
                  className="w-full text-left px-3 py-2 md:py-2 max-md:py-3 text-sm text-text-secondary hover:bg-surface-700/50 cursor-pointer transition-colors"
                >
                  Rename
                </button>
                {group.alias && (
                  <button
                    onClick={() => {
                      setContextMenu(null);
                      onUpdateAppearance(group.id, { alias: null });
                    }}
                    className="w-full text-left px-3 py-2 md:py-2 max-md:py-3 text-sm text-text-secondary hover:bg-surface-700/50 cursor-pointer transition-colors"
                  >
                    Clear alias
                  </button>
                )}
                <div className="border-t border-surface-700/20 my-1" />
                <div className="px-3 py-1 text-[11px] font-mono uppercase tracking-widest text-text-muted">
                  Background
                </div>
                <div className="grid grid-cols-4 gap-1 px-3 py-1.5">
                  {REPO_COLOR_OPTIONS.map((option) => (
                    <button
                      key={option.id}
                      type="button"
                      onClick={() => {
                        setContextMenu(null);
                        onUpdateAppearance(group.id, { color: option.id });
                      }}
                      data-testid={`sidebar-group-color-${option.id}`}
                      aria-label={`Set ${option.label} background`}
                      className={`h-8 rounded-md border cursor-pointer transition-colors ${
                        group.color === option.id ? "border-text-primary" : "border-surface-700"
                      }`}
                      style={repoSwatchStyle(option.id)}
                    />
                  ))}
                  <button
                    type="button"
                    onClick={() => {
                      setContextMenu(null);
                      onUpdateAppearance(group.id, { color: null });
                    }}
                    data-testid="sidebar-group-color-clear"
                    aria-label="Clear background"
                    className="h-8 rounded-md border border-surface-700 bg-surface-900 text-[10px] font-mono text-text-dim cursor-pointer hover:bg-surface-700/40"
                  >
                    None
                  </button>
                </div>
              </>
            )}
          </div>,
          document.body,
        )}
    </>
  );
});

function workspaceMatchesFilter(ws: Workspace, q: string): boolean {
  return (
    ws.displayName.toLowerCase().includes(q) ||
    ws.projectPath.toLowerCase().includes(q) ||
    (ws.branch?.toLowerCase().includes(q) ?? false) ||
    ws.agents.some((a) => a.toLowerCase().includes(q)) ||
    ws.sessions.some((s) => s.title.toLowerCase().includes(q))
  );
}

// The grouping toggle cycles through the three axes on each click. Order is
// chosen so the first click off the default still lands on the flat group
// axis (preserving the pre-#1720 repo -> group step), then adds nesting.
const NEXT_AXIS: Record<SidebarAxis, SidebarAxis> = {
  repo: "group",
  group: "repo+group",
  "repo+group": "repo",
};

const AXIS_HEADING: Record<SidebarAxis, string> = {
  repo: "Sessions",
  group: "Groups",
  "repo+group": "Sessions",
};

const AXIS_TOOLTIP: Record<SidebarAxis, string> = {
  repo: "Grouping: by repository",
  group: "Grouping: by user group",
  "repo+group": "Grouping: by repository, then user group",
};

const AXIS_ARIA: Record<SidebarAxis, string> = {
  repo: "Group sessions by repository",
  group: "Group sessions by user group",
  "repo+group": "Group sessions by repository, then user group",
};

export function WorkspaceSidebar({
  groups,
  nestedGroups,
  trashedWorkspaces = [],
  onToggleSubgroup,
  onReorderWorkspaces,
  onReorderGroups,
  activeId,
  open,
  onToggle,
  onSelect,
  onToggleGroup,
  onUpdateRepoAppearance,
  onNew,
  onCreateSession,
  onPinProject,
  onUnpinProject,
  savedProjects,
  onAddProject,
  onEditProject,
  onRemoveProject,
  onSettings,
  onDeleteSession,
  onRestoreSession,
  onStopSession,
  onStartSession,
  readOnly,
  sortMode,
  onSortModeChange,
  pluginSortRef,
  onPluginSortChange,
  axis,
  onAxisChange,
}: Props) {
  // Plugin sort/filter slots (#2401). Read the live snapshot here so the facet
  // control and the sort-picker options stay local to the sidebar; the active
  // plugin sort comparator itself is built and threaded by AppContent.
  const pluginUiEntries = usePluginUiEntries();
  const pluginSorts = useMemo(() => pluginSortSpecs(pluginUiEntries), [pluginUiEntries]);
  const facetSpecs = useMemo(() => pluginFacetSpecs(pluginUiEntries), [pluginUiEntries]);
  const pluginSortActive =
    pluginSortRef != null &&
    pluginSorts.some((s) => s.pluginId === pluginSortRef.pluginId && s.entryId === pluginSortRef.entryId);
  // Selected facet values keyed by `${pluginId}\0${entryId}`; ephemeral, like
  // the plugin entries themselves. A selection for a facet that vanishes from
  // the snapshot is simply ignored: `activeFacets` below only reads live
  // `facetSpecs`, so a stale entry never filters and resumes if the facet
  // reappears on a later poll.
  const [facetSelection, setFacetSelection] = useState<Map<string, Set<string>>>(new Map());
  const toggleFacetValue = useCallback((pluginId: string, entryId: string, value: string) => {
    const key = `${pluginId}\u0000${entryId}`;
    setFacetSelection((prev) => {
      const next = new Map(prev);
      const values = new Set(next.get(key));
      if (values.has(value)) values.delete(value);
      else values.add(value);
      if (values.size === 0) next.delete(key);
      else next.set(key, values);
      return next;
    });
  }, []);
  const activeFacets = useMemo<ActiveFacet[]>(() => {
    const out: ActiveFacet[] = [];
    for (const f of facetSpecs) {
      const values = facetSelection.get(`${f.pluginId}\u0000${f.entryId}`);
      if (values && values.size > 0) out.push({ pluginId: f.pluginId, column: f.column, values });
    }
    return out;
  }, [facetSpecs, facetSelection]);
  const workspaceMatchesFacets = useCallback(
    (ws: Workspace) =>
      activeFacets.length === 0 || ws.sessions.some((s) => sessionMatchesFacets(pluginUiEntries, s.id, activeFacets)),
    [activeFacets, pluginUiEntries],
  );

  // A facet filter narrows the visible rows, but handleDragEnd rebuilds order
  // from the full group list, so a drag inside a filtered subset could move
  // hidden rows and persist an order the user never saw. Gate reorder off while
  // facets are active, like the computed sort modes. See #2401.
  const dragDisabled = !!readOnly || sortMode === "lastActivity" || pluginSortActive || activeFacets.length > 0;
  // Reorder (group drag + row drag) is also off whenever any visible group
  // forbids it, which is the whole user-group axis: groups have no manual
  // order in v1. Gating here keeps the shared DndContext from firing a
  // reorder on an axis that cannot persist one.
  const reorderDisabled = dragDisabled || groups.some((g) => !g.capabilities.reorder);
  const dragSuppressRef = useRef<number>(0);
  useSuppressClickAfterDrag(dragSuppressRef);
  const offline = useServerDown();
  const [width, setWidth] = useState(loadSavedWidth);
  // Publish the live width so the TopBar's left zone can size itself to match
  // the column, extending the sidebar's right border up through the header.
  // Updates on every drag frame (cheap: a CSS var write, no React re-render).
  useEffect(() => {
    document.documentElement.style.setProperty("--aoe-sidebar-width", `${width}px`);
  }, [width]);
  const [filterOpen, setFilterOpen] = useState(false);
  const [filterQuery, setFilterQuery] = useState("");
  const [facetOpen, setFacetOpen] = useState(false);
  const [sunkExpanded, setSunkExpanded] = useState<boolean>(loadSunkExpanded);
  const toggleSunkExpanded = useCallback(() => {
    setSunkExpanded((prev) => {
      const next = !prev;
      safeSetItem(SUNK_EXPANDED_KEY, next ? "true" : "false");
      return next;
    });
  }, []);
  const [optimisticActive, setOptimisticActive] = useState<{
    id: string;
    fromActiveId: string | null;
  } | null>(null);
  const filterRef = useRef<HTMLInputElement>(null);
  const dragging = useRef(false);
  // Drop the optimistic hint once the parent's activeId has moved off
  // fromActiveId. Otherwise a later navigation back to fromActiveId
  // (e.g. browser back, deep link) would re-engage the stale id and
  // highlight the wrong row. Adjusting state during render is the
  // pattern React docs recommend for derived resets like this.
  if (optimisticActive && optimisticActive.fromActiveId !== activeId) {
    setOptimisticActive(null);
  }
  const displayedActiveId = optimisticActive?.fromActiveId === activeId ? optimisticActive.id : activeId;

  // Whole-row drag. Desktop uses distance activation so a deliberate
  // but stationary click still navigates; touch keeps a long-press delay
  // so scroll-flicks and taps do not reorder rows.
  const sensors = useSensors(
    useSensor(MouseSensor, { activationConstraint: { distance: 8 } }),
    useSensor(TouchSensor, {
      activationConstraint: { delay: 150, tolerance: 8 },
    }),
  );

  const handleDragEnd = useCallback(
    (e: DragEndEvent) => {
      const { active, over } = e;
      if (!over || active.id === over.id) return;

      // Two sortable layers share this context: group headers and session
      // rows. Branch on the typed `data` payload, not on the id shape
      // (repo paths vs uuid-like ids), so the dispatch stays robust if
      // either id domain changes. Typed collision detection already keeps
      // a header drag from landing on a row; the guard here is belt and
      // braces. See #1644.
      if (active.data.current?.type === "group") {
        if (over.data.current?.type !== "group") return;
        // Persist the full visible group order, synthetic groups
        // included: they default to the bottom but can be dragged to any
        // position, after which their stored rank holds. See #1644.
        const orderedIds = groups.map((g) => g.id);
        const oldIndex = orderedIds.indexOf(String(active.id));
        const newIndex = orderedIds.indexOf(String(over.id));
        if (oldIndex < 0 || newIndex < 0) return;
        onReorderGroups(arrayMove(orderedIds, oldIndex, newIndex));
        return;
      }

      // Workspace-row drag is constrained to within a single repo group
      // (each group has its own SortableContext), so finding the active
      // group and reordering inside it is sufficient.
      const groupIndex = groups.findIndex((g) => g.workspaces.some((v) => v.key === active.id));
      const group = groups[groupIndex];
      if (groupIndex < 0 || !group) return;
      const oldIndex = group.workspaces.findIndex((v) => v.key === active.id);
      const newIndex = group.workspaces.findIndex((v) => v.key === over.id);
      if (oldIndex < 0 || newIndex < 0) return;

      // Build the new full visual order by replacing the affected
      // group's local order, then concat in the existing group order.
      // We persist the full flat list of workspace ids so cross-device
      // clients can render the same layout without re-deriving per-group
      // ordering. Reorder only ever runs on the repo axis (the user-group
      // axis disables drag), where each view key equals its workspace id.
      const reordered = arrayMove(group.workspaces, oldIndex, newIndex);
      const flat: string[] = [];
      groups.forEach((g, i) => {
        const views = i === groupIndex ? reordered : g.workspaces;
        views.forEach((v) => flat.push(v.workspace.id));
      });
      onReorderWorkspaces(flat);
    },
    [groups, onReorderWorkspaces, onReorderGroups],
  );

  // All workspaces flattened across groups, deduped by id. A workspace can
  // appear under more than one group (group axis), so without the dedupe the
  // same id would surface twice in `selectedWorkspaces` and bulk actions
  // would fan out to the same session more than once. First occurrence wins.
  const allWorkspaces = useMemo(() => {
    const seen = new Set<string>();
    const out: Workspace[] = [];
    for (const g of groups) {
      for (const v of g.workspaces) {
        if (seen.has(v.workspace.id)) continue;
        seen.add(v.workspace.id);
        out.push(v.workspace);
      }
    }
    return out;
  }, [groups]);

  // Optimistic triage overlay + single-id PATCH wiring, lifted out of
  // SessionRow so single-row and (in #1724) bulk actions share one source of
  // truth. Triage always targets the workspace's primary session.
  const triage = useSidebarTriage(allWorkspaces);

  const q = filterQuery.trim().toLowerCase();

  const isNested = axis === "repo+group";

  // A row survives the text query when there is none, or it matches the
  // workspace/group name; a plugin facet filter (#2401) is ANDed on top, so an
  // active facet narrows rows even with an empty text query. `hasFilter` gates
  // whether the list is filtered at all.
  const hasFilter = !!q || activeFacets.length > 0;
  const textMatches = (v: SidebarWorkspaceView, ...groupNames: string[]) =>
    !q || workspaceMatchesFilter(v.workspace, q) || groupNames.some((n) => n.toLowerCase().includes(q));

  const filteredGroups = hasFilter
    ? groups
        .map((g) => ({
          ...g,
          workspaces: g.workspaces.filter((v) => textMatches(v, g.displayName) && workspaceMatchesFacets(v.workspace)),
        }))
        .filter((g) => g.workspaces.length > 0)
    : groups;

  // Filter the nested model the same way the flat list is filtered: a row
  // survives if it matches, or if its subgroup or repo header name matches;
  // empty subgroups and then empty repos drop out. See #1720.
  const filteredNested: NestedSidebarGroup[] = hasFilter
    ? nestedGroups
        .map((ng) => ({
          repo: ng.repo,
          subgroups: ng.subgroups
            .map((sg) => ({
              ...sg,
              workspaces: sg.workspaces.filter(
                (v) => textMatches(v, sg.displayName, ng.repo.displayName) && workspaceMatchesFacets(v.workspace),
              ),
            }))
            .filter((sg) => sg.workspaces.length > 0),
        }))
        .filter((ng) => ng.subgroups.length > 0)
    : nestedGroups;

  // A filter query that matches only a saved project (no live session) still
  // populates the Projects section, so it must not trigger the "No matches"
  // empty state below it. The no-query empty state ("No sessions yet") is left
  // alone: saved projects are not sessions.
  const savedProjectsMatchQuery =
    !!q && savedProjects.some((p) => p.displayName.toLowerCase().includes(q) || p.repoPath.toLowerCase().includes(q));
  const hasResults = (isNested ? filteredNested.length > 0 : filteredGroups.length > 0) || savedProjectsMatchQuery;

  // Sidebar multi-select. Selection is ephemeral sidebar UI state (not routed
  // or persisted); the anchor pivots Shift+click ranges. See #1724.
  const [selection, dispatchSelection] = useReducer(selectionReducer, EMPTY_SELECTION);

  // Workspace ids in the exact order they render, so a Shift+click range
  // spans only what the user can see: collapsed groups and (when collapsed)
  // the sunk section contribute no rows, and a filter trims to matches. This
  // must mirror the render below; both walk filteredGroups the same way.
  const flatRenderedOrder = useMemo(() => {
    const ids: string[] = [];
    const seen = new Set<string>();
    // A workspace that renders under more than one group still resolves to a
    // single selectable id, so the range order keeps only its first
    // occurrence; pushing it twice would make Shift+range math ambiguous.
    const push = (id: string) => {
      if (seen.has(id)) return;
      seen.add(id);
      ids.push(id);
    };
    for (const g of filteredGroups) {
      if (!sidebarGroupHasLiveWorkspace(g)) continue;
      const expanded = hasFilter ? true : !g.collapsed;
      if (!expanded) continue;
      for (const v of g.workspaces) {
        if (!workspaceIsSunk(v.workspace)) push(v.workspace.id);
      }
    }
    if (sunkExpanded) {
      for (const g of filteredGroups) {
        for (const v of g.workspaces) {
          if (workspaceIsSunk(v.workspace)) push(v.workspace.id);
        }
      }
    }
    return ids;
  }, [filteredGroups, hasFilter, sunkExpanded]);

  // Drop selected ids for workspaces that no longer exist (a session was
  // deleted or moved). Existence-based, not visibility-based: collapsing a
  // group or filtering keeps the selection, matching file-manager behavior;
  // only a vanished workspace is pruned. Range math above is already scoped
  // to the visible order.
  const existingWorkspaceIds = useMemo(() => new Set(allWorkspaces.map((w) => w.id)), [allWorkspaces]);
  useEffect(() => {
    dispatchSelection({ type: "prune", validIds: existingWorkspaceIds });
  }, [existingWorkspaceIds]);

  const clearSelection = useCallback(() => dispatchSelection({ type: "clear" }), []);

  // Read-only viewers can't act on a selection (the bulk bar is hidden), so
  // never let one accumulate: drop any existing selection when read-only
  // turns on. Row clicks are forced down the navigate path below.
  if (readOnly && selection.selectedIds.size > 0) {
    dispatchSelection({ type: "clear" });
  }

  // Selected workspaces (existing ones only) and their per-action eligibility
  // buckets, for the bulk bar. Deduped against existence so a stale id never
  // resolves to a phantom workspace.
  const selectedWorkspaces = useMemo(
    () => allWorkspaces.filter((w) => selection.selectedIds.has(w.id)),
    [allWorkspaces, selection.selectedIds],
  );

  // Run a bulk triage action over its eligible subset, then summarize and
  // clear the selection. One summary toast instead of per-row toasts.
  const runBulkAction = useCallback(
    async (verb: string, run: () => Promise<readonly { ok: boolean; skipped?: boolean }[]>) => {
      const results = await run();
      const summary = summarizeBulkResults(verb, results);
      if (results.some((r) => !r.ok && !r.skipped)) reportError(summary);
      else reportInfo(summary);
      clearSelection();
    },
    [clearSelection],
  );

  const onBulkPin = useCallback(
    (wss: Workspace[], pinned: boolean) =>
      void runBulkAction(pinned ? "Pinned" : "Unpinned", () => triage.bulkPin(wss, pinned)),
    [runBulkAction, triage],
  );
  const onBulkArchive = useCallback(
    (wss: Workspace[], archived: boolean) =>
      void runBulkAction(archived ? "Archived" : "Unarchived", () => triage.bulkArchive(wss, archived)),
    [runBulkAction, triage],
  );
  const onBulkSnooze = useCallback(
    (wss: Workspace[], minutes: number | null) =>
      void runBulkAction(minutes == null ? "Unsnoozed" : "Snoozed", () => triage.bulkSnooze(wss, minutes)),
    [runBulkAction, triage],
  );

  // Live selection state behind a ref so `rowBulkApi` keeps a stable identity:
  // passing the selection arrays/handlers to every memo'd SessionRow would
  // defeat React.memo. Refreshed on every render. See #2312.
  const bulkStateRef = useRef({
    selectedIds: selection.selectedIds,
    selectedWorkspaces,
    optimisticFor: triage.optimisticFor,
    readOnly,
    onBulkPin,
    onBulkArchive,
    onBulkSnooze,
  });
  // Keep the ref current after each commit (prepareScope only reads it on a
  // user right-click, well after the effect has run). Writing it during render
  // trips react-hooks/refs.
  useLayoutEffect(() => {
    bulkStateRef.current = {
      selectedIds: selection.selectedIds,
      selectedWorkspaces,
      optimisticFor: triage.optimisticFor,
      readOnly,
      onBulkPin,
      onBulkArchive,
      onBulkSnooze,
    };
  });
  const rowBulkApi = useMemo<RowBulkApi>(
    () => ({
      prepareScope: (ws) => {
        const { selectedIds, selectedWorkspaces, optimisticFor, readOnly } = bulkStateRef.current;
        if (selectedIds.has(ws.id) && selectedIds.size > 1) {
          return {
            kind: "bulk",
            count: selectedWorkspaces.length,
            buckets: bucketSelectionForBulk(selectedWorkspaces, optimisticFor),
          };
        }
        // Right-clicking a row outside the selection makes it the sole
        // selection (without navigating), matching file-manager behavior. Skip
        // it for read-only viewers who can't act on a selection anyway.
        if (!readOnly && !(selectedIds.has(ws.id) && selectedIds.size === 1)) {
          dispatchSelection({ type: "select-only", id: ws.id });
        }
        return { kind: "single" };
      },
      pin: (wss, pinned) => bulkStateRef.current.onBulkPin(wss, pinned),
      archive: (wss, archived) => bulkStateRef.current.onBulkArchive(wss, archived),
      snooze: (wss, minutes) => bulkStateRef.current.onBulkSnooze(wss, minutes),
    }),
    [],
  );

  // Escape clears the multi-selection (plain-clicking a row also clears it via
  // navigate). Ignored while typing in an input/textarea. See #2312.
  useEffect(() => {
    if (selection.selectedIds.size === 0) return;
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      const t = e.target as HTMLElement | null;
      if (t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.isContentEditable)) return;
      dispatchSelection({ type: "clear" });
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [selection.selectedIds.size]);

  // Archive every active session under a group at once. Archiving a whole
  // project is a bigger hammer than a single row, so it confirms first
  // (matching the TUI's `z`-over-a-project prompt). Reversible, so a plain
  // confirm rather than a destructive warning; the bulk fan-out reuses the
  // same per-session path as multi-select archive. The caller must pass the
  // unfiltered group (see `fullGroup`/`fullSubgroup` below) so an active
  // search filter doesn't shrink "archive all" to the visible matches.
  const onArchiveGroup = useCallback(
    (group: SidebarGroup) => {
      const wss = archivableWorkspaces(group);
      if (wss.length === 0) return;
      const noun = wss.length === 1 ? "session" : "sessions";
      if (!window.confirm(`Archive all ${wss.length} ${noun} in "${group.displayName}"?`)) return;
      onBulkArchive(wss, true);
    },
    [onBulkArchive],
  );

  // Unfiltered flat-axis groups keyed by id, so the group header's
  // "Archive all" count and action can resolve full project membership even
  // while a search filter has sliced the rendered `workspaces`. The nested
  // repo header already carries full membership (`filteredNested` copies
  // `ng.repo` unchanged); only the flat header and nested subgroups need it.
  const groupById = useMemo(() => new Map(groups.map((g) => [g.id, g])), [groups]);

  // Interpret a row click: plain click clears the selection and navigates
  // (today's behavior), modifier clicks build the selection instead. The row
  // has already guarded button / deleting / drag, and called preventDefault.
  const handleRowActivate = useCallback(
    (workspaceId: string, e: { metaKey: boolean; ctrlKey: boolean; shiftKey: boolean }) => {
      // Read-only: ignore modifier gestures entirely and always navigate, so
      // no hidden selection state can build up.
      if (readOnly) {
        dispatchSelection({ type: "clear" });
        setOptimisticActive({ id: workspaceId, fromActiveId: activeId });
        onSelect(workspaceId);
        return;
      }
      const intent = classifyClick(e);
      switch (intent) {
        case "navigate":
          dispatchSelection({ type: "navigate", id: workspaceId });
          setOptimisticActive({ id: workspaceId, fromActiveId: activeId });
          onSelect(workspaceId);
          break;
        case "toggle":
          dispatchSelection({ type: "toggle", id: workspaceId });
          break;
        case "range":
          dispatchSelection({
            type: "range",
            targetId: workspaceId,
            orderedIds: flatRenderedOrder,
            additive: false,
          });
          break;
        case "additive-range":
          dispatchSelection({
            type: "range",
            targetId: workspaceId,
            orderedIds: flatRenderedOrder,
            additive: true,
          });
          break;
      }
    },
    [activeId, onSelect, flatRenderedOrder, readOnly],
  );

  const toggleFilter = () => {
    setFilterOpen((o) => {
      if (o) setFilterQuery("");
      return !o;
    });
    if (!filterOpen) {
      requestAnimationFrame(() => filterRef.current?.focus());
    }
  };

  const handleMouseDown = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    dragging.current = true;
    document.body.style.cursor = "col-resize";
    document.body.style.userSelect = "none";
  }, []);

  useEffect(() => {
    const handleMouseMove = (e: MouseEvent) => {
      if (!dragging.current) return;
      const newWidth = Math.min(MAX_WIDTH, Math.max(MIN_WIDTH, e.clientX));
      setWidth(newWidth);
    };

    const handleMouseUp = () => {
      if (!dragging.current) return;
      dragging.current = false;
      document.body.style.cursor = "";
      document.body.style.userSelect = "";
      setWidth((w) => {
        safeSetItem(SIDEBAR_WIDTH_KEY, String(w));
        return w;
      });
    };

    document.addEventListener("mousemove", handleMouseMove);
    document.addEventListener("mouseup", handleMouseUp);
    return () => {
      document.removeEventListener("mousemove", handleMouseMove);
      document.removeEventListener("mouseup", handleMouseUp);
    };
  }, []);

  return (
    <>
      <div
        className={`fixed top-12 inset-x-0 bottom-0 z-30 md:hidden transition-opacity duration-300 ${
          open ? "bg-black/50" : "opacity-0 pointer-events-none"
        }`}
        onClick={onToggle}
      />
      <div
        {...tourAnchor(TOUR_ANCHORS.sidebar)}
        style={{ width }}
        className={`fixed top-12 bottom-0 left-0 z-40 md:static md:z-auto bg-surface-800 border-r border-surface-700/60 flex flex-col md:h-full shrink-0 transition-transform duration-300 ease-in-out md:transition-none ${
          open ? "translate-x-0" : "-translate-x-full md:hidden"
        }`}
      >
        <div className="px-3 pt-3 pb-1 flex items-center">
          <span data-testid="sidebar-axis-heading" className="text-sm text-text-muted flex-1">
            {AXIS_HEADING[axis]}
          </span>
          <Tooltip text={AXIS_TOOLTIP[axis]}>
            <button
              onClick={() => onAxisChange(NEXT_AXIS[axis])}
              aria-pressed={axis !== "repo"}
              aria-label={axis === "repo" ? AXIS_ARIA[axis] : `${AXIS_ARIA[axis]}, currently pressed`}
              data-testid="sidebar-axis-toggle"
              data-axis={axis}
              className={`w-8 h-8 flex items-center justify-center cursor-pointer rounded-md transition-colors ${
                axis !== "repo" ? "text-brand-500" : "text-text-dim hover:text-text-secondary"
              }`}
            >
              <Layers className="h-3.5 w-3.5" />
            </button>
          </Tooltip>
          <SidebarSortPicker
            sortMode={sortMode}
            onSortModeChange={onSortModeChange}
            pluginSorts={pluginSorts}
            pluginSortRef={pluginSortRef}
            onPluginSortChange={onPluginSortChange}
          />
          {facetSpecs.length > 0 && (
            <Tooltip text="Plugin facets">
              <button
                onClick={() => setFacetOpen((o) => !o)}
                aria-haspopup="true"
                aria-expanded={facetOpen}
                aria-label="Plugin facet filters"
                data-testid="sidebar-facet-toggle"
                className={`relative w-8 h-8 flex items-center justify-center cursor-pointer rounded-md transition-colors ${
                  activeFacets.length > 0 || facetOpen ? "text-brand-500" : "text-text-dim hover:text-text-secondary"
                }`}
              >
                <ListFilter className="h-3.5 w-3.5" />
                {activeFacets.length > 0 && (
                  <span className="absolute -right-0.5 -top-0.5 h-2 w-2 rounded-full bg-brand-500" aria-hidden />
                )}
              </button>
            </Tooltip>
          )}
          <Tooltip text="Filter">
            <button
              onClick={toggleFilter}
              className={`w-8 h-8 flex items-center justify-center cursor-pointer rounded-md transition-colors ${
                filterOpen ? "text-text-secondary" : "text-text-dim hover:text-text-secondary"
              }`}
              aria-label="Filter sessions"
            >
              <svg
                width="14"
                height="14"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                strokeWidth="2"
                strokeLinecap="round"
                strokeLinejoin="round"
              >
                <polygon points="22 3 2 3 10 12.46 10 19 14 21 14 12.46 22 3" />
              </svg>
            </button>
          </Tooltip>
          <Tooltip text={offline ? OFFLINE_TITLE : "New project session"}>
            <button
              onClick={onNew}
              disabled={offline}
              className="w-8 h-8 flex items-center justify-center text-text-muted hover:text-text-secondary hover:bg-surface-800 cursor-pointer rounded-md transition-colors disabled:opacity-40 disabled:cursor-not-allowed disabled:hover:text-text-muted disabled:hover:bg-transparent"
              aria-label="New project session"
            >
              <svg
                width="16"
                height="16"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                strokeWidth="1.5"
                strokeLinecap="round"
                strokeLinejoin="round"
              >
                <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z" />
                <line x1="12" y1="11" x2="12" y2="17" />
                <line x1="9" y1="14" x2="15" y2="14" />
              </svg>
            </button>
          </Tooltip>
          <button
            onClick={onToggle}
            className="md:hidden w-8 h-8 flex items-center justify-center text-text-dim hover:text-text-secondary cursor-pointer rounded-md hover:bg-surface-800 ml-1"
          >
            &times;
          </button>
        </div>

        {filterOpen && (
          <div className="px-3 pb-2">
            <input
              ref={filterRef}
              type="text"
              value={filterQuery}
              onChange={(e) => setFilterQuery(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Escape") toggleFilter();
              }}
              placeholder="Filter by name, branch, agent..."
              data-testid="sidebar-filter-input"
              className="w-full bg-surface-800 border border-surface-700 rounded-md px-2.5 py-1.5 text-[13px] text-text-primary placeholder:text-text-dim focus:border-brand-600 focus:outline-none"
            />
          </div>
        )}

        {facetOpen && facetSpecs.length > 0 && (
          <div className="px-3 pb-2 flex flex-col gap-2" data-testid="sidebar-facet-panel">
            {facetSpecs.map((facet) => {
              const selected = facetSelection.get(`${facet.pluginId}\u0000${facet.entryId}`);
              return (
                <div key={`${facet.pluginId}:${facet.entryId}`} data-plugin-id={facet.pluginId}>
                  <div className="text-[11px] font-semibold uppercase tracking-wide text-text-dim mb-1">
                    {facet.label}
                  </div>
                  <div className="flex flex-wrap gap-1">
                    {facet.options.map((opt) => {
                      const on = selected?.has(opt.value) ?? false;
                      return (
                        <button
                          key={opt.value}
                          type="button"
                          aria-pressed={on}
                          data-testid={`sidebar-facet-option-${facet.entryId}-${opt.value}`}
                          onClick={() => toggleFacetValue(facet.pluginId, facet.entryId, opt.value)}
                          className={`inline-flex items-center rounded-full px-2 py-0.5 text-[11px] font-mono cursor-pointer transition-colors ${
                            on
                              ? "bg-brand-500/15 text-brand-500 ring-1 ring-brand-500/40"
                              : `bg-surface-700/40 hover:bg-surface-700/70 ${toneTextClass(opt.tone)}`
                          }`}
                        >
                          {opt.label}
                        </button>
                      );
                    })}
                  </div>
                </div>
              );
            })}
          </div>
        )}

        <div className="flex-1 overflow-y-auto overflow-x-hidden border-t border-surface-700/60">
          {!isNested && (
            <DragSuppressContext.Provider value={dragSuppressRef}>
              <DndContext
                sensors={sensors}
                collisionDetection={typedClosestCenter}
                onDragEnd={reorderDisabled ? undefined : handleDragEnd}
              >
                {(() => {
                  const liveGroups = filteredGroups.filter(sidebarGroupShouldRender);
                  // Every visible group is sortable, synthetic Multi-repo /
                  // Scratch included: they default to the bottom but can be
                  // dragged to any position. Group drag is off while a filter
                  // is active (the visible list is a partial projection),
                  // whenever row drag is off (read-only or last-activity
                  // sort), or on the user-group axis (no manual order). See
                  // #1644, #1234.
                  const sortableGroupIds = liveGroups.map((g) => g.id);
                  const groupDragDisabled = reorderDisabled || q.length > 0;

                  const renderGroupBody = (group: SidebarGroup, dragHandle?: DragHandleProps) => {
                    const showExpanded = hasFilter ? true : !group.collapsed;
                    const hasActiveChild = group.workspaces.some((v) => v.workspace.id === displayedActiveId);
                    // Header archive count + action operate on the full group,
                    // not the filter-sliced one, so "Archive all" never silently
                    // skips hidden members. Rows below still render `group`.
                    const fullGroup = groupById.get(group.id) ?? group;
                    return (
                      <>
                        <SidebarGroupHeader
                          group={{ ...fullGroup, collapsed: !showExpanded }}
                          hasActiveChild={!showExpanded && hasActiveChild}
                          onClick={() => !hasFilter && onToggleGroup(group.id)}
                          onUpdateAppearance={onUpdateRepoAppearance}
                          onArchiveAll={readOnly || offline ? undefined : () => onArchiveGroup(fullGroup)}
                          onPin={readOnly || offline ? undefined : onPinProject}
                          onUnpin={readOnly || offline ? undefined : onUnpinProject}
                          onNewSession={() =>
                            group.capabilities.create === "repo" && group.repoPath
                              ? onCreateSession(group.repoPath)
                              : onNew()
                          }
                          offline={offline}
                          dragHandle={dragHandle}
                        />
                        {showExpanded &&
                          (() => {
                            // Each group renders only its live tier. Sunk
                            // workspaces (archived or actively snoozed across
                            // every session) are pulled out into a single
                            // global "Snoozed & archived" section at the very
                            // bottom of the sidebar, rather than one footer
                            // per repo group. See #1581.
                            const liveWorkspaces = group.workspaces.filter((v) => !workspaceIsSunk(v.workspace));
                            return (
                              <SortableContext
                                items={liveWorkspaces.map((v) => v.key)}
                                strategy={verticalListSortingStrategy}
                              >
                                {liveWorkspaces.map((v) => (
                                  <SortableSessionRow
                                    key={v.key}
                                    rowKey={v.key}
                                    workspace={v.workspace}
                                    isActive={v.workspace.id === displayedActiveId}
                                    isSelected={!readOnly && selection.selectedIds.has(v.workspace.id)}
                                    onActivate={(e) => handleRowActivate(v.workspace.id, e)}
                                    onDelete={onDeleteSession}
                                    onStop={onStopSession}
                                    onStart={onStartSession}
                                    onCreateSession={onCreateSession}
                                    readOnly={readOnly}
                                    optimistic={triage.optimisticFor(v.workspace.id)}
                                    onPinToggle={triage.pinToggle}
                                    onArchiveToggle={triage.archiveToggle}
                                    onSnooze={triage.snooze}
                                    onUnreadToggle={triage.unreadToggle}
                                    bulkApi={rowBulkApi}
                                    // Drag is disabled when the tier
                                    // comparator already controls placement:
                                    // lastActivity mode has no manual
                                    // concept, pinned rows always float to
                                    // the top of their group, and the
                                    // user-group axis has no manual order.
                                    // See #1581, #1234.
                                    dragDisabled={reorderDisabled || workspaceIsPinned(v.workspace)}
                                  />
                                ))}
                              </SortableContext>
                            );
                          })()}
                      </>
                    );
                  };

                  return (
                    <SortableContext items={sortableGroupIds} strategy={verticalListSortingStrategy}>
                      {liveGroups.map((group) => (
                        <SortableRepoGroup key={group.id} groupId={group.id} disabled={groupDragDisabled}>
                          {(handle) =>
                            // Hide the grip when group drag is off (the
                            // visible order is computed or filtered) so
                            // there is no dead affordance, mirroring how
                            // session rows drop their drag wiring.
                            renderGroupBody(group, groupDragDisabled ? undefined : handle)
                          }
                        </SortableRepoGroup>
                      ))}
                    </SortableContext>
                  );
                })()}
              </DndContext>
            </DragSuppressContext.Provider>
          )}
          {isNested &&
            filteredNested.filter(nestedSidebarGroupShouldRender).map((ng) => {
              const repo = ng.repo;
              const repoExpanded = hasFilter ? true : !repo.collapsed;
              const repoHasActiveChild = ng.subgroups.some((sg) =>
                sg.workspaces.some((v) => v.workspace.id === displayedActiveId),
              );
              return (
                <div key={repo.id} data-testid="sidebar-nested-repo" data-repo-id={repo.id}>
                  <SidebarGroupHeader
                    group={{ ...repo, collapsed: !repoExpanded }}
                    hasActiveChild={!repoExpanded && repoHasActiveChild}
                    onClick={() => !hasFilter && onToggleGroup(repo.id)}
                    onUpdateAppearance={onUpdateRepoAppearance}
                    onArchiveAll={readOnly || offline ? undefined : () => onArchiveGroup(repo)}
                    onPin={readOnly || offline ? undefined : onPinProject}
                    onUnpin={readOnly || offline ? undefined : onUnpinProject}
                    onNewSession={() =>
                      repo.capabilities.create === "repo" && repo.repoPath ? onCreateSession(repo.repoPath) : onNew()
                    }
                    offline={offline}
                  />
                  {repoExpanded &&
                    ng.subgroups.filter(sidebarGroupHasLiveWorkspace).map((sg) => {
                      const groupPath = sg.groupPath ?? "";
                      const subExpanded = hasFilter ? true : !sg.collapsed;
                      const subHasActiveChild = sg.workspaces.some((v) => v.workspace.id === displayedActiveId);
                      // Sunk rows are pulled into the single global
                      // footer below, exactly like the flat axes, so
                      // each subgroup renders only its live tier.
                      const liveWorkspaces = sg.workspaces.filter((v) => !workspaceIsSunk(v.workspace));
                      // Resolve the unfiltered subgroup so "Archive all"
                      // covers the whole subgroup, not just filter matches.
                      const fullSubgroup =
                        nestedGroups
                          .find((n) => n.repo.id === repo.id)
                          ?.subgroups.find((s) => (s.groupPath ?? "") === groupPath) ?? sg;
                      return (
                        <div
                          key={`${repo.id}::${groupPath}`}
                          className="pl-3"
                          data-testid="sidebar-nested-subgroup"
                          data-repo-id={repo.id}
                        >
                          <SidebarGroupHeader
                            group={{ ...fullSubgroup, collapsed: !subExpanded }}
                            hasActiveChild={!subExpanded && subHasActiveChild}
                            onClick={() => !hasFilter && onToggleSubgroup(repo.id, groupPath)}
                            onUpdateAppearance={onUpdateRepoAppearance}
                            onArchiveAll={readOnly || offline ? undefined : () => onArchiveGroup(fullSubgroup)}
                            onNewSession={onNew}
                            offline={offline}
                          />
                          {subExpanded &&
                            liveWorkspaces.map((v) => (
                              <SessionRow
                                key={`${repo.id}::${groupPath}::${v.key}`}
                                workspace={v.workspace}
                                isActive={v.workspace.id === displayedActiveId}
                                isSelected={!readOnly && selection.selectedIds.has(v.workspace.id)}
                                onActivate={(e) => handleRowActivate(v.workspace.id, e)}
                                onDelete={onDeleteSession}
                                onStop={onStopSession}
                                onStart={onStartSession}
                                readOnly={readOnly}
                                optimistic={triage.optimisticFor(v.workspace.id)}
                                onPinToggle={triage.pinToggle}
                                onArchiveToggle={triage.archiveToggle}
                                onSnooze={triage.snooze}
                                onUnreadToggle={triage.unreadToggle}
                                bulkApi={rowBulkApi}
                                indented
                              />
                            ))}
                        </div>
                      );
                    })}
                </div>
              );
            })}
          {(() => {
            // Single global "Snoozed & archived" section at the very
            // bottom of the sidebar. Aggregates sunk workspaces from
            // every repo group (live filtered) so users see one
            // collapsible bucket rather than one footer per repo.
            // Rows are listed flat in the order they appear inside
            // their respective groups; each row's SessionRow already
            // surfaces the title/branch/repo chips that anchor it to
            // its project. The nested axis flattens across its
            // repo -> subgroup tree to feed the same bucket. See #1581,
            // #1720.
            const sunkWorkspaces = isNested
              ? filteredNested.flatMap((ng) =>
                  ng.subgroups.flatMap((sg) =>
                    sg.workspaces.filter((v) => workspaceIsSunk(v.workspace) && !workspaceIsTrashed(v.workspace)),
                  ),
                )
              : filteredGroups.flatMap((g) =>
                  g.workspaces.filter((v) => workspaceIsSunk(v.workspace) && !workspaceIsTrashed(v.workspace)),
                );
            if (sunkWorkspaces.length === 0) return null;
            return (
              <div data-testid="sidebar-sunk-section">
                <button
                  onClick={toggleSunkExpanded}
                  data-testid="sidebar-sunk-toggle"
                  aria-expanded={sunkExpanded}
                  className="w-full flex items-center gap-2 px-3 py-1.5 text-[11px] font-mono uppercase tracking-widest text-text-muted hover:text-text-secondary hover:bg-surface-800/40 cursor-pointer transition-colors border-t border-surface-800/60"
                >
                  <svg
                    width="10"
                    height="10"
                    viewBox="0 0 10 10"
                    fill="currentColor"
                    className={`shrink-0 transition-transform duration-75 ${sunkExpanded ? "" : "-rotate-90"}`}
                  >
                    <path
                      d="M2 3 L5 6.5 L8 3"
                      fill="none"
                      stroke="currentColor"
                      strokeWidth="1.5"
                      strokeLinecap="round"
                      strokeLinejoin="round"
                    />
                  </svg>
                  <span>Snoozed &amp; archived ({sunkWorkspaces.length})</span>
                </button>
                {sunkExpanded &&
                  sunkWorkspaces.map((v) => (
                    <SessionRow
                      key={v.key}
                      workspace={v.workspace}
                      isActive={v.workspace.id === displayedActiveId}
                      isSelected={!readOnly && selection.selectedIds.has(v.workspace.id)}
                      onActivate={(e) => handleRowActivate(v.workspace.id, e)}
                      onDelete={onDeleteSession}
                      onStop={onStopSession}
                      onStart={onStartSession}
                      readOnly={readOnly}
                      optimistic={triage.optimisticFor(v.workspace.id)}
                      onPinToggle={triage.pinToggle}
                      onArchiveToggle={triage.archiveToggle}
                      onSnooze={triage.snooze}
                      onUnreadToggle={triage.unreadToggle}
                      bulkApi={rowBulkApi}
                      indented
                    />
                  ))}
              </div>
            );
          })()}

          <ProjectsSection
            projects={savedProjects}
            query={q}
            readOnly={readOnly}
            offline={offline}
            onCreateSession={onCreateSession}
            onAddProject={onAddProject}
            onEditProject={onEditProject}
            onRemoveProject={onRemoveProject}
          />

          {!hasResults && hasFilter && (
            <div className="px-4 py-8 text-center">
              <p className="text-sm text-text-muted">No matches for &ldquo;{filterQuery}&rdquo;</p>
            </div>
          )}

          {!hasResults && !hasFilter && (
            <div className="px-4 py-10 text-center" data-testid="sidebar-empty-state">
              <p className="text-sm font-medium text-text-secondary">No sessions yet</p>
              <p className="mt-1 text-[13px] text-text-muted">Create a session to start working in a repo.</p>
              <button
                onClick={onNew}
                disabled={offline}
                className="mt-4 inline-flex items-center gap-1.5 rounded-md bg-brand-600 px-3 py-1.5 text-[13px] font-medium text-white hover:bg-brand-500 cursor-pointer transition-colors disabled:opacity-40 disabled:cursor-not-allowed disabled:hover:bg-brand-600"
              >
                <svg
                  width="14"
                  height="14"
                  viewBox="0 0 24 24"
                  fill="none"
                  stroke="currentColor"
                  strokeWidth="2"
                  strokeLinecap="round"
                  strokeLinejoin="round"
                >
                  <line x1="12" y1="5" x2="12" y2="19" />
                  <line x1="5" y1="12" x2="19" y2="12" />
                </svg>
                New session
              </button>
            </div>
          )}
        </div>

        <div className="border-t border-surface-700/20 p-2 flex items-center gap-1">
          {trashedWorkspaces.length > 0 && (
            <TrashMenu
              trashedWorkspaces={trashedWorkspaces}
              readOnly={readOnly}
              onOpen={handleRowActivate}
              onRestore={(ids) => onRestoreSession?.(ids)}
              onDelete={(id) => onDeleteSession?.(id)}
            />
          )}
          <button
            onClick={onSettings}
            {...tourAnchor(TOUR_ANCHORS.sidebarSettings)}
            className="w-8 h-8 flex items-center justify-center text-text-secondary hover:text-text-primary hover:bg-surface-800/50 cursor-pointer rounded-md transition-colors"
            title="Settings"
            aria-label="Settings"
          >
            <svg
              width="16"
              height="16"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="1.5"
              strokeLinecap="round"
              strokeLinejoin="round"
            >
              <path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z" />
              <circle cx="12" cy="12" r="3" />
            </svg>
          </button>
        </div>
      </div>
      {/* Resize handle (desktop only). Gated on `open`: when the sidebar is
          collapsed the panel is hidden, so the handle must hide too — otherwise
          a dead drag bar lingers at the left edge with nothing to resize. */}
      <div
        data-testid="sidebar-resize-handle"
        onMouseDown={handleMouseDown}
        className={`${open ? "hidden md:block" : "hidden"} w-1 cursor-col-resize shrink-0 bg-surface-800 hover:bg-brand-600/50 transition-colors duration-75`}
      />
    </>
  );
}
