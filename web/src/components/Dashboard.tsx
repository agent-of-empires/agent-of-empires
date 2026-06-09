import { useMemo } from "react";
import type { SessionResponse } from "../lib/types";
import { isSessionActive } from "../lib/session";
import { useIdleDecayWindowMs } from "../lib/idleDecay";
import { TOUR_ANCHORS, type TourAnchorId } from "../lib/tourSteps";

interface Props {
  sessions: SessionResponse[];
  onSelectSession: (sessionId: string) => void;
  onNewSession: () => void;
  onCloneFromUrl: () => void;
  onToggleSidebar: () => void;
  readOnly?: boolean;
}

export function Dashboard({ sessions, onNewSession, onCloneFromUrl, onToggleSidebar, readOnly }: Props) {
  const idleDecayWindowMs = useIdleDecayWindowMs();
  const stats = useMemo(() => {
    const projects = new Set<string>();
    let active = 0;
    let waiting = 0;
    let errors = 0;
    for (const s of sessions) {
      projects.add(s.main_repo_path || s.project_path);
      if (isSessionActive(s, idleDecayWindowMs)) active++;
      if (s.status === "Waiting") waiting++;
      if (s.status === "Error") errors++;
    }
    return { active, waiting, errors, projects: projects.size };
  }, [idleDecayWindowMs, sessions]);

  return (
    <div className="flex-1 flex flex-col items-center justify-center bg-surface-950 px-4">
      {/* Logo + Title */}
      <svg viewBox="0 0 128 128" className="w-12 h-12 md:w-16 md:h-16 mb-3" aria-hidden="true">
        <defs>
          <linearGradient id="home-win-back" x1="0" y1="0" x2="0" y2="1">
            <stop offset="0%" stopColor="#78350f" />
            <stop offset="100%" stopColor="#451a03" />
          </linearGradient>
          <linearGradient id="home-win-mid" x1="0" y1="0" x2="0" y2="1">
            <stop offset="0%" stopColor="#92400e" />
            <stop offset="100%" stopColor="#78350f" />
          </linearGradient>
          <linearGradient id="home-win-front" x1="0" y1="0" x2="0" y2="1">
            <stop offset="0%" stopColor="#f59e0b" />
            <stop offset="100%" stopColor="#d97706" />
          </linearGradient>
          <linearGradient id="home-titlebar" x1="0" y1="0" x2="0" y2="1">
            <stop offset="0%" stopColor="#fbbf24" />
            <stop offset="100%" stopColor="#f59e0b" />
          </linearGradient>
        </defs>
        <rect x="10" y="38" width="76" height="60" rx="6" fill="url(#home-win-back)" opacity="0.6" />
        <rect x="20" y="28" width="76" height="60" rx="6" fill="url(#home-win-mid)" opacity="0.7" />
        <g>
          <rect x="32" y="18" width="82" height="66" rx="6" fill="url(#home-win-front)" />
          <rect x="32" y="18" width="82" height="18" rx="6" fill="url(#home-titlebar)" />
          <rect x="32" y="30" width="82" height="6" fill="url(#home-titlebar)" />
          <circle cx="46" cy="28" r="2.8" fill="#b45309" opacity="0.55" />
          <circle cx="55" cy="28" r="2.8" fill="#b45309" opacity="0.55" />
          <circle cx="64" cy="28" r="2.8" fill="#b45309" opacity="0.55" />
          <rect x="36" y="39" width="74" height="41" rx="3" fill="#b45309" opacity="0.22" />
          <text
            x="45"
            y="65"
            fontFamily="'Courier New', monospace"
            fontSize="20"
            fontWeight="bold"
            fill="#fef3c7"
            opacity="0.85"
          >
            $
          </text>
          <rect x="64" y="51" width="9" height="17" rx="2" fill="#fef3c7" opacity="0.75" />
        </g>
      </svg>
      <div className="mb-1 text-center">
        <p className="text-[11px] md:text-xs font-mono text-text-muted uppercase tracking-[0.2em]">agent of</p>
        <h1
          className="text-3xl md:text-5xl font-mono font-semibold text-brand-500 uppercase tracking-tight"
          style={{
            textShadow: "0 0 24px rgba(245,158,11,0.5), 0 0 48px rgba(245,158,11,0.25), 0 0 80px rgba(245,158,11,0.1)",
          }}
        >
          empires
        </h1>
      </div>

      {/* Session summary for returning users */}
      {sessions.length > 0 && (
        <div className="flex items-center gap-2 text-xs font-mono text-text-muted mb-6">
          {stats.active > 0 && <span className="text-status-running">{stats.active} running</span>}
          {stats.waiting > 0 && <span className="text-status-waiting">{stats.waiting} waiting</span>}
          {stats.errors > 0 && (
            <span className="text-status-error">
              {stats.errors} error{stats.errors !== 1 ? "s" : ""}
            </span>
          )}
          <span>
            {sessions.length} session{sessions.length !== 1 ? "s" : ""} across {stats.projects} project
            {stats.projects !== 1 ? "s" : ""}
          </span>
        </div>
      )}

      {/* Mobile sidebar toggle */}
      <button
        onClick={onToggleSidebar}
        className="md:hidden mb-4 w-full max-w-md px-4 py-2.5 rounded-lg bg-surface-900 border border-surface-700/40 text-text-secondary text-sm flex items-center justify-center gap-2 cursor-pointer hover:bg-surface-850 active:bg-surface-800 transition-colors"
      >
        <svg
          width="16"
          height="16"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          strokeWidth="2"
          strokeLinecap="round"
          strokeLinejoin="round"
          aria-hidden="true"
        >
          <rect x="3" y="3" width="18" height="18" rx="2" />
          <line x1="9" y1="3" x2="9" y2="21" />
        </svg>
        Show sessions
      </button>

      {/* Action panes */}
      {readOnly ? (
        <div className="max-w-sm w-full">
          <p className="text-xs text-text-dim text-center mb-3">This dashboard is in read-only mode.</p>
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
          <ActionPane title="Clone URL" subtitle="Clone a repo from a URL" onClick={onCloneFromUrl} icon="git" />
          <ActionPane
            title="Docs"
            subtitle="Guides and reference"
            href="https://www.agent-of-empires.com/docs"
            icon="book"
          />
        </div>
      )}

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
      <svg
        width="24"
        height="24"
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinecap="round"
        strokeLinejoin="round"
        className="text-brand-500"
        aria-hidden="true"
      >
        <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z" />
      </svg>
    ),
    git: (
      <svg
        width="24"
        height="24"
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinecap="round"
        strokeLinejoin="round"
        className="text-brand-500"
        aria-hidden="true"
      >
        <circle cx="12" cy="18" r="3" />
        <circle cx="6" cy="6" r="3" />
        <circle cx="18" cy="6" r="3" />
        <path d="M18 9v2c0 .6-.4 1-1 1H7c-.6 0-1-.4-1-1V9" />
        <line x1="12" y1="12" x2="12" y2="15" />
      </svg>
    ),
    book: (
      <svg
        width="24"
        height="24"
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinecap="round"
        strokeLinejoin="round"
        className="text-brand-500"
        aria-hidden="true"
      >
        <path d="M4 19.5A2.5 2.5 0 0 1 6.5 17H20" />
        <path d="M6.5 2H20v20H6.5A2.5 2.5 0 0 1 4 19.5v-15A2.5 2.5 0 0 1 6.5 2z" />
      </svg>
    ),
  };

  const classes = `flex flex-col items-start gap-2 px-4 rounded-lg bg-surface-900 border border-surface-700/40 transition-colors cursor-pointer hover:border-brand-600/40 hover:bg-surface-850 active:bg-surface-800 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-brand-600 ${
    featured ? "md:col-span-2 md:row-span-2 py-6" : "py-4"
  }`;

  if (href) {
    return (
      <a href={href} target="_blank" rel="noopener noreferrer" data-tour={dataTour} className={classes}>
        {iconSvg[icon]}
        <div>
          <p className={`font-medium text-text-primary ${featured ? "text-base" : "text-sm"}`}>{title}</p>
          <p className="text-xs text-text-muted mt-0.5">{subtitle}</p>
        </div>
      </a>
    );
  }

  return (
    <button onClick={onClick} data-tour={dataTour} className={`text-left ${classes}`}>
      {iconSvg[icon]}
      <div>
        <p className={`font-medium text-text-primary ${featured ? "text-base" : "text-sm"}`}>{title}</p>
        <p className="text-xs text-text-muted mt-0.5">{subtitle}</p>
      </div>
    </button>
  );
}
