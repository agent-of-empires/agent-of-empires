import { memo, useEffect, useRef, useState } from "react";
import { Folder } from "lucide-react";
import { archivableWorkspaces, type SidebarGroup } from "../../lib/sidebarGroups";
import {
  REPO_COLOR_OPTIONS,
  repoColorStyle,
  repoSwatchStyle,
  type RepoAppearanceUpdate,
} from "../../lib/repoAppearance";
import { STATUS_DOT_CLASS } from "../../lib/session";
import { workspaceAttentionCount, workspaceIsSunk } from "../../lib/sidebarSort";
import { useSidebarCompact } from "../../lib/sidebarCompact";
import { OFFLINE_TITLE } from "../../lib/connectionState";
import { ContextMenu, MenuHeading, MenuItem, MenuSeparator } from "../ContextMenu";
import { OwnerAvatar } from "../OwnerAvatar";
import { Tooltip } from "../Tooltip";
import { useContextMenu } from "../useContextMenu";
import { FoldChevron, PlusIcon } from "../icons";
import { DISABLED_ICON_BUTTON } from "./styles";
import type { DragHandleProps } from "./types";

interface Props {
  group: SidebarGroup;
  hasActiveChild: boolean;
  onClick: () => void;
  onNewSession: () => void;
  onUpdateAppearance: (repoId: string, update: RepoAppearanceUpdate) => void;
  /** Omitted (read-only or offline) hides "Archive all"; the parent confirms. */
  onArchiveAll?: () => void;
  /** Registers the repo so it persists with zero sessions; omitted hides it. */
  onPin?: (repoPath: string) => void;
  onUnpin?: (group: SidebarGroup) => void;
  /** Opens project settings, registering the repo first if needed; omitted hides it. */
  onEditProject?: (group: SidebarGroup) => void;
  offline: boolean;
  dragHandle?: DragHandleProps;
}

/** Swallows clicks while the header is dragged and for 250ms after, so a reorder cannot also toggle or create. */
function useDragClickSuppression(isDragging: boolean) {
  const until = useRef(0);
  useEffect(() => {
    if (isDragging) until.current = Number.POSITIVE_INFINITY;
    else if (until.current > Date.now()) until.current = Date.now() + 250;
  }, [isDragging]);
  return (e: React.MouseEvent) => {
    if (until.current > Date.now()) {
      e.preventDefault();
      e.stopPropagation();
    }
  };
}

export const SidebarGroupHeader = memo(function SidebarGroupHeader(props: Props) {
  const { group, hasActiveChild, onClick, onNewSession, offline, dragHandle } = props;
  // Appearance is repo-axis only; archive-all works on every axis.
  const canAppearance = group.capabilities.appearance;
  const archivableCount = props.onArchiveAll ? archivableWorkspaces(group).length : 0;
  const canPin = !!props.onPin && group.capabilities.create === "repo" && !!group.repoPath && !group.pinned;
  const canUnpin = !!props.onUnpin && group.kind === "repo" && group.pinned;
  const canEditProject = !!props.onEditProject && group.capabilities.create === "repo" && !!group.repoPath;
  const hasMenu = canAppearance || archivableCount > 0 || canPin || canUnpin || canEditProject;
  const [renaming, setRenaming] = useState(false);
  const [renameValue, setRenameValue] = useState(group.alias ?? group.displayName);
  const renameRef = useRef<HTMLInputElement>(null);
  const { menu, menuRef, openMenu, closeMenu } = useContextMenu<{ x: number; y: number }>();
  const compact = useSidebarCompact();
  const suppressClickAfterDrag = useDragClickSuppression(dragHandle?.isDragging ?? false);

  const dotClass = STATUS_DOT_CLASS[group.status === "active" ? "Running" : "Idle"] ?? "bg-status-idle";
  const rowClass = `${group.color ? "" : "hover:bg-surface-800/50"} ${hasActiveChild ? "border-l-2 border-session-active" : ""}`;
  const headerStyle = repoColorStyle(group.color);
  const dot = <span className={`w-2 h-2 rounded-full shrink-0 ${dotClass}`} />;

  const commitRename = () => {
    setRenaming(false);
    props.onUpdateAppearance(group.id, { alias: renameValue.trim() || null });
  };

  if (renaming) {
    return (
      <div
        data-testid="sidebar-group-header"
        data-group-id={group.id}
        className={`flex items-center gap-2 px-3 py-2 transition-colors duration-75 text-text-secondary ${rowClass}`}
        style={headerStyle}
      >
        {dot}
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

  // Sunk workspaces render in the footer, so they do not count toward the visible total.
  const sessionCount = group.workspaces.filter((v) => !workspaceIsSunk(v.workspace)).length;
  const attentionCount = group.workspaces.reduce((n, v) => n + workspaceAttentionCount(v.workspace), 0);

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
                openMenu({ x: e.clientX, y: e.clientY });
              }
            : undefined
        }
        onKeyDown={
          hasMenu
            ? (e) => {
                if (e.target !== e.currentTarget) return;
                if (!["Enter", " ", "ContextMenu"].includes(e.key) && !(e.shiftKey && e.key === "F10")) return;
                e.preventDefault();
                const rect = e.currentTarget.getBoundingClientRect();
                openMenu({ x: rect.left + 12, y: rect.bottom + 4 });
              }
            : undefined
        }
        onClickCapture={suppressClickAfterDrag}
        className={`group flex items-center gap-2 ${compact ? "px-2" : "px-3"} py-2 transition-colors duration-75 text-text-secondary focus:outline-none focus:ring-2 focus:ring-brand-600 ${rowClass}`}
        style={headerStyle}
        // The whole row activates the drag; `attributes` would collide with the menu's role and tabIndex.
        ref={dragHandle?.setActivatorNodeRef}
        {...dragHandle?.listeners}
      >
        {dot}
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
              <FoldChevron
                aria-hidden="true"
                className={`text-text-dim transition-transform duration-75 ${group.collapsed ? "-rotate-90" : ""}`}
              />
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
          <span
            className="text-[13px] md:text-[14px] font-medium truncate flex-1"
            title={group.groupPath ?? group.repoPath}
          >
            {group.displayName}
          </span>
          {attentionCount > 0 && (
            <Tooltip
              text={`${attentionCount} session${attentionCount === 1 ? "" : "s"} need${attentionCount === 1 ? "s" : ""} attention`}
            >
              <span
                className="shrink-0 min-w-[1.25rem] rounded-full bg-status-error px-1.5 text-[11px] font-semibold leading-[1.25rem] tabular-nums text-white text-center"
                data-testid="sidebar-group-attention-badge"
                aria-label={`${attentionCount} needing attention`}
              >
                {attentionCount}
              </span>
            </Tooltip>
          )}
          {!compact && (
            <span className="shrink-0 text-[12px] tabular-nums text-text-dim" data-testid="sidebar-group-session-count">
              ({sessionCount})
            </span>
          )}
        </button>
        {!compact && (
          <Tooltip text={offline ? OFFLINE_TITLE : "New session"}>
            <button
              onClick={onNewSession}
              disabled={offline}
              className={`w-8 h-8 flex items-center justify-center shrink-0 rounded-md transition-colors text-text-muted hover:text-text-secondary hover:bg-surface-700/50 cursor-pointer ${DISABLED_ICON_BUTTON}`}
              aria-label={`New session in ${group.displayName}`}
            >
              <PlusIcon size={14} strokeWidth="2.5" round={false} />
            </button>
          </Tooltip>
        )}
      </div>
      {hasMenu && menu && (
        <ContextMenu menu={menu} menuRef={menuRef} testId="sidebar-group-context-menu">
          <GroupMenuItems
            {...props}
            archivableCount={archivableCount}
            canPin={canPin}
            canUnpin={canUnpin}
            canEditProject={canEditProject}
            close={closeMenu}
            startRename={() => {
              setRenameValue(group.alias ?? group.defaultDisplayName);
              setRenaming(true);
              requestAnimationFrame(() => renameRef.current?.select());
            }}
          />
        </ContextMenu>
      )}
    </>
  );
});

function GroupMenuItems({
  group,
  onUpdateAppearance,
  onArchiveAll,
  onPin,
  onUnpin,
  onEditProject,
  archivableCount,
  canPin,
  canUnpin,
  canEditProject,
  close,
  startRename,
}: Props & {
  archivableCount: number;
  canPin: boolean;
  canUnpin: boolean;
  canEditProject: boolean;
  close: () => void;
  startRename: () => void;
}) {
  const canAppearance = group.capabilities.appearance;
  const act = (fn: () => void) => () => {
    close();
    fn();
  };
  return (
    <>
      {canPin && (
        <MenuItem
          onClick={act(() => group.repoPath && onPin?.(group.repoPath))}
          testId="sidebar-group-context-menu-pin"
        >
          Pin project
        </MenuItem>
      )}
      {canUnpin && (
        <MenuItem onClick={act(() => onUnpin?.(group))} testId="sidebar-group-context-menu-unpin">
          Unpin project
        </MenuItem>
      )}
      {canEditProject && (
        <MenuItem onClick={act(() => onEditProject?.(group))} testId="sidebar-group-context-menu-settings">
          Project settings
        </MenuItem>
      )}
      {(canPin || canUnpin || canEditProject) && (archivableCount > 0 || canAppearance) && <MenuSeparator />}
      {archivableCount > 0 && (
        <MenuItem onClick={act(() => onArchiveAll?.())} testId="sidebar-group-context-menu-archive-all">
          {`Archive all (${archivableCount})`}
        </MenuItem>
      )}
      {archivableCount > 0 && canAppearance && <MenuSeparator />}
      {canAppearance && (
        <>
          <MenuItem onClick={act(startRename)} testId="sidebar-group-context-menu-rename">
            Rename
          </MenuItem>
          {group.alias && (
            <MenuItem onClick={act(() => onUpdateAppearance(group.id, { alias: null }))}>Clear alias</MenuItem>
          )}
          <MenuSeparator />
          <MenuHeading>Background</MenuHeading>
          <div className="grid grid-cols-4 gap-1 px-3 py-1.5">
            {REPO_COLOR_OPTIONS.map((option) => (
              <button
                key={option.id}
                type="button"
                onClick={act(() => onUpdateAppearance(group.id, { color: option.id }))}
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
              onClick={act(() => onUpdateAppearance(group.id, { color: null }))}
              data-testid="sidebar-group-color-clear"
              aria-label="Clear background"
              className="h-8 rounded-md border border-surface-700 bg-surface-900 text-[10px] font-mono text-text-dim cursor-pointer hover:bg-surface-700/40"
            >
              None
            </button>
          </div>
        </>
      )}
    </>
  );
}
