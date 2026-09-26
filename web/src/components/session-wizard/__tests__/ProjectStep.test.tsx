// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { ProjectStep } from "../steps/ProjectStep";
import { initialData, type WizardData } from "../wizardReducer";
import type { AgentInfo, ClaudeSessionSummary, ProjectInfo } from "../../../lib/types";
import type { RecentProjectEntry } from "../../../lib/api";
import { agent, mockSession } from "./fixtures";

vi.mock("../../../lib/api", () => ({
  fetchSessions: vi.fn(),
  fetchRecentProjects: vi.fn(),
  fetchProjects: vi.fn(),
  cloneRepo: vi.fn(),
  getHomePath: vi.fn(),
  browseFilesystem: vi.fn(),
  listClaudeSessions: vi.fn(),
}));

import {
  browseFilesystem,
  fetchProjects,
  fetchRecentProjects,
  fetchSessions,
  getHomePath,
  listClaudeSessions,
} from "../../../lib/api";

beforeEach(() => {
  vi.mocked(fetchSessions).mockResolvedValue({ sessions: [], workspace_ordering: [] });
  vi.mocked(fetchRecentProjects).mockResolvedValue({ projects: [] });
  vi.mocked(fetchProjects).mockResolvedValue([]);
  vi.mocked(getHomePath).mockResolvedValue(null);
  vi.mocked(browseFilesystem).mockResolvedValue({ ok: false, entries: [] } as never);
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

function renderStep(data: Partial<WizardData> = {}, props: { initialTab?: "import"; agents?: AgentInfo[] } = {}) {
  const onChange = vi.fn();
  render(<ProjectStep data={{ ...initialData, ...data }} onChange={onChange} {...props} />);
  return { onChange };
}

const sessions = (...list: ReturnType<typeof mockSession>[]) =>
  vi.mocked(fetchSessions).mockResolvedValue({ sessions: list, workspace_ordering: [] });
const savedProjects = (...paths: string[]) =>
  vi
    .mocked(fetchProjects)
    .mockResolvedValue(
      paths.map((path) => ({ name: path.split("/").pop()!, path, scope: "global", pinned: false }) as ProjectInfo),
    );
const recentEntries = (entries: Partial<RecentProjectEntry>[]) =>
  vi.mocked(fetchRecentProjects).mockResolvedValue({
    projects: entries.map((e) => ({
      path: "/repo/frontend",
      display_name: "frontend",
      tool: "claude",
      last_used_at: "2025-09-09T00:00:00+00:00",
      ...e,
    })),
  });
// The Recent tab button also reads "Recent"; section headers are <p>.
const header = (text: string) => screen.queryByText(text, { selector: "p" });

describe("scratch toggle", () => {
  it("toggles scratch once per click on the switch", () => {
    const { onChange } = renderStep();
    const toggle = screen.getByRole("switch", { name: "Skip project folder" });
    expect(toggle.getAttribute("aria-checked")).toBe("false");
    fireEvent.click(toggle);
    expect(onChange).toHaveBeenCalledTimes(1);
    expect(onChange).toHaveBeenCalledWith("scratch", true);
  });
});

describe("recent and saved projects", () => {
  it("shows a persisted project with no live session, and a live one only once", async () => {
    sessions(mockSession({ project_path: "/repo/live", last_accessed_at: "2025-09-10T00:00:00Z" }));
    recentEntries([{}, { path: "/repo/live", display_name: "live", last_used_at: "2025-01-01T00:00:00+00:00" }]);
    renderStep();
    expect(await screen.findByText("frontend")).toBeTruthy();
    expect(screen.getByText("0 sessions")).toBeTruthy();
    expect(screen.getAllByText("live")).toHaveLength(1);
    expect(screen.getByText("1 session")).toBeTruthy();
  });

  it("renders recents when the recent-projects fetch fails", async () => {
    sessions(mockSession({ project_path: "/repo/alpha" }));
    vi.mocked(fetchRecentProjects).mockResolvedValue(null);
    renderStep();
    expect((await screen.findAllByText("alpha")).length).toBeGreaterThan(0);
  });

  it("renders Saved and Recent sections, deduping a shared path into Saved", async () => {
    savedProjects("/repo/alpha", "/repo/dup");
    sessions(mockSession({ id: "b", project_path: "/repo/beta" }), mockSession({ id: "d", project_path: "/repo/dup" }));
    renderStep();
    expect(await screen.findByText("Saved projects", { selector: "p" })).toBeTruthy();
    expect(header("Recent")).toBeTruthy();
    expect(screen.getByText("/repo/alpha")).toBeTruthy();
    expect(screen.getByText("/repo/beta")).toBeTruthy();
    expect(screen.getAllByText("/repo/dup")).toHaveLength(1);
  });

  it.each([
    ["saved", () => savedProjects("/repo/alpha"), "/repo/alpha"],
    ["recent", () => sessions(mockSession({ project_path: "/repo/beta" })), "/repo/beta"],
  ])("selects a %s project's path on click", async (_, seed, path) => {
    seed();
    const { onChange } = renderStep();
    fireEvent.click((await screen.findByText(path)).closest("button")!);
    expect(onChange).toHaveBeenCalledWith("path", path);
  });

  it("reports each selection's saved worktree override, undefined when there is none", async () => {
    vi.mocked(fetchProjects).mockResolvedValue([
      { name: "alpha", path: "/repo/alpha", scope: "global", pinned: false, overrides: { worktree_enabled: true } },
      { name: "beta", path: "/repo/beta", scope: "global", pinned: false },
    ] as ProjectInfo[]);
    const onSelectSavedProject = vi.fn();
    render(<ProjectStep data={initialData} onChange={vi.fn()} onSelectSavedProject={onSelectSavedProject} />);
    fireEvent.click((await screen.findByText("/repo/alpha")).closest("button")!);
    fireEvent.click((await screen.findByText("/repo/beta")).closest("button")!);
    expect(onSelectSavedProject.mock.calls).toEqual([[true], [undefined]]);
  });

  it("falls back to the Browse tab with nothing to pick", async () => {
    vi.mocked(fetchSessions).mockResolvedValue(null);
    renderStep();
    expect(await screen.findByRole("button", { name: "Browse", exact: true })).toBeTruthy();
    expect(header("Recent")).toBeNull();
  });
});

describe("project search", () => {
  const NAMES = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "zebra"];
  beforeEach(() => {
    recentEntries(
      NAMES.map((name, i) => ({
        path: `/repo/${name}`,
        display_name: name,
        last_used_at: `2025-09-${String(20 - i).padStart(2, "0")}T00:00:00+00:00`,
      })),
    );
  });

  const search = async (value: string) =>
    fireEvent.change(await screen.findByLabelText("Search projects"), { target: { value } });

  it("caps recents with an empty query and searches the full list by name or path", async () => {
    renderStep();
    expect(await screen.findByText("foxtrot")).toBeTruthy();
    expect(screen.queryByText("golf")).toBeNull();
    await search("zeb");
    expect(await screen.findByText("zebra")).toBeTruthy();
    expect(screen.queryByText("alpha")).toBeNull();
    await search("/repo/golf");
    expect(await screen.findByText("golf")).toBeTruthy();
    await search("");
    expect(await screen.findByText("alpha")).toBeTruthy();
    expect(screen.queryByText("zebra")).toBeNull();
  });

  it("filters saved projects too", async () => {
    savedProjects("/repo/saved-yankee");
    renderStep();
    expect(await screen.findByText("saved-yankee")).toBeTruthy();
    await search("yank");
    expect(screen.queryByText("alpha")).toBeNull();
    await search("alpha");
    expect(await screen.findByText("alpha")).toBeTruthy();
    expect(screen.queryByText("saved-yankee")).toBeNull();
  });
});

describe("Import from Claude tab", () => {
  const SESSIONS: ClaudeSessionSummary[] = [
    {
      session_id: "713b",
      cwd: "/Users/me/alpha",
      title: "Fix the spinner bug",
      last_modified_ms: 1_700_000_000_000,
      cwd_exists: true,
    },
    {
      session_id: "dead",
      cwd: "/Users/me/gone",
      title: "Old work",
      last_modified_ms: 1_600_000_000_000,
      cwd_exists: false,
    },
  ];
  const CLAUDE = agent("claude", { acp_installed: true, acp_command: "claude-agent-acp" });
  const renderImport = (importAcpSessionId = "", agents = [CLAUDE]) =>
    renderStep({ importAcpSessionId }, { initialTab: "import", agents });
  const row = async (title: string) => (await screen.findByText(title)).closest("button") as HTMLButtonElement;

  beforeEach(() => {
    vi.mocked(listClaudeSessions).mockResolvedValue(SESSIONS);
  });

  it("hides missing-cwd sessions until toggled, then shows them disabled", async () => {
    renderImport();
    await screen.findByText("Fix the spinner bug");
    expect(screen.getByText("/Users/me/alpha")).toBeTruthy();
    expect(screen.queryByText("Old work")).toBeNull();
    fireEvent.click(screen.getByLabelText("Show sessions with missing directories"));
    expect((await row("Old work")).disabled).toBe(true);
  });

  it("selecting a session prefills a structured claude import", async () => {
    const { onChange } = renderImport();
    fireEvent.click(await row("Fix the spinner bug"));
    expect(Object.fromEntries(onChange.mock.calls)).toMatchObject({
      importAcpSessionId: "713b",
      path: "/Users/me/alpha",
      tool: "claude",
      useStructuredView: true,
      useWorktree: false,
    });
  });

  it("highlights the selected session and filters by title", async () => {
    renderImport("713b");
    expect((await row("Fix the spinner bug")).getAttribute("aria-pressed")).toBe("true");
    fireEvent.change(screen.getByLabelText("Filter Claude sessions"), { target: { value: "zzznomatch" } });
    await waitFor(() => expect(screen.queryByText("Fix the spinner bug")).toBeNull());
  });

  it("is not offered without claude-agent-acp", async () => {
    renderImport("", [{ ...CLAUDE, acp_installed: false }]);
    await Promise.resolve();
    expect(screen.queryByLabelText("Filter Claude sessions")).toBeNull();
  });
});
