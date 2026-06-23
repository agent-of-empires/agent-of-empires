// @vitest-environment jsdom
//
// Vitest coverage for the ProjectStep "Import Claude" tab (#2276): the
// picker lists on-disk Claude Code sessions, disables ones whose cwd is
// gone, and selecting one prefills a structured-view claude session that
// resumes the chosen id (path = original cwd, worktree off,
// importAcpSessionId set). Live Playwright exercises the end-to-end
// resume + transcript render; this isolates the field-wiring so it does
// not depend on a real `aoe serve`.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, fireEvent, waitFor } from "@testing-library/react";

import { ProjectStep } from "../steps/ProjectStep";
import { initialData } from "../wizardReducer";
import type { AgentInfo, ClaudeSessionSummary } from "../../../lib/types";

vi.mock("../../../lib/api", () => ({
  fetchSessions: vi.fn().mockResolvedValue({ sessions: [], workspace_ordering: [] }),
  fetchRecentProjects: vi.fn(),
  fetchProjects: vi.fn().mockResolvedValue([]),
  cloneRepo: vi.fn(),
  getHomePath: vi.fn().mockResolvedValue(null),
  browseFilesystem: vi.fn().mockResolvedValue({ ok: false, entries: [] }),
  listClaudeSessions: vi.fn(),
}));

import { listClaudeSessions } from "../../../lib/api";

const SESSIONS: ClaudeSessionSummary[] = [
  {
    session_id: "713b7f46-d0f2-454e-91be-a3305d35660c",
    cwd: "/Users/me/projects/alpha",
    title: "Fix the spinner bug",
    last_modified_ms: 1_700_000_000_000,
    cwd_exists: true,
  },
  {
    session_id: "dead-beef",
    cwd: "/Users/me/gone",
    title: "Old work",
    last_modified_ms: 1_600_000_000_000,
    cwd_exists: false,
  },
];

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

const CLAUDE_INSTALLED: AgentInfo = {
  name: "claude",
  kind: "builtin",
  binary: "claude",
  host_only: false,
  installed: true,
  install_hint: "",
  acp_capable: true,
  acp_installed: true,
  acp_command: "claude-agent-acp",
};

function renderStep(importAcpSessionId = "", agents: AgentInfo[] = [CLAUDE_INSTALLED]) {
  const onChange = vi.fn();
  const utils = render(
    <ProjectStep
      data={{ ...initialData, path: "", extraRepoPaths: [], scratch: false, importAcpSessionId }}
      onChange={onChange}
      initialTab="import"
      agents={agents}
    />,
  );
  return { onChange, ...utils };
}

describe("ProjectStep Import from Claude tab (#2276)", () => {
  beforeEach(() => {
    vi.mocked(listClaudeSessions).mockResolvedValue(SESSIONS);
  });

  it("lists discovered sessions and hides missing-cwd ones until toggled", async () => {
    const { findByText, getByText, queryByText, getByLabelText } = renderStep();
    await findByText("Fix the spinner bug");
    expect(getByText("/Users/me/projects/alpha")).toBeTruthy();
    // Missing-cwd session hidden by default.
    expect(queryByText("Old work")).toBeNull();
    // Toggle reveals it, disabled.
    fireEvent.click(getByLabelText("Show sessions with missing directories"));
    const missingRow = (await findByText("Old work")).closest("button") as HTMLButtonElement;
    expect(missingRow.disabled).toBe(true);
  });

  it("selecting a session prefills a structured claude import", async () => {
    const { onChange, findByText } = renderStep();
    const row = (await findByText("Fix the spinner bug")).closest("button") as HTMLButtonElement;
    fireEvent.click(row);

    const calls = Object.fromEntries(onChange.mock.calls);
    expect(calls.importAcpSessionId).toBe("713b7f46-d0f2-454e-91be-a3305d35660c");
    expect(calls.path).toBe("/Users/me/projects/alpha");
    expect(calls.tool).toBe("claude");
    expect(calls.useStructuredView).toBe(true);
    expect(calls.useWorktree).toBe(false);
  });

  it("does not render the import picker when claude-agent-acp is not installed", async () => {
    const notInstalled: AgentInfo = { ...CLAUDE_INSTALLED, acp_installed: false };
    const { queryByLabelText } = renderStep("", [notInstalled]);
    // Let any pending effects settle, then confirm the picker never mounted.
    await Promise.resolve();
    expect(queryByLabelText("Filter Claude sessions")).toBeNull();
  });

  it("highlights the currently selected session", async () => {
    const { findByText } = renderStep("713b7f46-d0f2-454e-91be-a3305d35660c");
    const row = (await findByText("Fix the spinner bug")).closest("button") as HTMLButtonElement;
    expect(row.getAttribute("aria-pressed")).toBe("true");
  });

  it("filters by title", async () => {
    const { findByText, getByLabelText, queryByText } = renderStep();
    await findByText("Fix the spinner bug");
    fireEvent.change(getByLabelText("Filter Claude sessions"), { target: { value: "spinner" } });
    expect(queryByText("Fix the spinner bug")).toBeTruthy();
    fireEvent.change(getByLabelText("Filter Claude sessions"), { target: { value: "zzznomatch" } });
    await waitFor(() => expect(queryByText("Fix the spinner bug")).toBeNull());
  });
});
