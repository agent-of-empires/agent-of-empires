import { useCallback, useMemo, useRef, useState } from "react";
import { flushSync } from "react-dom";
import { sessionRawFileUrl } from "../lib/api";
import { useFilesIndex } from "./acp/useFilesIndex";
import { FileContentViewer } from "./diff/FileContentViewer";
import { FileContextMenu, type PathMenuState } from "./diff/FileContextMenu";
import { Centered } from "./diff/viewerChrome";
import { INPUT } from "../lib/surfaces";
import { BUTTON_SECONDARY_SM } from "../lib/surfaces";

interface Props {
  sessionId: string | null;
  structured?: boolean;
}

/** Browse the files under a session's project_path and view them. */
export function FilesPane({ sessionId, structured }: Props) {
  const { files, loading, error, reload } = useFilesIndex(sessionId ?? "");
  const [filter, setFilter] = useState("");
  const [selected, setSelected] = useState<string | null>(null);
  // Row nodes by path, so closing the viewer can return focus to the row the
  // user opened instead of dumping them at the top of the pane.
  const rowRefs = useRef(new Map<string, HTMLButtonElement>());
  const [menu, setMenu] = useState<PathMenuState | null>(null);
  const closeMenu = useCallback(() => setMenu(null), []);

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
    return <Centered className="bg-surface-900 text-text-dim">No active session</Centered>;
  }

  if (selected) {
    return (
      <FileContentViewer
        structured={structured}
        sessionId={sessionId}
        filePath={selected}
        onBack={() => handleBack(selected)}
      />
    );
  }

  return (
    <div className="flex-1 flex flex-col bg-surface-900 overflow-hidden">
      <FileContextMenu menu={menu} onClose={closeMenu} />
      <div className="px-3 py-2 border-b border-surface-700/50 shrink-0">
        <input
          type="text"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          placeholder="Filter files..."
          aria-label="Filter files"
          className={`${INPUT} !py-1 font-mono`}
        />
      </div>
      <div className="flex-1 min-h-0 overflow-auto">
        {loading ? (
          <Centered className="h-full text-text-dim">Loading files...</Centered>
        ) : error ? (
          // A failed fetch must not read as "this session has no files".
          <Centered className="h-full flex-col gap-2 text-status-error-text">
            <span>Could not load the file list</span>
            <button type="button" onClick={reload} className={BUTTON_SECONDARY_SM}>
              Retry
            </button>
          </Centered>
        ) : shown.length === 0 ? (
          <Centered className="h-full text-text-dim">
            {files.length === 0 ? "No files in this session" : "No matching files"}
          </Centered>
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
              onContextMenu={(e) => {
                e.preventDefault();
                setMenu({ x: e.clientX, y: e.clientY, path: f, open: { url: sessionRawFileUrl(sessionId, f) } });
              }}
              className="w-full text-left px-3 py-1 max-md:py-2.5 font-mono text-[12px] max-md:text-[13px] text-text-secondary hover:bg-surface-800 hover:text-text-primary truncate cursor-pointer"
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
