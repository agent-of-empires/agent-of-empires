import { useMemo, useState, type ReactNode } from "react";
import type { SessionResponse } from "../lib/types";
import { isSessionActive } from "../lib/session";
import { useIdleDecayWindowMs } from "../lib/idleDecay";
import { AOE_BRAND_MARK_COLORS, AOE_BRAND_MARK_TEXT_SHADOW } from "../lib/brandMark";
import { TOUR_ANCHORS, type TourAnchorId } from "../lib/tourSteps";
import { DashboardSuggestions } from "./DashboardSuggestions";
import { SuggestionsSetupHint } from "./SuggestionsSetupHint";
import type { WizardPrefill } from "./session-wizard/SessionWizard";
import { PluginCards, PluginHomePanes } from "./plugin/PluginSlots";
import { StrokeIcon } from "./icons";
import { CARD } from "../lib/surfaces";

interface Props {
  sessions: SessionResponse[];
  /** Opens a Session a suggestion already started. */
  onSelectSession: (sessionId: string) => void;
  onNewSession: () => void;
  onCloneFromUrl: () => void;
  /** When false (CityHall client mode), the "Clone URL" action is hidden;
   *  cloning a repo is a project-management action. Defaults to true. See #7. */
  canManageProjects?: boolean;
  readOnly?: boolean;
  /** Shown above the action panes, e.g. the setup checklist. */
  banner?: ReactNode;
  /** Both required to show the Suggested sessions section. */
  onPrepareSuggestion?: (prefill: WizardPrefill) => void;
  onEditSuggestionPreset?: (providerId: string, criterion: string) => void;
  canLinkSuggestions?: boolean;
  /** False in CityHall: suggestions are read-only there. Defaults to true. */
  canManageSuggestions?: boolean;
  onViewAllSuggestions?: () => void;
  /** The onboarding tour is running; the first-run hint stays out of it. */
  tourActive?: boolean;
  /** Opens Settings > Plugins > Marketplace filtered to Suggestions; the first-run hint shows only when set. */
  onOpenSuggestionsMarketplace?: () => void;
}

export function Dashboard({
  sessions,
  onSelectSession,
  onNewSession,
  onCloneFromUrl,
  readOnly,
  canManageProjects = true,
  banner,
  onPrepareSuggestion,
  onEditSuggestionPreset,
  canLinkSuggestions,
  canManageSuggestions = true,
  onViewAllSuggestions,
  onOpenSuggestionsMarketplace,
  tourActive = false,
}: Props) {
  // Bumped when the setup sheet finishes so the Suggested section refetches.
  const [suggestionsEpoch, setSuggestionsEpoch] = useState(0);
  const idleDecayWindowMs = useIdleDecayWindowMs();
  const stats = useMemo(() => {
    const projects = new Set<string>();
    let total = 0;
    let active = 0;
    let waiting = 0;
    let errors = 0;
    for (const s of sessions) {
      // Trashed sessions are conceptually deleted (the sidebar buckets them into a dedicated Trash section, out of
      // the active/archived buckets), so they must not skew this summary: a session left in an Error state does
      // not matter once it is in the trash.
      if (s.trashed_at) continue;
      total++;
      projects.add(s.main_repo_path || s.project_path);
      if (isSessionActive(s, idleDecayWindowMs)) active++;
      if (s.status === "Waiting") waiting++;
      if (s.status === "Error") errors++;
    }
    return { total, active, waiting, errors, projects: projects.size };
  }, [idleDecayWindowMs, sessions]);
  return (
    <div
      className="flex-1 flex flex-col items-center justify-start overflow-y-auto bg-surface-950 px-4 py-6 md:justify-center md:py-0"
      // Bottom home-indicator clearance: the App root no longer reserves it
      // (see index.css .safe-area-inset). Collapses to the base gap off-device.
      style={{ paddingBottom: "max(1.5rem, env(safe-area-inset-bottom))" }}
    >
      {/* Logo + Title */}
      <svg viewBox="0 0 128 128" className="w-12 h-12 md:w-16 md:h-16 mb-3" aria-hidden="true">
        <defs>
          <linearGradient id="home-win-back" x1="0" y1="0" x2="0" y2="1">
            <stop offset="0%" stopColor={AOE_BRAND_MARK_COLORS.backGradientStart} />
            <stop offset="100%" stopColor={AOE_BRAND_MARK_COLORS.backGradientEnd} />
          </linearGradient>
          <linearGradient id="home-win-mid" x1="0" y1="0" x2="0" y2="1">
            <stop offset="0%" stopColor={AOE_BRAND_MARK_COLORS.midGradientStart} />
            <stop offset="100%" stopColor={AOE_BRAND_MARK_COLORS.midGradientEnd} />
          </linearGradient>
          <linearGradient id="home-win-front" x1="0" y1="0" x2="0" y2="1">
            <stop offset="0%" stopColor={AOE_BRAND_MARK_COLORS.frontGradientStart} />
            <stop offset="100%" stopColor={AOE_BRAND_MARK_COLORS.frontGradientEnd} />
          </linearGradient>
          <linearGradient id="home-titlebar" x1="0" y1="0" x2="0" y2="1">
            <stop offset="0%" stopColor={AOE_BRAND_MARK_COLORS.titlebarGradientStart} />
            <stop offset="100%" stopColor={AOE_BRAND_MARK_COLORS.titlebarGradientEnd} />
          </linearGradient>
        </defs>
        <rect x="10" y="38" width="76" height="60" rx="6" fill="url(#home-win-back)" opacity="0.6" />
        <rect x="20" y="28" width="76" height="60" rx="6" fill="url(#home-win-mid)" opacity="0.7" />
        <g>
          <rect x="32" y="18" width="82" height="66" rx="6" fill="url(#home-win-front)" />
          <rect x="32" y="18" width="82" height="18" rx="6" fill="url(#home-titlebar)" />
          <rect x="32" y="30" width="82" height="6" fill="url(#home-titlebar)" />
          <circle cx="46" cy="28" r="2.8" fill={AOE_BRAND_MARK_COLORS.detail} opacity="0.55" />
          <circle cx="55" cy="28" r="2.8" fill={AOE_BRAND_MARK_COLORS.detail} opacity="0.55" />
          <circle cx="64" cy="28" r="2.8" fill={AOE_BRAND_MARK_COLORS.detail} opacity="0.55" />
          <rect x="36" y="39" width="74" height="41" rx="3" fill={AOE_BRAND_MARK_COLORS.detail} opacity="0.22" />
          <text
            x="45"
            y="65"
            fontFamily="'Courier New', monospace"
            fontSize="20"
            fontWeight="bold"
            fill={AOE_BRAND_MARK_COLORS.prompt}
            opacity="0.85"
          >
            $
          </text>
          <rect x="64" y="51" width="9" height="17" rx="2" fill={AOE_BRAND_MARK_COLORS.prompt} opacity="0.75" />
        </g>
      </svg>
      <div className="mb-1 text-center">
        <p className="text-[11px] md:text-xs font-mono text-text-muted uppercase tracking-[0.2em]">agent of</p>
        <h1
          className="text-3xl md:text-5xl font-mono font-semibold text-brand-500 uppercase tracking-tight"
          style={{
            textShadow: AOE_BRAND_MARK_TEXT_SHADOW,
          }}
        >
          empires
        </h1>
      </div>

      {/* Session summary for returning users */}
      {stats.total > 0 && (
        <div className="flex items-center gap-2 text-xs font-mono text-text-muted mb-6">
          {stats.active > 0 && <span className="text-status-running">{stats.active} running</span>}
          {stats.waiting > 0 && <span className="text-status-waiting">{stats.waiting} waiting</span>}
          {stats.errors > 0 && (
            <span className="text-status-error">
              {stats.errors} error{stats.errors !== 1 ? "s" : ""}
            </span>
          )}
          <span>
            {stats.total} session{stats.total !== 1 ? "s" : ""} across {stats.projects} project
            {stats.projects !== 1 ? "s" : ""}
          </span>
        </div>
      )}

      {onPrepareSuggestion && onViewAllSuggestions && (
        <DashboardSuggestions
          key={suggestionsEpoch}
          focusHeading={suggestionsEpoch > 0}
          readOnly={readOnly || !canManageSuggestions}
          cityhall={!canManageSuggestions}
          onPrepare={onPrepareSuggestion}
          onEditPreset={onEditSuggestionPreset}
          canLink={canLinkSuggestions}
          onViewAll={onViewAllSuggestions}
          onOpenSession={onSelectSession}
          sessions={sessions}
        />
      )}

      {banner}

      {/* Action panes */}
      {readOnly ? (
        <div className="max-w-sm w-full">
          <p className="text-[13px] text-text-dim text-center mb-3">This dashboard is in read-only mode.</p>
          <ActionPane
            title="Docs"
            subtitle="Guides and reference"
            href="https://www.agent-of-empires.com/docs"
            icon="book"
          />
        </div>
      ) : (
        <div className="grid grid-cols-1 md:grid-cols-3 gap-3 max-w-2xl w-full">
          <ActionPane
            title="New session"
            subtitle="Pick a project, then launch a new session"
            onClick={onNewSession}
            icon="folder"
            featured
            dataTour={TOUR_ANCHORS.dashboardNewSession}
          />
          {canManageProjects && (
            <ActionPane title="Clone URL" subtitle="Clone a repo from a URL" onClick={onCloneFromUrl} icon="git" />
          )}
          <ActionPane
            title="Docs"
            subtitle="Guides and reference"
            href="https://www.agent-of-empires.com/docs"
            icon="book"
          />
        </div>
      )}

      {!readOnly && canManageProjects && !tourActive && onOpenSuggestionsMarketplace && (
        <SuggestionsSetupHint
          onOpenMarketplace={onOpenSuggestionsMarketplace}
          onFinished={() => setSuggestionsEpoch((n) => n + 1)}
        />
      )}

      {/* Plugin-contributed dashboard cards (#2366). Renders nothing (and adds
          no spacing) until a plugin pushes a card. */}
      <PluginCards />

      {/* Host-wide plugin panes (the home-pane slot). Renders nothing until a
          plugin pushes one. */}
      <PluginHomePanes />

      {/* Keyboard hint (desktop only) */}
      {!readOnly && (
        <p className="mt-4 text-[11px] font-mono text-text-dim hidden md:block">
          press <kbd className="px-1 py-0.5 rounded bg-surface-800 border border-surface-700/40">n</kbd> to create a
          session
        </p>
      )}
    </div>
  );
}

function ActionPane({
  title,
  subtitle,
  onClick,
  href,
  icon,
  featured,
  dataTour,
}: {
  title: string;
  subtitle: string;
  onClick?: () => void;
  href?: string;
  icon: "folder" | "git" | "book";
  featured?: boolean;
  dataTour?: TourAnchorId;
}) {
  const iconSvg = {
    folder: (
      <StrokeIcon size={24} strokeWidth="1.5" className="text-brand-500" hidden>
        <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z" />
      </StrokeIcon>
    ),
    git: (
      <StrokeIcon size={24} strokeWidth="1.5" className="text-brand-500" hidden>
        <circle cx="12" cy="18" r="3" />
        <circle cx="6" cy="6" r="3" />
        <circle cx="18" cy="6" r="3" />
        <path d="M18 9v2c0 .6-.4 1-1 1H7c-.6 0-1-.4-1-1V9" />
        <line x1="12" y1="12" x2="12" y2="15" />
      </StrokeIcon>
    ),
    book: (
      <StrokeIcon size={24} strokeWidth="1.5" className="text-brand-500" hidden>
        <path d="M4 19.5A2.5 2.5 0 0 1 6.5 17H20" />
        <path d="M6.5 2H20v20H6.5A2.5 2.5 0 0 1 4 19.5v-15A2.5 2.5 0 0 1 6.5 2z" />
      </StrokeIcon>
    ),
  };

  const classes = `${CARD} flex flex-col items-start gap-2 transition-colors cursor-pointer hover:border-brand-600/40 hover:bg-surface-800 active:bg-surface-700/60 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-brand-600 ${
    featured ? "md:col-span-2 md:row-span-2 py-6" : "py-4"
  }`;

  const content = (
    <>
      {iconSvg[icon]}
      <div>
        <p className={`font-medium text-text-primary ${featured ? "text-base" : "text-sm"}`}>{title}</p>
        <p className="text-[13px] text-text-secondary mt-0.5">{subtitle}</p>
      </div>
    </>
  );

  if (href) {
    return (
      <a href={href} target="_blank" rel="noopener noreferrer" data-tour={dataTour} className={classes}>
        {content}
      </a>
    );
  }

  return (
    <button onClick={onClick} data-tour={dataTour} className={`text-left ${classes}`}>
      {content}
    </button>
  );
}
