import { useContext } from "react";
import { OFFLINE_TITLE } from "../../lib/connectionState";
import { SidebarCompactContext } from "../../lib/sidebarCompact";
import { suggestionKey, useSuggestions } from "../../hooks/useSuggestions";
import type { WizardPrefill } from "../session-wizard/SessionWizard";
import { FoldChevron } from "../icons";
import { usePersistedFlag } from "../usePersistedFlag";
import { NAV_ITEM, ROWS, onSidebarArrowKey } from "./keyboardNav";

const SUGGESTED_ROWS = "sidebar-suggested-rows";
const MAX_ROWS = 3;

interface Props {
  readOnly?: boolean;
  onPrepare: (prefill: WizardPrefill) => void;
  onViewAll: () => void;
  onOpenSession: (sessionId: string) => void;
}

/** The "Suggested" Group at the bottom of the session list, shown when the `sidebar` pref is on. */
export function SidebarSuggestions({ readOnly = false, onPrepare, onViewAll, onOpenSession }: Props) {
  const compact = useContext(SidebarCompactContext);
  const [expanded, toggleExpanded] = usePersistedFlag("aoe-sidebar-suggested-expanded", false);
  const ctrl = useSuggestions(onPrepare, onOpenSession, (s) => !s.prefs.sidebar);
  const { snapshot, busy, offline, failures, notice, start } = ctrl;

  if (!snapshot?.prefs.sidebar) return null;
  // A started suggestion is a Session already listed under its Project.
  const active = snapshot.items.filter((item) => !item.dismissed && !item.started_session);
  if (active.length === 0) return null;
  const unavailable = readOnly ? "Read-only: can't create Sessions" : offline ? OFFLINE_TITLE : undefined;
  const rowClass = `w-full text-left py-1.5 hover:bg-surface-800/40 cursor-pointer transition-colors disabled:opacity-50 disabled:cursor-not-allowed focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-brand-500 ${
    compact ? "px-2" : "pl-7 pr-3"
  }`;

  return (
    <div data-testid="sidebar-suggested-section">
      <button
        type="button"
        onClick={toggleExpanded}
        onKeyDown={onSidebarArrowKey}
        data-testid="sidebar-suggested-toggle"
        aria-expanded={expanded}
        aria-controls={SUGGESTED_ROWS}
        {...{ [NAV_ITEM]: "" }}
        className={`w-full flex items-center gap-2 py-1.5 text-[11px] font-mono uppercase text-text-muted hover:text-text-secondary hover:bg-surface-800/40 cursor-pointer transition-colors border-t border-surface-800/60 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-brand-500 ${
          compact ? "px-2" : "px-3 tracking-widest"
        }`}
      >
        <FoldChevron className={`shrink-0 transition-transform duration-75 ${expanded ? "" : "-rotate-90"}`} />
        <span className="truncate">Suggested{compact ? "" : ` (${active.length})`}</span>
      </button>
      <div id={SUGGESTED_ROWS} {...{ [ROWS]: "" }}>
        {expanded && (
          <>
            {active.slice(0, MAX_ROWS).map((item) => {
              const key = suggestionKey(item);
              const failure = failures[key];
              return (
                <button
                  key={key}
                  type="button"
                  data-testid="sidebar-suggested-row"
                  onKeyDown={onSidebarArrowKey}
                  {...{ [NAV_ITEM]: "" }}
                  disabled={busy !== null || ctrl.starting.includes(key) || !!unavailable || item.stale}
                  title={unavailable ?? item.title}
                  onClick={() => void start(item)}
                  className={rowClass}
                >
                  <span
                    className={`block truncate text-[13px] text-text-primary ${item.stale ? "opacity-[0.55]" : ""}`}
                  >
                    {item.title}
                  </span>
                  <span className="block truncate text-[11px] text-text-muted">
                    {failure
                      ? `Couldn't start: ${failure.reason}`
                      : [snapshot.prefs.show_source_ref ? item.source_ref : null, item.reason]
                          .filter(Boolean)
                          .join(" · ")}
                  </span>
                </button>
              );
            })}
            <button
              type="button"
              data-testid="sidebar-suggested-view-all"
              onKeyDown={onSidebarArrowKey}
              {...{ [NAV_ITEM]: "" }}
              onClick={onViewAll}
              className={`${rowClass} text-xs text-brand-500`}
            >
              View all ({active.length})
            </button>
            {notice && (
              <p role="alert" className={`py-1 text-[11px] text-status-error ${compact ? "px-2" : "pl-7 pr-3"}`}>
                {notice}
              </p>
            )}
          </>
        )}
      </div>
    </div>
  );
}
