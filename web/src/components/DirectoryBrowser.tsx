import { useCallback, useEffect, useRef, useState } from "react";
import { browseFilesystem, getHomePath } from "../lib/api";
import { safeGetItem, safeSetItem } from "../lib/safeStorage";
import type { DirEntry } from "../lib/types";

const LAST_DIR_KEY = "aoe-last-browse-dir";
const BROWSE_PAGE_SIZE = 100;

function loadLastDir(): string | null {
  return safeGetItem(LAST_DIR_KEY);
}

function saveLastDir(path: string) {
  safeSetItem(LAST_DIR_KEY, path);
}

interface Props {
  initialPath?: string;
  onSelect: (path: string) => void;
}

export function DirectoryBrowser({ initialPath, onSelect }: Props) {
  const [currentPath, setCurrentPath] = useState(initialPath || "");
  const [entries, setEntries] = useState<DirEntry[]>([]);
  const [hasMore, setHasMore] = useState(false);
  const [entryLimit, setEntryLimit] = useState(BROWSE_PAGE_SIZE);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [filter, setFilter] = useState("");
  const initialized = useRef(false);
  const loadSeq = useRef(0);
  const loadedFilter = useRef("");
  const filterDebounceHandle = useRef<number | null>(null);

  const clearFilterDebounce = useCallback(() => {
    if (filterDebounceHandle.current == null) return;
    window.clearTimeout(filterDebounceHandle.current);
    filterDebounceHandle.current = null;
  }, []);

  const loadPath = useCallback(
    async (path: string, limit: number, options: { filter?: string; resetFilter?: boolean } = {}): Promise<boolean> => {
      const seq = loadSeq.current + 1;
      loadSeq.current = seq;
      setLoading(true);
      setError(null);
      if (options.resetFilter) {
        clearFilterDebounce();
        loadedFilter.current = "";
        setFilter("");
      }
      try {
        const resp = await browseFilesystem(path, limit, options.filter);
        if (seq !== loadSeq.current) return false;
        if (!resp.ok) {
          setError("Can't access this folder. It may not exist or is outside the home directory.");
          return false;
        }
        // Success: update state even if empty (empty dir is valid)
        setEntries(resp.entries);
        setHasMore(resp.has_more);
        setEntryLimit(limit);
        setCurrentPath(path);
        loadedFilter.current = options.filter ?? "";
        return true;
      } catch {
        if (seq === loadSeq.current) {
          setError("Can't access this folder. It may not exist or is outside the home directory.");
        }
        return false;
      } finally {
        if (seq === loadSeq.current) setLoading(false);
      }
    },
    [clearFilterDebounce],
  );

  const navigate = useCallback(
    async (path: string): Promise<boolean> => loadPath(path, BROWSE_PAGE_SIZE, { resetFilter: true }),
    [loadPath],
  );

  const loadMore = useCallback(() => {
    if (loading) return;
    clearFilterDebounce();
    void loadPath(currentPath, entryLimit + BROWSE_PAGE_SIZE, {
      filter: filter.trim(),
    });
  }, [clearFilterDebounce, currentPath, entryLimit, filter, loadPath, loading]);

  useEffect(() => {
    if (!currentPath) return;
    const nextFilter = filter.trim();
    if (nextFilter === loadedFilter.current) return;
    clearFilterDebounce();
    filterDebounceHandle.current = window.setTimeout(() => {
      filterDebounceHandle.current = null;
      if (nextFilter === loadedFilter.current) return;
      void loadPath(currentPath, BROWSE_PAGE_SIZE, { filter: nextFilter });
    }, 150);
    return clearFilterDebounce;
  }, [clearFilterDebounce, currentPath, filter, loadPath]);

  // Discover and navigate to a starting dir on mount.
  // Priority: explicit initialPath -> last-used dir from localStorage -> home -> "/".
  useEffect(() => {
    if (initialized.current) return;
    initialized.current = true;

    queueMicrotask(async () => {
      if (initialPath && (await navigate(initialPath))) return;
      const lastDir = loadLastDir();
      if (lastDir && (await navigate(lastDir))) return;
      const home = await getHomePath();
      await navigate(home || "/");
    });
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  const pathSegments = currentPath.split("/").filter(Boolean);

  const goUp = () => {
    const parent = currentPath.split("/").slice(0, -1).join("/") || "/";
    navigate(parent);
  };

  const goToSegment = (index: number) => {
    const target = "/" + pathSegments.slice(0, index + 1).join("/");
    navigate(target);
  };

  const handleEntryClick = (entry: DirEntry) => {
    if (entry.is_git_repo) {
      // Save parent so reopening the picker lands one level up,
      // where this repo lives. Mirrors the TUI behavior.
      const parent = entry.path.split("/").slice(0, -1).join("/") || "/";
      saveLastDir(parent);
      onSelect(entry.path);
    } else {
      navigate(entry.path);
    }
  };

  // Breadcrumb truncation for mobile: show first + "..." + last 2
  const renderBreadcrumbs = () => {
    if (pathSegments.length <= 3) {
      return pathSegments.map((seg, i) => (
        <span key={i} className="flex items-center">
          <span className="text-text-dim mx-1">/</span>
          <button
            onClick={() => goToSegment(i)}
            className="text-text-secondary hover:text-text-primary cursor-pointer text-sm truncate max-w-[120px]"
          >
            {seg}
          </button>
        </span>
      ));
    }
    return (
      <>
        <span className="flex items-center">
          <span className="text-text-dim mx-1">/</span>
          <button
            onClick={() => goToSegment(0)}
            className="text-text-secondary hover:text-text-primary cursor-pointer text-sm"
          >
            {pathSegments[0]}
          </button>
        </span>
        <span className="text-text-dim mx-1">/...</span>
        {pathSegments.slice(-2).map((seg, i) => (
          <span key={i} className="flex items-center">
            <span className="text-text-dim mx-1">/</span>
            <button
              onClick={() => goToSegment(pathSegments.length - 2 + i)}
              className="text-text-secondary hover:text-text-primary cursor-pointer text-sm truncate max-w-[120px]"
            >
              {seg}
            </button>
          </span>
        ))}
      </>
    );
  };

  return (
    <div>
      {/* Breadcrumbs */}
      <nav aria-label="Directory path" className="flex items-center flex-wrap gap-0.5 mb-3 min-h-[28px]">
        <button
          onClick={() => navigate(currentPath.split("/").slice(0, 2).join("/") || "/")}
          className="text-text-dim hover:text-text-secondary cursor-pointer text-sm"
          title="Go to home"
        >
          ~
        </button>
        {renderBreadcrumbs()}
      </nav>

      {/* Search/filter */}
      <div className="mb-3">
        <input
          type="text"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          placeholder="Type to filter..."
          className="w-full bg-surface-900 border border-surface-700 rounded-md px-3 py-2 text-sm font-mono text-text-primary placeholder:text-text-dim focus:border-brand-600 focus:outline-none"
        />
      </div>

      {/* Directory listing */}
      <div
        className="border border-surface-700 rounded-lg overflow-y-auto max-h-[50vh]"
        role="listbox"
        aria-label="Directories"
      >
        {loading ? (
          // Skeleton loading rows
          <div className="animate-pulse">
            {[...Array(5)].map((_, i) => (
              <div key={i} className="flex items-center px-3 h-[44px] border-b border-surface-800 last:border-0">
                <div className="w-5 h-5 bg-surface-700 rounded mr-3" />
                <div className="h-4 bg-surface-700 rounded w-1/2" />
              </div>
            ))}
          </div>
        ) : error ? (
          <div className="px-4 py-6 text-center">
            <p className="text-sm text-status-error mb-3">{error}</p>
            <button
              onClick={() => navigate(currentPath || "/")}
              className="text-sm text-brand-600 hover:text-brand-500 cursor-pointer"
            >
              Retry
            </button>
          </div>
        ) : (
          <>
            {/* Parent directory link */}
            {pathSegments.length > 1 && (
              <button
                onClick={goUp}
                className="flex items-center w-full px-3 h-[44px] text-left hover:bg-surface-700/50 cursor-pointer transition-colors border-b border-surface-800 text-text-dim"
                role="option"
              >
                <span className="w-5 mr-3 text-center">..</span>
                <span className="text-sm">(parent directory)</span>
              </button>
            )}

            {/* Empty state */}
            {entries.length === 0 && (
              <div className="px-4 py-6 text-center">
                <p className="text-sm text-text-dim">
                  {filter ? "No folders match your filter" : "No visible subfolders here"}
                </p>
                {!filter && (
                  <p className="text-xs text-text-dim mt-1">Hidden folders (starting with .) are not shown</p>
                )}
              </div>
            )}

            {/* Directory entries */}
            {entries.map((entry) => (
              <button
                key={entry.path}
                onClick={() => handleEntryClick(entry)}
                className="flex items-center w-full px-3 h-[44px] text-left hover:bg-surface-700/50 cursor-pointer transition-colors border-b border-surface-800 last:border-0"
                role="option"
              >
                <span className="w-5 mr-3 text-center text-text-dim">
                  {entry.is_git_repo ? (
                    <svg className="w-4 h-4 inline text-accent-600" viewBox="0 0 16 16" fill="currentColor">
                      <path d="M15.698 7.287 8.712.302a1.03 1.03 0 0 0-1.457 0l-1.45 1.45 1.84 1.84a1.223 1.223 0 0 1 1.55 1.56l1.773 1.774a1.224 1.224 0 1 1-.733.693L8.535 5.918v4.27a1.229 1.229 0 1 1-1.008-.036V5.847a1.224 1.224 0 0 1-.664-1.608L5.045 2.422l-4.743 4.743a1.03 1.03 0 0 0 0 1.457l6.986 6.986a1.03 1.03 0 0 0 1.457 0l6.953-6.953a1.031 1.031 0 0 0 0-1.457" />
                    </svg>
                  ) : (
                    <svg className="w-4 h-4 inline text-text-dim" viewBox="0 0 20 20" fill="currentColor">
                      <path d="M2 6a2 2 0 0 1 2-2h5l2 2h5a2 2 0 0 1 2 2v6a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V6z" />
                    </svg>
                  )}
                </span>
                <span
                  className={`text-sm font-mono truncate ${entry.is_git_repo ? "text-text-primary font-medium" : "text-text-secondary"}`}
                >
                  {entry.name}
                </span>
                {entry.is_git_repo && (
                  <span className="ml-auto text-[10px] font-mono uppercase tracking-wider text-accent-600 bg-accent-600/10 px-1.5 py-0.5 rounded">
                    repo
                  </span>
                )}
              </button>
            ))}

            {hasMore && (
              <div className="px-3 py-3 text-center border-t border-surface-800">
                <p className="text-xs text-text-dim mb-2">Showing first {entries.length} entries.</p>
                <button
                  type="button"
                  onClick={loadMore}
                  disabled={loading}
                  className="h-8 px-3 text-xs font-sans text-text-secondary border border-surface-700 rounded-md hover:bg-surface-700/40 disabled:opacity-50 disabled:cursor-not-allowed cursor-pointer"
                >
                  Load {BROWSE_PAGE_SIZE} more
                </button>
              </div>
            )}
          </>
        )}
      </div>

      {/* Use the current folder as the working directory, even when it is not
          itself a git repo. Lets a user point a session at a root like
          ~/projects and let the agent discover the repos inside, instead of
          drilling down to pick one repo by hand. See #2680. */}
      <div className="mt-3 flex items-center justify-between gap-3">
        <span className="font-mono text-xs text-text-dim truncate min-w-0" title={currentPath}>
          {currentPath || "…"}
        </span>
        <button
          type="button"
          onClick={() => {
            if (!currentPath) return;
            saveLastDir(currentPath);
            onSelect(currentPath);
          }}
          disabled={!currentPath || loading}
          className="shrink-0 h-8 px-3 text-xs font-sans text-text-secondary border border-surface-700 rounded-md hover:bg-surface-700/40 disabled:opacity-50 disabled:cursor-not-allowed cursor-pointer"
        >
          Use this folder
        </button>
      </div>
    </div>
  );
}
