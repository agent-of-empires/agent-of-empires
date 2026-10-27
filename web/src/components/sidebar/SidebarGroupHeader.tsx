import { memo, useRef, useState } from "react";
import { Folder } from "lucide-react";
import { archivableWorkspaces, type SidebarGroup } from "../../lib/sidebarGroups";
import { SCRATCH_GROUP_ID } from "../../hooks/useRepoGroups";
import {
  REPO_COLOR_OPTIONS,
  repoColorStyle,
  repoSwatchStyle,
  type RepoAppearanceUpdate,
} from "../../lib/repoAppearance";
import { STATUS_DOT_CLASS } from "../../lib/session";
import { usePluginAttention } from "../../lib/pluginUiContext";
import { workspaceAttentionCount, workspaceIsSunk } from "../../lib/sidebarSort";
import { useSidebarCompact } from "../../lib/sidebarCompact";
import { OFFLINE_TITLE } from "../../lib/connectionState";
import { ContextMenu, MenuChoiceRow, MenuItem, MenuSeparator, MenuSwatches } from "../ContextMenu";
import { OwnerAvatar } from "../OwnerAvatar";
import { Tooltip } from "../Tooltip";
import { useContextMenu } from "../useContextMenu";
import { FoldChevron, PlusIcon } from "../icons";
import { DISABLED_ICON_BUTTON } from "./styles";
import { NAV_ITEM, onSidebarArrowKey } from "./keyboardNav";
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
  /** Id of the element wrapping this Group's rows, for `aria-controls`. */
  rowsId: string;
}

export const SidebarGroupHeader = memo(function SidebarGroupHeader(props: Props) {
  const { group, hasActiveChild, onClick, onNewSession, offline, dragHandle, rowsId } = props;
  const pluginAttention = usePluginAttention();
  // Appearance is repo-axis only; archive-all works on every axis.
  const canAppearance = group.capabilities.appearance;
  const archivableCount = props.onArchiveAll ? archivableWorkspaces(group).length : 0;
  const canPin = !!props.onPin && group.capabilities.create === "repo" && !!group.repoPath && !group.pinned;
  const canUnpin = !!props.onUnpin && group.kind === "repo" && group.pinned;
  const canEditProject =
    !!props.onEditProject &&
    (group.id === SCRATCH_GROUP_ID || (group.capabilities.create === "repo" && !!group.repoPath));
  const hasMenu = canAppearance || archivableCount > 0 || canPin || canUnpin || canEditProject;
  const [renaming, setRenaming] = useState(false);
  const [renameValue, setRenameValue] = useState(group.alias ?? group.displayName);
  const renameRef = useRef<HTMLInputElement>(null);
  const rowRef = useRef<HTMLDivElement | null>(null);
  const buttonRef = useRef<HTMLButtonElement>(null);
  const { menu, menuRef, openMenu, closeMenu } = useContextMenu<{ x: number; y: number }>();
  const compact = useSidebarCompact();

  const dotClass = STATUS_DOT_CLASS[group.status === "active" ? "Running" : "Idle"] ?? "bg-status-idle";
  const rowClass = `${group.color ? "" : "hover:bg-surface-800/50"} ${hasActiveChild ? "border-l-2 border-session-active" : ""}`;
  const headerStyle = repoColorStyle(group.color);
  const dot = <span className={`w-2 h-2 rounded-full shrink-0 ${dotClass}`} />;

  const commitRename = () => {
    setRenaming(false);
    props.onUpdateAppearance(group.id, { alias: renameValue.trim() || null });
  };
  // Enter and Escape unmount the focused input; send focus back to the header button once
  // it renders again. A blur (Tab, a click) keeps its own destination.
  const refocusHeader = () => requestAnimationFrame(() => buttonRef.current?.focus());

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
            if (e.key === "Enter") {
              commitRename();
              refocusHeader();
            }
            if (e.key === "Escape") {
              setRenaming(false);
              refocusHeader();
            }
          }}
          data-testid="sidebar-group-rename-input"
          className="min-w-0 flex-1 rounded border border-brand-600 bg-surface-900 px-2 py-1 text-[13px] md:text-[14px] font-mono text-text-primary focus:outline-none"
        />
      </div>
    );
  }

  // Sunk workspaces render in the footer, so they do not count toward the visible total.
  const sessionCount = group.workspaces.filter((v) => !workspaceIsSunk(v.workspace)).length;
  const attentionCount = group.workspaces.reduce(
    (n, v) => n + workspaceAttentionCount(v.workspace, pluginAttention),
    0,
  );

  return (
    <>
      <div
        data-testid="sidebar-group-header"
        data-group-id={group.id}
        data-draggable={dragHandle ? "true" : undefined}
        onContextMenu={
          hasMenu
            ? (e) => {
                e.preventDefault();
                openMenu({ x: e.clientX, y: e.clientY });
              }
            : undefined
        }
        className={`group flex items-center gap-2 ${compact ? "px-2" : "px-3"} py-2 transition-colors duration-75 text-text-secondary ${rowClass}`}
        style={headerStyle}
        // The whole row activates the drag; `attributes` would add a role and a second tab stop.
        ref={(el) => {
          rowRef.current = el;
          dragHandle?.setActivatorNodeRef(el);
        }}
        {...dragHandle?.listeners}
      >
        {dot}
        <button
          ref={buttonRef}
          onClick={onClick}
          onKeyDown={(e) => {
            if (hasMenu && (e.key === "ContextMenu" || (e.shiftKey && e.key === "F10"))) {
              e.preventDefault();
              const rect = (rowRef.current ?? e.currentTarget).getBoundingClientRect();
              openMenu({ x: rect.left + 12, y: rect.bottom + 4 });
              // The desktop menu isn't modal, so it takes focus only when the keyboard opened it.
              requestAnimationFrame(() =>
                menuRef.current?.querySelector<HTMLElement>("button:not([disabled])")?.focus({ preventScroll: true }),
              );
              return;
            }
            if (e.key === "Escape" && menu) {
              e.preventDefault();
              closeMenu();
              return;
            }
            onSidebarArrowKey(e);
          }}
          {...{ [NAV_ITEM]: "" }}
          aria-expanded={!group.collapsed}
          aria-controls={rowsId}
          aria-keyshortcuts={hasMenu ? "Shift+F10" : undefined}
          className="flex items-center gap-2 flex-1 min-w-0 text-left cursor-pointer rounded focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-brand-500"
        >
          <span className="relative h-4 w-4 shrink-0">
            <span
              data-testid="sidebar-group-icon"
              className="absolute inset-0 flex items-center justify-center transition-opacity duration-75 group-hover:opacity-0 group-focus-within:opacity-0"
            >
              <OwnerAvatar
                ownerKey={group.remoteOwnerKey}
                size={16}
                fallback={<Folder className="h-3.5 w-3.5 text-text-dim" />}
              />
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
        <ContextMenu
          menu={menu}
          menuRef={menuRef}
          testId="sidebar-group-context-menu"
          onClose={() => {
            closeMenu();
            buttonRef.current?.focus();
          }}
        >
          <GroupMenuItems
            {...props}
            archivableCount={archivableCount}
            canPin={canPin}
            canUnpin={canUnpin}
            canEditProject={canEditProject}
            close={closeMenu}
            closeToHeader={() => {
              closeMenu();
              buttonRef.current?.focus();
            }}
            startRename={() => {
              setRenameValue(group.alias ?? group.defaultDisplayName);
              setRenaming(true);
              // select() alone doesn't move focus in every browser.
              requestAnimationFrame(() => {
                renameRef.current?.focus();
                renameRef.current?.select();
              });
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
  closeToHeader,
  startRename,
}: Props & {
  archivableCount: number;
  canPin: boolean;
  canUnpin: boolean;
  canEditProject: boolean;
  close: () => void;
  /** Close and return focus to the header button, for actions that leave the header in place. */
  closeToHeader: () => void;
  startRename: () => void;
}) {
  const canAppearance = group.capabilities.appearance;
  // Rename focuses its input and Project settings its dialog; every other action keeps the
  // header, so focus goes back to it rather than to the menu item that just unmounted.
  const act =
    (fn: () => void, toHeader = true) =>
    () => {
      (toHeader ? closeToHeader : close)();
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
        <MenuItem onClick={act(() => onEditProject?.(group), false)} testId="sidebar-group-context-menu-settings">
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
          <MenuItem onClick={act(startRename, false)} testId="sidebar-group-context-menu-rename">
            Rename
          </MenuItem>
          {group.alias && (
            <MenuItem onClick={act(() => onUpdateAppearance(group.id, { alias: null }))}>Clear alias</MenuItem>
          )}
          <MenuSeparator />
          <MenuChoiceRow label="Color">
            <MenuSwatches
              options={REPO_COLOR_OPTIONS.map((o) => ({ key: o.id, label: o.label, style: repoSwatchStyle(o.id) }))}
              value={group.color}
              onPick={(color) => onUpdateAppearance(group.id, { color })}
              testIdPrefix="sidebar-group-color"
            />
          </MenuChoiceRow>
        </>
      )}
    </>
  );
}
