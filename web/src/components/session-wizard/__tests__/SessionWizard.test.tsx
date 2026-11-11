// @vitest-environment jsdom

import { StrictMode, useState } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, renderHook, screen, waitFor } from "@testing-library/react";

import { SessionWizard, type WizardPrefill } from "../SessionWizard";
import { initialData } from "../wizardReducer";
import {
  fetchProfileAgents,
  fetchCreateProgress,
  fetchDockerStatus,
  fetchGroups,
  fetchIsGitRepo,
  fetchProfiles,
  fetchProjects,
  fetchRecentProjects,
  fetchSettings,
  fetchVolumeIgnoresPreview,
  type SettingsResponse,
} from "../../../lib/api";
import { agent } from "./fixtures";
import { toastBus } from "../../../lib/toastBus";
import { useAgentDisplayNames } from "../../../lib/agentDisplayName";
import { startPendingCreates } from "../../../lib/pendingCreates";
import type { AgentInfo, DockerStatusResponse } from "../../../lib/types";

const createSession = vi.fn();
const reviewCreationTrust = vi.fn();
const FINGERPRINT = {
  project_path: "/tmp/proj",
  base_hooks_hash: "base",
  hooks_hash: "repo",
  mcp_hash: null,
};

vi.mock("../../../lib/api", () => ({
  fetchCreateProgress: vi.fn().mockResolvedValue(null),
  fetchCreateBootId: vi.fn().mockResolvedValue("boot-1"),
  fetchSettings: vi.fn().mockResolvedValue({}),
  fetchProfileAgents: vi.fn().mockResolvedValue([]),
  fetchAgents: vi.fn().mockResolvedValue([]),
  fetchIsGitRepo: vi.fn().mockResolvedValue(true),
  fetchBranches: vi.fn().mockResolvedValue([]),
  fetchGroups: vi.fn().mockResolvedValue([]),
  fetchDockerStatus: vi.fn().mockResolvedValue({ available: false, runtime: "docker", runtime_name: "Docker" }),
  fetchProfiles: vi.fn().mockResolvedValue([]),
  fetchVolumeIgnoresPreview: vi.fn().mockResolvedValue([]),
  markVolumeIgnoresGlobsAcknowledged: vi.fn().mockResolvedValue(undefined),
  fetchSessions: vi.fn().mockResolvedValue({ sessions: [] }),
  // One recent keeps ProjectStep on the Recent tab instead of the directory browser.
  fetchRecentProjects: vi.fn().mockResolvedValue({
    projects: [{ path: "/tmp/proj", display_name: "proj", tool: "claude", last_used_at: "2026-01-01T00:00:00Z" }],
  }),
  fetchProjects: vi.fn().mockResolvedValue([]),
  listImportableSessions: vi.fn().mockResolvedValue({
    ok: true,
    truncated: false,
    sessions: [
      {
        session_id: "native",
        cwd: "/tmp/proj",
        title: "Native title",
        updated_at: "2020-01-01T00:00:00Z",
        cwd_exists: true,
      },
    ],
  }),
  createSession: (...args: unknown[]) => createSession(...args),
  reviewCreationTrust: (...args: unknown[]) => reviewCreationTrust(...args),
}));

const INSTRUCTION_KEY = "aoe-new-session-last-instruction";

beforeEach(() => {
  vi.clearAllMocks();
  // clearAllMocks keeps queued answers, and a test can leave some unread.
  vi.mocked(fetchProfileAgents).mockReset().mockResolvedValue([]);
  localStorage.clear();
  createSession.mockResolvedValue({ ok: true, session: { id: "s1" } });
  reviewCreationTrust.mockResolvedValue({
    ok: true,
    review: {
      fingerprint: FINGERPRINT,
      merged_hooks: {
        on_create: ["bash scripts/setup-worktree.sh"],
        on_launch: ["npm start"],
      },
      repo_hooks: {},
      mcp_summaries: [],
      hooks_need_trust: true,
      mcp_need_trust: false,
    },
  });
  vi.mocked(fetchIsGitRepo).mockImplementation(async (path) => path !== "/tmp/plain");
  vi.mocked(fetchProjects).mockResolvedValue([]);
});

afterEach(() => {
  cleanup();
  localStorage.clear();
});

function renderWizard(prefill: WizardPrefill = { path: "/tmp/proj", tool: "claude" }, servedProfile = "default") {
  const onCreated = vi.fn();
  const onClose = vi.fn();
  const onCreatedInBackground = vi.fn();
  render(
    <SessionWizard
      servedProfile={servedProfile}
      onClose={onClose}
      onCreated={onCreated}
      onCreatedInBackground={onCreatedInBackground}
      prefill={prefill}
    />,
  );
  return { onCreated, onClose, onCreatedInBackground };
}

// Launch stays disabled until the profile defaults settle, as for a real click.
const launch = async () => {
  const button = screen.getByText(/Launch session/).closest("button") as HTMLButtonElement;
  await waitFor(() => expect(button.disabled).toBe(false));
  fireEvent.click(button);
};
const payload = (call = 0) => createSession.mock.calls[call]![0];

describe("SessionWizard native import late defaults", () => {
  it.each([true, false])(
    "keeps a native import on the host after the original registry settles (success %s)",
    async (success) => {
      let resolveProjects!: (projects: Awaited<ReturnType<typeof fetchProjects>>) => void;
      let resolveSettings!: (settings: Awaited<ReturnType<typeof fetchSettings>>) => void;
      vi.mocked(fetchProjects).mockImplementationOnce(
        () =>
          new Promise((resolve) => {
            resolveProjects = resolve;
          }),
      );
      vi.mocked(fetchSettings).mockImplementationOnce(
        () =>
          new Promise((resolve) => {
            resolveSettings = resolve;
          }),
      );
      vi.mocked(fetchProfileAgents).mockResolvedValueOnce([agent("claude", { acp_installed: true })]);
      renderWizard({ path: "/tmp/proj", worktreeEnabled: true, sandboxEnabled: true });
      fireEvent.click(screen.getByText("Project"));
      fireEvent.click(await screen.findByRole("button", { name: "Import session" }));
      fireEvent.click((await screen.findByText("Native title")).closest("button")!);
      await act(async () => {
        resolveProjects(
          success
            ? [
                {
                  name: "proj",
                  path: "/tmp/proj",
                  scope: "global",
                  pinned: false,
                  overrides: { worktree_enabled: true, sandbox_enabled: true },
                },
              ]
            : null,
        );
        resolveSettings({ worktree: { enabled: true }, sandbox: { enabled_by_default: true } });
      });
      await launch();
      await waitFor(() => expect(createSession).toHaveBeenCalledOnce());
      expect(payload()).toMatchObject({
        import_acp_session_id: "native",
        path: "/tmp/proj",
        sandbox: false,
        worktree_enabled: false,
      });
      expect(payload().worktree_branch).toBeUndefined();
      expect(payload().extra_repo_paths).toBeUndefined();
      expect(payload().base_branch).toBeUndefined();
    },
  );
});

describe("SessionWizard structured view payload", () => {
  it.each([
    [false, "structured"],
    [true, "terminal"],
  ])("opting out=%s sends view %s", async (optOut, view) => {
    renderWizard();
    if (optOut) fireEvent.click(screen.getByRole("radio", { name: "Terminal view" }));
    await launch();
    await waitFor(() => expect(createSession).toHaveBeenCalled());
    expect(payload()).toMatchObject({ tool: "claude", view });
  });

  it.each([
    ["auto", "structured"],
    ["terminal", "terminal"],
    ["structured", "structured"],
    ["off", "terminal"],
  ])("opens on and sends the configured default view %s (#3517)", async (setting, view) => {
    vi.mocked(fetchSettings).mockResolvedValueOnce({ acp: { structured_view: setting } } as never);
    renderWizard();
    await launch();
    await waitFor(() => expect(createSession).toHaveBeenCalled());
    expect(payload()).toMatchObject({ tool: "claude", view });
  });

  it("hides the View row when structured_view is off", async () => {
    vi.mocked(fetchSettings).mockResolvedValueOnce({ acp: { structured_view: "off" } } as never);
    renderWizard();
    await screen.findByRole("switch", { name: "Yolo mode" });
    await waitFor(() => expect(screen.queryByTestId("wizard-view-row")).toBeNull());
  });

  it("sends profile-resolved agent model and effort defaults", async () => {
    vi.mocked(fetchSettings).mockResolvedValueOnce({
      session: { default_tool: "opencode" },
      acp: { acp_defaults: { opencode: { model: "openai/gpt-5.5", effort: "high" } } },
      sandbox: {},
    } as never);
    renderWizard({ path: "/tmp/proj" });
    // The Agent row names opencode once the defaults have applied.
    await waitFor(() => expect(screen.getByTestId("wizard-agent-row").textContent).toContain("opencode"));
    await launch();
    await waitFor(() => expect(createSession).toHaveBeenCalled());
    expect(payload()).toMatchObject({
      tool: "opencode",
      view: "structured",
      agent_model: "openai/gpt-5.5",
      agent_effort: "high",
    });
  });

  it("drops the previous agent's model and effort after switching agent", async () => {
    vi.mocked(fetchSettings).mockResolvedValueOnce({
      session: { default_tool: "opencode" },
      acp: { acp_defaults: { opencode: { model: "openai/gpt-5.5", effort: "high" } } },
      sandbox: {},
    } as never);
    vi.mocked(fetchProfileAgents).mockResolvedValueOnce([
      agent("opencode", { acp_installed: true }),
      agent("claude", { acp_installed: true }),
    ]);
    renderWizard({ path: "/tmp/proj" });
    await waitFor(() => expect(screen.getByTestId("wizard-agent-row").textContent).toContain("opencode"));
    fireEvent.click(screen.getByTestId("wizard-agent-row"));
    fireEvent.click(await screen.findByRole("button", { name: /^claude/ }));
    fireEvent.click(screen.getByRole("button", { name: "Done" }));
    await launch();
    await waitFor(() => expect(createSession).toHaveBeenCalled());
    expect(payload().tool).toBe("claude");
    expect(payload().agent_model).toBeUndefined();
    expect(payload().agent_effort).toBeUndefined();
  });
});

describe("SessionWizard last instruction memory", () => {
  it("prefills the stored instruction into the create payload", async () => {
    localStorage.setItem(INSTRUCTION_KEY, "always be terse");
    const { onCreated } = renderWizard();
    await launch();
    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
    expect(payload()).toMatchObject({ custom_instruction: "always be terse" });
    await waitFor(() => expect(onCreated).toHaveBeenCalledWith({ id: "s1" }));
  });

  it.each(["review for security", ""])("stores the submitted instruction %j", async (text) => {
    localStorage.setItem(INSTRUCTION_KEY, "stale text");
    renderWizard();
    fireEvent.click(screen.getByTestId("wizard-agent-row"));
    fireEvent.change(screen.getByPlaceholderText("Custom instructions for this session..."), {
      target: { value: text },
    });
    fireEvent.click(screen.getByRole("button", { name: "Done" }));
    await launch();
    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
    expect(payload().custom_instruction).toBe(text || undefined);
    await waitFor(() => expect(localStorage.getItem(INSTRUCTION_KEY)).toBe(text));
  });
});

describe("SessionWizard hooks trust", () => {
  const REFUSAL = {
    ok: false,
    error: "Repository hooks require trust.",
    hooksNeedTrust: {
      onCreate: ["bash scripts/setup-worktree.sh"],
      onLaunch: ["npm start"],
      onDestroy: [],
      needsMcpTrust: false,
    },
  };
  const openDialog = async () => {
    await launch();
    await waitFor(() => expect(screen.getByTestId("hooks-trust-dialog")).toBeTruthy());
  };

  it("renders redacted MCP summaries as inert review text", async () => {
    const summary = 'project-search <img src=x onerror="window.__mcpXss=true">';
    createSession.mockResolvedValueOnce(REFUSAL);
    reviewCreationTrust.mockResolvedValueOnce({
      ok: true,
      review: {
        fingerprint: FINGERPRINT,
        merged_hooks: { on_create: [] },
        repo_hooks: {},
        mcp_summaries: [summary],
        hooks_need_trust: false,
        mcp_need_trust: true,
      },
    });
    renderWizard();
    await openDialog();

    const list = screen.getByTestId("hooks-trust-list");
    expect(list.textContent).toContain(summary);
    expect(list.querySelector("img")).toBeNull();
    expect((window as Window & { __mcpXss?: boolean }).__mcpXss).toBeUndefined();
  });

  it("pauses on the trust dialog, then resubmits with trust_hooks on Proceed", async () => {
    createSession.mockResolvedValueOnce(REFUSAL).mockResolvedValueOnce({ ok: true, session: { id: "s1" } });
    const { onCreated } = renderWizard();
    await openDialog();
    expect(screen.getByTestId("hooks-trust-list").textContent).toContain("bash scripts/setup-worktree.sh");
    expect(screen.getByTestId("hooks-trust-list").textContent).toContain("npm start");
    expect(payload()).not.toHaveProperty("trust_hooks", true);
    fireEvent.click(screen.getByTestId("hooks-trust-proceed"));
    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(2));
    expect(payload(1)).toMatchObject({ trust_hooks: true, trust_review: FINGERPRINT });
    await waitFor(() => expect(onCreated).toHaveBeenCalledWith({ id: "s1" }));
  });

  it("re-reviews after creation_trust_changed instead of auto-approving", async () => {
    const changed = {
      ok: false,
      error: "Repository hook configuration changed; review it again",
      trustChanged: true,
    };
    createSession.mockResolvedValueOnce(REFUSAL).mockResolvedValueOnce(changed);
    reviewCreationTrust
      .mockResolvedValueOnce({
        ok: true,
        review: {
          fingerprint: FINGERPRINT,
          merged_hooks: { on_create: ["old"] },
          repo_hooks: {},
          mcp_summaries: [],
          hooks_need_trust: true,
          mcp_need_trust: false,
        },
      })
      .mockResolvedValueOnce({
        ok: true,
        review: {
          fingerprint: { ...FINGERPRINT, hooks_hash: "new" },
          merged_hooks: { on_create: ["new reviewed command"] },
          repo_hooks: {},
          mcp_summaries: [],
          hooks_need_trust: true,
          mcp_need_trust: false,
        },
      });
    renderWizard();
    await openDialog();
    fireEvent.click(screen.getByTestId("hooks-trust-proceed"));
    await waitFor(() => expect(reviewCreationTrust).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(screen.getByTestId("hooks-trust-list").textContent).toContain("new reviewed command"));
    expect(createSession).toHaveBeenCalledTimes(2);
  });

  it("does not submit an approval when the canonical review fails", async () => {
    createSession.mockResolvedValueOnce(REFUSAL);
    reviewCreationTrust.mockResolvedValueOnce({ ok: false, error: "review unavailable" });
    renderWizard();
    await launch();
    await waitFor(() => expect(screen.getByText("review unavailable")).toBeTruthy());
    expect(createSession).toHaveBeenCalledTimes(1);
    expect(screen.queryByTestId("hooks-trust-dialog")).toBeNull();
  });

  it("Cancel dismisses the dialog without a second submit", async () => {
    createSession.mockResolvedValue(REFUSAL);
    renderWizard();
    await openDialog();
    fireEvent.click(screen.getByText("Cancel"));
    await waitFor(() => expect(screen.queryByTestId("hooks-trust-dialog")).toBeNull());
    expect(createSession).toHaveBeenCalledTimes(1);
  });

  it("shows the error instead of looping when a trusted retry is refused again", async () => {
    createSession.mockResolvedValue(REFUSAL);
    renderWizard();
    await openDialog();
    fireEvent.click(screen.getByTestId("hooks-trust-proceed"));
    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(screen.getByText("Repository hooks require trust.")).toBeTruthy());
  });
});

describe("SessionWizard rows", () => {
  // resolved.rs asserts text-primary on the group's surface-950 and text-on-brand on brand-600.
  it("labels an inactive View segment in the asserted readable token", async () => {
    renderWizard();
    const terminal = await screen.findByRole("radio", { name: "Terminal view" });
    expect(terminal.getAttribute("aria-checked")).toBe("false");
    expect(terminal.className).toContain("text-text-primary");
    expect(terminal.className).not.toContain("text-text-secondary");
    const structured = screen.getByRole("radio", { name: "Structured view" });
    expect(structured.className).toContain("bg-brand-600");
    expect(structured.className).toContain("text-text-on-brand");
  });

  it.each([
    ["sandbox_enabled", "Sandbox unavailable: settl runs on the host only"],
    ["worktree_enabled", "Worktree unavailable: settl runs on the host only"],
  ])("refuses a host-only agent with a seeded %s project override", async (field, message) => {
    vi.mocked(fetchProfileAgents).mockResolvedValueOnce([agent("settl", { host_only: true })]);
    vi.mocked(fetchProjects).mockResolvedValueOnce([
      {
        name: "proj",
        path: "/tmp/proj",
        scope: "global",
        pinned: false,
        overrides: { [field]: true },
      },
    ]);
    renderWizard({ path: "/tmp/proj", tool: "settl" });
    await launch();
    await screen.findByText(message + ". Turn it off to launch.");
    expect(createSession).not.toHaveBeenCalled();
  });
  const viewSwitch = () => screen.getByRole("radio", { name: "Structured view" }) as HTMLButtonElement;

  const noAdapter = [agent("claude", { acp_create_ready: false })];
  const cards = /plan, tool calls and diffs as cards/;
  const viewGates: [
    string,
    string,
    AgentInfo[] | undefined,
    WizardPrefill & { commandOverride?: string },
    boolean,
    RegExp,
  ][] = [
    ["claude", "claude", undefined, {}, false, cards],
    ["aider", "aider", [agent("aider", { acp_capable: false })], {}, true, /no ACP adapter/],
    ["custom", "helper", [agent("helper", { kind: "custom", acp_capable: false })], {}, true, /needs agent_acp_cmd/],
    ["not allowed", "claude", [agent("claude", { acp_allowed: false })], {}, true, /not on the allowed agents list/],
    ["adapter missing", "claude", noAdapter, {}, true, /ACP adapter not installed/],
    ["adapter missing, Sandbox on", "claude", noAdapter, { sandboxEnabled: true }, false, cards],
    ["adapter missing, command override", "claude", noAdapter, { commandOverride: "my-wrapper acp" }, false, cards],
    // The list is loaded for the selected Profile, so its flag answers for that Profile.
    ["adapter missing, other Profile", "claude", noAdapter, { profile: "other" }, true, /ACP adapter not installed/],
    // A Profile hook that installs the adapter makes the create ready though it isn't installed yet.
    [
      "installed by a Profile hook",
      "claude",
      [agent("claude", { acp_installed: false, acp_create_ready: true })],
      {},
      false,
      cards,
    ],
  ];
  it.each(viewGates)("gates the structured view: %s", async (_label, tool, agents, setup, disabled, summary) => {
    if (agents) vi.mocked(fetchProfileAgents).mockResolvedValueOnce(agents);
    const { commandOverride, ...prefill } = setup;
    renderWizard({ path: "/tmp/proj", tool, ...prefill });
    if (agents) await waitFor(() => expect(fetchProfileAgents).toHaveBeenCalled());
    if (commandOverride) {
      fireEvent.click(screen.getByTestId("wizard-agent-row"));
      fireEvent.change(screen.getByLabelText("Command override"), { target: { value: commandOverride } });
      fireEvent.click(screen.getByRole("button", { name: "Done" }));
    }
    await waitFor(() => expect(viewSwitch().disabled).toBe(disabled));
    expect(screen.getByTestId("wizard-view-row").textContent).toMatch(summary);
    // A reason cut off at md stays readable on hover.
    expect(screen.getByTestId("wizard-view-row").querySelector("[title]")?.getAttribute("title")).toMatch(summary);
    // Screen readers hear the reason with the group.
    const describedBy = screen.getByRole("radiogroup", { name: "View" }).getAttribute("aria-describedby");
    expect(document.getElementById(describedBy ?? "")?.textContent).toMatch(summary);
    // Terminal view is what an agent without Structured view gets.
    const terminal = screen.getByRole("radio", { name: "Terminal view" });
    expect(terminal.getAttribute("aria-checked")).toBe(String(disabled));
  });

  it("asks for the selected project's readiness, which counts its repository hooks", async () => {
    // Only an answer for the project counts the repository hook that installs the adapter.
    vi.mocked(fetchProfileAgents).mockImplementation(async (_profile, project) => [
      agent("claude", { acp_installed: false, acp_create_ready: project?.path === "/tmp/proj" }),
    ]);
    renderWizard();
    await launch();
    await waitFor(() => expect(createSession).toHaveBeenCalled());
    expect(fetchProfileAgents).toHaveBeenCalledWith("default", { path: "/tmp/proj", scratch: false });
    expect(payload()).toMatchObject({ view: "structured" });
    expect(screen.getByTestId("wizard-view-row").textContent).not.toMatch(/ACP adapter not installed/);
  });

  it("keeps Structured view selectable while the new project's readiness is pending", async () => {
    vi.mocked(fetchRecentProjects).mockResolvedValueOnce({
      projects: [
        { path: "/tmp/proj", display_name: "proj", tool: "claude", last_used_at: "2026-01-01T00:00:00Z" },
        { path: "/tmp/other", display_name: "other", tool: "claude", last_used_at: "2026-01-01T00:00:00Z" },
      ],
    });
    let answerOther!: (agents: AgentInfo[]) => void;
    vi.mocked(fetchProfileAgents).mockImplementation((_profile, project) =>
      project?.path === "/tmp/other" ? new Promise((resolve) => (answerOther = resolve)) : Promise.resolve(noAdapter),
    );
    renderWizard();
    await waitFor(() => expect(viewSwitch().disabled).toBe(true));
    fireEvent.click(screen.getByTestId("wizard-project-row"));
    fireEvent.click((await screen.findByText("/tmp/other")).closest("button")!);
    // /tmp/proj's answer isn't /tmp/other's, before the fetch starts and while it is held.
    expect(fetchProfileAgents).not.toHaveBeenCalledWith("default", { path: "/tmp/other", scratch: false });
    expect(viewSwitch().disabled).toBe(false);
    await waitFor(() =>
      expect(fetchProfileAgents).toHaveBeenCalledWith("default", { path: "/tmp/other", scratch: false }),
    );
    expect(viewSwitch().disabled).toBe(false);
    await act(async () => answerOther(noAdapter));
    expect(viewSwitch().disabled).toBe(true);
  });

  it("asks a scratch create's readiness without a project path", async () => {
    renderWizard({ scratch: true, tool: "claude" });
    await waitFor(() => expect(fetchProfileAgents).toHaveBeenCalledWith("default", { path: "", scratch: true }));
  });

  it("creates a terminal session when the adapter is missing", async () => {
    vi.mocked(fetchProfileAgents).mockResolvedValueOnce(noAdapter);
    renderWizard();
    await waitFor(() => expect(viewSwitch().disabled).toBe(true));
    await launch();
    await waitFor(() => expect(createSession).toHaveBeenCalled());
    expect(payload()).toMatchObject({ view: "terminal" });
  });

  it("shows the Daemon's message when it refuses a missing adapter", async () => {
    const message = "ACP adapter `claude-agent-acp` is not installed or not on $PATH. Install: npm install -g x";
    createSession.mockResolvedValueOnce({
      ok: false,
      error: message,
      failure: { code: "acp_adapter_missing", message, stage: null, reference: null },
    });
    renderWizard();
    await launch();
    expect(await screen.findByText(message)).toBeTruthy();
  });

  it.each([
    [{ scratch: true }, "not for scratch sessions"],
    [{ path: "/tmp/plain" }, "not a git repository"],
  ])("disables the worktree switch with a reason: %o", async (prefill, reason) => {
    renderWizard(prefill);
    await waitFor(() => expect(screen.getByTestId("wizard-worktree-row").textContent).toContain(reason));
    expect((screen.getByRole("switch", { name: "Create a worktree" }) as HTMLButtonElement).disabled).toBe(true);
  });

  // A blocked switch left on shows on, matching the create payload, and can be turned off.
  it.each([
    [[agent("settl", { host_only: true })], "Run in a safe container", "settl runs on the host only"],
    [undefined, "Run in a safe container", "Docker is not running"],
    [[agent("settl", { host_only: true })], "Create a worktree", "settl runs on the host only"],
  ])("a blocked switch left on shows on and turns off: %#", async (agents, name, reason) => {
    if (agents) vi.mocked(fetchProfileAgents).mockResolvedValueOnce(agents);
    const tool = agents ? "settl" : "claude";
    renderWizard({ path: "/tmp/proj", tool, sandboxEnabled: true, worktreeEnabled: true });
    const toggle = () => screen.getByRole("switch", { name }) as HTMLButtonElement;
    await waitFor(() => expect(toggle().closest("div")!.textContent).toContain(reason));
    expect(toggle().getAttribute("aria-checked")).toBe("true");
    fireEvent.click(toggle());
    await waitFor(() => expect(toggle().disabled).toBe(true));
    expect(toggle().getAttribute("aria-checked")).toBe("false");
  });

  it("opens the project picker with no project, and returns to the form on a pick", async () => {
    renderWizard({});
    fireEvent.click((await screen.findByText("/tmp/proj")).closest("button")!);
    expect(screen.getByTestId("wizard-project-row").textContent).toContain("/tmp/proj");
  });

  describe("profile", () => {
    const PROFILES = [
      { name: "default", is_default: true, description: "Stock setup" },
      { name: "work", is_default: false },
    ];

    it("picks a profile in its panel and applies its defaults", async () => {
      vi.mocked(fetchProfiles).mockResolvedValue(PROFILES);
      renderWizard();
      fireEvent.click(await screen.findByText("Profile"));
      fireEvent.click(screen.getByRole("radio", { name: /work/ }));
      await waitFor(() => expect(fetchSettings).toHaveBeenCalledWith("work"));
      // Groups are per profile.
      await waitFor(() => expect(fetchGroups).toHaveBeenLastCalledWith("work"));
      await launch();
      await waitFor(() => expect(createSession).toHaveBeenCalled());
      expect(payload()).toMatchObject({ profile: "work" });
    });

    // #4086: a view toggled while the settings response is pending is newer than the response.
    it("keeps a view toggled while the profile response is pending", async () => {
      vi.mocked(fetchProfiles).mockResolvedValue(PROFILES);
      const terminal = { acp: { structured_view: "terminal" } } as never;
      let release!: (v: never) => void;
      vi.mocked(fetchSettings)
        .mockResolvedValueOnce(terminal)
        .mockReturnValueOnce(new Promise((r) => (release = r)));
      renderWizard();
      const view = () => screen.getByRole("radio", { name: "Structured view" });
      await screen.findByRole("radio", { name: "Structured view" });
      await waitFor(() => expect(view().getAttribute("aria-checked")).toBe("false"));
      fireEvent.click(screen.getByText("Profile"));
      fireEvent.click(screen.getByRole("radio", { name: /work/ }));
      await waitFor(() => expect(fetchSettings).toHaveBeenCalledWith("work"));
      fireEvent.click(view());
      expect(view().getAttribute("aria-checked")).toBe("true");
      release(terminal);
      await launch();
      await waitFor(() => expect(createSession).toHaveBeenCalled());
      expect(view().getAttribute("aria-checked")).toBe("true");
      expect(payload()).toMatchObject({ profile: "work", view: "structured" });
    });

    it("keeps a workspace choice made while the profile response is pending", async () => {
      vi.mocked(fetchProfiles).mockResolvedValue(PROFILES);
      const worktrees = { worktree: { enabled: true } } as never;
      let release!: (v: never) => void;
      vi.mocked(fetchSettings)
        .mockResolvedValueOnce(worktrees)
        .mockReturnValueOnce(new Promise((r) => (release = r)));
      renderWizard();
      await waitFor(() =>
        expect(screen.getByRole("switch", { name: "Create a worktree" }).getAttribute("aria-checked")).toBe("true"),
      );
      fireEvent.click(screen.getByText("Profile"));
      fireEvent.click(screen.getByRole("radio", { name: /work/ }));
      await waitFor(() => expect(fetchSettings).toHaveBeenCalledWith("work"));
      fireEvent.click(screen.getByRole("button", { name: "Configure worktree" }));
      fireEvent.click(await screen.findByRole("switch", { name: "Workspace directory" }));
      fireEvent.click(screen.getByRole("button", { name: "Back" }));
      await act(async () => release(worktrees));
      await launch();
      await waitFor(() => expect(createSession).toHaveBeenCalled());
      expect(payload()).toMatchObject({ profile: "work", workspace: true });
    });

    it("resets a view set before the profile was picked", async () => {
      vi.mocked(fetchProfiles).mockResolvedValue(PROFILES);
      const confirmSpy = vi.spyOn(window, "confirm").mockReturnValue(true);
      try {
        vi.mocked(fetchSettings)
          .mockResolvedValueOnce({} as never)
          .mockResolvedValueOnce({ acp: { structured_view: "terminal" } } as never);
        renderWizard();
        fireEvent.click(await screen.findByRole("radio", { name: "Terminal view" }));
        fireEvent.click(screen.getByText("Profile"));
        fireEvent.click(screen.getByRole("radio", { name: /work/ }));
        await launch();
        await waitFor(() => expect(createSession).toHaveBeenCalled());
        expect(payload()).toMatchObject({ profile: "work", view: "terminal" });
      } finally {
        confirmSpy.mockRestore();
      }
    });

    it("confirms before a profile overwrites edits", async () => {
      vi.mocked(fetchProfiles).mockResolvedValue(PROFILES);
      const confirmSpy = vi.spyOn(window, "confirm").mockReturnValue(false);
      try {
        renderWizard();
        fireEvent.click(await screen.findByRole("switch", { name: "Yolo mode" }));
        fireEvent.click(screen.getByText("Profile"));
        fireEvent.click(screen.getByRole("radio", { name: /work/ }));
        expect(confirmSpy).toHaveBeenCalled();
        expect(fetchSettings).not.toHaveBeenCalledWith("work");
      } finally {
        confirmSpy.mockRestore();
      }
    });
  });
});

describe("SessionWizard under StrictMode", () => {
  // The app root renders in StrictMode, whose dev setup/cleanup/setup must not leave the
  // wizard thinking it was closed.
  it("completes an ordinary Launch in the foreground", async () => {
    const onCreated = vi.fn();
    const onCreatedInBackground = vi.fn();
    render(
      <StrictMode>
        <SessionWizard
          servedProfile="default"
          onClose={vi.fn()}
          onCreated={onCreated}
          onCreatedInBackground={onCreatedInBackground}
          prefill={{ path: "/tmp/proj", tool: "claude" }}
        />
      </StrictMode>,
    );
    await launch();
    await waitFor(() => expect(onCreated).toHaveBeenCalledWith({ id: "s1" }));
    expect(onCreatedInBackground).not.toHaveBeenCalled();
  });
});

describe("SessionWizard create progress", () => {
  const deferred = () => {
    let resolve!: (v: unknown) => void;
    const promise = new Promise((r) => (resolve = r));
    return { promise, resolve };
  };

  it("shows hook output while a slow create runs, polled by its idempotency key", async () => {
    const pending = deferred();
    createSession.mockReturnValueOnce(pending.promise);
    vi.mocked(fetchCreateProgress).mockResolvedValue({
      stage: "running_hooks",
      hook: "npm install",
      output: ["added 12 packages"],
    });
    const { onCreated } = renderWizard();
    await launch();
    const key = payload().idempotency_key;
    expect(key).toBeTruthy();
    await waitFor(() => expect(screen.getByTestId("create-progress-hook").textContent).toContain("npm install"), {
      timeout: 3000,
    });
    expect(screen.getByTestId("create-progress-output").textContent).toContain("added 12 packages");
    expect(fetchCreateProgress).toHaveBeenCalledWith(key);
    pending.resolve({ ok: true, session: { id: "s1" } });
    await waitFor(() => expect(onCreated).toHaveBeenCalledWith({ id: "s1" }));
  });

  it("hands a backgrounded create to onCreatedInBackground instead of navigating", async () => {
    const pending = deferred();
    createSession.mockReturnValueOnce(pending.promise);
    const { onClose, onCreated, onCreatedInBackground } = renderWizard();
    await launch();
    fireEvent.click(await screen.findByText("Continue in background", undefined, { timeout: 3000 }));
    expect(onClose).toHaveBeenCalled();
    pending.resolve({ ok: true, session: { id: "s1" } });
    await waitFor(() => expect(onCreatedInBackground).toHaveBeenCalledWith({ id: "s1" }));
    expect(onCreated).not.toHaveBeenCalled();
  });

  it("retries a dropped request with the same idempotency key", async () => {
    createSession
      .mockResolvedValueOnce({ ok: false, error: "Network error", network: true })
      .mockResolvedValueOnce({ ok: true, session: { id: "s1" } });
    const { onCreated } = renderWizard();
    await launch();
    await waitFor(() => expect(onCreated).toHaveBeenCalledWith({ id: "s1" }), { timeout: 3000 });
    expect(createSession).toHaveBeenCalledTimes(2);
    expect(payload(1).idempotency_key).toBe(payload(0).idempotency_key);
  });
});

describe("SessionWizard unknown create outcome", () => {
  const LOST = { ok: false, error: "Network error: offline", network: true };
  const toastError = vi.fn();

  beforeEach(() => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    toastBus.handler = { push: vi.fn(), error: toastError, info: vi.fn(), openLink: vi.fn() };
  });
  afterEach(() => {
    vi.useRealTimers();
    toastBus.handler = null;
    toastError.mockReset();
  });

  // Every response lost: the server may still have created the session.
  const loseEveryResponse = async () => {
    createSession.mockResolvedValue(LOST);
    await launch();
    await vi.advanceTimersByTimeAsync(60_000);
  };

  const DAY_MS = 24 * 60 * 60 * 1000;

  it("stops without sending once the replay window passes while the browser is offline", async () => {
    let online = true;
    Object.defineProperty(window.navigator, "onLine", { configurable: true, get: () => online });
    try {
      renderWizard();
      createSession.mockImplementation(async () => {
        online = false;
        return LOST;
      });
      await launch();
      await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));

      // A day offline, then the connection returns.
      vi.setSystemTime(Date.now() + DAY_MS + 1_000);
      online = true;
      window.dispatchEvent(new Event("online"));
      await vi.advanceTimersByTimeAsync(10_000);

      await waitFor(() => expect(screen.getByText(/Gave up waiting/)).toBeTruthy());
      expect(createSession).toHaveBeenCalledTimes(1);
    } finally {
      Object.defineProperty(window.navigator, "onLine", { configurable: true, get: () => true });
    }
  });

  it("does not resend an unknown outcome from Launch once its replay window has passed", async () => {
    renderWizard();
    await loseEveryResponse();
    await waitFor(() => expect(screen.getByText(/may still be created/)).toBeTruthy());
    const sent = createSession.mock.calls.length;

    vi.setSystemTime(Date.now() + DAY_MS);
    createSession.mockResolvedValue({ ok: true, session: { id: "s1" } });
    await launch();
    await waitFor(() => expect(screen.getByText(/Gave up waiting/)).toBeTruthy());
    expect(createSession).toHaveBeenCalledTimes(sent);

    // The expired request is gone, so the next Launch is a fresh one.
    await launch();
    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(sent + 1));
    expect(payload(sent).idempotency_key).not.toBe(payload(0).idempotency_key);
  });

  it("sends the first attempt plain and names its daemon run on every retry", async () => {
    renderWizard();
    await loseEveryResponse();
    await waitFor(() => expect(screen.getByText(/may still be created/)).toBeTruthy());
    expect(payload(0).retry_origin).toBeUndefined();
    const retries = createSession.mock.calls.slice(1).map(([body]) => body.retry_origin);
    expect(retries.length).toBeGreaterThan(0);
    expect(new Set(retries)).toEqual(new Set(["boot-1"]));

    // Launch resumes the same attempt, so it is a retry too.
    createSession.mockResolvedValue({ ok: true, session: { id: "s1" } });
    await launch();
    await waitFor(() => expect(payload(createSession.mock.calls.length - 1).retry_origin).toBe("boot-1"));
  });

  it("stops at a restarted server's unknown outcome instead of calling it a failure", async () => {
    renderWizard();
    await loseEveryResponse();
    await waitFor(() => expect(screen.getByText(/may still be created/)).toBeTruthy());
    const message = "The server restarted before confirming this session.";
    createSession.mockResolvedValue({ ok: false, error: message, outcomeUnknown: true });
    await launch();
    await waitFor(() => expect(screen.getByText(message)).toBeTruthy());
    const sent = createSession.mock.calls.length;

    // Settled: the next Launch is a fresh request under a new key.
    createSession.mockResolvedValue({ ok: true, session: { id: "s1" } });
    await launch();
    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(sent + 1));
    expect(payload(sent).idempotency_key).not.toBe(payload(0).idempotency_key);
    expect(payload(sent).retry_origin).toBeUndefined();
  });

  it("keeps the key and retries the same request from Launch", async () => {
    const { onCreated } = renderWizard();
    await loseEveryResponse();
    await waitFor(() => expect(screen.getByText(/may still be created/)).toBeTruthy());
    const calls = createSession.mock.calls.length;
    expect(new Set(createSession.mock.calls.map(([body]) => body.idempotency_key)).size).toBe(1);

    createSession.mockResolvedValue({ ok: true, session: { id: "s1" } });
    await launch();
    await waitFor(() => expect(onCreated).toHaveBeenCalledWith({ id: "s1" }));
    expect(payload(calls).idempotency_key).toBe(payload(0).idempotency_key);
  });

  it("reconciles under the same key when the connection returns", async () => {
    const { onCreated } = renderWizard();
    await loseEveryResponse();
    await waitFor(() => expect(screen.getByText(/may still be created/)).toBeTruthy());
    createSession.mockResolvedValue({ ok: true, session: { id: "s1" } });
    window.dispatchEvent(new Event("online"));
    await waitFor(() => expect(onCreated).toHaveBeenCalledWith({ id: "s1" }));
    expect(payload(createSession.mock.calls.length - 1).idempotency_key).toBe(payload(0).idempotency_key);
  });

  // The wizard really unmounts on close, as in App, so only a longer-lived owner can keep the key.
  const launchInBackgroundAndClose = async () => {
    createSession.mockResolvedValue(LOST);
    const onCreatedInBackground = vi.fn();
    const view = render(
      <SessionWizard
        servedProfile="default"
        onClose={() => view.unmount()}
        onCreated={vi.fn()}
        onCreatedInBackground={onCreatedInBackground}
        prefill={{ path: "/tmp/proj", tool: "claude" }}
      />,
    );
    await launch();
    fireEvent.click(await screen.findByText("Continue in background", undefined, { timeout: 3000 }));
    expect(screen.queryByTestId("session-wizard")).toBeNull();
    await vi.advanceTimersByTimeAsync(5 * 60_000);
    const key = payload(0).idempotency_key;
    expect(new Set(createSession.mock.calls.map(([body]) => body.idempotency_key))).toEqual(new Set([key]));
    expect(toastError).not.toHaveBeenCalled();
    return { key, onCreatedInBackground };
  };

  it("a reopened wizard retries a closed wizard's unresolved create under its key", async () => {
    const { key } = await launchInBackgroundAndClose();
    const { onCreated } = renderWizard({ path: "/tmp/other", tool: "claude" });
    await waitFor(() => expect(screen.getByText(/may still be created/)).toBeTruthy());

    createSession.mockResolvedValue({ ok: true, session: { id: "s1" } });
    await launch();
    await waitFor(() => expect(onCreated).toHaveBeenCalledWith({ id: "s1" }));
    expect(payload(createSession.mock.calls.length - 1).idempotency_key).toBe(key);
  });

  it("the app-level owner reconciles a closed wizard's create once the server answers", async () => {
    const onCreatedByOwner = vi.fn();
    startPendingCreates({ onCreated: onCreatedByOwner, onFailed: vi.fn(), onUnknown: vi.fn() });
    const { key } = await launchInBackgroundAndClose();

    createSession.mockResolvedValue({ ok: true, session: { id: "s1" } });
    await vi.advanceTimersByTimeAsync(60_000);
    await waitFor(() => expect(onCreatedByOwner).toHaveBeenCalledWith({ id: "s1" }, expect.anything()));
    expect(payload(createSession.mock.calls.length - 1).idempotency_key).toBe(key);
    // Resolved, so a later wizard starts a fresh request.
    renderWizard();
    expect(screen.queryByText(/may still be created/)).toBeNull();
  });

  const REFUSED = {
    ok: false,
    error: "boom",
    failure: { code: "create_failed", stage: "preparing", message: "boom", reference: null },
  };

  it("a wizard reopened from a failure sends its own form, not a closed wizard's unresolved create", async () => {
    const { key } = await launchInBackgroundAndClose();
    const restore = { data: { ...initialData, path: "/tmp/other", tool: "claude" }, failure: REFUSED.failure };
    render(<SessionWizard servedProfile="default" onClose={vi.fn()} onCreated={vi.fn()} restore={restore} />);
    expect(screen.queryByText(/may still be created/)).toBeNull();
    const before = createSession.mock.calls.length;
    createSession.mockResolvedValue({ ok: true, session: { id: "s2" } });
    const tryAgain = () => screen.getByTestId("create-failure-try_again") as HTMLButtonElement;
    await waitFor(() => expect(tryAgain().disabled).toBe(false));
    fireEvent.click(tryAgain());
    await waitFor(() =>
      expect(createSession.mock.calls.slice(before).map(([body]) => body.path)).toContain("/tmp/other"),
    );
    const sent = createSession.mock.calls.slice(before).find(([body]) => body.path === "/tmp/other")![0];
    expect(sent.idempotency_key).not.toBe(key);
  });

  it("an adopted create refused in the background toasts plainly, since there is no form to reopen", async () => {
    await launchInBackgroundAndClose();
    const action = vi.fn();
    toastBus.handler = { ...toastBus.handler!, action };
    let finish!: (r: unknown) => void;
    createSession.mockReturnValue(new Promise((r) => (finish = r)));
    render(
      <SessionWizard
        servedProfile="default"
        onClose={vi.fn()}
        onCreated={vi.fn()}
        onCreatedInBackground={vi.fn()}
        onReopenFailure={vi.fn()}
        prefill={{ path: "/tmp/other", tool: "claude" }}
      />,
    );
    await waitFor(() => expect(screen.getByText(/may still be created/)).toBeTruthy());
    await launch();
    fireEvent.click(await screen.findByText("Continue in background", undefined, { timeout: 3000 }));
    await act(async () => finish(REFUSED));
    await waitFor(() => expect(toastError).toHaveBeenCalledWith("Session was not created: boom"));
    expect(action).not.toHaveBeenCalled();
  });
});

describe("SessionWizard served-profile authority", () => {
  const profiles = [
    { name: "Main", is_default: true },
    { name: "Alpha", is_default: false },
    { name: "Beta", is_default: false },
  ];
  const settings = (profile: string) => ({
    session: { yolo_mode_default: profile === "Beta", agent_command_override: { claude: profile + "-claude" } },
    worktree: { enabled: true },
    acp: { structured_view: "terminal" },
  });
  const open = (label: string) => fireEvent.click(screen.getByText(label).closest("button")!);
  const chooseProfile = async (profile: string) => {
    open("Profile");
    fireEvent.click(screen.getByRole("radio", { name: profile === "" ? /Server default/ : new RegExp(profile) }));
    await waitFor(() =>
      expect(screen.getByRole("switch", { name: "Yolo mode" }).getAttribute("aria-checked")).toBe(
        profile === "Beta" ? "true" : "false",
      ),
    );
  };

  it("scopes settings, remembered registry overrides, project and extra-repo suggestions to Alpha, including after Beta -> implicit", async () => {
    vi.mocked(fetchProfiles).mockResolvedValueOnce(profiles as never);
    vi.mocked(fetchSettings).mockImplementation(async (profile) => settings(profile ?? "Alpha"));
    vi.mocked(fetchProjects).mockImplementation(
      async ({ profile }) =>
        [
          {
            name: profile + "-only-project",
            path: "/" + profile.toLowerCase(),
            scope: "profile",
            pinned: false,
          },
        ] as never,
    );
    renderWizard({ path: "/tmp/proj", tool: "claude" }, "Alpha");
    await waitFor(() => expect(fetchSettings).toHaveBeenCalledWith("Alpha"));
    await waitFor(() => expect(screen.getByText("Profile")).toBeTruthy());
    await waitFor(() => expect(fetchProjects).toHaveBeenCalledWith({ profile: "Alpha" }));
    for (const profile of ["Alpha", "Beta", "Alpha"]) {
      if (profile === "Beta") await chooseProfile("Beta");
      else if (vi.mocked(fetchSettings).mock.calls.some(([p]) => p === "Beta")) await chooseProfile("");
      fireEvent.click(screen.getByTestId("wizard-project-row"));
      expect(await screen.findByText(profile + "-only-project")).toBeTruthy();
      expect(screen.queryByText("Main-only-project")).toBeNull();
      fireEvent.click(screen.getByRole("button", { name: "Back", exact: true }));
      open("Extra repos");
      expect(await screen.findByText(profile + "-only-project")).toBeTruthy();
      expect(screen.queryByText("Main-only-project")).toBeNull();
      fireEvent.click(screen.getByRole("button", { name: "Back", exact: true }));
      fireEvent.click(screen.getByTestId("wizard-agent-row"));
      expect(await screen.findByText(new RegExp(profile + "-claude"))).toBeTruthy();
      fireEvent.click(screen.getByRole("button", { name: "Back", exact: true }));
    }
    await launch();
    await waitFor(() => expect(createSession).toHaveBeenCalled());
    expect(payload().profile).toBeUndefined();
    expect(payload().yolo_mode).toBe(false);
    expect(vi.mocked(fetchSettings).mock.calls.some(([profile]) => profile === "Main")).toBe(false);
    expect(vi.mocked(fetchProjects).mock.calls.some(([context]) => context.profile === "Main")).toBe(false);
  });

  it("does not let a delayed Alpha seed overwrite Beta selection, and keeps explicit prefill authority", async () => {
    vi.mocked(fetchProfiles).mockResolvedValueOnce(profiles as never);
    const seed = Promise.withResolvers<Awaited<ReturnType<typeof fetchSettings>>>();
    vi.mocked(fetchSettings).mockImplementation(async (profile) =>
      profile === "Alpha" ? seed.promise : settings(profile!),
    );
    renderWizard({ path: "/tmp/proj", tool: "claude" }, "Alpha");
    await waitFor(() => expect(fetchSettings).toHaveBeenCalledWith("Alpha"));
    await waitFor(() => expect(screen.getByText("Profile")).toBeTruthy());
    await chooseProfile("Beta");
    await act(async () => seed.resolve(settings("Alpha")));
    expect(screen.getByRole("switch", { name: "Yolo mode" }).getAttribute("aria-checked")).toBe("true");
    await launch();
    expect(payload()).toMatchObject({ profile: "Beta", yolo_mode: true });
    cleanup();
    createSession.mockClear();
    renderWizard(
      { path: "/tmp/proj", tool: "claude", profile: "Beta", yoloMode: false, worktreeEnabled: false },
      "Alpha",
    );
    await launch();
    expect(payload()).toMatchObject({ profile: "Beta", yolo_mode: false, worktree_enabled: false });
  });

  it("waits for served registry scope instead of guessing the machine default", async () => {
    vi.mocked(fetchProfiles).mockResolvedValueOnce(profiles as never);
    vi.mocked(fetchSettings).mockResolvedValue({});
    const view = render(<SessionWizard onClose={() => {}} onCreated={() => {}} />);
    await waitFor(() => expect(fetchProfiles).toHaveBeenCalled());
    expect(fetchSettings).not.toHaveBeenCalled();
    expect(fetchProjects).not.toHaveBeenCalled();
    view.rerender(<SessionWizard servedProfile="Alpha" onClose={() => {}} onCreated={() => {}} />);
    await waitFor(() => expect(fetchSettings).toHaveBeenCalledWith("Alpha"));
    await waitFor(() => expect(fetchProjects).toHaveBeenCalledWith({ profile: "Alpha" }));
  });

  it("recovers a single served profile after an initial failure without reseeding dirty choices", async () => {
    vi.mocked(fetchProfiles).mockResolvedValueOnce([{ name: "Alpha", is_default: false }] as never);
    vi.mocked(fetchSettings).mockImplementation(async (profile) => settings(profile!));
    let publish!: (profile: string) => void;
    let attempts = 0;
    const retry = vi.fn(async () => {
      if (++attempts > 1) publish("Alpha");
    });
    function Harness() {
      const [served, setServed] = useState<string>();
      publish = setServed;
      return (
        <SessionWizard
          servedProfile={served}
          onRetryServedProfile={retry}
          prefill={{ path: "/tmp/proj", tool: "claude" }}
          onClose={() => {}}
          onCreated={() => {}}
        />
      );
    }
    render(<Harness />);
    const retryButton = await screen.findByRole("button", { name: "Retry server profile" });
    await waitFor(() => expect(retryButton.hasAttribute("disabled")).toBe(false));
    expect(screen.getByText(/Launch session/).closest("button")!.disabled).toBe(true);
    expect(fetchSettings).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("switch", { name: "Yolo mode" }));
    fireEvent.click(retryButton);
    await launch();
    await waitFor(() => expect(createSession).toHaveBeenCalled());
    expect(payload()).toMatchObject({ yolo_mode: true });
    expect(payload().profile).toBeUndefined();
    expect(fetchProjects).toHaveBeenCalledWith({ profile: "Alpha" });
    expect(vi.mocked(fetchSettings).mock.calls.some(([profile]) => profile === "Main")).toBe(false);
  });

  it.each([false, true])(
    "resolves Server default after explicit Beta and preserves newer view edits (%s)",
    async (editWhilePending) => {
      const confirmSpy = vi.spyOn(window, "confirm").mockReturnValue(true);
      try {
        vi.mocked(fetchProfiles).mockResolvedValueOnce(profiles as never);
        const pending = Promise.withResolvers<SettingsResponse | null>();
        vi.mocked(fetchSettings).mockImplementation(async (profile) =>
          profile === "Alpha" ? pending.promise : settings(profile!),
        );
        let publish!: (profile: string) => void;
        const retry = async () => publish("Alpha");
        function Harness() {
          const [served, setServed] = useState<string>();
          publish = setServed;
          return (
            <SessionWizard
              servedProfile={served}
              onRetryServedProfile={retry}
              prefill={{ path: "/tmp/proj", tool: "claude", profile: "Beta", yoloMode: true }}
              onClose={() => {}}
              onCreated={() => {}}
            />
          );
        }
        render(<Harness />);
        const structured = () => screen.getByRole("radio", { name: "Structured view" });
        await waitFor(() => expect(structured().getAttribute("aria-checked")).toBe("false"));
        fireEvent.click(structured());
        open("Profile");
        fireEvent.click(screen.getByRole("radio", { name: /Server default/ }));
        await waitFor(() => expect(fetchSettings).toHaveBeenCalledWith("Alpha"));
        if (editWhilePending) {
          fireEvent.click(screen.getByRole("radio", { name: "Terminal view" }));
          fireEvent.click(structured());
        }
        await act(async () => pending.resolve(settings("Alpha")));
        await launch();
        await waitFor(() => expect(createSession).toHaveBeenCalled());
        expect(payload()).toMatchObject({ yolo_mode: false, view: editWhilePending ? "structured" : "terminal" });
        expect(payload().profile).toBeUndefined();
        expect(confirmSpy).toHaveBeenCalledTimes(1);
      } finally {
        confirmSpy.mockRestore();
      }
    },
  );
});
// #4517: only a sandboxed launch has to wait on the Docker probe.
describe("SessionWizard Docker readiness", () => {
  const launchButton = () => screen.getByText(/Launch session/).closest("button") as HTMLButtonElement;
  const pendingProbe = () => {
    let answer!: (v: DockerStatusResponse) => void;
    vi.mocked(fetchDockerStatus).mockReturnValueOnce(new Promise((r) => (answer = r)));
    return (available: boolean) => answer({ available, runtime: "docker", runtime_name: "Docker" });
  };

  it("enables Launch without waiting for Docker when the sandbox is off", async () => {
    pendingProbe();
    renderWizard();
    await waitFor(() => expect(launchButton().disabled).toBe(false));
  });

  it("holds a sandboxed Launch for the probe, then reports an unavailable runtime", async () => {
    const answer = pendingProbe();
    vi.mocked(fetchSettings).mockResolvedValueOnce({ session: { default_tool: "opencode" }, sandbox: {} } as never);
    renderWizard({ path: "/tmp/proj", sandboxEnabled: true });
    // The seeded defaults are applied, so only the Docker probe can still hold Launch.
    await waitFor(() => expect(screen.getByTestId("wizard-agent-row").textContent).toContain("opencode"));
    await act(async () => {});
    expect(launchButton().disabled).toBe(true);
    answer(false);
    await waitFor(() => expect(launchButton().disabled).toBe(false));
    fireEvent.click(launchButton());
    expect(await screen.findByText(/Sandbox unavailable: Docker is not running/)).toBeTruthy();
    expect(createSession).not.toHaveBeenCalled();
  });

  it("re-holds Launch when a profile switch turns the sandbox on while the probe is pending", async () => {
    const answer = pendingProbe();
    vi.mocked(fetchProfiles).mockResolvedValue([
      { name: "default", is_default: true },
      { name: "work", is_default: false },
    ]);
    vi.mocked(fetchSettings).mockImplementation((async (profile?: string) =>
      profile === "work" ? { sandbox: { enabled_by_default: true } } : {}) as never);
    renderWizard();
    await waitFor(() => expect(launchButton().disabled).toBe(false));

    fireEvent.click(await screen.findByText("Profile"));
    fireEvent.click(screen.getByRole("radio", { name: /work/ }));
    const sandbox = () => screen.getByRole("switch", { name: "Run in a safe container" });
    await waitFor(() => expect(sandbox().getAttribute("aria-checked")).toBe("true"));
    expect(launchButton().disabled).toBe(true);

    answer(true);
    await waitFor(() => expect(launchButton().disabled).toBe(false));
  });
});

describe("SessionWizard Group field", () => {
  it("never offers one profile's Groups under another while its list loads", async () => {
    vi.mocked(fetchProfiles).mockResolvedValue([
      { name: "default", is_default: true },
      { name: "work", is_default: false },
    ]);
    vi.mocked(fetchGroups).mockResolvedValueOnce([{ path: "home-group", session_count: 1 }]);
    renderWizard();
    const field = await screen.findByRole("combobox", { name: "Group" });
    fireEvent.focus(field);
    await screen.findByRole("option", { name: "home-group · 1 session" });
    fireEvent.blur(field);
    let release: (groups: { path: string; session_count: number }[]) => void = () => {};
    vi.mocked(fetchGroups).mockReturnValueOnce(new Promise((resolve) => (release = resolve)));
    fireEvent.click(await screen.findByText("Profile"));
    fireEvent.click(screen.getByRole("radio", { name: /work/ }));
    await waitFor(() => expect(fetchGroups).toHaveBeenLastCalledWith("work"));
    const again = await screen.findByRole("combobox", { name: "Group" });
    fireEvent.focus(again);
    expect(screen.queryByRole("option", { name: /home-group/ })).toBeNull();
    await act(async () => release([{ path: "work-group", session_count: 2 }]));
    await screen.findByRole("option", { name: "work-group · 2 sessions" });
  });

  it("refetches Groups when the served profile changes, dropping the old list first", async () => {
    vi.mocked(fetchGroups).mockResolvedValueOnce([{ path: "alpha-group", session_count: 1 }]);
    const props = { onClose: () => {}, onCreated: () => {}, prefill: { path: "/tmp/proj", tool: "claude" } };
    const view = render(<SessionWizard servedProfile="Alpha" {...props} />);
    const field = await screen.findByRole("combobox", { name: "Group" });
    fireEvent.focus(field);
    await screen.findByRole("option", { name: "alpha-group · 1 session" });
    fireEvent.blur(field);
    let release: (groups: { path: string; session_count: number }[]) => void = () => {};
    vi.mocked(fetchGroups).mockReturnValueOnce(new Promise((resolve) => (release = resolve)));
    const calls = vi.mocked(fetchGroups).mock.calls.length;
    view.rerender(<SessionWizard servedProfile="Beta" {...props} />);
    await waitFor(() => expect(vi.mocked(fetchGroups).mock.calls.length).toBe(calls + 1));
    fireEvent.focus(field);
    expect(screen.queryByRole("option", { name: /alpha-group/ })).toBeNull();
    await act(async () => release([{ path: "beta-group", session_count: 2 }]));
    await screen.findByRole("option", { name: "beta-group · 2 sessions" });
  });

  it("launches with the typed group while its list is open", async () => {
    vi.mocked(fetchGroups).mockResolvedValue([{ path: "client/api", session_count: 2 }]);
    renderWizard();
    const field = await screen.findByRole("combobox", { name: "Group" });
    fireEvent.focus(field);
    fireEvent.change(field, { target: { value: "client/new" } });
    expect(field.getAttribute("aria-expanded")).toBe("true");
    await launch();
    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
    expect(payload()).toMatchObject({ group: "client/new" });
  });
});

describe("SessionWizard Yolo mode", () => {
  const yolo = () => screen.getByRole("switch", { name: "Yolo mode" }) as HTMLButtonElement;
  const yoloRow = () => screen.getByTestId("wizard-yolo-row").textContent ?? "";
  const pickAgent = (name: string) => {
    fireEvent.click(screen.getByTestId("wizard-agent-row"));
    fireEvent.click(screen.getByRole("button", { name: new RegExp(`^${name}`) }));
    fireEvent.click(screen.getByRole("button", { name: "Done" }));
  };

  it("states what each value does", async () => {
    renderWizard();
    await waitFor(() => expect(yoloRow()).toContain("asks before running commands"));
    expect(yolo().disabled).toBe(false);
    fireEvent.click(yolo());
    expect(yoloRow()).toContain("skips permission prompts");
  });

  it("locks on for an agent that always runs in Yolo mode, and gives the value back after", async () => {
    vi.mocked(fetchProfileAgents).mockResolvedValue([
      agent("claude"),
      agent("pi", { always_yolo: true, acp_capable: false }),
    ]);
    vi.mocked(fetchSettings).mockResolvedValueOnce({ session: { yolo_mode_default: false }, sandbox: {} } as never);
    renderWizard({ path: "/tmp/proj", tool: "pi" });
    await waitFor(() => expect(yoloRow()).toContain("always on for pi"));
    expect(yolo().disabled).toBe(true);
    expect(yolo().getAttribute("aria-checked")).toBe("true");

    pickAgent("claude");
    await waitFor(() => expect(yoloRow()).toContain("asks before running commands"));
    expect(yolo().getAttribute("aria-checked")).toBe("false");
    expect(yolo().disabled).toBe(false);

    pickAgent("pi");
    await waitFor(() => expect(yoloRow()).toContain("always on for pi"));
    await launch();
    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
    expect(payload()).toMatchObject({ tool: "pi", yolo_mode: false });
  });

  it("keeps a profile's Yolo mode default for an agent that can turn it off", async () => {
    vi.mocked(fetchProfileAgents).mockResolvedValue([
      agent("claude"),
      agent("pi", { always_yolo: true, acp_capable: false }),
    ]);
    vi.mocked(fetchSettings).mockResolvedValueOnce({ session: { yolo_mode_default: true }, sandbox: {} } as never);
    renderWizard({ path: "/tmp/proj", tool: "pi" });
    await waitFor(() => expect(yoloRow()).toContain("always on for pi"));
    pickAgent("claude");
    await waitFor(() => expect(yoloRow()).toContain("skips permission prompts"));
    expect(yolo().getAttribute("aria-checked")).toBe("true");
  });

  it("locks on for a custom agent with its own adapter only in the view that runs its always-Yolo base", async () => {
    // A structured session runs own-pi's adapter; a terminal one runs the Pi CLI it detects as.
    const ownPi = agent("own-pi", { kind: "custom", always_yolo: true, acp_always_yolo: false });
    vi.mocked(fetchProfileAgents).mockResolvedValue([agent("claude"), ownPi]);
    vi.mocked(fetchSettings).mockResolvedValueOnce({ session: { yolo_mode_default: false }, sandbox: {} } as never);
    renderWizard({ path: "/tmp/proj", tool: "own-pi" });
    await waitFor(() => expect(yoloRow()).toContain("asks before running commands"));
    expect(yolo().disabled).toBe(false);
    fireEvent.click(screen.getByRole("radio", { name: "Terminal view" }));
    await waitFor(() => expect(yoloRow()).toContain("always on for own-pi"));
    expect(yolo().disabled).toBe(true);
    fireEvent.click(screen.getByRole("radio", { name: "Structured view" }));
    await waitFor(() => expect(yoloRow()).toContain("asks before running commands"));
  });

  const AGENTS = [agent("claude"), agent("pi", { always_yolo: true, acp_capable: false })];
  const launchButton = () => screen.getByText(/Launch session/).closest("button") as HTMLButtonElement;

  it("reads a custom agent's Yolo mode from the profile it creates in", async () => {
    vi.mocked(fetchProfiles).mockResolvedValueOnce([
      { name: "alpha", is_default: true },
      { name: "beta", is_default: false },
    ]);
    // Alpha's `wrapper` runs Pi and Beta's runs Claude. Beta's first answer is held.
    const wrapper = (always_yolo: boolean) => [
      agent("claude"),
      agent("wrapper", {
        kind: "custom",
        always_yolo,
        acp_always_yolo: always_yolo,
        display_name: always_yolo ? "Pi wrapper" : "Claude wrapper",
      }),
    ];
    let answerBeta: (agents: AgentInfo[]) => void = () => {};
    let betaCalls = 0;
    vi.mocked(fetchProfileAgents).mockImplementation((profile) => {
      if (profile !== "beta") return Promise.resolve(wrapper(true));
      return betaCalls++ === 0 ? new Promise((resolve) => (answerBeta = resolve)) : Promise.resolve(wrapper(false));
    });
    const pickProfile = (name: string) => {
      fireEvent.click(screen.getByText("Profile"));
      fireEvent.click(screen.getByRole("radio", { name: new RegExp(name) }));
    };
    renderWizard({ path: "/tmp/proj", tool: "wrapper" }, "alpha");
    await waitFor(() => expect(yoloRow()).toContain("always on for wrapper"));

    pickProfile("beta");
    await waitFor(() => expect(yoloRow()).toContain("checking the agent…"));
    expect(launchButton().disabled).toBe(true);
    pickProfile("alpha");
    await waitFor(() => expect(yoloRow()).toContain("always on for wrapper"));
    // Beta's late answer is for a profile no longer selected.
    await act(async () => answerBeta(wrapper(false)));
    expect(yoloRow()).toContain("always on for wrapper");

    pickProfile("beta");
    await waitFor(() => expect(yoloRow()).toContain("asks before running commands"));
    expect(yolo().disabled).toBe(false);
    await launch();
    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
    expect(payload()).toMatchObject({ profile: "beta", tool: "wrapper", yolo_mode: false });
    // The app-wide names stay the served profile's; read before the hook's own load answers.
    expect(renderHook(() => useAgentDisplayNames()).result.current("wrapper")).toBe("Pi wrapper");
  });

  it("waits for the agent list before promising prompts or launching", async () => {
    let answer: (agents: typeof AGENTS) => void = () => {};
    vi.mocked(fetchProfileAgents).mockReturnValue(new Promise((resolve) => (answer = resolve)));
    vi.mocked(fetchSettings).mockResolvedValueOnce({ session: { yolo_mode_default: false }, sandbox: {} } as never);
    renderWizard({ path: "/tmp/proj", tool: "pi" });
    await waitFor(() => expect(fetchSettings).toHaveBeenCalled());
    // Let the profile defaults settle, which alone used to enable Launch.
    await act(async () => {});
    expect(yoloRow()).toContain("checking the agent…");
    expect(yoloRow()).not.toContain("asks before running commands");
    expect(launchButton().disabled).toBe(true);

    await act(async () => answer(AGENTS));
    expect(yoloRow()).toContain("always on for pi");
    await launch();
    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
    expect(payload()).toMatchObject({ tool: "pi", yolo_mode: false });
  });

  it("retries a failed agent list, keeping Launch off until it loads", async () => {
    vi.mocked(fetchProfileAgents).mockResolvedValueOnce(null).mockResolvedValue(AGENTS);
    renderWizard({ path: "/tmp/proj", tool: "pi" });
    await screen.findByText(/Couldn't load the agent list/);
    expect(yoloRow()).toContain("checking the agent…");
    expect(launchButton().disabled).toBe(true);
    await waitFor(() => expect(yoloRow()).toContain("always on for pi"), { timeout: 3000 });
    expect(fetchProfileAgents).toHaveBeenCalledTimes(2);
    expect(screen.queryByText(/Couldn't load the agent list/)).toBeNull();
    await launch();
    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
    expect(payload()).toMatchObject({ tool: "pi", yolo_mode: false });
  });

  it("stops retrying a profile the Daemon doesn't have, and loads the next one chosen", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      vi.mocked(fetchProfiles).mockResolvedValueOnce([
        { name: "gone", is_default: true },
        { name: "beta", is_default: false },
      ]);
      vi.mocked(fetchProfileAgents).mockImplementation((profile) =>
        Promise.resolve(profile === "gone" ? "profile_not_found" : AGENTS),
      );
      renderWizard({ path: "/tmp/proj", tool: "pi" }, "gone");
      await screen.findByText('The profile "gone" no longer exists. Choose another profile to launch.');
      expect(launchButton().disabled).toBe(true);
      await act(async () => vi.advanceTimersByTimeAsync(60_000));
      expect(vi.mocked(fetchProfileAgents).mock.calls.filter(([p]) => p === "gone")).toHaveLength(1);

      fireEvent.click(screen.getByText("Profile"));
      fireEvent.click(screen.getByRole("radio", { name: /beta/ }));
      await waitFor(() => expect(yoloRow()).toContain("always on for pi"));
      expect(screen.queryByText(/no longer exists/)).toBeNull();
      await waitFor(() => expect(launchButton().disabled).toBe(false));
    } finally {
      vi.useRealTimers();
    }
  });

  it("offers the Profile row for a deleted served Profile with one Profile left", async () => {
    vi.mocked(fetchProfiles).mockResolvedValue([{ name: "default", is_default: true }]);
    vi.mocked(fetchProfileAgents).mockImplementation((profile) =>
      Promise.resolve(profile === "gone" ? "profile_not_found" : AGENTS),
    );
    renderWizard({ path: "/tmp/proj", tool: "pi" }, "gone");
    await screen.findByText(/The profile "gone" no longer exists/);
    fireEvent.click(screen.getByText("Profile"));
    fireEvent.click(screen.getByRole("radio", { name: /^default/ }));
    await waitFor(() => expect(yoloRow()).toContain("always on for pi"));
    expect(screen.queryByText(/no longer exists/)).toBeNull();
    await waitFor(() => expect(launchButton().disabled).toBe(false));
  });

  it("keeps the Profile row hidden for one Profile while its agent list fails", async () => {
    vi.mocked(fetchProfiles).mockResolvedValue([{ name: "default", is_default: true }]);
    vi.mocked(fetchProfileAgents).mockResolvedValue(null);
    renderWizard({ path: "/tmp/proj", tool: "pi" });
    await screen.findByText(/Couldn't load the agent list/);
    expect(screen.queryByText("Profile")).toBeNull();
  });

  it("names a deleted Profile even when its project lookup failed too", async () => {
    vi.mocked(fetchProfileAgents).mockResolvedValue("profile_not_found");
    vi.mocked(fetchProjects).mockResolvedValue(null);
    renderWizard({ path: "/tmp/proj", tool: "pi" }, "gone");
    await waitFor(() => expect(fetchProjects).toHaveBeenCalledWith({ profile: "gone" }));
    await act(async () => {});
    expect(screen.getByText('The profile "gone" no longer exists. Choose another profile to launch.')).toBeTruthy();
    expect(screen.queryByText(/Could not load this project's settings/)).toBeNull();
  });

  it("drops the last Profile's agents when the next one's list fails", async () => {
    vi.mocked(fetchProfiles).mockResolvedValueOnce([
      { name: "alpha", is_default: true },
      { name: "beta", is_default: false },
    ]);
    vi.mocked(fetchProfileAgents).mockImplementation((profile) =>
      Promise.resolve(profile === "alpha" ? [agent("claude"), agent("aider")] : null),
    );
    const agentButton = () => screen.queryByRole("button", { name: /^aider/ });
    renderWizard({ path: "/tmp/proj", tool: "claude" }, "alpha");
    await waitFor(() => expect(yoloRow()).toContain("asks before running commands"));
    fireEvent.click(screen.getByTestId("wizard-agent-row"));
    expect(agentButton()).not.toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Done" }));

    fireEvent.click(screen.getByText("Profile"));
    fireEvent.click(screen.getByRole("radio", { name: /beta/ }));
    await screen.findByText(/Couldn't load the agent list/);
    fireEvent.click(screen.getByTestId("wizard-agent-row"));
    expect(agentButton()).toBeNull();
  });

  it("launches a name-only form without the agent list", async () => {
    // The retry never answers, so the list stays failed.
    vi.mocked(fetchProfileAgents)
      .mockResolvedValueOnce(null)
      .mockReturnValueOnce(new Promise(() => {}));
    render(<SessionWizard servedProfile="default" onClose={vi.fn()} onCreated={vi.fn()} nameOnly />);
    await waitFor(() => expect(fetchProfileAgents).toHaveBeenCalled());
    await launch();
    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(1));
    expect(screen.queryByText(/Couldn't load the agent list/)).toBeNull();
  });
});

describe("SessionWizard Container runtime", () => {
  const down = (runtime: string, runtime_name: string) => () =>
    Promise.resolve({ available: false, runtime, runtime_name });
  it.each<[string, () => Promise<unknown>, string]>([
    ["Docker", down("docker", "Docker"), "Docker is not running"],
    ["Podman", down("podman", "Podman"), "Podman is not running"],
    ["Apple Container", down("apple_container", "Apple Container"), "Apple Container is not running"],
    // fetchDockerStatus never rejects: a failed request resolves to this fallback.
    [
      "a failed status request",
      () => Promise.resolve({ available: false, runtime: null, runtime_name: null }),
      "Container runtime is not running",
    ],
  ])("names %s when it isn't running, in the row and the launch error", async (_label, status, message) => {
    vi.mocked(fetchDockerStatus).mockImplementationOnce(status as never);
    renderWizard({ path: "/tmp/proj", tool: "claude", sandboxEnabled: true });
    await waitFor(() => expect(screen.getByTestId("wizard-sandbox-row").textContent).toContain(message));
    await launch();
    expect(await screen.findByText(`Sandbox unavailable: ${message}. Turn it off to launch.`)).toBeTruthy();
    expect(createSession).not.toHaveBeenCalled();
  });
});

describe("SessionWizard create failures", () => {
  const failure = (code: string, stage: string | null, message: string) => ({
    ok: false,
    error: message,
    failure: { code, stage, message, reference: "3f9c1a2b-77d0-4e0b-9b1f-0d3b5c2a11a1" },
  });
  const UNAVAILABLE = failure(
    "sandbox_unavailable",
    "starting_container",
    "Docker isn't running, so the Sandbox can't start. Start Docker, then try again.",
  );
  const box = () => screen.getByTestId("create-failure");
  // A failed case must not leave a queued Docker answer for the next one.
  afterEach(() => {
    vi.mocked(fetchDockerStatus)
      .mockReset()
      .mockResolvedValue({ available: false, runtime: "docker", runtime_name: "Docker" });
    vi.mocked(fetchCreateProgress).mockResolvedValue(null);
  });
  const sandboxed = () => {
    vi.mocked(fetchDockerStatus).mockResolvedValueOnce({ available: true, runtime: "docker", runtime_name: "Docker" });
    vi.mocked(fetchVolumeIgnoresPreview).mockResolvedValue(null);
    return renderWizard({ path: "/tmp/proj", tool: "claude", sandboxEnabled: true });
  };

  it("Retry asks the Container runtime again and, still down, makes Launch without Sandbox the way forward", async () => {
    createSession.mockResolvedValueOnce(UNAVAILABLE);
    sandboxed();
    await launch();
    await screen.findByTestId("create-failure");
    await waitFor(() => expect(document.activeElement).toBe(box()));
    expect(screen.getByTestId("wizard-sandbox-row").textContent).toContain("couldn't start");
    vi.mocked(fetchDockerStatus).mockResolvedValueOnce({ available: false, runtime: "docker", runtime_name: "Docker" });
    const checks = vi.mocked(fetchDockerStatus).mock.calls.length;

    fireEvent.click(screen.getByTestId("create-failure-retry_runtime"));
    await waitFor(() => expect(fetchDockerStatus).toHaveBeenCalledTimes(checks + 1));
    const actions = await waitFor(() => {
      const buttons = Array.from(box().querySelectorAll("button")).map((b) => b.textContent);
      expect(buttons[0]).toBe("Launch without Sandbox");
      return buttons;
    });
    expect(actions.slice(0, 2)).toEqual(["Launch without Sandbox", "Retry"]);
    expect(screen.getByTestId("wizard-sandbox-row").textContent).toContain("Docker is not running");
    expect(createSession).toHaveBeenCalledOnce();
  });

  it.each([
    ["on the first Retry", [true]],
    ["after a Retry that found it still down", [false, true]],
  ])("Retry resends under a new key once the Container runtime answers %s", async (_label, answers) => {
    createSession.mockResolvedValueOnce(UNAVAILABLE);
    sandboxed();
    await launch();
    for (const available of answers) {
      vi.mocked(fetchDockerStatus).mockResolvedValueOnce({ available, runtime: "docker", runtime_name: "Docker" });
      const checks = vi.mocked(fetchDockerStatus).mock.calls.length;
      fireEvent.click(await screen.findByTestId("create-failure-retry_runtime"));
      await waitFor(() => expect(fetchDockerStatus).toHaveBeenCalledTimes(checks + 1));
    }
    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(2));
    expect(payload(1).sandbox).toBe(payload(0).sandbox);
    expect(payload(1).idempotency_key).not.toBe(payload(0).idempotency_key);
  });

  it.each(["Launch", "close"])("while Retry re-checks the runtime, %s sends no second create", async (during) => {
    createSession.mockResolvedValueOnce(UNAVAILABLE);
    sandboxed();
    await launch();
    let answer!: (v: DockerStatusResponse) => void;
    vi.mocked(fetchDockerStatus).mockReturnValueOnce(new Promise((r) => (answer = r)));
    const checks = vi.mocked(fetchDockerStatus).mock.calls.length;
    fireEvent.click(await screen.findByTestId("create-failure-retry_runtime"));
    await waitFor(() => expect(fetchDockerStatus).toHaveBeenCalledTimes(checks + 1));

    if (during === "Launch") {
      const button = screen.getByText(/Launch session/).closest("button") as HTMLButtonElement;
      expect(button.disabled).toBe(true);
      fireEvent.click(button);
      fireEvent.keyDown(window, { key: "Enter", metaKey: true });
      fireEvent.keyDown(window, { key: "Enter", ctrlKey: true });
      await act(async () => answer({ available: true, runtime: "docker", runtime_name: "Docker" }));
      // Retry's own resend is the only one.
      await waitFor(() => expect(createSession).toHaveBeenCalledTimes(2));
      await act(async () => {});
      expect(createSession).toHaveBeenCalledTimes(2);
    } else {
      cleanup();
      await act(async () => answer({ available: true, runtime: "docker", runtime_name: "Docker" }));
      await act(async () => {});
      expect(createSession).toHaveBeenCalledOnce();
    }
  });

  it("turning the Sandbox off by hand takes down the box and the row's cue", async () => {
    createSession.mockResolvedValueOnce(UNAVAILABLE);
    sandboxed();
    await launch();
    await screen.findByTestId("create-failure");
    fireEvent.click(screen.getByRole("switch", { name: "Run in a safe container" }));
    expect(screen.queryByTestId("create-failure")).toBeNull();
    expect(screen.getByTestId("wizard-sandbox-row").textContent).not.toContain("couldn't start");
    expect(createSession).toHaveBeenCalledOnce();
  });

  it("Launch without Sandbox sends the Session on the host under a fresh key", async () => {
    createSession.mockResolvedValueOnce(
      failure("sandbox_image_pull_failed", "starting_container", "Couldn't pull the Sandbox image ghcr.io/x/y:1."),
    );
    sandboxed();
    await launch();
    expect((await screen.findByTestId("create-failure")).textContent).toContain("ghcr.io/x/y:1");
    fireEvent.click(screen.getByTestId("create-failure-launch_without_sandbox"));
    await waitFor(() => expect(createSession).toHaveBeenCalledTimes(2));
    expect(payload(0).sandbox).toBeTruthy();
    expect(payload(1).sandbox).toBeFalsy();
    expect(payload(1).idempotency_key).toBeTruthy();
    expect(payload(1).idempotency_key).not.toBe(payload(0).idempotency_key);
  });

  it("Launch without Sandbox turns the Sandbox off without sending when the host has no adapter for Structured view", async () => {
    vi.mocked(fetchProfileAgents).mockResolvedValueOnce([agent("claude", { acp_create_ready: false })]);
    createSession.mockResolvedValueOnce(
      failure("sandbox_image_pull_failed", "starting_container", "Couldn't pull the Sandbox image ghcr.io/x/y:1."),
    );
    sandboxed();
    await waitFor(() => expect(fetchProfileAgents).toHaveBeenCalled());
    await launch();
    await screen.findByTestId("create-failure");
    expect(payload(0).view).toBe("structured");
    fireEvent.click(screen.getByTestId("create-failure-launch_without_sandbox"));
    await act(async () => {});
    expect(createSession).toHaveBeenCalledOnce();
    expect(screen.queryByTestId("create-failure")).toBeNull();
    expect(screen.getByRole("switch", { name: "Run in a safe container" }).getAttribute("aria-checked")).toBe("false");
    expect(screen.getByTestId("wizard-view-row").textContent).toContain("ACP adapter not installed");
    await waitFor(() => expect(document.activeElement).toBe(screen.getByRole("radio", { name: "Terminal view" })));
  });

  it("a failed hook shows the message as sent and its last output, with no Sandbox action", async () => {
    let finish!: (r: unknown) => void;
    createSession.mockReturnValueOnce(new Promise((r) => (finish = r)));
    vi.mocked(fetchCreateProgress).mockResolvedValue({ stage: "running_hooks", hook: null, output: ["npm ERR! 404"] });
    sandboxed();
    await launch();
    await waitFor(() => expect(fetchCreateProgress).toHaveBeenCalled(), { timeout: 3000 });
    await act(async () => {});
    await act(async () => finish(failure("hook_failed", "running_hooks", "An on_create hook exited with code 1.")));
    const shown = await screen.findByTestId("create-failure");
    expect(shown.textContent).toContain("An on_create hook exited with code 1.");
    expect(screen.getByTestId("create-failure-output").textContent).toContain("npm ERR! 404");
    expect(screen.queryByTestId("create-failure-launch_without_sandbox")).toBeNull();
    expect(screen.queryByTestId("create-failure-retry_runtime")).toBeNull();
  });

  /** Backgrounds a create that then fails with `sent`, and returns what Reopen hands the next wizard.
   *  `whileSending` runs after Launch, before the progress view replaces the form. */
  const backgroundFailure = async (sent: ReturnType<typeof failure>, tool = "claude", whileSending?: () => void) => {
    const toastAction = vi.fn();
    toastBus.handler = { push: vi.fn(), error: vi.fn(), info: vi.fn(), openLink: vi.fn(), action: toastAction };
    let finish!: (r: unknown) => void;
    createSession.mockReturnValueOnce(new Promise((r) => (finish = r)));
    const onReopenFailure = vi.fn();
    render(
      <SessionWizard
        servedProfile="default"
        onClose={vi.fn()}
        onCreated={vi.fn()}
        onCreatedInBackground={vi.fn()}
        onReopenFailure={onReopenFailure}
        prefill={{ path: "/tmp/proj", tool }}
      />,
    );
    await launch();
    whileSending?.();
    fireEvent.click(await screen.findByText("Continue in background", undefined, { timeout: 3000 }));
    await act(async () => finish(sent));
    await waitFor(() => expect(toastAction).toHaveBeenCalledOnce());
    toastBus.handler = null;
    const [message, label, reopen] = toastAction.mock.calls[0]!;
    reopen();
    cleanup();
    return { message, label, restore: onReopenFailure.mock.calls[0]![0] };
  };

  it("a background failure toasts its title and reference, and Reopen restores the box", async () => {
    const sent = failure(
      "working_dir_missing",
      "preparing",
      "The Working directory doesn't exist where the Daemon runs.",
    );
    const { message, label, restore } = await backgroundFailure(sent);
    expect(message).toBe(
      "Session was not created: The Working directory is missing. Reference 3f9c1a2b… · in debug.log",
    );
    expect(label).toBe("Reopen");
    expect(restore.failure).toEqual(sent.failure);
    expect(restore.data.path).toBe("/tmp/proj");

    render(<SessionWizard servedProfile="default" onClose={vi.fn()} onCreated={vi.fn()} restore={restore} />);
    expect(box().textContent).toContain("The Working directory is missing");
    expect(screen.getByTestId("wizard-project-row").textContent).toContain("not found where the Daemon runs");
    fireEvent.click(screen.getByTestId("create-failure-choose_project"));
    expect(screen.getByText("Back")).toBeTruthy();
  });

  it("Reopen restores the form as sent, not an edit made while the create ran", async () => {
    const title = () => document.getElementById("wizard-title") as HTMLInputElement;
    const { restore } = await backgroundFailure(UNAVAILABLE, "claude", () => {
      fireEvent.change(title(), { target: { value: "edited while sending" } });
      expect(title().value).toBe("edited while sending");
    });
    expect(payload().title).toBeUndefined();
    expect(restore.data.title).toBe("");
  });

  // Try again sends the form as it stands, so it waits for the same loads as Launch.
  it.each(["a switched Profile's defaults", "a reopened custom Agent's metadata"])(
    "Try again waits for %s, then sends the resolved form",
    async (pending) => {
      const tryAgain = () => screen.getByTestId("create-failure-try_again") as HTMLButtonElement;
      const helper = agent("helper", { kind: "custom" });
      let release!: () => void;
      if (pending === "a switched Profile's defaults") {
        vi.mocked(fetchProfiles).mockResolvedValueOnce([
          { name: "default", is_default: true },
          { name: "work", is_default: false },
        ]);
        createSession.mockResolvedValueOnce(failure("create_failed", "preparing", "boom"));
        renderWizard();
        await launch();
        await screen.findByTestId("create-failure");
        let answer!: (v: never) => void;
        vi.mocked(fetchSettings).mockReturnValueOnce(new Promise((r) => (answer = r)));
        fireEvent.click(screen.getByText("Profile"));
        fireEvent.click(screen.getByRole("radio", { name: /work/ }));
        await waitFor(() => expect(fetchSettings).toHaveBeenCalledWith("work"));
        release = () => answer({} as never);
      } else {
        vi.mocked(fetchProfileAgents).mockResolvedValueOnce([helper]);
        const { restore } = await backgroundFailure(failure("create_failed", "preparing", "boom"), "helper");
        expect(payload(0).view).toBe("structured");
        let answer!: (v: AgentInfo[]) => void;
        vi.mocked(fetchProfileAgents).mockReturnValueOnce(new Promise((r) => (answer = r)));
        render(<SessionWizard servedProfile="default" onClose={vi.fn()} onCreated={vi.fn()} restore={restore} />);
        release = () => answer([helper]);
      }
      const sent = createSession.mock.calls.length;
      await waitFor(() => expect(tryAgain().disabled).toBe(true));
      fireEvent.click(tryAgain());
      await act(async () => {});
      expect(createSession).toHaveBeenCalledTimes(sent);

      await act(async () => release());
      await waitFor(() => expect(tryAgain().disabled).toBe(false));
      fireEvent.click(tryAgain());
      await waitFor(() => expect(createSession).toHaveBeenCalledTimes(sent + 1));
      expect(payload(sent)).toMatchObject(
        pending === "a switched Profile's defaults" ? { profile: "work" } : { tool: "helper", view: "structured" },
      );
    },
  );

  it.each([
    ["up", true, "Retry"],
    ["still down", false, "Launch without Sandbox"],
  ])(
    "Retry sends nothing when the form changed while it re-checked the runtime, which answered %s",
    async (_label, available, primary) => {
      createSession.mockResolvedValueOnce(UNAVAILABLE);
      sandboxed();
      await launch();
      let answer!: (v: DockerStatusResponse) => void;
      vi.mocked(fetchDockerStatus).mockReturnValueOnce(new Promise((r) => (answer = r)));
      fireEvent.click(await screen.findByTestId("create-failure-retry_runtime"));
      fireEvent.click(screen.getByRole("switch", { name: "Yolo mode" }));
      await act(async () => answer({ available, runtime: "docker", runtime_name: "Docker" }));
      await act(async () => {});
      expect(createSession).toHaveBeenCalledOnce();
      expect(box().querySelector("button")?.textContent).toBe(primary);
    },
  );

  it("a restored failure still says why its actions wait while the agent list fails", async () => {
    vi.mocked(fetchProfileAgents).mockResolvedValue(null);
    const restore = {
      data: { ...initialData, path: "/tmp/proj", tool: "claude" },
      failure: failure("create_failed", "preparing", "boom").failure,
    };
    render(<SessionWizard servedProfile="default" onClose={vi.fn()} onCreated={vi.fn()} restore={restore} />);
    expect(box().textContent).toContain("boom");
    expect(await screen.findByText(/Couldn't load the agent list/)).toBeTruthy();
  });

  it("a runtime check that answers late can't overwrite Retry's newer one", async () => {
    let mount!: (v: DockerStatusResponse) => void;
    vi.mocked(fetchDockerStatus)
      .mockReturnValueOnce(new Promise((r) => (mount = r)))
      .mockResolvedValueOnce({ available: true, runtime: "docker", runtime_name: "Docker" });
    vi.mocked(fetchVolumeIgnoresPreview).mockResolvedValue(null);
    createSession.mockResolvedValueOnce(UNAVAILABLE);
    const restore = {
      data: { ...initialData, path: "/tmp/proj", tool: "claude", sandboxEnabled: true },
      failure: UNAVAILABLE.failure,
    };
    render(<SessionWizard servedProfile="default" onClose={vi.fn()} onCreated={vi.fn()} restore={restore} />);
    fireEvent.click(await screen.findByTestId("create-failure-retry_runtime"));
    await waitFor(() => expect(createSession).toHaveBeenCalledOnce());
    await act(async () => mount({ available: false, runtime: "docker", runtime_name: "Docker" }));
    expect(screen.getByTestId("wizard-sandbox-row").textContent).not.toContain("is not running");
  });

  it("a Retry that finds the runtime back up offers Retry again even when the form changed", async () => {
    createSession.mockResolvedValueOnce(UNAVAILABLE);
    sandboxed();
    await launch();
    vi.mocked(fetchDockerStatus).mockResolvedValueOnce({ available: false, runtime: "docker", runtime_name: "Docker" });
    fireEvent.click(await screen.findByTestId("create-failure-retry_runtime"));
    await waitFor(() => expect(box().querySelector("button")?.textContent).toBe("Launch without Sandbox"));
    let answer!: (v: DockerStatusResponse) => void;
    vi.mocked(fetchDockerStatus).mockReturnValueOnce(new Promise((r) => (answer = r)));
    fireEvent.click(screen.getByTestId("create-failure-retry_runtime"));
    fireEvent.click(screen.getByRole("switch", { name: "Yolo mode" }));
    await act(async () => answer({ available: true, runtime: "docker", runtime_name: "Docker" }));
    await act(async () => {});
    expect(createSession).toHaveBeenCalledOnce();
    expect(box().querySelector("button")?.textContent).toBe("Retry");
  });
});

describe("SessionWizard View row", () => {
  const radio = (name: string) => screen.getByRole("radio", { name }) as HTMLButtonElement;
  const selected = () => (radio("Structured view").getAttribute("aria-checked") === "true" ? "structured" : "terminal");
  const pickAgent = (name: string) => {
    fireEvent.click(screen.getByTestId("wizard-agent-row"));
    fireEvent.click(screen.getByRole("button", { name: new RegExp(`^${name}`) }));
    fireEvent.click(screen.getByRole("button", { name: "Done" }));
  };
  const AGENTS = [agent("claude"), agent("aider", { acp_capable: false })];

  it.each([
    ["Structured view", "structured"],
    ["Terminal view", "terminal"],
  ])("gives %s back after an agent that can't run Structured view", async (choice, expected) => {
    vi.mocked(fetchProfileAgents).mockResolvedValue(AGENTS);
    renderWizard();
    await waitFor(() => expect(radio("Structured view").disabled).toBe(false));
    fireEvent.click(radio(choice));
    pickAgent("aider");
    await waitFor(() => expect(radio("Structured view").disabled).toBe(true));
    expect(selected()).toBe("terminal");
    pickAgent("claude");
    await waitFor(() => expect(radio("Structured view").disabled).toBe(false));
    expect(selected()).toBe(expected);
  });

  it.each([
    ["clicking Terminal view", () => fireEvent.click(radio("Terminal view"))],
    ["an arrow key", () => fireEvent.keyDown(radio("Terminal view"), { key: "ArrowLeft" })],
  ])("re-picking the forced Terminal view by %s keeps Structured view for later", async (_label, repick) => {
    vi.mocked(fetchProfileAgents).mockResolvedValue(AGENTS);
    renderWizard();
    await waitFor(() => expect(selected()).toBe("structured"));
    pickAgent("aider");
    await waitFor(() => expect(radio("Structured view").disabled).toBe(true));
    repick();
    pickAgent("claude");
    await waitFor(() => expect(radio("Structured view").disabled).toBe(false));
    expect(selected()).toBe("structured");
  });

  it("arrow keys move the selection and skip a disabled option", async () => {
    vi.mocked(fetchProfileAgents).mockResolvedValue(AGENTS);
    renderWizard();
    await waitFor(() => expect(selected()).toBe("structured"));
    expect(radio("Structured view").tabIndex).toBe(0);
    expect(radio("Terminal view").tabIndex).toBe(-1);
    radio("Structured view").focus();
    fireEvent.keyDown(radio("Structured view"), { key: "ArrowRight" });
    expect(selected()).toBe("terminal");
    expect(document.activeElement).toBe(radio("Terminal view"));
    fireEvent.keyDown(radio("Terminal view"), { key: "ArrowLeft" });
    expect(selected()).toBe("structured");

    pickAgent("aider");
    await waitFor(() => expect(radio("Structured view").disabled).toBe(true));
    fireEvent.keyDown(radio("Terminal view"), { key: "ArrowLeft" });
    expect(selected()).toBe("terminal");
  });
});
