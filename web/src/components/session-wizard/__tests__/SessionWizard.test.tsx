// @vitest-environment jsdom

import { StrictMode, useState } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { SessionWizard, type WizardPrefill } from "../SessionWizard";
import {
  fetchAgents,
  fetchCreateProgress,
  fetchDockerStatus,
  fetchGroups,
  fetchIsGitRepo,
  fetchProfiles,
  fetchProjects,
  fetchSettings,
  type SettingsResponse,
} from "../../../lib/api";
import { agent } from "./fixtures";
import { toastBus } from "../../../lib/toastBus";
import { startPendingCreates } from "../../../lib/pendingCreates";

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
  fetchAgents: vi.fn().mockResolvedValue([]),
  fetchIsGitRepo: vi.fn().mockResolvedValue(true),
  fetchGroups: vi.fn().mockResolvedValue([]),
  fetchDockerStatus: vi.fn().mockResolvedValue({ available: false }),
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
      vi.mocked(fetchAgents).mockResolvedValueOnce([agent("claude", { acp_installed: true })]);
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
    ["terminal", "terminal"],
    ["auto", "structured"],
    ["structured", "structured"],
  ])("opens on and sends the configured default view %s (#3517)", async (setting, view) => {
    vi.mocked(fetchSettings).mockResolvedValueOnce({ acp: { default_new_session_view: setting } } as never);
    renderWizard();
    await launch();
    await waitFor(() => expect(createSession).toHaveBeenCalled());
    expect(payload()).toMatchObject({ tool: "claude", view });
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
    vi.mocked(fetchAgents).mockResolvedValueOnce([agent("settl", { host_only: true })]);
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

  it.each([
    ["claude", undefined, false, /plan, tool calls and diffs as cards/],
    ["aider", [agent("aider", { acp_capable: false })], true, /no ACP adapter/],
    ["helper", [agent("helper", { kind: "custom", acp_capable: false })], true, /needs agent_acp_cmd/],
    ["claude", [agent("claude", { acp_allowed: false })], true, /not on the allowed agents list/],
  ])("gates the structured view for %s", async (tool, agents, disabled, summary) => {
    if (agents) vi.mocked(fetchAgents).mockResolvedValueOnce(agents);
    renderWizard({ path: "/tmp/proj", tool });
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
    if (agents) vi.mocked(fetchAgents).mockResolvedValueOnce(agents);
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
      const terminal = { acp: { default_new_session_view: "terminal" } } as never;
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

    it("resets a view set before the profile was picked", async () => {
      vi.mocked(fetchProfiles).mockResolvedValue(PROFILES);
      const confirmSpy = vi.spyOn(window, "confirm").mockReturnValue(true);
      try {
        vi.mocked(fetchSettings)
          .mockResolvedValueOnce({} as never)
          .mockResolvedValueOnce({ acp: { default_new_session_view: "terminal" } } as never);
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
        fireEvent.click(await screen.findByRole("switch", { name: "Auto-approve actions" }));
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
    acp: { default_new_session_view: "terminal" },
  });
  const open = (label: string) => fireEvent.click(screen.getByText(label).closest("button")!);
  const chooseProfile = async (profile: string) => {
    open("Profile");
    fireEvent.click(screen.getByRole("radio", { name: profile === "" ? /Server default/ : new RegExp(profile) }));
    await waitFor(() =>
      expect(screen.getByRole("switch", { name: "Auto-approve actions" }).getAttribute("aria-checked")).toBe(
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
    expect(screen.getByRole("switch", { name: "Auto-approve actions" }).getAttribute("aria-checked")).toBe("true");
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
    fireEvent.click(screen.getByRole("switch", { name: "Auto-approve actions" }));
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
    let answer!: (v: { available: boolean; runtime: string | null }) => void;
    vi.mocked(fetchDockerStatus).mockReturnValueOnce(new Promise((r) => (answer = r)));
    return (available: boolean) => answer({ available, runtime: null });
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
    vi.mocked(fetchAgents).mockResolvedValue(AGENTS);
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
    vi.mocked(fetchAgents).mockResolvedValue(AGENTS);
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
    vi.mocked(fetchAgents).mockResolvedValue(AGENTS);
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
