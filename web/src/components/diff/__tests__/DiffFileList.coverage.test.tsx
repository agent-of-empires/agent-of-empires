// @vitest-environment jsdom
//
// Branch coverage for DiffFileList: loading / empty states, flat vs tree
// rendering, the view-mode toggle, directory expand/collapse (click +
// keyboard), per-file selection, status letters for every git status,
// keyboard navigation, multi-repo grouping (collapse, empty repo, tree body),
// and the per-session BasePicker popover.
//
// The branch picker hits `fetchBranches` / `setSessionDiffBase`; both are
// mocked. `useWebSettings` is left real (it reads/writes localStorage), so the
// view-mode and collapsed-dir persistence paths exercise the actual hook.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { DiffFileList } from "../DiffFileList";
import type { RepoBase, RichDiffFile } from "../../../lib/types";

const mock = vi.hoisted(() => ({
  fetchBranches: vi.fn(),
  setSessionDiffBase: vi.fn(),
}));

vi.mock("../../../lib/api", () => ({
  fetchBranches: mock.fetchBranches,
  setSessionDiffBase: mock.setSessionDiffBase,
}));

const file = (over: Partial<RichDiffFile> & { path: string }): RichDiffFile => ({
  old_path: null,
  status: "modified",
  additions: 1,
  deletions: 0,
  ...over,
});

const singleBase: RepoBase[] = [{ base_branch: "main" }];

function renderList(props: Partial<React.ComponentProps<typeof DiffFileList>> = {}) {
  const onSelectFile = vi.fn();
  const utils = render(
    <DiffFileList
      files={[]}
      perRepoBases={singleBase}
      warning={null}
      selectedPath={null}
      selectedRepoName={undefined}
      loading={false}
      onSelectFile={onSelectFile}
      {...props}
    />,
  );
  return { onSelectFile, ...utils };
}

beforeEach(() => {
  window.localStorage.clear();
  mock.fetchBranches.mockReset();
  mock.setSessionDiffBase.mockReset();
  // jsdom has no scrollIntoView; keyboard nav calls it.
  Element.prototype.scrollIntoView = vi.fn();
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  window.localStorage.clear();
});

describe("DiffFileList state branches", () => {
  it("shows the loading placeholder when loading with no files yet", () => {
    renderList({ loading: true });
    expect(screen.getByText("Loading files...")).toBeTruthy();
  });

  it("shows the empty state naming the base and hides file count / toggle when there are no files", () => {
    renderList();
    // Single-repo empty state names the base (#2152) instead of a base-less line.
    expect(screen.getByText(/No changes vs/)).toBeTruthy();
    expect(screen.getByText("main")).toBeTruthy();
    // No file-count chip, totals, or view toggle without files.
    expect(screen.queryByTitle("Switch to tree view")).toBeNull();
    expect(screen.queryByTitle("Switch to flat list")).toBeNull();
  });

  it("lists every repo with its base in the multi-repo empty state (#2152)", () => {
    // Multi-repo empty routes through MultiRepoGroups, which shows each
    // member's header (name + "vs <base>") and a per-repo "no changes" note.
    renderList({
      files: [],
      perRepoBases: [
        { repo_name: "taskrunner", base_branch: "origin/develop" },
        { repo_name: "MessageManager", base_branch: "origin/develop" },
        { repo_name: "SmartCaller", base_branch: "origin/main" },
      ],
    });
    expect(screen.getByText("taskrunner")).toBeTruthy();
    expect(screen.getByText("MessageManager")).toBeTruthy();
    expect(screen.getByText("SmartCaller")).toBeTruthy();
    expect(screen.getAllByText("vs origin/develop")).toHaveLength(2);
    expect(screen.getByText("vs origin/main")).toBeTruthy();
    expect(screen.getAllByText("No changes in this repo.")).toHaveLength(3);
  });

  it("renders the warning banner when provided", () => {
    renderList({ files: [file({ path: "a.ts" })], warning: "diff truncated" });
    expect(screen.getByText("diff truncated")).toBeTruthy();
  });

  it("renders the file count, pluralization, and aggregated +/- totals", () => {
    renderList({
      files: [file({ path: "a.ts", additions: 3, deletions: 1 }), file({ path: "b.ts", additions: 2, deletions: 4 })],
    });
    expect(screen.getByText("2 files")).toBeTruthy();
    expect(screen.getByText("+5")).toBeTruthy();
    expect(screen.getByText("-5")).toBeTruthy();
  });

  it("uses the singular file label for a single file", () => {
    renderList({ files: [file({ path: "a.ts" })] });
    expect(screen.getByText("1 file")).toBeTruthy();
  });

  it("shows the plain `vs <base>` chip without a session id", () => {
    renderList({ files: [file({ path: "a.ts" })] });
    expect(screen.getByText("vs main")).toBeTruthy();
  });
});

describe("DiffFileList tree view (default)", () => {
  const files = [file({ path: "src/app/foo.rs" }), file({ path: "src/app/bar.rs" }), file({ path: "top.rs" })];

  it("renders directory rows and file leaves", () => {
    renderList({ files });
    // Top-level dir aggregates its children.
    const dir = screen.getByText("src").closest("button")!;
    expect(dir.getAttribute("aria-expanded")).toBe("true");
    expect(screen.getByText("foo.rs")).toBeTruthy();
    expect(screen.getByText("bar.rs")).toBeTruthy();
    expect(screen.getByText("top.rs")).toBeTruthy();
  });

  it("collapses and expands a directory on click, hiding its files", () => {
    renderList({ files });
    const srcDir = screen.getByText("src").closest("button")!;
    fireEvent.click(srcDir);
    // Collapsing src hides app/ and the leaves below it.
    expect(screen.queryByText("app")).toBeNull();
    expect(screen.queryByText("foo.rs")).toBeNull();
    expect(screen.getByText("src").closest("button")!.getAttribute("aria-expanded")).toBe("false");
    // Persisted to settings.
    const persisted = JSON.parse(window.localStorage.getItem("aoe-web-settings") ?? "{}");
    expect(persisted.collapsedDiffDirs).toContain("src");

    fireEvent.click(screen.getByText("src").closest("button")!);
    expect(screen.getByText("foo.rs")).toBeTruthy();
  });

  it("selects a file leaf via click", () => {
    const { onSelectFile } = renderList({ files });
    fireEvent.click(screen.getByText("top.rs").closest("button")!);
    expect(onSelectFile).toHaveBeenCalledWith("top.rs", undefined);
  });

  it("marks the selected leaf row", () => {
    renderList({ files, selectedPath: "top.rs", selectedRepoName: undefined });
    const row = screen.getByText("top.rs").closest("button")!;
    expect(row.className).toContain("bg-surface-850");
  });
});

describe("DiffFileList view-mode toggle + flat list", () => {
  const files = [file({ path: "src/app/foo.rs", additions: 2, deletions: 3 })];

  it("toggles from tree to flat and shows the inline dir prefix", () => {
    renderList({ files });
    fireEvent.click(screen.getByTitle("Switch to flat list"));
    const row = screen.getByText("foo.rs").closest("button")!;
    expect(row.textContent).toContain("src/app/");
    // Persisted.
    expect(JSON.parse(window.localStorage.getItem("aoe-web-settings") ?? "{}").diffViewMode).toBe("flat");
  });

  it("selects from a flat row and renders +/- counts", () => {
    window.localStorage.setItem("aoe-web-settings", JSON.stringify({ diffViewMode: "flat" }));
    const { onSelectFile } = renderList({ files });
    const row = screen.getByText("foo.rs").closest("button")!;
    expect(within(row).getByText("+2")).toBeTruthy();
    expect(within(row).getByText("-3")).toBeTruthy();
    fireEvent.click(row);
    expect(onSelectFile).toHaveBeenCalledWith("src/app/foo.rs", undefined);
  });

  it("renders a status letter for every git status in flat mode", () => {
    window.localStorage.setItem("aoe-web-settings", JSON.stringify({ diffViewMode: "flat" }));
    renderList({
      files: [
        file({ path: "a.rs", status: "added" }),
        file({ path: "d.rs", status: "deleted" }),
        file({ path: "r.rs", status: "renamed" }),
        file({ path: "c.rs", status: "copied" }),
        file({ path: "u.rs", status: "untracked" }),
        file({ path: "x.rs", status: "conflicted" }),
        file({ path: "m.rs", status: "modified" }),
      ],
    });
    for (const letter of ["A", "D", "R", "C", "?", "U", "M"]) {
      expect(screen.getAllByText(letter).length).toBeGreaterThan(0);
    }
  });
});

describe("DiffFileList keyboard navigation", () => {
  const files = [file({ path: "src/foo.rs" }), file({ path: "src/bar.rs" })];

  function list() {
    renderList({ files });
    // The scroll container holds the keydown handler.
    return document.querySelector('[tabindex="0"]') as HTMLElement;
  }

  it("ArrowDown then Enter on a directory toggles it", () => {
    const el = list();
    fireEvent.keyDown(el, { key: "ArrowDown" }); // focus dir at index 0
    fireEvent.keyDown(el, { key: "Enter" }); // collapse it
    expect(screen.getByText("src").closest("button")!.getAttribute("aria-expanded")).toBe("false");
  });

  it("ArrowRight expands a collapsed dir, ArrowLeft collapses it", () => {
    window.localStorage.setItem("aoe-web-settings", JSON.stringify({ collapsedDiffDirs: ["src"] }));
    const el = list();
    fireEvent.keyDown(el, { key: "ArrowDown" }); // focus the collapsed src dir
    fireEvent.keyDown(el, { key: "ArrowRight" }); // expand
    expect(screen.getByText("src").closest("button")!.getAttribute("aria-expanded")).toBe("true");
    fireEvent.keyDown(el, { key: "ArrowLeft" }); // collapse again
    expect(screen.getByText("src").closest("button")!.getAttribute("aria-expanded")).toBe("false");
  });

  it("ArrowUp clamps at the top and Enter selects a focused file", () => {
    window.localStorage.setItem("aoe-web-settings", JSON.stringify({ diffViewMode: "flat" }));
    const { onSelectFile } = renderList({ files });
    const el = document.querySelector('[tabindex="0"]') as HTMLElement;
    fireEvent.keyDown(el, { key: "ArrowDown" }); // index 0
    fireEvent.keyDown(el, { key: "ArrowUp" }); // clamp at 0
    fireEvent.keyDown(el, { key: "Enter" });
    expect(onSelectFile).toHaveBeenCalledTimes(1);
  });

  it("ignores keydown when there are no items", () => {
    renderList({ files: [] });
    const el = document.querySelector('[tabindex="0"]') as HTMLElement;
    // Should not throw.
    fireEvent.keyDown(el, { key: "ArrowDown" });
    expect(screen.getByText(/No changes vs/)).toBeTruthy();
  });
});

describe("DiffFileList multi-repo grouping", () => {
  const perRepoBases: RepoBase[] = [
    { repo_name: "api", base_branch: "main" },
    { repo_name: "web", base_branch: "develop" },
    { repo_name: "empty", base_branch: "trunk" },
  ];
  const files = [
    file({ path: "src/handler.rs", repo_name: "api", additions: 5, deletions: 1 }),
    file({ path: "index.ts", repo_name: "web" }),
  ];

  it("shows the repo count chip and per-repo headers with their bases", () => {
    renderList({ files, perRepoBases });
    expect(screen.getByText("3 repos")).toBeTruthy();
    expect(screen.getByText("api")).toBeTruthy();
    expect(screen.getByText("vs develop")).toBeTruthy();
  });

  it("renders an empty-repo note for a repo with no files", () => {
    renderList({ files, perRepoBases });
    expect(screen.getByText("No changes in this repo.")).toBeTruthy();
  });

  it("collapses a repo group, hiding its files", () => {
    renderList({ files, perRepoBases });
    expect(screen.getByText("handler.rs")).toBeTruthy();
    fireEvent.click(screen.getByText("api").closest("button")!);
    expect(screen.queryByText("handler.rs")).toBeNull();
    expect(screen.getByText("api").closest("button")!.getAttribute("aria-expanded")).toBe("false");
  });

  it("selects a file in flat multi-repo mode passing the repo name", () => {
    window.localStorage.setItem("aoe-web-settings", JSON.stringify({ diffViewMode: "flat" }));
    const { onSelectFile } = renderList({ files, perRepoBases });
    fireEvent.click(screen.getByText("index.ts").closest("button")!);
    expect(onSelectFile).toHaveBeenCalledWith("index.ts", "web");
  });

  it("renders the per-repo tree body and selects with the repo name", () => {
    // Default (tree) view exercises RepoBody's TreeView branch.
    const { onSelectFile } = renderList({ files, perRepoBases });
    fireEvent.click(screen.getByText("handler.rs").closest("button")!);
    expect(onSelectFile).toHaveBeenCalledWith("src/handler.rs", "api");
  });
});

describe("DiffFileList BasePicker", () => {
  const files = [file({ path: "a.ts" })];
  const baseProps = { files, sessionId: "s1", repoPath: "/repo", baseBranchOverride: null };

  it("opens the popover, loads branches, and filters by query", async () => {
    mock.fetchBranches.mockResolvedValue([
      { name: "main", is_current: true },
      { name: "feature/x", is_current: false },
      { name: "release", is_current: false, remote_only: true },
    ]);
    renderList(baseProps);

    fireEvent.click(screen.getByRole("button", { name: /Change diff base/ }));
    // Branches load asynchronously.
    expect(await screen.findByText("feature/x")).toBeTruthy();
    expect(screen.getByText("release")).toBeTruthy();

    const input = screen.getByPlaceholderText("Search branches...");
    fireEvent.change(input, { target: { value: "feat" } });
    expect(screen.getByText("feature/x")).toBeTruthy();
    expect(screen.queryByText("release")).toBeNull();
  });

  it("applies a branch on mousedown and calls onChanged", async () => {
    mock.fetchBranches.mockResolvedValue([{ name: "develop", is_current: false }]);
    mock.setSessionDiffBase.mockResolvedValue({});
    const onBaseBranchChanged = vi.fn();
    renderList({ ...baseProps, onBaseBranchChanged });

    fireEvent.click(screen.getByRole("button", { name: /Change diff base/ }));
    const option = await screen.findByText("develop");
    fireEvent.mouseDown(option);

    await vi.waitFor(() => expect(mock.setSessionDiffBase).toHaveBeenCalledWith("s1", "develop"));
    await vi.waitFor(() => expect(onBaseBranchChanged).toHaveBeenCalled());
  });

  it("shows a Reset affordance when an override is active and clears it", async () => {
    mock.fetchBranches.mockResolvedValue([]);
    mock.setSessionDiffBase.mockResolvedValue({});
    const onBaseBranchChanged = vi.fn();
    renderList({ ...baseProps, baseBranchOverride: "custom-base", onBaseBranchChanged });

    // The chip is styled as an override.
    fireEvent.click(screen.getByRole("button", { name: /Change diff base/ }));
    const reset = await screen.findByText(/Reset to auto-detected/);
    fireEvent.click(reset);
    await vi.waitFor(() => expect(mock.setSessionDiffBase).toHaveBeenCalledWith("s1", null));
  });

  it("picks the highlighted branch via keyboard Enter and navigates with arrows", async () => {
    mock.fetchBranches.mockResolvedValue([
      { name: "one", is_current: false },
      { name: "two", is_current: false },
    ]);
    mock.setSessionDiffBase.mockResolvedValue({});
    renderList(baseProps);

    fireEvent.click(screen.getByRole("button", { name: /Change diff base/ }));
    const input = await screen.findByPlaceholderText("Search branches...");
    fireEvent.keyDown(input, { key: "ArrowDown" }); // highlight index 1 ("two")
    fireEvent.keyDown(input, { key: "Enter" });
    await vi.waitFor(() => expect(mock.setSessionDiffBase).toHaveBeenCalledWith("s1", "two"));
  });

  it("falls back to the typed query when Enter is pressed with no suggestion", async () => {
    mock.fetchBranches.mockResolvedValue([]);
    mock.setSessionDiffBase.mockResolvedValue({});
    renderList(baseProps);

    fireEvent.click(screen.getByRole("button", { name: /Change diff base/ }));
    const input = await screen.findByPlaceholderText("Search branches...");
    fireEvent.change(input, { target: { value: "typed-branch" } });
    fireEvent.keyDown(input, { key: "Enter" });
    await vi.waitFor(() => expect(mock.setSessionDiffBase).toHaveBeenCalledWith("s1", "typed-branch"));
  });

  it("closes the popover on Escape", async () => {
    mock.fetchBranches.mockResolvedValue([{ name: "main", is_current: true }]);
    renderList(baseProps);

    fireEvent.click(screen.getByRole("button", { name: /Change diff base/ }));
    const input = await screen.findByPlaceholderText("Search branches...");
    fireEvent.keyDown(input, { key: "Escape" });
    expect(screen.queryByPlaceholderText("Search branches...")).toBeNull();
  });
});
