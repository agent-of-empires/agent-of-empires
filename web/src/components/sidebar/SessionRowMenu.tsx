import { useId, useState } from "react";
import {
  Archive,
  ArrowLeftRight,
  ChevronRight,
  CircleDot,
  CircleStop,
  FolderPen,
  FolderPlus,
  Folders,
  GitFork,
  Moon,
  Pencil,
  Pin,
  Play,
  Plus,
  ScrollText,
  SquareTerminal,
  Sparkles,
  Trash2,
} from "lucide-react";
import { triageMenuShape, triageStateOf } from "../../lib/sidebarSort";
import type { BulkTriageBuckets } from "../../lib/sidebarBulk";
import { MenuHeading, MenuItem, MenuSeparator } from "../ContextMenu";
import { SNOOZE_PRESETS } from "./format";
import { SESSION_COLOR_OPTIONS, type NotifyPreset, type RowModel } from "./rowModel";
import type { RowBulkApi } from "./types";

const icon = (Icon: typeof Pin, className = "") => <Icon className={`h-3.5 w-3.5 shrink-0 ${className}`.trim()} />;
const PinIcon = () => icon(Pin, "-rotate-45");

/** Count-labelled triage for a multi-selection; single-row actions are absent here. */
export function BulkTriageMenuItems({
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
      <MenuHeading>{count} selected</MenuHeading>
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
  switchView: () => void;
  switchAgent: () => void;
  fork: () => void;
  autoName: () => void;
  summarize: () => void;
  stop: () => void;
  start: () => void;
  notify: (preset: NotifyPreset) => void;
  color: (color: string | null) => void;
  pin: () => void;
  archive: () => void;
  openSnooze: () => void;
  unsnooze: () => void;
  unread: () => void;
  remove: () => void;
}

const NOTIFY_LABELS: [NotifyPreset, string][] = [
  ["off", "Off"],
  ["default", "Default"],
  ["all", "All"],
];

/** Icon-over-label button for the quick triage row. */
function QuickAction({
  onClick,
  testId,
  glyph,
  children,
}: {
  onClick: () => void;
  testId: string;
  glyph: React.ReactNode;
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      data-testid={testId}
      className="flex flex-1 min-w-0 flex-col items-center gap-1 rounded-md px-1 py-2 max-md:py-2.5 text-[11px] text-text-secondary hover:bg-surface-700/50 hover:text-text-primary cursor-pointer transition-colors"
    >
      {glyph}
      <span className="truncate max-w-full">{children}</span>
    </button>
  );
}

/** A labelled row of inline choices, replacing a heading plus one item per option. */
function ChoiceRow({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex items-center gap-2 px-3 py-1.5 max-md:py-2">
      <span className="w-20 shrink-0 text-xs text-text-dim">{label}</span>
      <div className="flex flex-1 items-center gap-1">{children}</div>
    </div>
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
  const { firstSession: first, acpSession: acp } = model;
  const write = !readOnly;
  const canSwitchView = write && !!first && (first.view === "structured" || first.acp_capable);
  const canFork = write && !!acp?.acp_session_id && acp.acp_can_fork;
  const more = write && {
    switchAgent: !!acp,
    autoName: !!acp,
    summarize: !!acp,
    editWorkdir: model.canEditWorkdir,
    addProject: !!model.sessionId && model.canAddProject,
    editGroup: true,
  };
  return (
    <>
      <div className="px-3 pt-1.5 pb-1 text-xs font-mono text-text-muted truncate max-w-[260px] max-md:max-w-none">
        {model.label}
      </div>
      {write && <TriageRow model={model} unreadEnabled={unreadEnabled} actions={a} />}
      <MenuSeparator />
      <MenuItem onClick={a.rename} testId="sidebar-context-menu-rename" icon={icon(Pencil)}>
        Rename
      </MenuItem>
      {write && model.canStop && (
        <MenuItem onClick={a.stop} testId="sidebar-context-menu-stop" icon={icon(CircleStop)}>
          Stop
        </MenuItem>
      )}
      {write && model.canStart && (
        <MenuItem onClick={a.start} testId="sidebar-context-menu-start" icon={icon(Play)}>
          Start
        </MenuItem>
      )}
      {canSwitchView && (
        <MenuItem onClick={a.switchView} testId="sidebar-context-menu-switch-view" icon={icon(SquareTerminal)}>
          {first!.view === "structured" ? "Switch to terminal" : "Switch to structured view"}
        </MenuItem>
      )}
      {canFork && (
        <MenuItem onClick={a.fork} testId="sidebar-context-menu-fork" icon={icon(GitFork)}>
          Fork session
        </MenuItem>
      )}
      {write && a.newSession && (
        <MenuItem onClick={a.newSession} testId="sidebar-context-menu-new-session" icon={icon(Plus)}>
          New session in this project
        </MenuItem>
      )}
      {more && (
        <>
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
            {more.switchAgent && (
              <MenuItem
                onClick={a.switchAgent}
                testId="sidebar-context-menu-switch-agent"
                icon={icon(ArrowLeftRight)}
                indent
              >
                Switch agent
              </MenuItem>
            )}
            {more.autoName && (
              <MenuItem onClick={a.autoName} testId="sidebar-context-menu-auto-name" icon={icon(Sparkles)} indent>
                Auto-name now
              </MenuItem>
            )}
            {more.summarize && (
              <MenuItem onClick={a.summarize} testId="sidebar-context-menu-summarize" icon={icon(ScrollText)} indent>
                Summarize conversation
              </MenuItem>
            )}
            {more.editWorkdir && (
              <MenuItem
                onClick={a.editWorkdir}
                testId="sidebar-context-menu-edit-workdir"
                icon={icon(FolderPen)}
                indent
              >
                Edit workdir name
              </MenuItem>
            )}
            {more.addProject && (
              <MenuItem onClick={a.addProject} testId="sidebar-context-menu-add-project" icon={icon(FolderPlus)} indent>
                Add project
              </MenuItem>
            )}
            <MenuItem onClick={a.editGroup} testId="sidebar-context-menu-edit-group" icon={icon(Folders)} indent>
              Edit group
            </MenuItem>
          </div>
        </>
      )}
      <MenuSeparator />
      <ChoiceRow label="Notify">
        {NOTIFY_LABELS.map(([preset, label]) => (
          <button
            key={preset}
            type="button"
            onClick={() => a.notify(preset)}
            data-testid={`sidebar-context-menu-notify-${preset}`}
            aria-pressed={model.notifyPreset === preset}
            className={`flex-1 rounded-md px-2 py-1 max-md:py-1.5 text-xs cursor-pointer transition-colors ${
              model.notifyPreset === preset
                ? "bg-surface-700 text-text-primary"
                : "text-text-secondary hover:bg-surface-700/50"
            }`}
          >
            {label}
          </button>
        ))}
      </ChoiceRow>
      {write && colorsEnabled && (
        <ChoiceRow label="Color">
          {SESSION_COLOR_OPTIONS.map((opt) => (
            <button
              key={opt.key}
              type="button"
              onClick={() => a.color(opt.key)}
              data-testid={`sidebar-context-menu-color-${opt.key}`}
              title={opt.label}
              aria-label={opt.label}
              aria-pressed={model.sessionColor === opt.key}
              className={`flex h-7 w-7 max-md:h-9 max-md:w-9 items-center justify-center rounded-md cursor-pointer hover:bg-surface-700/50 ${
                model.sessionColor === opt.key ? "bg-surface-700" : ""
              }`}
            >
              <span className={`h-3 w-3 rounded-full ${opt.dotClass}`} />
            </button>
          ))}
          {model.sessionColor && (
            <button
              type="button"
              onClick={() => a.color(null)}
              data-testid="sidebar-context-menu-color-clear"
              title="Clear color"
              aria-label="Clear color"
              className="flex h-7 w-7 max-md:h-9 max-md:w-9 items-center justify-center rounded-md cursor-pointer hover:bg-surface-700/50"
            >
              <span className="h-3 w-3 rounded-full border border-surface-600" />
            </button>
          )}
        </ChoiceRow>
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
        <QuickAction onClick={a.pin} testId="sidebar-context-menu-pin" glyph={glyph(Pin, "-rotate-45")}>
          {shape.showPin ? "Pin" : "Unpin"}
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
        <QuickAction onClick={a.unread} testId="sidebar-context-menu-unread" glyph={glyph(CircleDot)}>
          {model.effectiveUnread ? "Read" : "Unread"}
        </QuickAction>
      )}
    </div>
  );
}
