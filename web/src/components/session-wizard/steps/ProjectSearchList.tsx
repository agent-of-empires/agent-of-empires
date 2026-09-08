import type { ProjectInfo } from "../../../lib/types";
import type { RecentProject } from "./projectPicker";

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

interface Props {
  query: string;
  onQueryChange: (query: string) => void;
  filteredSaved: ProjectInfo[];
  filteredRecent: RecentProject[];
  isSelected: (path: string) => boolean;
  onSelect: (path: string) => void;
  /** Shown when a query matches nothing in either list. */
  emptyMessage?: string;
}

/** Search box + saved/recent rows shared by the main Project step and the
 *  extra-repos picker (#3743), so both offer the same searchable list
 *  instead of two divergent UIs over the same underlying project data. */
export function ProjectSearchList({
  query,
  onQueryChange,
  filteredSaved,
  filteredRecent,
  isSelected,
  onSelect,
  emptyMessage = "No projects match that search.",
}: Props) {
  return (
    <div className="flex flex-col gap-4">
      <input
        type="text"
        value={query}
        onChange={(e) => onQueryChange(e.target.value)}
        placeholder="Search projects by name or path"
        aria-label="Search projects"
        className="w-full px-3 py-2.5 text-sm bg-surface-900 border border-surface-700/40 rounded-md text-text-primary placeholder:text-text-dim focus:outline-none focus:border-brand-600"
      />

      {filteredSaved.length === 0 && filteredRecent.length === 0 && (
        <p className="text-sm text-text-dim">{emptyMessage}</p>
      )}

      {filteredSaved.length > 0 && (
        <div className="flex flex-col gap-1.5">
          <p className="text-[10px] font-mono uppercase tracking-wider text-text-dim">Saved projects</p>
          {filteredSaved.map((s) => (
            <button
              key={`saved:${s.scope}:${s.path}`}
              type="button"
              onClick={() => onSelect(s.path)}
              title={s.path}
              className={`flex items-center gap-3 px-3 py-2.5 rounded-md border transition-colors text-left cursor-pointer ${
                isSelected(s.path)
                  ? "border-brand-600 bg-surface-900"
                  : "border-surface-700/40 bg-surface-900 hover:border-surface-700 hover:bg-surface-850"
              }`}
            >
              <div className="flex-1 min-w-0">
                <div className="flex items-center gap-2">
                  <span className="text-sm font-medium text-text-primary truncate">{s.name}</span>
                  <span className="text-[10px] font-mono text-text-dim shrink-0">{s.scope}</span>
                </div>
                <div className="flex items-center gap-2 mt-0.5">
                  <span className="font-mono text-[11px] text-text-dim truncate">{s.path}</span>
                </div>
              </div>
            </button>
          ))}
        </div>
      )}

      {filteredRecent.length > 0 && (
        <div className="flex flex-col gap-1.5">
          {filteredSaved.length > 0 && (
            <p className="text-[10px] font-mono uppercase tracking-wider text-text-dim">Recent</p>
          )}
          {filteredRecent.map((r) => (
            <button
              key={r.path}
              type="button"
              onClick={() => onSelect(r.path)}
              title={r.path}
              className={`flex items-center gap-3 px-3 py-2.5 rounded-md border transition-colors text-left cursor-pointer ${
                isSelected(r.path)
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
                <span className="text-[10px] text-text-dim">
                  {r.sessionCount} session{r.sessionCount !== 1 ? "s" : ""}
                </span>
              </div>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
