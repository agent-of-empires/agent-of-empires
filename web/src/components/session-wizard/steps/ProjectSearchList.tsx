import type { Ref } from "react";
import type { ProjectInfo } from "../../../lib/types";
import type { RecentProject } from "./projectPicker";
import { CAPTION, INPUT } from "../../../lib/surfaces";
import { CHOICE, CHOICE_SELECTED } from "../../../lib/surfaces";

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
  inputRef?: Ref<HTMLInputElement>;
}

/** Search box plus saved and recent rows, shared by ProjectStep and ExtraReposPicker. */
export function ProjectSearchList({
  query,
  onQueryChange,
  filteredSaved,
  filteredRecent,
  isSelected,
  onSelect,
  emptyMessage = "No projects match that search.",
  inputRef,
}: Props) {
  return (
    <div className="flex flex-col gap-4">
      <input
        ref={inputRef}
        type="text"
        value={query}
        onChange={(e) => onQueryChange(e.target.value)}
        placeholder="Search projects by name or path"
        aria-label="Search projects"
        className={INPUT}
      />

      {filteredSaved.length === 0 && filteredRecent.length === 0 && (
        <p className="text-[15px] md:text-sm text-text-dim">{emptyMessage}</p>
      )}

      {filteredSaved.length > 0 && (
        <div className="flex flex-col gap-1.5">
          <p className={`${CAPTION} !mb-0.5`}>Projects</p>
          {filteredSaved.map((s) => (
            <button
              key={`saved:${s.scope}:${s.path}`}
              type="button"
              onClick={() => onSelect(s.path)}
              title={s.path}
              className={`${isSelected(s.path) ? CHOICE_SELECTED : CHOICE} flex items-center gap-3 cursor-pointer`}
            >
              <div className="flex-1 min-w-0">
                <div className="flex items-center gap-2">
                  <span className="text-[15px] md:text-sm font-medium text-text-primary truncate">{s.name}</span>
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
          {filteredSaved.length > 0 && <p className={`${CAPTION} !mb-0.5`}>Recent</p>}
          {filteredRecent.map((r) => (
            <button
              key={r.path}
              type="button"
              onClick={() => onSelect(r.path)}
              title={r.path}
              className={`${isSelected(r.path) ? CHOICE_SELECTED : CHOICE} flex items-center gap-3 cursor-pointer`}
            >
              <div className="flex-1 min-w-0">
                <div className="flex items-center gap-2">
                  <span className="text-[15px] md:text-sm font-medium text-text-primary truncate">{r.displayName}</span>
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
