import type { RichDiffFile } from "./types";

export interface DiffTreeFile {
  kind: "file";
  file: RichDiffFile;
  depth: number;
}

export interface DiffTreeDir {
  kind: "dir";
  name: string;
  path: string;
  depth: number;
  additions: number;
  deletions: number;
  fileCount: number;
  collapsed: boolean;
}

export type DiffTreeNode = DiffTreeFile | DiffTreeDir;

interface InternalDir {
  name: string;
  path: string;
  children: Map<string, InternalDir>;
  files: RichDiffFile[];
  additions: number;
  deletions: number;
  fileCount: number;
}

function getOrCreateDir(root: InternalDir, segments: string[]): InternalDir {
  let current = root;
  for (let i = 0; i < segments.length; i++) {
    const seg = segments[i]!;
    let child = current.children.get(seg);
    if (!child) {
      child = {
        name: seg,
        path: segments.slice(0, i + 1).join("/"),
        children: new Map(),
        files: [],
        additions: 0,
        deletions: 0,
        fileCount: 0,
      };
      current.children.set(seg, child);
    }
    current = child;
  }
  return current;
}

function buildInternalTree(files: RichDiffFile[]): InternalDir {
  const root: InternalDir = {
    name: "",
    path: "",
    children: new Map(),
    files: [],
    additions: 0,
    deletions: 0,
    fileCount: 0,
  };

  for (const file of files) {
    const parts = file.path.split("/");
    parts.pop(); // remove filename
    if (parts.length > 0) {
      const dir = getOrCreateDir(root, parts);
      dir.files.push(file);
    } else {
      root.files.push(file);
    }
  }

  // Propagate aggregated stats
  function propagate(dir: InternalDir): {
    additions: number;
    deletions: number;
    fileCount: number;
  } {
    let additions = 0;
    let deletions = 0;
    let fileCount = dir.files.length;
    for (const file of dir.files) {
      additions += file.additions;
      deletions += file.deletions;
    }
    for (const child of dir.children.values()) {
      const stats = propagate(child);
      additions += stats.additions;
      deletions += stats.deletions;
      fileCount += stats.fileCount;
    }
    dir.additions = additions;
    dir.deletions = deletions;
    dir.fileCount = fileCount;
    return { additions, deletions, fileCount };
  }
  propagate(root);

  return root;
}

/** Flatten the tree into a list of visible nodes, respecting collapsed state. */
export function buildDiffTree(
  files: RichDiffFile[],
  collapsedDirs: Set<string>,
): DiffTreeNode[] {
  const root = buildInternalTree(files);
  const result: DiffTreeNode[] = [];

  function walk(dir: InternalDir, depth: number, isRoot: boolean) {
    // Sort: directories first (alphabetical), then files (alphabetical)
    const sortedDirs = [...dir.children.values()].sort((a, b) =>
      a.name.localeCompare(b.name),
    );
    const sortedFiles = [...dir.files].sort((a, b) => {
      const aName = a.path.split("/").pop()!;
      const bName = b.path.split("/").pop()!;
      return aName.localeCompare(bName);
    });

    if (!isRoot) {
      const collapsed = collapsedDirs.has(dir.path);
      result.push({
        kind: "dir",
        name: dir.name,
        path: dir.path,
        depth,
        additions: dir.additions,
        deletions: dir.deletions,
        fileCount: dir.fileCount,
        collapsed,
      });
      if (collapsed) return;
    }

    const childDepth = isRoot ? depth : depth + 1;
    for (const child of sortedDirs) {
      walk(child, childDepth, false);
    }
    for (const file of sortedFiles) {
      result.push({ kind: "file", file, depth: childDepth });
    }
  }

  walk(root, 0, true);
  return result;
}
