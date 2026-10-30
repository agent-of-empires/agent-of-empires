import { useId, useState } from "react";
import {
  Archive,
  ArrowLeftRight,
  ChevronRight,
  CircleDot,
  CircleStop,
  Container,
  FolderPen,
  FolderPlus,
  Focus,
  Folders,
  GitFork,
  Moon,
  Pencil,
  Pin,
  Play,
  Plus,
  RefreshCw,
  ScrollText,
  SquareTerminal,
  Sparkles,
  SquareArrowOutUpRight,
  Trash2,
} from "lucide-react";
import type { SessionResponse } from "../../lib/types";
import { triageMenuShape, triageStateOf } from "../../lib/sidebarSort";
import type { BulkTriageBuckets } from "../../lib/sidebarBulk";
import { MenuChoiceRow, MenuHeading, MenuItem, MenuSeparator, MenuSwatches } from "../ContextMenu";
import { SNOOZE_PRESETS } from "./format";
import { SESSION_COLOR_OPTIONS, type NotifyPreset, type RowModel } from "./rowModel";
import type { SessionAction } from "./sessionActionTargets";
import type { RowBulkApi } from "./types";

const icon = (Icon: typeof Pin, className = "") => <Icon className={`h-3.5 w-3.5 shrink-0 ${className}`.trim()} />;
const PinIcon = () => icon(Pin, "-rotate-45");

/** Count-labelled triage for a multi-selection; single-row actions are absent here. */
export function BulkTriageMenuItems({
  buckets,
  api,
  onDone,
}: {
  buckets: BulkTriageBuckets;
  api: RowBulkApi;
  onDone: () => void;
}) {
  const act = (run: () => void) => () => {
    onDone();
    run();
  };
  const item = (list: typeof buckets.pinnable, verb: string, id: string, glyph: React.ReactNode, run: () => void) =>
    list.length > 0 && (
      <MenuItem testId={`sidebar-context-menu-bulk-${id}`} icon={glyph} onClick={act(run)}>
        {verb} {list.length}
      </MenuItem>
    );
  return (
    <>
      {item(buckets.pinnable, "Pin", "pin", <PinIcon />, () => api.pin(buckets.pinnable, true))}
      {item(buckets.unpinnable, "Unpin", "unpin", <PinIcon />, () => api.pin(buckets.unpinnable, false))}
      {item(buckets.archivable, "Archive", "archive", icon(Archive), () => api.archive(buckets.archivable, true))}
      {item(buckets.unarchivable, "Unarchive", "unarchive", icon(Archive), () =>
        api.archive(buckets.unarchivable, false),
      )}
      {buckets.snoozable.length > 0 && (
        <>
          <MenuHeading>Snooze {buckets.snoozable.length}</MenuHeading>
          <div className="grid grid-cols-4 gap-1 px-2 pb-1">
            {SNOOZE_PRESETS.map((preset) => (
              <button
                key={preset.minutes}
                type="button"
                data-testid="sidebar-context-menu-bulk-snooze"
                onClick={act(() => api.snooze(buckets.snoozable, preset.minutes))}
                className="rounded-md px-1 py-1.5 max-md:py-2.5 text-xs text-text-secondary hover:bg-surface-700/50 hover:text-text-primary cursor-pointer transition-colors"
              >
                {preset.label}
              </button>
            ))}
          </div>
        </>
      )}
      {item(buckets.unsnoozable, "Unsnooze", "unsnooze", icon(Moon), () => api.snooze(buckets.unsnoozable, null))}
    </>
  );
}

export interface SingleRowActions {
  newSession?: () => void;
  rename: () => void;
  editWorkdir: () => void;
  addProject: () => void;
  editGroup: () => void;
  switchView: (sessionId: string) => void;
  switchAgent: (sessionId: string) => void;
  fork: (sessionId: string) => void;
  autoName: (sessionId: string) => void;
  summarize: (sessionId: string) => void;
  /** Shows only this session in the current window (focus mode). */
  showOnly: (sessionId: string) => void;
  /** Opens the session alone in a new window; absent where a second window cannot open (phones). */
  openOwnWindow?: (sessionId: string) => void;
  stop: (sessionId: string) => void;
  start: (sessionId: string) => void;
  restartUpdate: (sessionId: string) => void;
  recreateContainer: (sessionId: string) => void;
  notify: (preset: NotifyPreset) => void;
  color: (color: string | null) => void;
  pin: () => void;
  archive: () => void;
  openSnooze: () => void;
  unsnooze: () => void;
  unread: () => void;
  remove: () => void;
}

const NOTIFY_OPTIONS: { preset: NotifyPreset; label: string; hint: string }[] = [
  { preset: "off", label: "Off", hint: "No notifications from this session" },
  { preset: "default", label: "Default", hint: "Follows your notification settings" },
  { preset: "all", label: "All", hint: "Notifies when waiting, idle, or on error" },
];

/** Icon-over-label button for the quick triage row. */
function QuickAction({
  onClick,
  testId,
  glyph,
  pressed,
  children,
}: {
  onClick: () => void;
  testId: string;
  glyph: React.ReactNode;
  /** For an in-place toggle, which keeps the menu open. */
  pressed?: boolean;
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      data-testid={testId}
      aria-pressed={pressed}
      className={`flex flex-1 min-w-0 flex-col items-center gap-1 rounded-md px-1 py-2 text-[11px] hover:text-text-primary cursor-pointer transition-colors ${
        pressed ? "bg-brand-600/20 text-text-primary" : "text-text-secondary hover:bg-surface-700/50"
      }`}
    >
      {glyph}
      <span className="truncate max-w-full">{children}</span>
    </button>
  );
}

export function SingleRowMenuItems({
  model,
  readOnly,
  colorsEnabled,
  unreadEnabled,
  actions: a,
}: {
  model: RowModel;
  readOnly?: boolean;
  colorsEnabled: boolean;
  unreadEnabled: boolean;
  actions: SingleRowActions;
}) {
  const [moreOpen, setMoreOpen] = useState(false);
  const moreId = useId();
  const [pickerOpen, setPickerOpen] = useState<SessionAction | null>(null);
  const { actionTargets: t } = model;
  const write = !readOnly;
  const item = (
    action: SessionAction,
    props: Omit<TargetedItemProps, "targets" | "open" | "onToggle" | "onRun">,
    run: ((sessionId: string) => void) | undefined = a[action],
  ) =>
    run && (
      <TargetedItem
        {...props}
        targets={t[action]}
        open={pickerOpen === action}
        onToggle={() => setPickerOpen((o) => (o === action ? null : action))}
        onRun={run}
      />
    );
  // More always opens: its window entries change no state, so a read-only dashboard still offers them.
  const more = {
    editWorkdir: write && model.canEditWorkdir,
    addProject: write && !!model.sessionId && model.canAddProject,
  };
  return (
    <>
      {write && <TriageRow model={model} unreadEnabled={unreadEnabled} actions={a} />}
      <MenuSeparator />
      {write && (
        <MenuItem onClick={a.rename} testId="sidebar-context-menu-rename" icon={icon(Pencil)}>
          Rename
        </MenuItem>
      )}
      {write && item("stop", { label: "Stop", testId: "sidebar-context-menu-stop", glyph: icon(CircleStop) })}
      {write && item("start", { label: "Start", testId: "sidebar-context-menu-start", glyph: icon(Play) })}
      {write &&
        item("restartUpdate", {
          label: "Restart to apply update (keeps conversation)",
          testId: "sidebar-context-menu-restart-update",
          glyph: icon(RefreshCw),
        })}
      {write &&
        item("switchView", {
          label: "Switch view",
          singleLabel: (s) => (s.view === "structured" ? "Switch to terminal" : "Switch to structured view"),
          detail: (s) => `${s.status}, to ${s.view === "structured" ? "terminal" : "structured"}`,
          testId: "sidebar-context-menu-switch-view",
          glyph: icon(SquareTerminal),
        })}
      {write && item("fork", { label: "Fork session", testId: "sidebar-context-menu-fork", glyph: icon(GitFork) })}
      {write && a.newSession && (
        <MenuItem onClick={a.newSession} testId="sidebar-context-menu-new-session" icon={icon(Plus)}>
          New session in this project
        </MenuItem>
      )}
      <MenuItem
        onClick={() => setMoreOpen((o) => !o)}
        testId="sidebar-context-menu-more"
        ariaExpanded={moreOpen}
        ariaControls={moreId}
        icon={icon(ChevronRight, `transition-transform ${moreOpen ? "rotate-90" : ""}`)}
      >
        More
      </MenuItem>
      <div id={moreId} hidden={!moreOpen}>
        {item("showOnly", {
          label: "Show only this Session",
          testId: "sidebar-context-menu-show-only",
          glyph: icon(Focus),
          indent: true,
        })}
        {item("openOwnWindow", {
          label: "Open in its own window",
          testId: "sidebar-context-menu-own-window",
          glyph: icon(SquareArrowOutUpRight),
          indent: true,
        })}
        {write &&
          item("switchAgent", {
            label: "Switch agent",
            testId: "sidebar-context-menu-switch-agent",
            glyph: icon(ArrowLeftRight),
            indent: true,
          })}
        {write &&
          item("recreateContainer", {
            label: "Recreate container",
            testId: "sidebar-context-menu-recreate-container",
            glyph: icon(Container),
            indent: true,
          })}
        {write &&
          item("autoName", {
            label: "Auto-name now",
            testId: "sidebar-context-menu-auto-name",
            glyph: icon(Sparkles),
            indent: true,
          })}
        {write &&
          item("summarize", {
            label: "Summarize conversation",
            testId: "sidebar-context-menu-summarize",
            glyph: icon(ScrollText),
            indent: true,
          })}
        {more.editWorkdir && (
          <MenuItem onClick={a.editWorkdir} testId="sidebar-context-menu-edit-workdir" icon={icon(FolderPen)} indent>
            Edit workdir name
          </MenuItem>
        )}
        {more.addProject && (
          <MenuItem onClick={a.addProject} testId="sidebar-context-menu-add-project" icon={icon(FolderPlus)} indent>
            Add project
          </MenuItem>
        )}
        {write && (
          <MenuItem onClick={a.editGroup} testId="sidebar-context-menu-edit-group" icon={icon(Folders)} indent>
            Edit group
          </MenuItem>
        )}
      </div>
      <MenuSeparator />
      <MenuChoiceRow label="Notify" hint={NOTIFY_OPTIONS.find((o) => o.preset === model.notifyPreset)?.hint}>
        <div
          role="group"
          aria-label="Notify"
          className="flex flex-1 rounded-md border border-surface-700 bg-surface-900 p-0.5"
        >
          {NOTIFY_OPTIONS.map(({ preset, label }) => (
            <button
              key={preset}
              type="button"
              onClick={() => a.notify(preset)}
              data-testid={`sidebar-context-menu-notify-${preset}`}
              aria-pressed={model.notifyPreset === preset}
              className={`flex-1 rounded px-2 py-1 max-md:py-2 text-xs cursor-pointer transition-colors ${
                model.notifyPreset === preset
                  ? "bg-brand-600/20 text-text-primary"
                  : "text-text-secondary hover:text-text-primary"
              }`}
            >
              {label}
            </button>
          ))}
        </div>
      </MenuChoiceRow>
      {write && colorsEnabled && (
        <MenuChoiceRow label="Color">
          <MenuSwatches
            options={SESSION_COLOR_OPTIONS.map((o) => ({ key: o.key, label: o.label, className: o.dotClass }))}
            value={model.sessionColor}
            onPick={a.color}
            testIdPrefix="sidebar-context-menu-color"
          />
        </MenuChoiceRow>
      )}
      {write && (
        <>
          <MenuSeparator />
          <MenuItem
            onClick={a.remove}
            testId="sidebar-context-menu-delete"
            icon={icon(Trash2)}
            className="text-status-error hover:bg-status-error/10"
          >
            Delete
          </MenuItem>
        </>
      )}
    </>
  );
}

interface TargetedItemProps {
  label: string;
  /** Label when exactly one sibling is a target and no choice is needed. */
  singleLabel?: (s: SessionResponse) => string;
  detail?: (s: SessionResponse) => string;
  testId: string;
  glyph: React.ReactNode;
  indent?: boolean;
  targets: SessionResponse[];
  open: boolean;
  onToggle: () => void;
  onRun: (sessionId: string) => void;
}

/** A session-scoped action: runs on its only target, or discloses a list of siblings to choose from. */
function TargetedItem({
  label,
  singleLabel,
  detail = (s) => s.status,
  testId,
  glyph,
  indent,
  targets,
  open,
  onToggle,
  onRun,
}: TargetedItemProps) {
  const pickerId = useId();
  const titleOf = (s: SessionResponse) => s.title.trim() || "Untitled session";
  // Siblings can share a title (a worktree's default is its branch) and a status; an id tells them apart.
  const clashes = (s: SessionResponse) =>
    targets.some((t) => t !== s && titleOf(t) === titleOf(s) && detail(t) === detail(s));
  if (targets.length === 0) return null;
  if (targets.length === 1) {
    const only = targets[0]!;
    return (
      <MenuItem onClick={() => onRun(only.id)} testId={testId} icon={glyph} indent={indent}>
        {singleLabel?.(only) ?? label}
      </MenuItem>
    );
  }
  return (
    <>
      <MenuItem
        onClick={onToggle}
        testId={testId}
        ariaExpanded={open}
        ariaControls={pickerId}
        icon={glyph}
        indent={indent}
      >
        {label}…
      </MenuItem>
      <div id={pickerId} hidden={!open} className="pl-3">
        {targets.map((s) => (
          <MenuItem
            key={s.id}
            onClick={() => onRun(s.id)}
            testId={`${testId}-target`}
            indent
            className="text-text-primary hover:bg-surface-700/50"
          >
            <span className="truncate">{titleOf(s)}</span>
            <span className="ml-2 shrink-0 text-xs">
              {detail(s)}
              {clashes(s) && ` #${s.id.slice(0, 8)}`}
            </span>
          </MenuItem>
        ))}
      </div>
    </>
  );
}

/** Pin, archive, snooze and read state as one row, gated so contradictory toggles never show; see `triageMenuShape`. */
function TriageRow({
  model,
  unreadEnabled,
  actions: a,
}: {
  model: RowModel;
  unreadEnabled: boolean;
  actions: SingleRowActions;
}) {
  const shape = triageMenuShape(
    triageStateOf({
      isPinned: model.effectivePinned,
      isArchived: model.effectiveArchived,
      isSnoozed: model.effectiveSnoozed,
    }),
  );
  const glyph = (Icon: typeof Pin, className = "") => <Icon className={`h-4 w-4 ${className}`.trim()} />;
  return (
    <div className="flex gap-1 px-2 py-1">
      {(shape.showPin || shape.showUnpin) && (
        <QuickAction
          onClick={a.pin}
          testId="sidebar-context-menu-pin"
          glyph={glyph(Pin, "-rotate-45")}
          pressed={shape.showUnpin}
        >
          {shape.showPin ? "Pin" : "Pinned"}
        </QuickAction>
      )}
      {(shape.showArchive || shape.showUnarchive) && (
        <QuickAction onClick={a.archive} testId="sidebar-context-menu-archive" glyph={glyph(Archive)}>
          {shape.showArchive ? "Archive" : "Unarchive"}
        </QuickAction>
      )}
      {shape.showSnooze && (
        <QuickAction onClick={a.openSnooze} testId="sidebar-context-menu-snooze" glyph={glyph(Moon)}>
          Snooze
        </QuickAction>
      )}
      {shape.showUnsnooze && (
        <QuickAction onClick={a.unsnooze} testId="sidebar-context-menu-unsnooze" glyph={glyph(Moon)}>
          Unsnooze
        </QuickAction>
      )}
      {unreadEnabled && (
        <QuickAction
          onClick={a.unread}
          testId="sidebar-context-menu-unread"
          glyph={glyph(CircleDot)}
          pressed={model.effectiveUnread}
        >
          Unread
        </QuickAction>
      )}
    </div>
  );
}
