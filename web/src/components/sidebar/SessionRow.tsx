import { memo, useId, useRef, useState } from "react";
import { Pin } from "lucide-react";
import type { Workspace } from "../../lib/types";
import { renameSession, type RuntimeCursor, type SessionMutation } from "../../lib/api";
import { isFreshIdle, isSessionActive } from "../../lib/session";
import { useIdleDecayWindowMs } from "../../lib/idleDecay";
import { sessionRowChromeClass } from "../../lib/sessionRowChrome";
import { useSessionColorsEnabled } from "../../lib/sessionColors";
import { useSidebarCompact } from "../../lib/sidebarCompact";
import type { OptimisticTriage } from "../../lib/sidebarOptimistic";
import { reportError, reportInfo } from "../../lib/toastBus";
import { MARK_MEANING, meaningText } from "../../lib/sidebarMarks";
import { statusMeaning } from "../../lib/statusLegend";
import { StatusGlyph } from "../StatusGlyph";
import { Tooltip } from "../Tooltip";
import { useContextMenu } from "../useContextMenu";
import { RowSubRows, RowTrailingBadges } from "./RowBadges";
import { NAV_ITEM, onSidebarArrowKey } from "./keyboardNav";
import { SessionActionsMenu, type SessionActionHandlers, type SessionMenuState } from "./SessionActions";
import { useSessionRowModel } from "./useSessionRowModel";
import type { RowActivate, RowBulkApi } from "./types";
import { useLongPress } from "./useLongPress";

export interface SessionRowProps extends SessionActionHandlers {
  workspace: Workspace;
  observedById: Record<string, RuntimeCursor>;
  runtimeEpoch: string | null;
  onSessionMutation: (mutation: SessionMutation) => void;
  isActive: boolean;
  isSelected: boolean;
  onActivate: RowActivate;
  readOnly?: boolean;
  indented?: boolean;
  optimistic: OptimisticTriage;
  bulkApi: RowBulkApi;
}

export const SessionRow = memo(function SessionRow(props: SessionRowProps) {
  const { workspace, isActive, isSelected, onActivate, readOnly, indented, bulkApi } = props;
  const idleDecayWindowMs = useIdleDecayWindowMs();
  const sessionColorsEnabled = useSessionColorsEnabled();
  const compact = useSidebarCompact();
  const { model, setNotify, setColor } = useSessionRowModel(workspace, props.optimistic, isActive, props);
  const { label, sessionId, isDeleting, navigationSession } = model;

  const [renaming, setRenaming] = useState(false);
  const [renameValue, setRenameValue] = useState(label);
  const renameRef = useRef<HTMLInputElement>(null);
  const rowRef = useRef<HTMLAnchorElement>(null);
  // The glyph's tooltip: on hovering the glyph, or on keyboard focus of the row, which stays the
  // focus target and is described by it.
  const [glyphTip, setGlyphTip] = useState<"hover" | "focus" | null>(null);
  const glyphTipId = useId();

  const openedAtRef = useRef(0);
  const { menu, menuRef, openMenu, closeMenu } = useContextMenu<SessionMenuState>(openedAtRef);
  const openMenuAt = (x: number, y: number) => openMenu({ x, y, scope: bulkApi.prepareScope(workspace) });
  const longPress = useLongPress(!!sessionId && !isDeleting, (x, y) => {
    openedAtRef.current = Date.now();
    openMenuAt(x, y);
  });

  const commitRename = async () => {
    setRenaming(false);
    const trimmed = renameValue.trim();
    // Compared with the title, not the label, so accepting a branch-prefilled value still sets a title.
    if (!trimmed || trimmed === model.sessionTitle || !sessionId) return;
    const result = await renameSession(sessionId, trimmed);
    if (result.ok) {
      props.onSessionMutation(result);
      // The title persisted; a tmux rekey warning is informational.
      result.warnings?.forEach((warning) => reportInfo(warning));
    } else {
      reportError(result.message ?? "Could not rename this session. Please try again.");
    }
  };

  const startRename = () => {
    closeMenu();
    if (renaming) return;
    setRenameValue(model.sessionTitle || label);
    setRenaming(true);
    requestAnimationFrame(() => renameRef.current?.select());
  };

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

  const attention = model.needsAttention && !model.showUnreadGlyph;
  const labelTone = model.showUnreadGlyph
    ? "text-status-unread font-semibold"
    : isSessionActive({ status: model.status, idle_entered_at: model.idleEnteredAt }, idleDecayWindowMs)
      ? model.textClass
      : isActive
        ? "text-text-primary"
        : "text-text-secondary";
  const glyphMeaning = model.showUnreadGlyph
    ? MARK_MEANING.unread
    : statusMeaning(model.status, {
        dormant: model.dormant,
        fresh: isFreshIdle({ status: model.status, idle_entered_at: model.idleEnteredAt }, idleDecayWindowMs),
      });
  const navigationSessionId = navigationSession?.id ?? null;
  const sessionPath = navigationSessionId ? `/session/${encodeURIComponent(navigationSessionId)}` : "/";

  return (
    <>
      <a
        ref={rowRef}
        href={sessionPath}
        tabIndex={isDeleting ? -1 : undefined}
        aria-disabled={isDeleting || undefined}
        aria-current={isActive && sessionPath === window.location.pathname ? "page" : undefined}
        data-active-session-row={isActive || undefined}
        data-testid="sidebar-session-row"
        aria-describedby={glyphTip ? glyphTipId : undefined}
        draggable={false}
        {...{ [NAV_ITEM]: "" }}
        onKeyDown={onSidebarArrowKey}
        onFocus={(e) => {
          if (e.target === e.currentTarget && e.currentTarget.matches(":focus-visible")) setGlyphTip("focus");
        }}
        onBlur={() => setGlyphTip(null)}
        onClick={(e) => {
          // Middle-click and Alt+click keep the browser's href behavior.
          if (e.button !== 0 || e.altKey) return;
          e.preventDefault();
          if (isDeleting || longPress.fired.current) return;
          onActivate(workspace.id, e, navigationSessionId);
        }}
        onContextMenu={(e) => {
          if (isDeleting) return;
          e.preventDefault();
          openMenuAt(e.clientX, e.clientY);
        }}
        {...longPress.handlers}
        data-selected={isSelected || undefined}
        className={`block w-full text-left py-2 cursor-pointer select-none [-webkit-touch-callout:none] transition-colors duration-75 ${
          compact ? (indented ? "pl-3 pr-1" : "px-2") : indented ? "pl-6 pr-3" : "px-3"
        } ${sessionRowChromeClass(isActive, isSelected)} ${isDeleting ? "opacity-50 pointer-events-none" : ""} focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-brand-500`}
      >
        {isSelected && <span className="sr-only">Selected</span>}
        <div className="flex items-center gap-2">
          <Tooltip
            text={meaningText(glyphMeaning)}
            detail={
              // The attention hint lost its `title`; the tooltip is where a mouse user reads it. Waiting
              // and Error already say it in the glyph's meaning, so only other attention adds a line.
              [
                // `needsAttention`, not `attention`: an unread glyph can stand in for the attention one,
                // and the reason still has to show.
                model.needsAttention && model.status !== "Waiting" && model.status !== "Error"
                  ? model.attentionHint.charAt(0).toUpperCase() + model.attentionHint.slice(1)
                  : null,
                glyphTip === "focus" ? "Press Enter to open" : null,
              ]
                .filter(Boolean)
                .join(" \u00b7 ") || undefined
            }
            id={glyphTipId}
            open={glyphTip !== null}
          >
            <span
              className={`text-sm shrink-0 leading-none font-mono ${model.showUnreadGlyph ? "text-status-unread font-semibold" : model.textClass} ${
                attention ? "motion-safe:animate-pulse font-semibold" : ""
              }`}
              data-attention={attention ? "true" : undefined}
              data-testid="sidebar-status-glyph"
              aria-label={attention ? `${model.status} · ${model.attentionHint}` : undefined}
              // Only a mouse hover opens it; a tap fires emulated mouse events and must not.
              onPointerEnter={(e) => e.pointerType === "mouse" && setGlyphTip((tip) => tip ?? "hover")}
              onPointerLeave={() => setGlyphTip((tip) => (tip === "hover" ? null : tip))}
            >
              {model.showUnreadGlyph ? (
                <span aria-label="Unread" data-testid="sidebar-unread-dot">
                  ●
                </span>
              ) : (
                <StatusGlyph
                  status={model.status}
                  createdAt={model.createdAt}
                  idleEnteredAt={model.idleEnteredAt}
                  dormant={model.dormant}
                />
              )}
            </span>
          </Tooltip>
          <div className="min-w-0 flex-1">
            <span
              className={`flex items-center gap-1.5 text-[13px] md:text-[14px] ${labelTone} ${model.effectivePinned ? "font-semibold" : ""} ${model.effectiveArchived || model.effectiveSnoozed ? "italic opacity-70" : ""}`}
            >
              {sessionColorsEnabled && model.sessionColorDot && (
                <span
                  title={`Color: ${model.sessionColor}`}
                  aria-label={`Color: ${model.sessionColor}`}
                  data-testid="sidebar-session-color-dot"
                  data-color={model.sessionColor ?? undefined}
                  className={`shrink-0 inline-block h-2 w-2 rounded-full ${model.sessionColorDot}`}
                />
              )}
              {!compact && model.effectivePinned && (
                <Tooltip text={meaningText(MARK_MEANING.pinned)}>
                  <span
                    aria-label="Pinned"
                    data-testid="sidebar-pinned-mark"
                    className="shrink-0 inline-flex text-brand-400"
                  >
                    <Pin className="h-3 w-3 -rotate-45" />
                  </span>
                </Tooltip>
              )}
              {model.isFavorited && (
                <Tooltip text={meaningText(MARK_MEANING.favorite)}>
                  <span aria-label="Favorited" data-testid="sidebar-favorite-mark" className="shrink-0 text-favorite">
                    ✦
                  </span>
                </Tooltip>
              )}
              <span className="truncate" title={label}>
                {label}
              </span>
              {!compact && <RowTrailingBadges workspace={workspace} model={model} />}
            </span>
            {!compact && <RowSubRows first={model.firstSession} />}
          </div>
        </div>
      </a>
      <SessionActionsMenu
        menu={menu}
        menuRef={menuRef}
        closeMenu={closeMenu}
        returnFocusTo={rowRef}
        workspace={workspace}
        model={model}
        setNotify={setNotify}
        setColor={setColor}
        readOnly={readOnly}
        handlers={props}
        onRename={startRename}
        bulkApi={bulkApi}
      />
    </>
  );
});
