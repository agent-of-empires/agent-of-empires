import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { RepoBase, RichDiffFile } from "../../lib/types";
import type { BranchInfo } from "../../lib/api";
import { buildDiffTree, type DiffTreeNode } from "../../lib/diffTree";
import { useWebSettings } from "../../hooks/useWebSettings";
import { fetchBranches, setSessionDiffBase } from "../../lib/api";
import { CopyPathContextMenu, type PathMenuState } from "./CopyPathContextMenu";

interface Props {
  files: RichDiffFile[];
  /** One entry per repo whose diff was computed. Single-repo sessions
   *  get a one-element array; workspace sessions get one per member.
   *  See #1047. */
  perRepoBases: RepoBase[];
  warning: string | null;
  selectedPath: string | null;
  selectedRepoName: string | undefined;
  loading: boolean;
  onSelectFile: (path: string, repoName?: string) => void;
  /** Session id used by the `vs <ref>` chip popover to PATCH the
   *  per-session base-branch override. `null` hides the picker. */
  sessionId?: string | null;
  /** Repo path used to populate the branch typeahead in the picker.
   *  `null` hides the picker. */
  repoPath?: string | null;
  /** Current persisted override (also reflected in `perRepoBases[0].base_branch`
   *  on next refetch). Used to label the "Reset" affordance and to
   *  pre-fill the typeahead. */
  baseBranchOverride?: string | null;
  /** Called after a successful PATCH so the parent re-fetches the diff
   *  against the new base. */
  onBaseBranchChanged?: () => void;
}

const STATUS_COLORS: Record<string, string> = {
  added: "text-status-running",
  modified: "text-status-waiting",
  deleted: "text-status-error",
  renamed: "text-accent-600",
  copied: "text-accent-600",
  untracked: "text-text-muted",
  conflicted: "text-status-waiting",
};

const STATUS_LETTERS: Record<string, string> = {
  added: "A",
  modified: "M",
  deleted: "D",
  renamed: "R",
  copied: "C",
  untracked: "?",
  conflicted: "U",
};

// Unique key for a tree node (dir path or file path)
function nodeKey(node: DiffTreeNode): string {
  return node.kind === "dir" ? `dir:${node.path}` : node.file.path;
}

function FlatList({
  files,
  selectedPath,
  selectedRepoName,
  onSelectFile,
  focusedIndex,
  onFocusIndex,
}: {
  files: RichDiffFile[];
  selectedPath: string | null;
  selectedRepoName: string | undefined;
  onSelectFile: (path: string, repoName?: string) => void;
  focusedIndex: number;
  onFocusIndex: (i: number) => void;
}) {
  return (
    <>
      {files.map((file, i) => {
        const parts = file.path.split("/");
        const fileName = parts.pop() || file.path;
        const dirPath = parts.length > 0 ? parts.join("/") + "/" : "";
        const isSelected = file.path === selectedPath && file.repo_name === selectedRepoName;
        const isFocused = i === focusedIndex;

        return (
          <button
            key={`${file.repo_name ?? ""}::${file.path}`}
            data-index={i}
            data-path={file.path}
            onClick={() => onSelectFile(file.path, file.repo_name)}
            onMouseEnter={() => onFocusIndex(i)}
            className={`w-full text-left px-3 py-1.5 cursor-pointer transition-colors flex items-center gap-2 ${
              isSelected ? "bg-surface-850 text-text-primary" : "text-text-secondary hover:bg-surface-800/50"
            } ${isFocused ? "outline outline-1 outline-brand-600/60 -outline-offset-1" : ""}`}
          >
            <span
              className={`shrink-0 font-mono text-[12px] w-3 text-center ${STATUS_COLORS[file.status] ?? "text-text-muted"}`}
            >
              {STATUS_LETTERS[file.status] ?? "?"}
            </span>
            <span className="truncate min-w-0 flex-1">
              {dirPath && <span className="font-mono text-[11px] text-text-dim">{dirPath}</span>}
              <span className="font-mono text-[12px]">{fileName}</span>
            </span>
            <span className="shrink-0 font-mono text-[11px] flex items-center gap-1">
              {file.additions > 0 && <span className="text-status-running">+{file.additions}</span>}
              {file.deletions > 0 && <span className="text-status-error">-{file.deletions}</span>}
            </span>
          </button>
        );
      })}
    </>
  );
}

function TreeView({
  nodes,
  selectedPath,
  selectedRepoName,
  onSelectFile,
  onToggleDir,
  focusedIndex,
  onFocusIndex,
  indentOffset = 0,
  repoNameForSelect,
}: {
  nodes: DiffTreeNode[];
  selectedPath: string | null;
  selectedRepoName: string | undefined;
  onSelectFile: (path: string, repoName?: string) => void;
  onToggleDir: (dirPath: string) => void;
  focusedIndex: number;
  onFocusIndex: (i: number) => void;
  /** Extra left padding in px applied to every row. Multi-repo passes
   *  a non-zero value so tree rows nest visually under the repo
   *  header. */
  indentOffset?: number;
  /** When set, file rows pass this as the `repoName` argument to
   *  `onSelectFile`. Multi-repo uses it so the right pane knows which
   *  repo the file belongs to; single-repo leaves it `undefined`. */
  repoNameForSelect?: string;
}) {
  return (
    <>
      {nodes.map((node, i) => {
        const isFocused = i === focusedIndex;
        const focusRing = isFocused ? "outline outline-1 outline-brand-600/60 -outline-offset-1" : "";

        if (node.kind === "dir") {
          return (
            <button
              key={nodeKey(node)}
              data-index={i}
              data-path={node.path}
              onClick={() => onToggleDir(node.path)}
              onMouseEnter={() => onFocusIndex(i)}
              aria-expanded={!node.collapsed}
              className={`w-full text-left py-1.5 cursor-pointer transition-colors flex items-center gap-1.5 text-text-muted hover:bg-surface-800/50 ${focusRing}`}
              style={{
                paddingLeft: `${node.depth * 16 + 12 + indentOffset}px`,
                paddingRight: 12,
              }}
            >
              <svg
                className={`w-3 h-3 shrink-0 text-text-dim transition-transform duration-75 ${
                  node.collapsed ? "-rotate-90" : ""
                }`}
                viewBox="0 0 16 16"
                fill="currentColor"
              >
                <path d="M4 6l4 4 4-4" />
              </svg>
              <span className="font-mono text-[12px] truncate flex-1">{node.name}</span>
              <span className="shrink-0 font-mono text-[10px] text-text-dim">{node.fileCount}</span>
              <span className="shrink-0 font-mono text-[11px] flex items-center gap-1">
                {node.additions > 0 && <span className="text-status-running">+{node.additions}</span>}
                {node.deletions > 0 && <span className="text-status-error">-{node.deletions}</span>}
              </span>
            </button>
          );
        }

        const file = node.file;
        const fileName = file.path.split("/").pop() || file.path;
        const isSelected = file.path === selectedPath && file.repo_name === selectedRepoName;

        return (
          <button
            key={`${file.repo_name ?? ""}::${file.path}`}
            data-index={i}
            data-path={file.path}
            onClick={() => onSelectFile(file.path, repoNameForSelect ?? file.repo_name)}
            onMouseEnter={() => onFocusIndex(i)}
            className={`w-full text-left py-1.5 cursor-pointer transition-colors flex items-center gap-2 ${
              isSelected ? "bg-surface-850 text-text-primary" : "text-text-secondary hover:bg-surface-800/50"
            } ${focusRing}`}
            style={{
              paddingLeft: `${node.depth * 16 + 12 + indentOffset}px`,
              paddingRight: 12,
            }}
          >
            <span
              className={`shrink-0 font-mono text-[12px] w-3 text-center ${STATUS_COLORS[file.status] ?? "text-text-muted"}`}
            >
              {STATUS_LETTERS[file.status] ?? "?"}
            </span>
            <span className="font-mono text-[12px] truncate flex-1">{fileName}</span>
            <span className="shrink-0 font-mono text-[11px] flex items-center gap-1">
              {file.additions > 0 && <span className="text-status-running">+{file.additions}</span>}
              {file.deletions > 0 && <span className="text-status-error">-{file.deletions}</span>}
            </span>
          </button>
        );
      })}
    </>
  );
}

function scrollToIndex(container: HTMLDivElement | null, index: number) {
  container?.querySelector(`[data-index="${index}"]`)?.scrollIntoView({ block: "nearest" });
}

export function DiffFileList({
  files,
  perRepoBases,
  warning,
  selectedPath,
  selectedRepoName,
  loading,
  onSelectFile,
  sessionId,
  repoPath,
  baseBranchOverride,
  onBaseBranchChanged,
}: Props) {
  const isMultiRepo = perRepoBases.length > 1;
  // Multi-repo workspaces compose multiple `(name, base_branch)` pairs;
  // single-repo sessions get one entry and we don't show a per-repo
  // header at all. See #1047.
  const singleBaseBranch = perRepoBases[0]?.base_branch ?? "main";
  const [collapsedRepos, setCollapsedRepos] = useState<Set<string>>(() => new Set());
  const toggleRepo = useCallback((name: string) => {
    setCollapsedRepos((prev) => {
      const next = new Set(prev);
      if (next.has(name)) next.delete(name);
      else next.add(name);
      return next;
    });
  }, []);
  const filesByRepo = useMemo(() => {
    const map = new Map<string, RichDiffFile[]>();
    for (const f of files) {
      const key = f.repo_name ?? "";
      const arr = map.get(key);
      if (arr) arr.push(f);
      else map.set(key, [f]);
    }
    return map;
  }, [files]);
  const { settings, update } = useWebSettings();
  const viewMode = settings.diffViewMode;
  const [collapsedDirs, setCollapsedDirs] = useState<Set<string>>(() => new Set(settings.collapsedDiffDirs));
  const [focusedIndex, setFocusedIndex] = useState(-1);
  const listRef = useRef<HTMLDivElement>(null);

  const totalAdditions = files.reduce((sum, f) => sum + f.additions, 0);
  const totalDeletions = files.reduce((sum, f) => sum + f.deletions, 0);

  const treeNodes = useMemo(() => buildDiffTree(files, collapsedDirs), [files, collapsedDirs]);

  // Persist collapsed dirs to settings whenever they change
  useEffect(() => {
    update({ collapsedDiffDirs: [...collapsedDirs] });
  }, [collapsedDirs, update]);

  const toggleDir = useCallback((dirPath: string) => {
    setCollapsedDirs((prev) => {
      const next = new Set(prev);
      if (next.has(dirPath)) {
        next.delete(dirPath);
      } else {
        next.add(dirPath);
      }
      return next;
    });
  }, []);

  const toggleViewMode = useCallback(() => {
    const next = viewMode === "flat" ? "tree" : "flat";
    update({ diffViewMode: next });
    setFocusedIndex(-1);
  }, [viewMode, update]);

  // Total number of visible items for keyboard nav
  const itemCount = viewMode === "tree" ? treeNodes.length : files.length;

  // Clamp focused index when item count shrinks
  const clampedFocusedIndex = focusedIndex >= itemCount ? -1 : focusedIndex;

  // Keyboard navigation
  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      if (itemCount === 0) return;

      switch (e.key) {
        case "ArrowDown": {
          e.preventDefault();
          setFocusedIndex((prev) => {
            const next = prev < itemCount - 1 ? prev + 1 : prev;
            scrollToIndex(listRef.current, next);
            return next;
          });
          break;
        }
        case "ArrowUp": {
          e.preventDefault();
          setFocusedIndex((prev) => {
            const next = prev > 0 ? prev - 1 : 0;
            scrollToIndex(listRef.current, next);
            return next;
          });
          break;
        }
        case "ArrowRight": {
          const rNode = viewMode === "tree" ? treeNodes[clampedFocusedIndex] : undefined;
          if (rNode?.kind === "dir" && rNode.collapsed) {
            e.preventDefault();
            toggleDir(rNode.path);
          }
          break;
        }
        case "ArrowLeft": {
          const lNode = viewMode === "tree" ? treeNodes[clampedFocusedIndex] : undefined;
          if (lNode?.kind === "dir" && !lNode.collapsed) {
            e.preventDefault();
            toggleDir(lNode.path);
          }
          break;
        }
        case "Enter":
        case " ": {
          e.preventDefault();
          if (clampedFocusedIndex < 0) break;
          if (viewMode === "tree") {
            const eNode = treeNodes[clampedFocusedIndex];
            if (eNode?.kind === "dir") {
              toggleDir(eNode.path);
            } else if (eNode?.kind === "file") {
              onSelectFile(eNode.file.path);
            }
          } else {
            const eFile = files[clampedFocusedIndex];
            if (eFile) onSelectFile(eFile.path);
          }
          break;
        }
      }
    },
    [itemCount, clampedFocusedIndex, viewMode, treeNodes, files, toggleDir, onSelectFile],
  );

  const [pathMenu, setPathMenu] = useState<PathMenuState | null>(null);
  const handleContextMenu = useCallback((e: React.MouseEvent) => {
    const el = (e.target as HTMLElement).closest<HTMLElement>("[data-path]");
    const path = el?.getAttribute("data-path");
    if (!path) return; // not on a file/folder row: let the native menu show
    e.preventDefault();
    setPathMenu({ x: e.clientX, y: e.clientY, path });
  }, []);
  const closePathMenu = useCallback(() => setPathMenu(null), []);

  return (
    <div className="flex flex-col h-full bg-surface-900 overflow-hidden" onContextMenu={handleContextMenu}>
      <CopyPathContextMenu menu={pathMenu} onClose={closePathMenu} />
      {/* Header */}
      <div className="px-3 py-2 border-b border-surface-700/20 shrink-0">
        <div className="flex items-center gap-2 flex-wrap">
          <span className="font-mono text-[11px] uppercase tracking-wider text-text-dim">Changes</span>
          {isMultiRepo ? (
            <span className="font-mono text-[10px] px-1.5 py-px rounded bg-surface-800 text-text-muted">
              {perRepoBases.length} repos
            </span>
          ) : sessionId && repoPath ? (
            <BasePicker
              sessionId={sessionId}
              repoPath={repoPath}
              currentBase={singleBaseBranch}
              hasOverride={Boolean(baseBranchOverride)}
              onChanged={onBaseBranchChanged}
            />
          ) : (
            <span className="font-mono text-[10px] px-1.5 py-px rounded bg-surface-800 text-text-muted">
              vs {singleBaseBranch}
            </span>
          )}
          {files.length > 0 && (
            <>
              <span className="font-mono text-[11px] text-text-muted">
                {files.length} file{files.length !== 1 ? "s" : ""}
              </span>
              <span className="font-mono text-[11px]">
                <span className="text-status-running">+{totalAdditions}</span>{" "}
                <span className="text-status-error">-{totalDeletions}</span>
              </span>
            </>
          )}
          {/* View mode toggle. In multi-repo mode the tree/flat choice
              applies inside each per-repo group; the repo-header row
              itself is always the top-level container regardless. */}
          {files.length > 0 && (
            <button
              onClick={toggleViewMode}
              className="ml-auto shrink-0 p-1 rounded text-text-dim hover:text-text-muted hover:bg-surface-800/50 transition-colors cursor-pointer"
              title={viewMode === "flat" ? "Switch to tree view" : "Switch to flat list"}
            >
              {viewMode === "flat" ? (
                // Tree icon
                <svg className="w-3.5 h-3.5" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5">
                  <path d="M2 3h12M5 7h9M5 11h9M2 7l1.5 1L2 9M2 11l1.5 1L2 13" />
                </svg>
              ) : (
                // List icon
                <svg className="w-3.5 h-3.5" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5">
                  <path d="M2 3h12M2 7h12M2 11h12" />
                </svg>
              )}
            </button>
          )}
        </div>
        {warning && <p className="text-[11px] text-status-waiting mt-1">{warning}</p>}
      </div>

      {/* File list */}
      <div ref={listRef} className="flex-1 overflow-y-auto" tabIndex={0} onKeyDown={handleKeyDown}>
        {loading && files.length === 0 ? (
          <div className="flex items-center justify-center h-full text-text-dim">
            <span className="text-xs">Loading files...</span>
          </div>
        ) : files.length === 0 ? (
          <div className="flex items-center justify-center h-full text-text-dim">
            <div className="text-center px-4">
              <div className="font-mono text-xl text-surface-700 mb-1">0</div>
              <p className="text-xs">No changes yet</p>
            </div>
          </div>
        ) : isMultiRepo ? (
          <MultiRepoGroups
            perRepoBases={perRepoBases}
            filesByRepo={filesByRepo}
            selectedPath={selectedPath}
            selectedRepoName={selectedRepoName}
            collapsedRepos={collapsedRepos}
            onToggleRepo={toggleRepo}
            onSelectFile={onSelectFile}
            viewMode={viewMode}
            collapsedDirs={collapsedDirs}
            onToggleDir={toggleDir}
          />
        ) : viewMode === "tree" ? (
          <TreeView
            nodes={treeNodes}
            selectedPath={selectedPath}
            selectedRepoName={selectedRepoName}
            onSelectFile={onSelectFile}
            onToggleDir={toggleDir}
            focusedIndex={clampedFocusedIndex}
            onFocusIndex={setFocusedIndex}
          />
        ) : (
          <FlatList
            files={files}
            selectedPath={selectedPath}
            selectedRepoName={selectedRepoName}
            onSelectFile={onSelectFile}
            focusedIndex={clampedFocusedIndex}
            onFocusIndex={setFocusedIndex}
          />
        )}
      </div>
    </div>
  );
}

// Sentinel separating the repo namespace from the dir path inside the
// shared `collapsedDirs` set. NUL is impossible in a real file path
// and avoids collisions with directory names that contain `::`.
const REPO_NS_SEP = "\u0000";

function MultiRepoGroups({
  perRepoBases,
  filesByRepo,
  selectedPath,
  selectedRepoName,
  collapsedRepos,
  onToggleRepo,
  onSelectFile,
  viewMode,
  collapsedDirs,
  onToggleDir,
}: {
  perRepoBases: RepoBase[];
  filesByRepo: Map<string, RichDiffFile[]>;
  selectedPath: string | null;
  selectedRepoName: string | undefined;
  collapsedRepos: Set<string>;
  onToggleRepo: (name: string) => void;
  onSelectFile: (path: string, repoName?: string) => void;
  viewMode: "flat" | "tree";
  collapsedDirs: Set<string>;
  onToggleDir: (dirPath: string) => void;
}) {
  return (
    <>
      {perRepoBases.map((repo) => {
        const name = repo.repo_name ?? "";
        const repoFiles = filesByRepo.get(name) ?? [];
        const collapsed = collapsedRepos.has(name);
        const adds = repoFiles.reduce((s, f) => s + f.additions, 0);
        const dels = repoFiles.reduce((s, f) => s + f.deletions, 0);
        return (
          <div key={name || "_default"} className="border-b border-surface-700/20 last:border-b-0">
            <button
              type="button"
              onClick={() => onToggleRepo(name)}
              aria-expanded={!collapsed}
              className="w-full text-left px-3 py-1.5 cursor-pointer transition-colors flex items-center gap-1.5 bg-surface-850 hover:bg-surface-800 text-text-secondary"
            >
              <svg
                className={`w-3 h-3 shrink-0 text-text-dim transition-transform duration-75 ${
                  collapsed ? "-rotate-90" : ""
                }`}
                viewBox="0 0 16 16"
                fill="currentColor"
              >
                <path d="M4 6l4 4 4-4" />
              </svg>
              <span className="font-mono text-[12px] truncate flex-1">{repo.repo_name ?? "(default)"}</span>
              <span className="font-mono text-[10px] text-text-dim">vs {repo.base_branch}</span>
              <span className="font-mono text-[11px] text-text-muted">{repoFiles.length}</span>
              <span className="font-mono text-[11px] flex items-center gap-1">
                {adds > 0 && <span className="text-status-running">+{adds}</span>}
                {dels > 0 && <span className="text-status-error">-{dels}</span>}
              </span>
            </button>
            {!collapsed && repoFiles.length === 0 && (
              <div className="px-3 py-2 text-[11px] text-text-dim italic">No changes in this repo.</div>
            )}
            {!collapsed && repoFiles.length > 0 && (
              <RepoBody
                repoName={name}
                files={repoFiles}
                viewMode={viewMode}
                selectedPath={selectedPath}
                selectedRepoName={selectedRepoName}
                collapsedDirs={collapsedDirs}
                onToggleDir={onToggleDir}
                onSelectFile={onSelectFile}
              />
            )}
          </div>
        );
      })}
    </>
  );
}

function RepoBody({
  repoName,
  files,
  viewMode,
  selectedPath,
  selectedRepoName,
  collapsedDirs,
  onToggleDir,
  onSelectFile,
}: {
  repoName: string;
  files: RichDiffFile[];
  viewMode: "flat" | "tree";
  selectedPath: string | null;
  selectedRepoName: string | undefined;
  collapsedDirs: Set<string>;
  onToggleDir: (dirPath: string) => void;
  onSelectFile: (path: string, repoName?: string) => void;
}) {
  const ns = `${repoName}${REPO_NS_SEP}`;
  // Per-repo collapsed set: strip the namespace prefix so the tree
  // builder sees bare dir paths. The toggle path round-trips through
  // the shared set so persistence keeps working unchanged.
  const localCollapsed = useMemo(() => {
    const out = new Set<string>();
    for (const k of collapsedDirs) {
      if (k.startsWith(ns)) out.add(k.slice(ns.length));
    }
    return out;
  }, [collapsedDirs, ns]);
  const localToggle = useCallback((p: string) => onToggleDir(`${ns}${p}`), [onToggleDir, ns]);
  const treeNodes = useMemo(
    () => (viewMode === "tree" ? buildDiffTree(files, localCollapsed) : []),
    [viewMode, files, localCollapsed],
  );

  if (viewMode === "tree") {
    return (
      <TreeView
        nodes={treeNodes}
        selectedPath={selectedPath}
        selectedRepoName={selectedRepoName}
        onSelectFile={onSelectFile}
        onToggleDir={localToggle}
        focusedIndex={-1}
        onFocusIndex={() => {}}
        indentOffset={12}
        repoNameForSelect={repoName || undefined}
      />
    );
  }
  return (
    <>
      {files.map((file) => {
        const parts = file.path.split("/");
        const fileName = parts.pop() || file.path;
        const dirPath = parts.length > 0 ? parts.join("/") + "/" : "";
        const isSelected = file.path === selectedPath && file.repo_name === selectedRepoName;
        return (
          <button
            key={`${file.repo_name ?? ""}::${file.path}`}
            type="button"
            data-path={file.path}
            onClick={() => onSelectFile(file.path, file.repo_name)}
            className={`w-full text-left px-6 py-1.5 cursor-pointer transition-colors flex items-center gap-2 ${
              isSelected ? "bg-surface-850 text-text-primary" : "text-text-secondary hover:bg-surface-800/50"
            }`}
          >
            <span
              className={`shrink-0 font-mono text-[12px] w-3 text-center ${STATUS_COLORS[file.status] ?? "text-text-muted"}`}
            >
              {STATUS_LETTERS[file.status] ?? "?"}
            </span>
            <span className="truncate min-w-0 flex-1">
              {dirPath && <span className="font-mono text-[11px] text-text-dim">{dirPath}</span>}
              <span className="font-mono text-[12px]">{fileName}</span>
            </span>
            <span className="shrink-0 font-mono text-[11px] flex items-center gap-1">
              {file.additions > 0 && <span className="text-status-running">+{file.additions}</span>}
              {file.deletions > 0 && <span className="text-status-error">-{file.deletions}</span>}
            </span>
          </button>
        );
      })}
    </>
  );
}

interface BasePickerProps {
  sessionId: string;
  repoPath: string;
  currentBase: string;
  hasOverride: boolean;
  onChanged?: () => void;
}

/// Clickable chip + typeahead popover for the per-session diff base.
/// Persists via `PATCH /api/sessions/{id}/diff-base`; the parent
/// triggers a diff refetch on success. See #970.
function BasePicker({ sessionId, repoPath, currentBase, hasOverride, onChanged }: BasePickerProps) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [branches, setBranches] = useState<BranchInfo[] | null>(null);
  const [busy, setBusy] = useState(false);
  const [highlightIdx, setHighlightIdx] = useState(0);
  const containerRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    let cancelled = false;
    fetchBranches(repoPath, true).then((rows) => {
      if (!cancelled) setBranches(rows ?? []);
    });
    return () => {
      cancelled = true;
    };
  }, [open, repoPath]);

  // Close the popover on outside click.
  useEffect(() => {
    if (!open) return;
    const onDocPointer = (e: PointerEvent) => {
      if (containerRef.current && !containerRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    document.addEventListener("pointerdown", onDocPointer);
    return () => document.removeEventListener("pointerdown", onDocPointer);
  }, [open]);

  const q = query.trim().toLowerCase();
  const suggestions = (branches ?? []).filter((b) => !q || b.name.toLowerCase().includes(q)).slice(0, 8);

  const apply = async (value: string | null) => {
    setBusy(true);
    const ok = await setSessionDiffBase(sessionId, value);
    setBusy(false);
    if (ok) {
      setOpen(false);
      setQuery("");
      onChanged?.();
    }
  };

  return (
    <div ref={containerRef} className="relative">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        aria-expanded={open}
        aria-label={`Change diff base (current: ${currentBase})`}
        title="Change diff base"
        className={`font-mono text-[10px] px-1.5 py-px rounded cursor-pointer transition-colors ${
          hasOverride
            ? "bg-brand-600/15 text-brand-500 hover:bg-brand-600/25"
            : "bg-surface-800 text-text-muted hover:bg-surface-700"
        }`}
      >
        vs {currentBase}
      </button>
      {open && (
        <div className="absolute left-0 top-full z-20 mt-1 w-64 bg-surface-900 border border-surface-700/60 rounded-lg shadow-lg p-2">
          <input
            type="text"
            autoFocus
            value={query}
            placeholder="Search branches..."
            onChange={(e) => {
              setQuery(e.target.value);
              setHighlightIdx(0);
            }}
            onKeyDown={(e) => {
              if (e.key === "ArrowDown") {
                e.preventDefault();
                setHighlightIdx((i) => Math.min(i + 1, suggestions.length - 1));
              } else if (e.key === "ArrowUp") {
                e.preventDefault();
                setHighlightIdx((i) => Math.max(i - 1, 0));
              } else if (e.key === "Enter") {
                e.preventDefault();
                const pick = suggestions[highlightIdx];
                if (pick) void apply(pick.name);
                else if (query.trim()) void apply(query.trim());
              } else if (e.key === "Escape") {
                setOpen(false);
              }
            }}
            disabled={busy}
            className="w-full bg-surface-950 border border-surface-700/60 rounded px-2 py-1.5 text-xs font-mono text-text-primary placeholder:text-text-dim focus:border-brand-600 focus:outline-none"
          />
          {hasOverride && (
            <button
              type="button"
              onClick={() => void apply(null)}
              disabled={busy}
              className="mt-1.5 w-full text-left px-2 py-1 rounded text-[11px] text-text-dim hover:bg-surface-800 hover:text-text-secondary cursor-pointer"
            >
              ↺ Reset to auto-detected
            </button>
          )}
          <ul role="listbox" aria-label="Branch suggestions" className="mt-1 max-h-56 overflow-y-auto">
            {suggestions.length === 0 && (
              <li className="px-2 py-1 text-[11px] text-text-dim italic">
                {branches === null ? "Loading branches..." : "No matches."}
              </li>
            )}
            {suggestions.map((b, i) => (
              <li
                key={`${b.name}-${b.remote_only ? "r" : "l"}`}
                role="option"
                aria-selected={i === highlightIdx}
                onMouseEnter={() => setHighlightIdx(i)}
                onMouseDown={(e) => {
                  e.preventDefault();
                  void apply(b.name);
                }}
                className={`flex items-center justify-between gap-2 px-2 py-1 text-xs font-mono cursor-pointer rounded ${
                  i === highlightIdx ? "bg-surface-800 text-text-primary" : "text-text-secondary"
                }`}
              >
                <span className="truncate">{b.name}</span>
                <span className="text-[10px] uppercase tracking-wider text-text-dim shrink-0">
                  {b.is_current ? "current" : b.remote_only ? "remote" : "local"}
                </span>
              </li>
            ))}
          </ul>
        </div>
      )}
    </div>
  );
}
