// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { DiffFileList } from "../DiffFileList";
import type { RepoBase, RichDiffFile } from "../../../lib/types";

const mock = vi.hoisted(() => ({
  fetchBranches: vi.fn(),
  setSessionDiffBase: vi.fn(),
  sessionDiffRawFileUrl: (id: string, path: string, repo?: string) => `${id}:${repo ?? ""}:${path}`,
}));
vi.mock("../../../lib/api", () => mock);
const openInNewTab = vi.hoisted(() => vi.fn());
vi.mock("../../../lib/openInNewTab", () => ({ openInNewTab }));

const file = (over: Partial<RichDiffFile> & { path: string }): RichDiffFile => ({
  old_path: null,
  status: "modified",
  additions: 1,
  deletions: 0,
  ...over,
});

function renderList(props: Partial<React.ComponentProps<typeof DiffFileList>> = {}) {
  const onSelectFile = vi.fn();
  const utils = render(
    <DiffFileList
      files={[]}
      perRepoBases={[{ base_branch: "main" }]}
      warning={null}
      selectedPath={null}
      selectedRepoName={undefined}
      loading={false}
      onSelectFile={onSelectFile}
      {...props}
    />,
  );
  const list = document.querySelector('[tabindex="0"]') as HTMLElement;
  const key = (k: string) => fireEvent.keyDown(list, { key: k });
  return { onSelectFile, key, ...utils };
}

const setSettings = (s: object) => window.localStorage.setItem("aoe-web-settings", JSON.stringify(s));
const settings = () => JSON.parse(window.localStorage.getItem("aoe-web-settings") ?? "{}");
const row = (text: string) => screen.getByText(text).closest("button")!;
const expanded = (text: string) => row(text).getAttribute("aria-expanded");

beforeEach(() => {
  window.localStorage.clear();
  mock.fetchBranches.mockReset();
  mock.setSessionDiffBase.mockReset().mockResolvedValue({});
  openInNewTab.mockReset().mockResolvedValue({ ok: true });
  Element.prototype.scrollIntoView = vi.fn();
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  window.localStorage.clear();
});

describe("header and empty states", () => {
  it("names the base when a single repo has no changes, with no view toggle", () => {
    const { key } = renderList();
    key("ArrowDown");
    expect(screen.getByText(/No changes vs/)).toBeTruthy();
    expect(screen.getByText("main")).toBeTruthy();
    expect(screen.queryByTitle("Switch to tree view")).toBeNull();
    expect(screen.queryByTitle("Switch to flat list")).toBeNull();
  });

  it("lists every repo with its base when a workspace has no changes", () => {
    renderList({
      perRepoBases: [
        { repo_name: "taskrunner", base_branch: "origin/develop" },
        { repo_name: "MessageManager", base_branch: "origin/develop" },
        { repo_name: "SmartCaller", base_branch: "origin/main" },
      ],
    });
    expect(screen.getAllByText("vs origin/develop")).toHaveLength(2);
    expect(screen.getByText("vs origin/main")).toBeTruthy();
    expect(screen.getAllByText("No changes in this repo.")).toHaveLength(3);
  });

  it("renders counts, totals, base chip and warning", () => {
    renderList({
      files: [file({ path: "a.ts", additions: 3, deletions: 1 }), file({ path: "b.ts", additions: 2, deletions: 4 })],
      warning: "diff truncated",
    });
    for (const t of ["2 files", "+5", "-5", "vs main", "diff truncated"]) {
      expect(screen.getAllByText(t).length).toBeGreaterThan(0);
    }
  });
});

describe("tree view", () => {
  const files = [file({ path: "src/app/foo.rs" }), file({ path: "src/app/bar.rs" }), file({ path: "top.rs" })];

  it("renders dirs and leaves, collapses with persistence, and selects leaves", () => {
    const { onSelectFile } = renderList({ files, selectedPath: "top.rs" });
    expect(expanded("src")).toBe("true");
    expect(row("top.rs").className).toContain("bg-surface-850");
    fireEvent.click(row("src"));
    expect(screen.queryByText("app")).toBeNull();
    expect(screen.queryByText("foo.rs")).toBeNull();
    expect(expanded("src")).toBe("false");
    expect(settings().collapsedDiffDirs).toContain("src");
    fireEvent.click(row("src"));
    expect(screen.getByText("foo.rs")).toBeTruthy();
    fireEvent.click(row("top.rs"));
    expect(onSelectFile).toHaveBeenCalledWith("top.rs", undefined);
  });

  it("navigates dirs with the keyboard", () => {
    setSettings({ collapsedDiffDirs: ["src"] });
    const { key } = renderList({ files: [file({ path: "src/foo.rs" }), file({ path: "src/bar.rs" })] });
    key("ArrowDown");
    key("ArrowRight");
    expect(expanded("src")).toBe("true");
    key("ArrowLeft");
    expect(expanded("src")).toBe("false");
    key("Enter");
    expect(expanded("src")).toBe("true");
  });
});

describe("flat view", () => {
  it("toggles to flat with persistence, shows dir prefixes and counts, and selects", () => {
    const { onSelectFile } = renderList({ files: [file({ path: "src/app/foo.rs", additions: 2, deletions: 3 })] });
    fireEvent.click(screen.getByTitle("Switch to flat list"));
    expect(settings().diffViewMode).toBe("flat");
    const r = row("foo.rs");
    expect(r.textContent).toContain("src/app/");
    expect(within(r).getByText("+2")).toBeTruthy();
    expect(within(r).getByText("-3")).toBeTruthy();
    fireEvent.click(r);
    expect(onSelectFile).toHaveBeenCalledWith("src/app/foo.rs", undefined);
  });
});

describe("copy relative path", () => {
  it("copies from a file row", () => {
    Object.defineProperty(window, "isSecureContext", { value: true, configurable: true });
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", { value: { writeText }, configurable: true });
    renderList({ files: [file({ path: "src/app/foo.rs" })] });
    fireEvent.contextMenu(row("foo.rs"));
    fireEvent.click(screen.getByText("Copy relative path"));
    expect(writeText).toHaveBeenCalledWith("src/app/foo.rs");
  });
});

describe("open file", () => {
  const menuItems = () => screen.getAllByRole("menuitem").map((b) => b.textContent);

  it("opens a shared path from the repo whose row was right-clicked", () => {
    setSettings({ diffViewMode: "flat" });
    renderList({
      files: [file({ path: "same.txt", repo_name: "api" }), file({ path: "same.txt", repo_name: "web" })],
      perRepoBases: [
        { repo_name: "api", base_branch: "main" },
        { repo_name: "web", base_branch: "main" },
      ],
      sessionId: "s1",
    });
    fireEvent.contextMenu(screen.getAllByText("same.txt")[1]!);
    expect(menuItems()).toEqual(["Open file", "Copy relative path"]);
    fireEvent.click(screen.getByText("Open file"));
    expect(openInNewTab).toHaveBeenCalledWith("s1:web:same.txt", "same.txt");
  });

  it("offers only the copy on a directory row", () => {
    renderList({ files: [file({ path: "src/app/foo.rs" })], sessionId: "s1" });
    fireEvent.contextMenu(row("src"));
    expect(menuItems()).toEqual(["Copy relative path"]);
  });
});

describe("multi-repo groups", () => {
  const perRepoBases: RepoBase[] = [
    { repo_name: "api", base_branch: "main", repo_path: "/ws/api" },
    { repo_name: "web", base_branch: "develop", repo_path: "/ws/web" },
    { repo_name: "empty", base_branch: "trunk", repo_path: "/ws/empty" },
  ];
  const files = [
    file({ path: "src/handler.rs", repo_name: "api", additions: 5, deletions: 1 }),
    file({ path: "index.ts", repo_name: "web" }),
  ];

  it("renders repo headers, empty notes, collapse, and tree selection with the repo name", () => {
    const { onSelectFile } = renderList({ files, perRepoBases });
    expect(screen.getByText("3 repos")).toBeTruthy();
    expect(screen.getByText("vs develop")).toBeTruthy();
    expect(screen.getByText("No changes in this repo.")).toBeTruthy();
    fireEvent.click(row("handler.rs"));
    expect(onSelectFile).toHaveBeenCalledWith("src/handler.rs", "api");
    fireEvent.click(row("api"));
    expect(screen.queryByText("handler.rs")).toBeNull();
    expect(expanded("api")).toBe("false");
  });

  it("gives each repo a base picker scoped to its own worktree", async () => {
    mock.fetchBranches.mockResolvedValue([{ name: "epic/checkout", is_current: false }]);
    const onBaseBranchChanged = vi.fn();
    renderList({ files, perRepoBases, sessionId: "s1", onBaseBranchChanged });
    const pickers = screen.getAllByRole("button", { name: /Change diff base/ });
    expect(pickers).toHaveLength(3);
    fireEvent.click(pickers[1]!);
    await vi.waitFor(() => expect(mock.fetchBranches).toHaveBeenCalledWith("/ws/web", true));
    fireEvent.mouseDown(await screen.findByText("epic/checkout"));
    await vi.waitFor(() => expect(mock.setSessionDiffBase).toHaveBeenCalledWith("s1", "epic/checkout", "web"));
    expect(mock.setSessionDiffBase).toHaveBeenCalledTimes(1);
    await vi.waitFor(() => expect(onBaseBranchChanged).toHaveBeenCalled());
  });

  it("offers Reset only on the overridden repo", async () => {
    mock.fetchBranches.mockResolvedValue([]);
    const overridden = perRepoBases.map((r) =>
      r.repo_name === "web" ? { ...r, base_override: "epic/checkout", base_branch: "epic/checkout" } : r,
    );
    renderList({ files, perRepoBases: overridden, sessionId: "s1" });
    fireEvent.click(screen.getAllByRole("button", { name: /Change diff base/ })[0]!);
    expect(screen.queryByText(/Reset to auto-detected/)).toBeNull();
    fireEvent.click(screen.getAllByRole("button", { name: /Change diff base/ })[1]!);
    fireEvent.click(await screen.findByText(/Reset to auto-detected/));
    await vi.waitFor(() => expect(mock.setSessionDiffBase).toHaveBeenCalledWith("s1", null, "web"));
  });
});

describe("base picker", () => {
  const openPicker = async (branches: object[]) => {
    mock.fetchBranches.mockResolvedValue(branches);
    renderList({ files: [file({ path: "a.ts" })], sessionId: "s1", repoPath: "/repo" });
    fireEvent.click(screen.getByRole("button", { name: /Change diff base/ }));
    const input = await screen.findByPlaceholderText("Search branches...");
    return { input };
  };
  const applied = (value: string | null) =>
    vi.waitFor(() => expect(mock.setSessionDiffBase).toHaveBeenCalledWith("s1", value, undefined));

  it("loads branches and filters by query", async () => {
    const { input } = await openPicker([
      { name: "main", is_current: true },
      { name: "feature/x", is_current: false },
      { name: "release", is_current: false, remote_only: true },
    ]);
    expect(await screen.findByText("release")).toBeTruthy();
    fireEvent.change(input, { target: { value: "feat" } });
    expect(screen.getByText("feature/x")).toBeTruthy();
    expect(screen.queryByText("release")).toBeNull();
  });

  it("picks with arrows and Enter, or falls back to the typed query", async () => {
    const { input } = await openPicker([
      { name: "one", is_current: false },
      { name: "two", is_current: false },
    ]);
    await screen.findByText("two");
    fireEvent.keyDown(input, { key: "ArrowDown" });
    fireEvent.keyDown(input, { key: "Enter" });
    await applied("two");
    cleanup();
    const typed = await openPicker([]);
    fireEvent.change(typed.input, { target: { value: "typed-branch" } });
    fireEvent.keyDown(typed.input, { key: "Enter" });
    await applied("typed-branch");
  });
});
