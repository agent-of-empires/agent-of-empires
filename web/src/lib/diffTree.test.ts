import { expect, it } from "vitest";
import { buildDiffTree, type DiffTreeNode } from "./diffTree";
import type { RichDiffFile } from "./types";

function makeFile(path: string, additions = 1, deletions = 0): RichDiffFile {
  return { path, old_path: null, status: "modified", additions, deletions };
}

const shape = (nodes: DiffTreeNode[]) =>
  nodes.map((n) =>
    n.kind === "dir"
      ? `${n.depth}:dir:${n.name} +${n.additions}-${n.deletions} n=${n.fileCount}${n.collapsed ? " collapsed" : ""}`
      : `${n.depth}:${n.file.path}`,
  );

it("sorts directories before files, aggregates stats, and assigns depths", () => {
  expect(buildDiffTree([], new Set())).toEqual([]);
  const files = [
    makeFile("z_file.rs"),
    makeFile("src/cli/add.rs", 20, 0),
    makeFile("src/cli/session.rs", 3, 1),
    makeFile("src/main.rs", 1, 0),
    makeFile("a_file.rs"),
  ];
  expect(shape(buildDiffTree(files, new Set()))).toEqual([
    "0:dir:src +24-1 n=3",
    "1:dir:cli +23-1 n=2",
    "2:src/cli/add.rs",
    "2:src/cli/session.rs",
    "1:src/main.rs",
    "0:a_file.rs",
    "0:z_file.rs",
  ]);
});

it("collapsing a directory hides its files and nested directories", () => {
  const files = [makeFile("src/cli/add.rs"), makeFile("src/main.rs"), makeFile("README.md")];
  expect(shape(buildDiffTree(files, new Set(["src"])))).toEqual(["0:dir:src +2-0 n=2 collapsed", "0:README.md"]);
});
