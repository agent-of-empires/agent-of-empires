/* eslint-disable react-refresh/only-export-components */
import { useEffect, useMemo, useState } from "react";
import { fetchSessions, cloneRepo } from "../../../lib/api";
import type { SessionResponse } from "../../../lib/types";
import { DirectoryBrowser } from "../../DirectoryBrowser";
import { ExtraReposPicker } from "./ExtraReposPicker";

interface WizardData {
  path: string;
  extraRepoPaths: string[];
  scratch: boolean;
  [key: string]: unknown;
}

/** Toggle switch matching the one used in `SessionStep.tsx`. Local copy
 *  rather than a shared import because exporting from `SessionStep`
 *  would force a circular component reference; the visual contract is
 *  the part that matters and is short. */
function Toggle({
  checked,
  onChange,
  ariaLabel,
}: {
  checked: boolean;
  onChange: (v: boolean) => void;
  ariaLabel: string;
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={ariaLabel}
      onClick={() => onChange(!checked)}
      className={`relative inline-flex h-7 w-12 shrink-0 items-center rounded-full transition-colors duration-200 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-brand-600 cursor-pointer ${
        checked ? "bg-brand-600" : "bg-surface-700"
      }`}
    >
      <span
        className={`inline-block h-5 w-5 rounded-full bg-white shadow-sm transition-transform duration-200 ${
          checked ? "translate-x-6" : "translate-x-1"
        }`}
      />
    </button>
  );
}

type Tab = "recent" | "browse" | "clone";

interface Props {
  data: WizardData;
  onChange: (field: string, value: unknown) => void;
  initialTab?: Tab;
}

interface RecentProject {
  path: string;
  displayName: string;
  lastAccessedAt: string | null;
  tool: string;
  sessionCount: number;
}

export function collectRecentProjects(
  sessions: SessionResponse[],
): RecentProject[] {
  const map = new Map<string, RecentProject>();
  for (const s of sessions) {
    // Scratch sessions live in transient `<app_dir>/scratch/<id>/`
    // directories that get deleted with the session (unless the user opts
    // in to keeping the dir). They must not appear in the Recent list,
    // where they would be re-selectable as a project.
    if (s.scratch) continue;
    // Multi-repo workspaces collapse to a single `main_repo_path` here, so
    // picking one from Recent would start a plain single-repo session and
    // silently drop the other repos. The project step cannot reconstruct a
    // workspace from one path, so keep them out of the list entirely.
    if (s.workspace_repos.length > 0) continue;
    // Normalize the trailing slash before keying, mirroring the backend's
    // dedup convention (`src/cli/add.rs` is_duplicate_session and
    // `src/server/api/sessions.rs` workspace_id_for_session both
    // `trim_end_matches('/')`). Without this, `/foo/bar` and `/foo/bar/`
    // become two separate entries with split session counts. The `|| "/"`
    // keeps the filesystem root from collapsing to an empty string.
    const raw = s.main_repo_path || s.project_path;
    if (!raw) continue;
    const path = raw.replace(/\/+$/, "") || "/";
    const existing = map.get(path);
    const ts = s.last_accessed_at ?? s.created_at ?? null;
    if (existing) {
      existing.sessionCount++;
      if ((ts ?? "") > (existing.lastAccessedAt ?? "")) {
        existing.lastAccessedAt = ts;
        existing.tool = s.tool;
      }
    } else {
      map.set(path, {
        path,
        displayName: path.split("/").filter(Boolean).pop() || path,
        lastAccessedAt: ts,
        tool: s.tool,
        sessionCount: 1,
      });
    }
  }
  return Array.from(map.values()).sort(
    (a, b) => (b.lastAccessedAt ?? "").localeCompare(a.lastAccessedAt ?? ""),
  );
}

function timeAgo(ts: string | null): string {
  if (!ts) return "";
  const diff = Date.now() - new Date(ts).getTime();
  const mins = Math.floor(diff / 60000);
  if (mins < 1) return "just now";
  if (mins < 60) return `${mins}m ago`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  return `${days}d ago`;
}

export function ProjectStep({ data, onChange, initialTab }: Props) {
  const [recent, setRecent] = useState<RecentProject[]>([]);
  const [loading, setLoading] = useState(true);
  const [activeTab, setActiveTab] = useState<Tab>(initialTab ?? "recent");

  // Clone state
  const [cloneUrl, setCloneUrl] = useState("");
  const [cloneDestination, setCloneDestination] = useState("");
  const [shallowClone, setShallowClone] = useState(false);
  const [cloning, setCloning] = useState(false);
  const [cloneError, setCloneError] = useState<string | null>(null);
  const [showAdvanced, setShowAdvanced] = useState(false);

  useEffect(() => {
    fetchSessions().then((envelope) => {
      if (envelope) {
        const projects = collectRecentProjects(envelope.sessions).slice(0, 6);
        setRecent(projects);
        if (projects.length === 0 && !initialTab) {
          setActiveTab("browse");
        }
      }
      setLoading(false);
    });
  }, [initialTab]);

  const filteredRecent = useMemo(() => {
    if (!data.path) return recent;
    const q = data.path.toLowerCase();
    return recent.filter(
      (r) => r.path.toLowerCase().includes(q) || r.displayName.toLowerCase().includes(q),
    );
  }, [recent, data.path]);

  const hasRecents = recent.length > 0;

  const handleBrowseSelect = (path: string) => {
    onChange("path", path);
    setActiveTab("recent");
  };

  const handleClone = async () => {
    const url = cloneUrl.trim();
    if (!url) return;
    setCloning(true);
    setCloneError(null);
    const dest = cloneDestination.trim() || undefined;
    const result = await cloneRepo(url, { destination: dest, shallow: shallowClone });
    setCloning(false);
    if (result.ok && result.path) {
      onChange("path", result.path);
      setCloneUrl("");
      setCloneDestination("");
      setActiveTab("recent");
    } else {
      setCloneError(result.error || "Clone failed");
    }
  };

  const tabs: { id: Tab; label: string }[] = [
    ...(hasRecents ? [{ id: "recent" as Tab, label: "Recent" }] : []),
    { id: "browse", label: "Browse" },
    { id: "clone", label: "Clone URL" },
  ];

  return (
    <div>
      <h2 className="text-lg font-semibold text-text-primary mb-1">Project folder</h2>
      <p className="text-sm text-text-muted mb-4">
        Pick a recent project, browse for one, or clone from a URL.
      </p>

      {/* Scratch-session toggle. Sits above the project-source tabs
          because it is a mode (skip the path picker entirely) rather
          than another path source. The reducer enforces mutual
          exclusion with path/extraRepoPaths/useWorktree; see
          `wizardReducer.ts`. */}
      <label
        className="flex items-center justify-between gap-3 p-3 bg-surface-900 border border-surface-700 rounded-lg cursor-pointer mb-4"
        onClick={(e) => {
          // Avoid double-toggle when the user clicks the switch itself:
          // both the label and the inner button fire onChange otherwise.
          if ((e.target as HTMLElement).closest('button[role="switch"]')) return;
          onChange("scratch", !data.scratch);
        }}
      >
        <div className="flex-1">
          <div className="text-sm font-medium text-text-primary">Skip project folder</div>
          <div className="text-xs text-text-dim mt-0.5 leading-snug">
            Run the agent in a fresh scratch directory under your AoE app data folder. The folder is removed when you delete the session.
          </div>
        </div>
        <Toggle
          checked={data.scratch}
          onChange={(v) => onChange("scratch", v)}
          ariaLabel="Skip project folder"
        />
      </label>

      {data.scratch && (
        <div className="px-3 py-2.5 bg-surface-900 border border-brand-600/30 rounded-md">
          <p className="text-[10px] font-mono uppercase tracking-wider text-text-dim mb-1">Scratch session</p>
          <p className="text-sm text-text-primary">
            A fresh scratch directory under your AoE app data folder is created when you launch this session.
          </p>
        </div>
      )}

      {!data.scratch && (
      <>
      {/* Tab bar */}
      {!loading && (
        <div className="flex gap-1 mb-4 border-b border-surface-700/30">
          {tabs.map((tab) => (
            <button
              key={tab.id}
              type="button"
              onClick={() => setActiveTab(tab.id)}
              className={`px-3 py-2 text-sm cursor-pointer transition-colors border-b-2 -mb-px ${
                activeTab === tab.id
                  ? "border-brand-600 text-text-primary"
                  : "border-transparent text-text-dim hover:text-text-secondary"
              }`}
            >
              {tab.label}
            </button>
          ))}
        </div>
      )}

      {/* Loading skeleton */}
      {loading && (
        <div className="animate-pulse space-y-2">
          {[...Array(3)].map((_, i) => (
            <div key={i} className="h-[60px] bg-surface-900 border border-surface-700/40 rounded-md" />
          ))}
        </div>
      )}

      {/* Recent projects tab */}
      {!loading && activeTab === "recent" && hasRecents && (
        <div className="flex flex-col gap-1.5">
          {filteredRecent.map((r) => (
            <button
              key={r.path}
              type="button"
              onClick={() => onChange("path", r.path)}
              className={`flex items-center gap-3 px-3 py-2.5 rounded-md border transition-colors text-left cursor-pointer ${
                data.path === r.path
                  ? "border-brand-600 bg-surface-900"
                  : "border-surface-700/40 bg-surface-900 hover:border-surface-700 hover:bg-surface-850"
              }`}
            >
              <div className="flex-1 min-w-0">
                <div className="flex items-center gap-2">
                  <span className="text-sm font-medium text-text-primary truncate">{r.displayName}</span>
                  <span className="text-[10px] font-mono text-text-dim shrink-0">{r.tool}</span>
                </div>
                <div className="flex items-center gap-2 mt-0.5">
                  <span className="font-mono text-[11px] text-text-dim truncate">{r.path}</span>
                </div>
              </div>
              <div className="flex flex-col items-end shrink-0 gap-0.5">
                <span className="text-[10px] text-text-dim">{timeAgo(r.lastAccessedAt)}</span>
                <span className="text-[10px] text-text-dim">{r.sessionCount} session{r.sessionCount !== 1 ? "s" : ""}</span>
              </div>
            </button>
          ))}
        </div>
      )}

      {/* Browse tab */}
      {!loading && activeTab === "browse" && (
        <DirectoryBrowser onSelect={handleBrowseSelect} />
      )}

      {/* Clone from URL tab */}
      {!loading && activeTab === "clone" && (
        <div className="space-y-3">
          <div>
            <label htmlFor="clone-url" className="block text-sm text-text-secondary mb-1.5">
              Repository URL
            </label>
            <input
              id="clone-url"
              type="text"
              value={cloneUrl}
              onChange={(e) => { setCloneUrl(e.target.value); setCloneError(null); }}
              onKeyDown={(e) => { if (e.key === "Enter" && cloneUrl.trim() && !cloning) handleClone(); }}
              placeholder="https://github.com/user/repo.git"
              className="w-full px-3 py-2.5 text-sm bg-surface-900 border border-surface-700/40 rounded-md text-text-primary placeholder:text-text-dim focus:outline-none focus:border-brand-600 font-mono"
              disabled={cloning}
              autoFocus
            />
          </div>

          {/* Advanced options (collapsed by default) */}
          <button
            type="button"
            onClick={() => setShowAdvanced(!showAdvanced)}
            className="text-[12px] text-text-dim hover:text-text-secondary cursor-pointer flex items-center gap-1 transition-colors"
          >
            <svg className={`w-3 h-3 transition-transform ${showAdvanced ? "rotate-90" : ""}`} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
              <polyline points="9 18 15 12 9 6" />
            </svg>
            Advanced
          </button>

          {showAdvanced && (
            <div className="space-y-3 pl-1 border-l-2 border-surface-700/30 ml-1">
              <div>
                <label htmlFor="clone-dest" className="block text-[12px] text-text-dim mb-1">
                  Destination path (optional)
                </label>
                <input
                  id="clone-dest"
                  type="text"
                  value={cloneDestination}
                  onChange={(e) => { setCloneDestination(e.target.value); setCloneError(null); }}
                  placeholder="~/my-repo"
                  className="w-full px-3 py-2 text-sm bg-surface-900 border border-surface-700/40 rounded-md text-text-primary placeholder:text-text-dim focus:outline-none focus:border-brand-600 font-mono"
                  disabled={cloning}
                />
              </div>
              <label className="flex items-center gap-2 cursor-pointer">
                <input
                  type="checkbox"
                  checked={shallowClone}
                  onChange={(e) => setShallowClone(e.target.checked)}
                  className="accent-brand-600"
                  disabled={cloning}
                />
                <span className="text-sm text-text-secondary">Shallow clone (--depth 1)</span>
                <span className="text-[10px] text-text-dim">faster for large repos</span>
              </label>
            </div>
          )}

          {cloneError && (
            <div className="px-3 py-2 bg-red-900/20 border border-red-700/30 rounded-md">
              <p className="text-sm text-red-400">{cloneError}</p>
            </div>
          )}

          <button
            type="button"
            onClick={handleClone}
            disabled={!cloneUrl.trim() || cloning}
            className={`w-full px-4 py-2.5 text-sm rounded-md font-medium transition-colors ${
              !cloneUrl.trim() || cloning
                ? "bg-brand-600/50 text-surface-900/50 cursor-not-allowed"
                : "bg-brand-600 hover:bg-brand-700 active:bg-brand-800 text-surface-900 cursor-pointer"
            }`}
          >
            {cloning ? (
              <span className="flex items-center justify-center gap-2">
                <svg className="animate-spin h-4 w-4" viewBox="0 0 24 24" fill="none">
                  <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" />
                  <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z" />
                </svg>
                Cloning...
              </span>
            ) : (
              "Clone repository"
            )}
          </button>

          <div className="flex items-start gap-1.5 text-[11px] text-text-dim">
            <span>The repository will be cloned into your home directory.</span>
            <span className="relative group/info inline-flex shrink-0 mt-px">
              <svg className="w-3.5 h-3.5 text-text-dim cursor-help" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                <circle cx="12" cy="12" r="10" />
                <path d="M12 16v-4" />
                <path d="M12 8h.01" />
              </svg>
              <span className="pointer-events-none absolute right-0 bottom-full mb-1.5 w-56 px-2.5 py-2 rounded bg-surface-950 border border-surface-700 text-[11px] leading-relaxed text-text-secondary opacity-0 scale-95 transition-all duration-100 group-hover/info:opacity-100 group-hover/info:scale-100 z-50">
                Uses the git credentials from the environment where the server is running (SSH keys, credential helpers, GH_TOKEN, etc). Private repos work if your git is already authenticated.
              </span>
            </span>
          </div>
        </div>
      )}

      {/* Selected path display */}
      {data.path && activeTab !== "browse" && (
        <div className="mt-4 px-3 py-2 bg-surface-900 border border-brand-600/30 rounded-md">
          <p className="text-[10px] font-mono uppercase tracking-wider text-text-dim mb-1">Selected project</p>
          <p className="text-sm font-mono text-text-primary truncate">{data.path}</p>
        </div>
      )}

      {/* Extra repos picker (multi-repo workspace) */}
      {data.path && activeTab !== "browse" && (
        <div className="mt-5 pt-4 border-t border-surface-700/30">
          <ExtraReposPicker
            primaryPath={data.path}
            selectedPaths={data.extraRepoPaths}
            onChange={(paths) => onChange("extraRepoPaths", paths)}
          />
        </div>
      )}
      </>
      )}
    </div>
  );
}
