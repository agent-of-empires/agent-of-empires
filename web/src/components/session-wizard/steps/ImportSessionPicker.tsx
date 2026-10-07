import { useEffect, useMemo, useState } from "react";
import { listImportableSessions, type ImportableSessionsResult } from "../../../lib/api";
import { safeGetItem, safeSetItem } from "../../../lib/safeStorage";
import type { ImportableSession } from "../../../lib/types";

const IMPORT_AGENT_KEY = "aoe.importAgent";
/** Activity this recent may mean the session is still open in another terminal. */
const RECENT_ACTIVITY_MS = 10 * 60 * 1000;

function initialAgent(agents: string[]): string {
  const stored = safeGetItem(IMPORT_AGENT_KEY);
  if (stored && agents.includes(stored)) return stored;
  return agents.includes("claude") ? "claude" : (agents[0] ?? "claude");
}

function activityWarning(s: ImportableSession): string | null {
  const ms = s.updated_at ? Date.parse(s.updated_at) : NaN;
  if (Number.isNaN(ms)) return "last activity unknown";
  if (Date.now() - ms < RECENT_ACTIVITY_MS) return "may still be open elsewhere";
  return null;
}

/** Lists one agent's native sessions to import; ones whose cwd is gone cannot be resumed. */
export function ImportSessionPicker({
  agents,
  profile,
  onSelect,
  selectedSessionId,
}: {
  agents: string[];
  profile?: string;
  onSelect: (session: ImportableSession, agent: string) => void;
  selectedSessionId?: string;
}) {
  const [agent, setAgent] = useState(() => initialAgent(agents));
  const [loaded, setLoaded] = useState<{
    agent: string;
    profile?: string;
    result: ImportableSessionsResult;
  } | null>(null);
  const result = loaded?.agent === agent && loaded.profile === profile ? loaded.result : null;
  const [filter, setFilter] = useState("");
  // Missing-cwd sessions are hidden until toggled, then shown disabled.
  const [showMissing, setShowMissing] = useState(false);

  useEffect(() => {
    let active = true;
    listImportableSessions(agent, profile).then((r) => {
      if (active) setLoaded({ agent, profile, result: r });
    });
    return () => {
      active = false;
    };
  }, [agent, profile]);

  const sessions = useMemo(() => (result?.ok ? result.sessions : []), [result]);
  const hasMissing = sessions.some((s) => !s.cwd_exists);

  const filtered = useMemo(() => {
    const q = filter.trim().toLowerCase();
    return sessions.filter((s) => {
      if (!showMissing && !s.cwd_exists) return false;
      if (!q) return true;
      return (s.title ?? "").toLowerCase().includes(q) || s.cwd.toLowerCase().includes(q);
    });
  }, [sessions, filter, showMissing]);

  const choose = (s: ImportableSession) => {
    const warning = activityWarning(s);
    if (
      warning &&
      !window.confirm(
        `${warning === "last activity unknown" ? "This session's last activity is unknown" : "This session may still be open elsewhere"}. Two agents writing the same session can corrupt it. Import anyway?`,
      )
    ) {
      return;
    }
    onSelect(s, agent);
  };

  const pickAgent = (name: string) => {
    setAgent(name);
    safeSetItem(IMPORT_AGENT_KEY, name);
  };

  return (
    <div className="flex flex-col gap-2">
      <select
        value={agent}
        onChange={(e) => pickAgent(e.target.value)}
        aria-label="Agent to import from"
        className="w-full rounded-md border border-surface-700 bg-surface-900 px-3 py-2 text-sm focus-visible:outline-2 focus-visible:outline-brand-600"
      >
        {agents.map((name) => (
          <option key={name} value={name}>
            {name}
          </option>
        ))}
      </select>
      <SessionList
        agent={agent}
        result={result}
        filtered={filtered}
        filter={filter}
        onFilterChange={setFilter}
        hasMissing={hasMissing}
        showMissing={showMissing}
        onShowMissingChange={setShowMissing}
        selectedSessionId={selectedSessionId}
        onChoose={choose}
      />
    </div>
  );
}

function SessionList({
  agent,
  result,
  filtered,
  filter,
  onFilterChange,
  hasMissing,
  showMissing,
  onShowMissingChange,
  selectedSessionId,
  onChoose,
}: {
  agent: string;
  result: ImportableSessionsResult | null;
  filtered: ImportableSession[];
  filter: string;
  onFilterChange: (value: string) => void;
  hasMissing: boolean;
  showMissing: boolean;
  onShowMissingChange: (value: boolean) => void;
  selectedSessionId?: string;
  onChoose: (s: ImportableSession) => void;
}) {
  if (result === null) {
    return <div className="p-4 text-sm text-content-subtle">Listing {agent} sessions…</div>;
  }
  if (!result.ok) {
    return (
      <div className="p-4 text-sm text-content-subtle">
        {result.error === "list_unsupported" ? "This agent can't list sessions." : result.message}
      </div>
    );
  }
  if (result.sessions.length === 0) {
    return (
      <div className="p-4 text-sm text-content-subtle">
        No existing {agent} sessions found. Run <code>{agent}</code> in a project first, then import it here.
      </div>
    );
  }

  return (
    <>
      <input
        type="text"
        value={filter}
        onChange={(e) => onFilterChange(e.target.value)}
        placeholder="Filter by title or path"
        aria-label="Filter sessions"
        className="w-full rounded-md border border-surface-700 bg-surface-900 px-3 py-2 text-sm focus-visible:outline-2 focus-visible:outline-brand-600"
      />
      {hasMissing && (
        <label className="flex items-center gap-2 text-xs text-content-subtle">
          <input
            type="checkbox"
            checked={showMissing}
            onChange={(e) => onShowMissingChange(e.target.checked)}
            aria-label="Show sessions with missing directories"
          />
          Show sessions whose directory is missing
        </label>
      )}
      {result.truncated && <p className="text-xs text-content-subtle">Showing newest 200.</p>}
      <ul className="flex max-h-80 flex-col gap-1 overflow-y-auto" aria-label="Importable sessions">
        {filtered.map((s) => {
          const selected = s.session_id === selectedSessionId;
          const warning = activityWarning(s);
          return (
            <li key={s.session_id}>
              <button
                type="button"
                disabled={!s.cwd_exists}
                aria-pressed={selected}
                onClick={() => onChoose(s)}
                title={s.cwd_exists ? s.cwd : `${s.cwd} (directory no longer exists)`}
                className={`flex w-full flex-col items-start gap-0.5 rounded-md border px-3 py-2 text-left transition-colors ${
                  selected ? "border-brand-500 bg-surface-800 ring-1 ring-brand-500" : "border-surface-700"
                } ${
                  s.cwd_exists
                    ? "cursor-pointer hover:border-brand-600 hover:bg-surface-800"
                    : "cursor-not-allowed opacity-50"
                }`}
              >
                <span className="line-clamp-1 text-sm font-medium">
                  {s.title || <span className="italic text-content-subtle">(no prompt yet)</span>}
                </span>
                <span className="line-clamp-1 text-xs text-content-subtle">{s.cwd}</span>
                <span className="text-xs text-content-subtle">
                  {s.updated_at ? formatRelative(s.updated_at) : "last activity unknown"}
                  {s.updated_at && warning && ` · ${warning}`}
                  {!s.cwd_exists && " · directory missing"}
                </span>
              </button>
            </li>
          );
        })}
        {filtered.length === 0 && (
          <li className="px-3 py-2 text-sm text-content-subtle">No sessions match "{filter}".</li>
        )}
      </ul>
    </>
  );
}

function formatRelative(updatedAt: string): string {
  const ms = Date.parse(updatedAt);
  if (Number.isNaN(ms)) return "unknown";
  const diff = Date.now() - ms;
  const min = Math.floor(diff / 60000);
  if (min < 1) return "just now";
  if (min < 60) return `${min}m ago`;
  const hr = Math.floor(min / 60);
  if (hr < 24) return `${hr}h ago`;
  const day = Math.floor(hr / 24);
  if (day < 30) return `${day}d ago`;
  const mon = Math.floor(day / 30);
  return `${mon}mo ago`;
}
