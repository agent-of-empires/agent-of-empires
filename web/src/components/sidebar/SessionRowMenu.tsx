import {
  Archive,
  ArrowLeftRight,
  CircleDot,
  CircleStop,
  FolderPlus,
  GitFork,
  Moon,
  Pin,
  Play,
  Plus,
  ScrollText,
  SquareTerminal,
  Sparkles,
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
          {SNOOZE_PRESETS.map((preset) => (
            <MenuItem
              key={preset.minutes}
              testId="sidebar-context-menu-bulk-snooze"
              icon={icon(Moon)}
              indent
              onClick={act(() => api.snooze(buckets.snoozable, preset.minutes))}
            >
              {preset.label}
            </MenuItem>
          ))}
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
  ["all", "All events"],
];

const selectedClass = (selected: boolean) =>
  `hover:bg-surface-700/50 ${selected ? "text-text-primary" : "text-text-secondary"}`;

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
  const { firstSession: first, acpSession: acp } = model;
  const write = !readOnly;
  return (
    <>
      {write && a.newSession && (
        <MenuItem onClick={a.newSession} testId="sidebar-context-menu-new-session" icon={icon(Plus)}>
          New Session
        </MenuItem>
      )}
      <MenuItem onClick={a.rename} testId="sidebar-context-menu-rename">
        Rename
      </MenuItem>
      {write && model.canEditWorkdir && (
        <MenuItem onClick={a.editWorkdir} testId="sidebar-context-menu-edit-workdir">
          Edit workdir name
        </MenuItem>
      )}
      {write && model.sessionId && model.canAddProject && (
        <MenuItem onClick={a.addProject} testId="sidebar-context-menu-add-project" icon={icon(FolderPlus)}>
          Add project
        </MenuItem>
      )}
      {write && (
        <MenuItem onClick={a.editGroup} testId="sidebar-context-menu-edit-group">
          Edit group
        </MenuItem>
      )}
      {write && first && (first.view === "structured" || first.acp_capable) && (
        <MenuItem onClick={a.switchView} testId="sidebar-context-menu-switch-view" icon={icon(SquareTerminal)}>
          {first.view === "structured" ? "Switch to terminal" : "Switch to structured view"}
        </MenuItem>
      )}
      {write && acp && (
        <MenuItem onClick={a.switchAgent} testId="sidebar-context-menu-switch-agent" icon={icon(ArrowLeftRight)}>
          Switch agent
        </MenuItem>
      )}
      {write && acp?.acp_session_id && acp.acp_can_fork && (
        <MenuItem onClick={a.fork} testId="sidebar-context-menu-fork" icon={icon(GitFork)}>
          Fork session
        </MenuItem>
      )}
      {write && acp?.default_name && (
        <MenuItem onClick={a.autoName} testId="sidebar-context-menu-auto-name" icon={icon(Sparkles)}>
          Auto-name now
        </MenuItem>
      )}
      {write && acp && (
        <MenuItem onClick={a.summarize} testId="sidebar-context-menu-summarize" icon={icon(ScrollText)}>
          Summarize conversation
        </MenuItem>
      )}
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
      <MenuSeparator />
      <MenuHeading>Notifications</MenuHeading>
      {NOTIFY_LABELS.map(([preset, label]) => {
        const selected = model.notifyPreset === preset;
        return (
          <MenuItem key={preset} onClick={() => a.notify(preset)} indent flex className={selectedClass(selected)}>
            <span className="w-3 text-brand-500">{selected ? "✓" : ""}</span>
            {label}
          </MenuItem>
        );
      })}
      {write && colorsEnabled && <ColorItems current={model.sessionColor} onPick={a.color} />}
      {write && (
        <>
          <MenuSeparator />
          <MenuHeading>Triage</MenuHeading>
          <TriageItems model={model} unreadEnabled={unreadEnabled} actions={a} />
          <MenuSeparator />
          <MenuItem
            onClick={a.remove}
            testId="sidebar-context-menu-delete"
            className="text-status-error hover:bg-status-error/10"
          >
            Delete
          </MenuItem>
        </>
      )}
    </>
  );
}

function ColorItems({ current, onPick }: { current: string | null; onPick: (color: string | null) => void }) {
  return (
    <>
      <MenuSeparator />
      <MenuHeading>Color</MenuHeading>
      {SESSION_COLOR_OPTIONS.map((opt) => {
        const selected = current === opt.key;
        return (
          <MenuItem
            key={opt.key}
            onClick={() => onPick(opt.key)}
            testId={`sidebar-context-menu-color-${opt.key}`}
            indent
            flex
            className={selectedClass(selected)}
          >
            <span className={`h-2.5 w-2.5 shrink-0 rounded-full ${opt.dotClass}`} />
            {opt.label}
            {selected && <span className="ml-auto text-brand-500">✓</span>}
          </MenuItem>
        );
      })}
      {current && (
        <MenuItem onClick={() => onPick(null)} testId="sidebar-context-menu-color-clear" indent flex>
          <span className="h-2.5 w-2.5 shrink-0 rounded-full border border-surface-600" />
          Clear color
        </MenuItem>
      )}
    </>
  );
}

/** Gated on the row's triage state so contradictory toggles never show; see `triageMenuShape`. */
function TriageItems({
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
  return (
    <>
      {(shape.showPin || shape.showUnpin) && (
        <MenuItem onClick={a.pin} testId="sidebar-context-menu-pin" icon={<PinIcon />} indent>
          {shape.showPin ? "Pin" : "Unpin"}
        </MenuItem>
      )}
      {(shape.showArchive || shape.showUnarchive) && (
        <MenuItem onClick={a.archive} testId="sidebar-context-menu-archive" icon={icon(Archive)} indent>
          {shape.showArchive ? "Archive" : "Unarchive"}
        </MenuItem>
      )}
      {shape.showSnooze && (
        <MenuItem onClick={a.openSnooze} testId="sidebar-context-menu-snooze" icon={icon(Moon)} indent>
          Snooze…
        </MenuItem>
      )}
      {shape.showUnsnooze && (
        <MenuItem onClick={a.unsnooze} testId="sidebar-context-menu-unsnooze" icon={icon(Moon)} indent>
          Unsnooze
        </MenuItem>
      )}
      {unreadEnabled && (
        <MenuItem onClick={a.unread} testId="sidebar-context-menu-unread" icon={icon(CircleDot)} indent>
          {model.effectiveUnread ? "Mark as read" : "Mark as unread"}
        </MenuItem>
      )}
    </>
  );
}
