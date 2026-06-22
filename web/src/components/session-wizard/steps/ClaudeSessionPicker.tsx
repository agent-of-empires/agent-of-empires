import { useEffect, useMemo, useState } from "react";
import { listClaudeSessions } from "../../../lib/api";
import type { ClaudeSessionSummary } from "../../../lib/types";

/** Picker for importing an existing Claude Code session into a structured-view
 *  session (#2276). Lists on-disk sessions newest-first with a filter box;
 *  selecting one hands the caller the id + cwd to prefill the wizard. Sessions
 *  whose recorded cwd no longer exists are shown disabled, since `claude
 *  --resume` has no valid working directory for them. */
export function ClaudeSessionPicker({
  onSelect,
  selectedSessionId,
}: {
  onSelect: (session: ClaudeSessionSummary) => void;
  /** Currently-picked session id, so the chosen row stays highlighted. */
  selectedSessionId?: string;
}) {
  const [sessions, setSessions] = useState<ClaudeSessionSummary[] | null>(null);
  const [filter, setFilter] = useState("");
  // Sessions whose recorded cwd is gone cannot be resumed, so hide them by
  // default; the toggle reveals them (shown disabled). See #2276.
  const [showMissing, setShowMissing] = useState(false);

  useEffect(() => {
    let active = true;
    listClaudeSessions().then((s) => {
      if (active) setSessions(s);
    });
    return () => {
      active = false;
    };
  }, []);

  const hasMissing = useMemo(() => (sessions ?? []).some((s) => !s.cwd_exists), [sessions]);

  const filtered = useMemo(() => {
    if (!sessions) return [];
    const q = filter.trim().toLowerCase();
    return sessions.filter((s) => {
      if (!showMissing && !s.cwd_exists) return false;
      if (!q) return true;
      return (s.title ?? "").toLowerCase().includes(q) || s.cwd.toLowerCase().includes(q);
    });
  }, [sessions, filter, showMissing]);

  if (sessions === null) {
    return <div className="p-4 text-sm text-content-subtle">Scanning for Claude Code sessions…</div>;
  }

  if (sessions.length === 0) {
    return (
      <div className="p-4 text-sm text-content-subtle">
        No existing Claude Code sessions found. Run <code>claude</code> in a project first, then import it here.
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-2">
      <input
        type="text"
        value={filter}
        onChange={(e) => setFilter(e.target.value)}
        placeholder="Filter by title or path"
        aria-label="Filter Claude sessions"
        className="w-full rounded-md border border-surface-700 bg-surface-900 px-3 py-2 text-sm focus-visible:outline-2 focus-visible:outline-brand-600"
      />
      {hasMissing && (
        <label className="flex items-center gap-2 text-xs text-content-subtle">
          <input
            type="checkbox"
            checked={showMissing}
            onChange={(e) => setShowMissing(e.target.checked)}
            aria-label="Show sessions with missing directories"
          />
          Show sessions whose directory is missing
        </label>
      )}
      <ul className="flex max-h-80 flex-col gap-1 overflow-y-auto" aria-label="Claude sessions">
        {filtered.map((s) => {
          const selected = s.session_id === selectedSessionId;
          return (
            <li key={s.session_id}>
              <button
                type="button"
                disabled={!s.cwd_exists}
                aria-pressed={selected}
                onClick={() => onSelect(s)}
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
                  {formatRelative(s.last_modified_ms)}
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
    </div>
  );
}

/** Coarse "x ago" stamp for the last-modified time. Avoids a date library;
 *  the picker only needs rough recency. */
function formatRelative(ms: number): string {
  if (!ms) return "unknown";
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
