import { describe, it, expect } from "vitest";
import { buildDiffTree } from "./diffTree";
import type { RichDiffFile } from "./types";

function makeFile(
  path: string,
  status: RichDiffFile["status"] = "modified",
  additions = 1,
  deletions = 0,
): RichDiffFile {
  return { path, old_path: null, status, additions, deletions };
}

describe("buildDiffTree", () => {
  it("returns empty array for no files", () => {
    expect(buildDiffTree([], new Set())).toEqual([]);
  });

  it("renders root-level files without directory nodes", () => {
    const files = [makeFile("README.md"), makeFile("Cargo.toml")];
    const nodes = buildDiffTree(files, new Set());
    expect(nodes).toHaveLength(2);
    expect(nodes.every((n) => n.kind === "file")).toBe(true);
    expect(nodes[0].kind === "file" && nodes[0].file.path).toBe("Cargo.toml");
    expect(nodes[1].kind === "file" && nodes[1].file.path).toBe("README.md");
  });

  it("groups files by directory with aggregated stats", () => {
    const files = [
      makeFile("src/main.rs", "modified", 10, 2),
      makeFile("src/lib.rs", "added", 5, 0),
    ];
    const nodes = buildDiffTree(files, new Set());
    // Should be: dir:src, file:src/lib.rs, file:src/main.rs
    expect(nodes).toHaveLength(3);
    expect(nodes[0].kind).toBe("dir");
    if (nodes[0].kind === "dir") {
      expect(nodes[0].name).toBe("src");
      expect(nodes[0].additions).toBe(15);
      expect(nodes[0].deletions).toBe(2);
      expect(nodes[0].fileCount).toBe(2);
    }
  });

  it("hides children of collapsed directories", () => {
    const files = [
      makeFile("src/main.rs"),
      makeFile("src/lib.rs"),
      makeFile("README.md"),
    ];
    const nodes = buildDiffTree(files, new Set(["src"]));
    // Should be: dir:src (collapsed), file:README.md
    expect(nodes).toHaveLength(2);
    expect(nodes[0].kind).toBe("dir");
    if (nodes[0].kind === "dir") {
      expect(nodes[0].collapsed).toBe(true);
    }
    expect(nodes[1].kind).toBe("file");
  });

  it("handles nested directories", () => {
    const files = [
      makeFile("src/cli/add.rs", "added", 20, 0),
      makeFile("src/cli/session.rs", "modified", 3, 1),
      makeFile("src/main.rs", "modified", 1, 0),
    ];
    const nodes = buildDiffTree(files, new Set());
    // src (dir) -> cli (dir) -> add.rs, session.rs + main.rs
    const dirNodes = nodes.filter((n) => n.kind === "dir");
    expect(dirNodes).toHaveLength(2);
    // src dir should aggregate all stats
    const srcDir = dirNodes.find((n) => n.kind === "dir" && n.name === "src");
    expect(srcDir).toBeDefined();
    if (srcDir?.kind === "dir") {
      expect(srcDir.additions).toBe(24);
      expect(srcDir.deletions).toBe(1);
      expect(srcDir.fileCount).toBe(3);
    }
  });

  it("collapsing a parent hides nested directories too", () => {
    const files = [makeFile("src/cli/add.rs"), makeFile("src/main.rs")];
    const nodes = buildDiffTree(files, new Set(["src"]));
    // Only the src dir should be visible
    expect(nodes).toHaveLength(1);
    expect(nodes[0].kind).toBe("dir");
  });

  it("sorts directories before files, both alphabetically", () => {
    const files = [
      makeFile("z_file.rs"),
      makeFile("a_dir/z.rs"),
      makeFile("b_dir/a.rs"),
      makeFile("a_file.rs"),
    ];
    const nodes = buildDiffTree(files, new Set());
    // Expected: a_dir, a_dir/z.rs, b_dir, b_dir/a.rs, a_file.rs, z_file.rs
    expect(
      nodes.map((n) => (n.kind === "dir" ? `dir:${n.name}` : n.file.path)),
    ).toEqual([
      "dir:a_dir",
      "a_dir/z.rs",
      "dir:b_dir",
      "b_dir/a.rs",
      "a_file.rs",
      "z_file.rs",
    ]);
  });

  it("assigns correct depth values", () => {
    const files = [makeFile("a/b/c.rs")];
    const nodes = buildDiffTree(files, new Set());
    // a (depth 0), b (depth 1), c.rs (depth 2)
    expect(nodes).toHaveLength(3);
    expect(nodes[0].kind === "dir" && nodes[0].depth).toBe(0);
    expect(nodes[1].kind === "dir" && nodes[1].depth).toBe(1);
    expect(nodes[2].kind === "file" && nodes[2].depth).toBe(2);
  });
});
