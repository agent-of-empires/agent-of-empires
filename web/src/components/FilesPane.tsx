import { useMemo, useRef, useState } from "react";
import { flushSync } from "react-dom";
import { useFilesIndex } from "./acp/useFilesIndex";
import { FileContentViewer } from "./diff/FileContentViewer";

interface Props {
  sessionId: string | null;
}

/**
 * Browse the files under a session's project_path and view them (#3088).
 * Backed by `GET /api/sessions/:id/acp/files` (git-agnostic, so non-git scratch
 * sessions list their files too); selecting a file opens it in
 * {@link FileContentViewer}, which renders Markdown and confines reads server
 * side. A flat, filterable list (ponytail: add a tree if it gets unwieldy).
 */
export function FilesPane({ sessionId }: Props) {
  const { files, loading, error, reload } = useFilesIndex(sessionId ?? "");
  const [filter, setFilter] = useState("");
  const [selected, setSelected] = useState<string | null>(null);
  // Row nodes by path, so closing the viewer can return focus to the row the
  // user opened instead of dumping them at the top of the pane.
  const rowRefs = useRef(new Map<string, HTMLButtonElement>());

  const shown = useMemo(() => {
    const q = filter.trim().toLowerCase();
    const list = q ? files.filter((f) => f.toLowerCase().includes(q)) : files;
    return list.slice(0, 500);
  }, [files, filter]);

  // The list is unmounted while the viewer is open, so flush the state change
  // first: the row has to exist again before it can take focus.
  const handleBack = (path: string) => {
    flushSync(() => setSelected(null));
    rowRefs.current.get(path)?.focus();
  };

  if (!sessionId) {
    return (
      <div className="flex-1 flex items-center justify-center bg-surface-900 text-text-dim">
        <span className="text-sm">No active session</span>
      </div>
    );
  }

  if (selected) {
    return <FileContentViewer sessionId={sessionId} filePath={selected} onBack={() => handleBack(selected)} />;
  }

  return (
    <div className="flex-1 flex flex-col bg-surface-900 overflow-hidden">
      <div className="px-3 py-2 border-b border-surface-700/20 shrink-0">
        <input
          type="text"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          placeholder="Filter files..."
          aria-label="Filter files"
          className="w-full bg-surface-950 border border-surface-700/40 rounded px-2 py-1 text-[12px] font-mono text-text-primary placeholder:text-text-dim focus:outline-none focus:border-brand-600"
        />
      </div>
      <div className="flex-1 min-h-0 overflow-auto">
        {loading ? (
          <div className="p-3 text-sm text-text-dim">Loading files...</div>
        ) : error ? (
          // A failed fetch must not read as "this session has no files".
          <div className="p-3 flex flex-col items-start gap-2">
            <span className="text-sm text-status-error">Could not load the file list</span>
            <button
              type="button"
              onClick={reload}
              className="px-2 py-0.5 text-[11px] font-mono rounded border border-surface-700/40 text-text-dim hover:text-text-secondary cursor-pointer transition-colors"
            >
              Retry
            </button>
          </div>
        ) : shown.length === 0 ? (
          <div className="p-3 text-sm text-text-dim">
            {files.length === 0 ? "No files in this session" : "No matching files"}
          </div>
        ) : (
          shown.map((f) => (
            <button
              key={f}
              type="button"
              ref={(el) => {
                if (el) rowRefs.current.set(f, el);
                else rowRefs.current.delete(f);
              }}
              onClick={() => setSelected(f)}
              className="w-full text-left px-3 py-1 font-mono text-[12px] text-text-secondary hover:bg-surface-800 hover:text-text-primary truncate cursor-pointer"
              title={f}
            >
              {f}
            </button>
          ))
        )}
      </div>
    </div>
  );
}
