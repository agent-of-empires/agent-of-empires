import { useContext } from "react";
import { useReportFeatureOnce } from "../../hooks/useReportFeatureOnce";
import { Play } from "lucide-react";
import { dismissSuggestion } from "../../lib/api";
import { failureText, lockTitle, shortRef, STALE_REASON, startMenuFor } from "../../lib/suggestionPlan";
import { SidebarCompactContext } from "../../lib/sidebarCompact";
import { toastBus } from "../../lib/toastBus";
import type { SuggestionItem, SuggestionsSnapshot } from "../../lib/types";
import { useAgentNames } from "../../hooks/useAgentNames";
import { useSuggestionPresets } from "../../hooks/useSuggestionPresets";
import { suggestionKey, useSuggestions } from "../../hooks/useSuggestions";
import type { WizardPrefill } from "../session-wizard/SessionWizard";
import { SuggestionHooksReview } from "../SuggestionHooksReview";
import { StartInMenu, StartMenu } from "../SuggestionStartMenu";
import { FoldChevron } from "../icons";
import { usePersistedFlag } from "../usePersistedFlag";
import { NAV_ITEM, ROWS, onSidebarArrowKey } from "./keyboardNav";

const SUGGESTED_ROWS = "sidebar-suggested-rows";
const MAX_ROWS = 3;

interface Props {
  readOnly?: boolean;
  /** Read-only because of CityHall mode, which the titles say. */
  cityhall?: boolean;
  onPrepare: (prefill: WizardPrefill) => void;
  /** Opens Settings › Suggestions on a kind's preset; absent where Settings is out of reach. */
  onEditPreset?: (providerId: string, criterion: string) => void;
  onViewAll: () => void;
  onOpenSession: (sessionId: string) => void;
}

/** The "Suggested" Group at the bottom of the session list, shown when the `sidebar` pref is on. */
export function SidebarSuggestions({
  readOnly = false,
  cityhall = false,
  onPrepare,
  onEditPreset,
  onViewAll,
  onOpenSession,
}: Props) {
  const compact = useContext(SidebarCompactContext);
  const [expanded, toggleExpanded] = usePersistedFlag("aoe-sidebar-suggested-expanded", false);
  const ctrl = useSuggestions(onPrepare, onOpenSession, (s) => !s.prefs.sidebar);
  const { snapshot, offline, notice } = ctrl;
  useReportFeatureOnce(
    "suggest_from_sidebar",
    Boolean(snapshot?.prefs.sidebar && snapshot.items.some((item) => !item.dismissed && !item.started_session)),
  );

  if (!snapshot?.prefs.sidebar) return null;
  // A started suggestion is a Session already listed under its Project.
  const active = snapshot.items.filter((item) => !item.dismissed && !item.started_session);
  if (active.length === 0) return null;
  const unavailable = lockTitle(readOnly, offline, cityhall);
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
            <SuggestedRows
              items={active.slice(0, MAX_ROWS)}
              snapshot={snapshot}
              ctrl={ctrl}
              readOnly={readOnly}
              unavailable={unavailable}
              rowClass={rowClass}
              compact={compact}
              onEditPreset={onEditPreset}
            />
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
      <SuggestionHooksReview ctrl={ctrl} />
    </div>
  );
}

/** Rendered only while the group is open, so its agent and preset lookups load on demand. */
function SuggestedRows({
  items,
  snapshot,
  ctrl,
  readOnly,
  unavailable,
  rowClass,
  compact,
  onEditPreset,
}: {
  items: SuggestionItem[];
  snapshot: SuggestionsSnapshot;
  ctrl: ReturnType<typeof useSuggestions>;
  readOnly: boolean;
  unavailable?: string;
  rowClass: string;
  compact: boolean;
  onEditPreset?: (providerId: string, criterion: string) => void;
}) {
  const agentName = useAgentNames();
  const presetOf = useSuggestionPresets();
  const { busy, failures, start } = ctrl;

  const dismiss = async (item: SuggestionItem) => {
    if (!(await ctrl.act(suggestionKey(item), () => dismissSuggestion(item.provider_id, item.id, true)))) return;
    toastBus.handler?.infoAction?.(
      `Dismissed \u201c${item.title}\u201d`,
      "Undo",
      () => void ctrl.act(suggestionKey(item), () => dismissSuggestion(item.provider_id, item.id, false)),
    );
  };

  return (
    <>
      {items.map((item) => {
        const key = suggestionKey(item);
        const descId = `sidebar-suggested-${key}-desc`;
        const failure = failures[key];
        const disabled = busy !== null || ctrl.starting.includes(key) || !!unavailable || !!item.stale;
        // A repo with no Project picks one first, as "Start in…" does on Home.
        const startIn = !item.resolved_project && !!item.mapping_key;
        const body = (
          <>
            <span className="flex items-center gap-1.5">
              <Play
                aria-hidden="true"
                className={`h-3 w-3 shrink-0 fill-current text-brand-500 ${compact ? "" : "absolute left-3 top-2.5"}`}
              />
              <span
                className={`min-w-0 flex-1 truncate text-[13px] text-text-primary ${item.stale ? "opacity-[0.55]" : ""}`}
              >
                {item.title}
              </span>
              <span className="hidden shrink-0 text-[11px] text-brand-500 group-hover:inline group-has-[:focus-visible]:inline">
                {startIn ? "Start in\u2026" : "Start"}
              </span>
            </span>
            <span id={descId} className="block truncate text-[11px] text-text-muted">
              {failure
                ? failureText(failure)
                : item.stale
                  ? STALE_REASON
                  : [snapshot.prefs.show_source_ref ? shortRef(item.source_ref) : null, item.reason]
                      .filter(Boolean)
                      .join(" · ")}
            </span>
          </>
        );
        const rowProps = {
          "data-testid": "sidebar-suggested-row",
          onKeyDown: onSidebarArrowKey,
          [NAV_ITEM]: "",
          title: unavailable ?? item.title,
          // The aria-label replaces the row's text, so the reason or failure line is its description.
          "aria-describedby": descId,
          className: `${rowClass} relative min-w-0 flex-1`,
        };
        return (
          <div key={key} className="group flex items-start">
            {startIn ? (
              <StartInMenu
                align="left"
                disabled={disabled}
                title={unavailable}
                onPick={(path) => void start(item, { project: { path } })}
                renderTrigger={({ open, disabled: off, onToggle }) => (
                  <button
                    type="button"
                    {...rowProps}
                    aria-label={`Start in a Project: ${item.title}`}
                    aria-haspopup="menu"
                    aria-expanded={open}
                    disabled={off}
                    onClick={onToggle}
                  >
                    {body}
                  </button>
                )}
              />
            ) : (
              <button
                type="button"
                {...rowProps}
                aria-label={`Start a session: ${item.title}`}
                disabled={disabled}
                onClick={() => void start(item)}
              >
                {body}
              </button>
            )}
            {!compact && !item.stale && (
              <StartMenu
                {...startMenuFor(item, snapshot, {
                  startWithChanges: (it) => void ctrl.startWithChanges(it),
                  agentName,
                  presetOf,
                  onEditPreset,
                })}
                align="left"
                disabled={busy !== null || !!unavailable}
                locked={readOnly ? unavailable : undefined}
                onDismiss={readOnly ? undefined : () => void dismiss(item)}
                triggerClassName="mr-1 mt-1 inline-flex h-6 w-6 shrink-0 items-center justify-center rounded text-text-dim hover:bg-surface-800 hover:text-text-secondary disabled:opacity-50"
              />
            )}
          </div>
        );
      })}
    </>
  );
}
