import type { CSSProperties, ReactNode } from "react";
import type { RichDiffFile } from "../../lib/types";
import type { DiffTreeNode } from "../../lib/diffTree";

const STATUS: Record<string, [letter: string, color: string]> = {
  added: ["A", "text-status-running"],
  modified: ["M", "text-status-waiting"],
  deleted: ["D", "text-status-error"],
  renamed: ["R", "text-accent-600"],
  copied: ["C", "text-accent-600"],
  untracked: ["?", "text-text-muted"],
  conflicted: ["U", "text-status-waiting"],
};

const FOCUS_RING = "outline outline-1 outline-brand-600/60 -outline-offset-1";

export function LineCounts({
  additions,
  deletions,
  className = "",
}: {
  additions: number;
  deletions: number;
  className?: string;
}) {
  return (
    <span className={`${className}font-mono text-[11px] flex items-center gap-1`}>
      {additions > 0 && <span className="text-status-running">+{additions}</span>}
      {deletions > 0 && <span className="text-status-error">-{deletions}</span>}
    </span>
  );
}

export function Chevron({ collapsed }: { collapsed: boolean }) {
  return (
    <svg
      className={`w-3 h-3 shrink-0 text-text-dim transition-transform duration-75 ${collapsed ? "-rotate-90" : ""}`}
      viewBox="0 0 16 16"
      fill="currentColor"
    >
      <path d="M4 6l4 4 4-4" />
    </svg>
  );
}

interface RowNav {
  index: number;
  focused: boolean;
  onFocus: () => void;
}

interface FileRowProps {
  file: RichDiffFile;
  selected: boolean;
  onClick: () => void;
  /** Flat rows show the directory prefix; tree rows show only the file name. */
  withDir: boolean;
  padding: { className?: string; style?: CSSProperties };
  /** Keyboard-navigable rows carry an index and focus ring. */
  nav?: RowNav;
}

export function FileRow({ file, selected, onClick, withDir, padding, nav }: FileRowProps) {
  const parts = file.path.split("/");
  const fileName = parts.pop() || file.path;
  const [letter, color] = STATUS[file.status] ?? ["?", "text-text-muted"];
  let name: ReactNode = <span className="font-mono text-[12px] truncate flex-1">{fileName}</span>;
  if (withDir) {
    name = (
      <span className="truncate min-w-0 flex-1">
        {parts.length > 0 && <span className="font-mono text-[11px] text-text-dim">{parts.join("/") + "/"}</span>}
        <span className="font-mono text-[12px]">{fileName}</span>
      </span>
    );
  }
  return (
    <button
      type={nav ? undefined : "button"}
      data-index={nav?.index}
      data-path={file.path}
      onClick={onClick}
      onMouseEnter={nav?.onFocus}
      className={`w-full text-left ${padding.className ?? ""} py-1.5 cursor-pointer transition-colors flex items-center gap-2 ${
        selected ? "bg-surface-850 text-text-primary" : "text-text-secondary hover:bg-surface-800/50"
      } ${nav?.focused ? FOCUS_RING : ""}`}
      style={padding.style}
    >
      <span className={`shrink-0 font-mono text-[12px] w-3 text-center ${color}`}>{letter}</span>
      {name}
      <LineCounts additions={file.additions} deletions={file.deletions} className="shrink-0 " />
    </button>
  );
}

interface ListProps {
  selectedPath: string | null;
  selectedRepoName: string | undefined;
  onSelectFile: (path: string, repoName?: string) => void;
  /** Omitted for rows that are not keyboard navigable. */
  focusedIndex?: number;
  onFocusIndex?: (i: number) => void;
}

const isSelected = (file: RichDiffFile, { selectedPath, selectedRepoName }: ListProps) =>
  file.path === selectedPath && file.repo_name === selectedRepoName;

const navFor = (i: number, props: ListProps): RowNav | undefined =>
  props.onFocusIndex && {
    index: i,
    focused: i === props.focusedIndex,
    onFocus: () => props.onFocusIndex!(i),
  };

export function FlatList({ files, indent = "px-3", ...props }: ListProps & { files: RichDiffFile[]; indent?: string }) {
  return files.map((file, i) => (
    <FileRow
      key={`${file.repo_name ?? ""}::${file.path}`}
      file={file}
      selected={isSelected(file, props)}
      onClick={() => props.onSelectFile(file.path, file.repo_name)}
      withDir
      padding={{ className: indent }}
      nav={navFor(i, props)}
    />
  ));
}

export function TreeView({
  nodes,
  onToggleDir,
  indentOffset = 0,
  repoNameForSelect,
  ...props
}: ListProps & {
  nodes: DiffTreeNode[];
  onToggleDir: (dirPath: string) => void;
  /** Extra left padding for rows nested under a repo header. */
  indentOffset?: number;
  /** Repo passed to `onSelectFile` in multi-repo mode. */
  repoNameForSelect?: string;
}) {
  return nodes.map((node, i) => {
    const style = { paddingLeft: `${node.depth * 16 + 12 + indentOffset}px`, paddingRight: 12 };
    const nav = navFor(i, props) ?? { index: i, focused: false, onFocus: () => {} };
    if (node.kind === "file") {
      const file = node.file;
      return (
        <FileRow
          key={`${file.repo_name ?? ""}::${file.path}`}
          file={file}
          selected={isSelected(file, props)}
          onClick={() => props.onSelectFile(file.path, repoNameForSelect ?? file.repo_name)}
          withDir={false}
          padding={{ style }}
          nav={nav}
        />
      );
    }
    return (
      <button
        key={`dir:${node.path}`}
        data-index={i}
        data-path={node.path}
        onClick={() => onToggleDir(node.path)}
        onMouseEnter={nav.onFocus}
        aria-expanded={!node.collapsed}
        className={`w-full text-left py-1.5 cursor-pointer transition-colors flex items-center gap-1.5 text-text-muted hover:bg-surface-800/50 ${nav.focused ? FOCUS_RING : ""}`}
        style={style}
      >
        <Chevron collapsed={node.collapsed} />
        <span className="font-mono text-[12px] truncate flex-1">{node.name}</span>
        <span className="shrink-0 font-mono text-[10px] text-text-dim">{node.fileCount}</span>
        <LineCounts additions={node.additions} deletions={node.deletions} className="shrink-0 " />
      </button>
    );
  });
}
