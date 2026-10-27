// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, waitFor, within } from "@testing-library/react";

import type { IncompatibleAgentDetail } from "../../../lib/acpTypes";
import type { AgentInstallInfo } from "../../../lib/api";
import { InstallAgentError } from "../../../lib/api";
import { StartupErrorScreen } from "../StartupErrorScreen";

const fetchAgentInstallInfo = vi.fn();
const installAcpAgent = vi.fn();
const writeClipboard = vi.fn().mockResolvedValue(true);
vi.mock("../../../lib/clipboard", () => ({
  writeClipboard: (text: string) => writeClipboard(text),
}));
vi.mock("../../../lib/api", async (importOriginal) => ({
  InstallAgentError: (await importOriginal<typeof import("../../../lib/api")>()).InstallAgentError,
  fetchAgentInstallInfo: (...args: unknown[]) => fetchAgentInstallInfo(...args),
  installAcpAgent: (...args: unknown[]) => installAcpAgent(...args),
}));

const CLAUDE_NPM = "npm install -g @agentclientprotocol/claude-agent-acp@latest";
const IN_CONTAINER = "podman exec aoe-sandbox-s1 npm install -g @agentclientprotocol/claude-agent-acp@latest";
// The catalogue hint, with the note that isn't part of the command.
const OPENCODE = "curl -fsSL https://opencode.ai/install | bash  (then `opencode acp`)";
const OPENCODE_COMMAND = "curl -fsSL https://opencode.ai/install | bash";

const incompatible = (auto_install = true, install_command = CLAUDE_NPM): IncompatibleAgentDetail => ({
  kind: "incompatible_agent_version",
  package_name: "@agentclientprotocol/claude-agent-acp",
  installed: "0.55.0",
  required: "0.82.0",
  install_command,
  auto_install,
});

const info = (over: Partial<AgentInstallInfo> = {}): AgentInstallInfo => ({
  agent: "claude",
  display_name: "Claude",
  install_command: CLAUDE_NPM,
  npm_installable: true,
  install_allowed: false,
  install_blocked: "setting_off",
  sandboxed: false,
  container_install_command: null,
  ...over,
});
const allowed = { install_allowed: true, install_blocked: null } as const;
const sandbox = { sandboxed: true, container_install_command: IN_CONTAINER } as const;
const opencode = info({
  agent: "opencode",
  display_name: "OpenCode",
  install_command: OPENCODE,
  npm_installable: false,
  ...allowed,
});

const installOk = (recovered_sessions = 0) => ({
  session_id: "s1",
  package: "@agentclientprotocol/claude-agent-acp@latest",
  success: true,
  exit_code: 0,
  stdout: "added 1 package",
  stderr: "",
  recovered_sessions,
});
const spawnCall = () =>
  expect(fetch).toHaveBeenCalledWith("/api/sessions/s1/acp/spawn", expect.objectContaining({ method: "POST" }));

/** Renders once install-info has answered. */
/** Text sits only on colours the theme contrast test holds to AA on this card. */
function expectValidatedText(screen: HTMLElement) {
  expect(screen.querySelectorAll(".text-text-secondary, .text-text-dim, .text-status-error")).toHaveLength(0);
  // Status green is no text colour; it only tints the decorative step indicator.
  expect(screen.querySelectorAll(".text-status-running:not([aria-hidden])")).toHaveLength(0);
}

async function renderScreen(detail: IncompatibleAgentDetail, answer: AgentInstallInfo | null = info()) {
  fetchAgentInstallInfo.mockResolvedValue(answer);
  const view = render(<StartupErrorScreen detail={detail} sessionId="s1" />);
  await waitFor(() => expect(fetchAgentInstallInfo).toHaveBeenCalledWith("s1"));
  if (answer) await waitFor(() => expect(view.getByRole("heading").textContent).toContain(answer.display_name));
  return view;
}

const buttons = (root: HTMLElement) =>
  within(root)
    .queryAllByRole("button")
    .map((b) => b.textContent)
    .filter((t) => t !== "Details" && t !== "Copy");

const stepTitles = (getByTestId: (id: string) => HTMLElement) =>
  [1, 2].map((n) => getByTestId(`startup-error-step-${n}`).querySelector("div > div")?.textContent ?? "");

/** A step names only buttons that render. */
function expectStepsNameRenderedButtons(screen: HTMLElement, titles: string[]) {
  for (const title of titles) {
    const named = /^Click (.+)$/.exec(title)?.[1];
    if (named) expect(buttons(screen), title).toContain(named);
  }
  for (const m of (screen.textContent ?? "").matchAll(/then (Check again|Update[^.:]*)/g)) {
    expect(buttons(screen), m[0]).toContain(m[1]);
  }
}

beforeEach(() => {
  fetchAgentInstallInfo.mockReset();
  installAcpAgent.mockReset();
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: true, text: () => Promise.resolve("") }));
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("StartupErrorScreen", () => {
  it.each<[string, IncompatibleAgentDetail, AgentInstallInfo, string, string]>([
    [
      "out of date",
      incompatible(),
      info(),
      "The Claude ACP adapter needs an update",
      "@agentclientprotocol/claude-agent-acp 0.55.0 installed · needs 0.82.0 or newer",
    ],
    [
      "OpenCode",
      { ...incompatible(false, OPENCODE), package_name: "opencode-acp" },
      opencode,
      "The OpenCode ACP adapter needs an update",
      "opencode-acp 0.55.0 installed · needs 0.82.0 or newer",
    ],
    [
      "no version",
      { kind: "missing_agent_info", expected_package: "pkg", install_command: CLAUDE_NPM, auto_install: true },
      info(),
      "The Claude ACP adapter didn't report its version",
      "pkg · no version reported",
    ],
    [
      "wrong package",
      {
        kind: "mismatched_agent_name",
        expected: "pkg",
        received: "wrapper",
        install_command: CLAUDE_NPM,
        auto_install: true,
      },
      info(),
      "The Claude ACP adapter isn't the package aoe expects",
      "wrapper reported · expected pkg",
    ],
    [
      "bad version",
      {
        kind: "unparseable_agent_version",
        package_name: "pkg",
        raw_version: "not-semver",
        required: "0.82.0",
        install_command: CLAUDE_NPM,
        auto_install: true,
      },
      info(),
      "The Claude ACP adapter reported a version aoe can't read",
      'pkg "not-semver" · needs 0.82.0 or newer',
    ],
    [
      "protocol",
      { kind: "unsupported_protocol_version", expected: "V1", received: "V2" },
      info(),
      "The Claude ACP adapter speaks an ACP protocol aoe doesn't support",
      "protocol V2 · needs V1",
    ],
  ])("%s: names the Agent and what is wrong", async (_label, detail, answer, headline, version) => {
    const { getByRole, getByTestId } = await renderScreen(detail, answer);
    expect(getByRole("heading").textContent).toBe(headline);
    expect(getByTestId("startup-error-version").textContent).toBe(version);
    expect(getByTestId("startup-error-screen").getAttribute("role")).toBe("alert");
  });

  it("copies only the command from a catalogue hint and shows its note beside it", async () => {
    const { getByTestId, getByRole } = await renderScreen(incompatible(false, OPENCODE), opencode);
    const block = getByTestId("startup-error-install-command").parentElement!.parentElement!;
    expect(block.textContent).toContain("then `opencode acp`");
    fireEvent.click(getByRole("button", { name: "Copy" }));
    await waitFor(() => expect(writeClipboard).toHaveBeenCalledWith(OPENCODE_COMMAND));
  });

  it("names no Agent until install-info answers", () => {
    fetchAgentInstallInfo.mockReturnValue(new Promise(() => {}));
    const { getByRole } = render(<StartupErrorScreen detail={incompatible()} sessionId="s1" />);
    expect(getByRole("heading").textContent).toBe("The ACP adapter needs an update");
  });

  it.each<{
    label: string;
    detail: IncompatibleAgentDetail;
    answer: AgentInstallInfo;
    eyebrow: string;
    steps: string[];
    stepOneDone?: boolean;
    buttons: string[];
    /** The primary button is disabled and described by step 1. */
    locked?: boolean;
    command: string | null;
    text?: string;
  }>([
    {
      label: "host, installs off",
      detail: incompatible(),
      answer: info(),
      eyebrow: "Adapter out of date",
      steps: ["Allow installs from the Web dashboard", "Click Update & restart"],
      stepOneDone: false,
      buttons: ["Update & restart", "Check again"],
      locked: true,
      command: CLAUDE_NPM,
    },
    {
      label: "host, installs allowed",
      detail: incompatible(),
      answer: info(allowed),
      eyebrow: "Adapter out of date",
      steps: ["Allow installs from the Web dashboardDone", "Click Update & restart"],
      stepOneDone: true,
      buttons: ["Update & restart", "Check again"],
      command: CLAUDE_NPM,
    },
    {
      label: "Sandbox, installs off",
      detail: incompatible(),
      answer: info(sandbox),
      eyebrow: "Adapter out of date",
      steps: ["Allow installs from the Web dashboard", "Click Update in Sandbox & restart"],
      stepOneDone: false,
      buttons: ["Update in Sandbox & restart", "Check again"],
      locked: true,
      command: IN_CONTAINER,
    },
    {
      label: "Sandbox, installs allowed",
      detail: incompatible(),
      answer: info({ ...sandbox, ...allowed }),
      eyebrow: "Adapter out of date",
      steps: ["Allow installs from the Web dashboardDone", "Click Update in Sandbox & restart"],
      stepOneDone: true,
      buttons: ["Update in Sandbox & restart", "Check again"],
      command: IN_CONTAINER,
    },
    {
      label: "host, adapter npm can't install",
      detail: incompatible(false, OPENCODE),
      answer: opencode,
      eyebrow: "Adapter out of date",
      steps: ["Run this where the Daemon runs", "Click Check again"],
      buttons: ["Check again"],
      command: OPENCODE_COMMAND,
    },
    {
      label: "Sandbox, adapter npm can't install",
      detail: incompatible(false, OPENCODE),
      answer: { ...opencode, sandboxed: true },
      eyebrow: "Adapter out of date",
      steps: ["Run this inside this Session's container", "Click Check again"],
      buttons: ["Check again"],
      command: OPENCODE_COMMAND,
    },
    {
      label: "read-only",
      detail: incompatible(),
      answer: info({ install_blocked: "read_only" }),
      eyebrow: "Adapter out of date",
      steps: ["Run this where the Daemon runs", "Restart the agent from the TUI"],
      buttons: [],
      command: CLAUDE_NPM,
      text: "This dashboard is read-only",
    },
    {
      label: "CityHall, Sandbox",
      detail: incompatible(),
      answer: info({ ...sandbox, install_blocked: "cityhall" }),
      eyebrow: "Adapter out of date",
      steps: ["Run this where the Daemon runs", "Restart the agent from the TUI"],
      buttons: [],
      command: IN_CONTAINER,
      text: "A CityHall dashboard can't restart the agent.",
    },
    {
      label: "unsupported protocol",
      detail: { kind: "unsupported_protocol_version", expected: "V1", received: "V2" },
      answer: info(allowed),
      eyebrow: "Adapter not supported",
      steps: ["Install an adapter that speaks ACP protocol V1", "Click Check again"],
      buttons: ["Check again"],
      command: null,
    },
  ])("$label: the steps and buttons follow the state", async (c) => {
    const { getByTestId, queryByTestId, getByText } = await renderScreen(c.detail, c.answer);
    const screen = getByTestId("startup-error-screen");
    getByText(c.eyebrow);
    const titles = stepTitles(getByTestId);
    expect(titles).toEqual(c.steps);
    expectStepsNameRenderedButtons(screen, titles);
    expect(buttons(screen)).toEqual(c.buttons);
    if (c.stepOneDone !== undefined) {
      expect(getByTestId("startup-error-step-1").hasAttribute("data-done")).toBe(c.stepOneDone);
    }
    const update = queryByTestId("startup-error-update-restart") as HTMLButtonElement | null;
    if (update) {
      expect(update.disabled).toBe(!!c.locked);
      const describedBy = update.getAttribute("aria-describedby");
      expect(describedBy ? document.getElementById(describedBy) : null).toBe(
        c.locked ? getByTestId("startup-error-step-1") : null,
      );
    }
    expect(queryByTestId("startup-error-install-command")?.textContent ?? null).toBe(c.command);
    if (c.locked) {
      expect(getByTestId("startup-error-step-1").textContent).toContain(
        'Turn on TUI › Settings › Global › Acp › Advanced › "Allow agent install from web". The install runs npm install as the Daemon\'s user.',
      );
    }
    if (c.text) expect(screen.textContent).toContain(c.text);
    for (const banned of ["Docker", "terminal app", "helper", "paused", "from the browser"]) {
      expect(screen.textContent).not.toContain(banned);
    }
  });

  it.each([
    ["focus", () => window.dispatchEvent(new Event("focus"))],
    [
      "visibilitychange",
      () => {
        Object.defineProperty(document, "visibilityState", { configurable: true, value: "visible" });
        document.dispatchEvent(new Event("visibilitychange"));
      },
    ],
  ])("re-reads install-info on %s, so a setting turned on in the TUI unlocks Update", async (_label, fire) => {
    const { getByTestId } = await renderScreen(incompatible(), info());
    expect((getByTestId("startup-error-update-restart") as HTMLButtonElement).disabled).toBe(true);
    fetchAgentInstallInfo.mockResolvedValue(info(allowed));
    act(fire);
    await waitFor(() =>
      expect((getByTestId("startup-error-update-restart") as HTMLButtonElement).disabled).toBe(false),
    );
    expect(getByTestId("startup-error-step-1").hasAttribute("data-done")).toBe(true);
    expect(fetchAgentInstallInfo).toHaveBeenCalledTimes(2);
  });

  it("an older install-info read that lands last does not overwrite the newer one", async () => {
    const { getByTestId } = await renderScreen(incompatible(), info());
    let resolveOlder: (v: AgentInstallInfo) => void = () => {};
    let resolveNewer: (v: AgentInstallInfo) => void = () => {};
    fetchAgentInstallInfo
      .mockReturnValueOnce(new Promise((r) => (resolveOlder = r)))
      .mockReturnValueOnce(new Promise((r) => (resolveNewer = r)));
    act(() => {
      window.dispatchEvent(new Event("focus"));
      window.dispatchEvent(new Event("focus"));
    });
    expect(fetchAgentInstallInfo).toHaveBeenCalledTimes(3);
    const update = getByTestId("startup-error-update-restart") as HTMLButtonElement;
    await act(async () => resolveNewer(info(allowed)));
    expect(update.disabled).toBe(false);
    await act(async () => resolveOlder(info()));
    expect(update.disabled).toBe(false);
  });

  it.each([
    ["ok", { ok: true, status: 200, json: () => Promise.resolve({}) }, "Checking again."],
    ["failed", { ok: false, status: 500, text: () => Promise.resolve("boom") }, "Check again failed"],
  ])("Check again POSTs to /acp/spawn and reports the outcome: %s", async (_label, response, line) => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(response));
    const { getByTestId } = await renderScreen(incompatible());
    fireEvent.click(getByTestId("startup-error-restart"));
    spawnCall();
    const screen = getByTestId("startup-error-screen");
    await waitFor(() => expect(screen.textContent).toContain(line));
    expectValidatedText(screen);
  });

  it("installing: the primary button shows progress and every action waits", async () => {
    installAcpAgent.mockReturnValue(new Promise(() => {}));
    const { getByTestId } = await renderScreen(incompatible(), info(allowed));
    fireEvent.click(getByTestId("startup-error-update-restart"));
    await waitFor(() => expect(getByTestId("startup-error-update-restart").textContent).toBe("Updating…"));
    expect((getByTestId("startup-error-update-restart") as HTMLButtonElement).disabled).toBe(true);
    expect((getByTestId("startup-error-restart") as HTMLButtonElement).disabled).toBe(true);
  });

  it("a successful install respawns and reports Sessions queued for recovery", async () => {
    installAcpAgent.mockResolvedValue(installOk(3));
    const { getByTestId, container } = await renderScreen(incompatible(), info(allowed));
    fireEvent.click(getByTestId("startup-error-update-restart"));
    expect(installAcpAgent).toHaveBeenCalledWith("s1");
    await waitFor(spawnCall);
    await waitFor(() => expect(container.textContent).toContain("3 other Sessions"));
  });

  it("install failed: the exit code, npm output and command stay outside Details", async () => {
    installAcpAgent.mockResolvedValue({ ...installOk(), success: false, exit_code: 243, stderr: "npm ERR! EACCES" });
    const { getByTestId, findByTestId, queryByTestId } = await renderScreen(
      incompatible(),
      info({ ...sandbox, ...allowed }),
    );
    fireEvent.click(getByTestId("startup-error-update-restart"));
    expect((await findByTestId("adapter-install-error")).textContent).toBe("Install exited with code 243.");
    expect(getByTestId("startup-error-screen").textContent).toContain("npm ERR! EACCES");
    expect(getByTestId("startup-error-install-command").textContent).toBe(IN_CONTAINER);
    expect(queryByTestId("startup-error-details-body")).toBeNull();
    expect(fetch).not.toHaveBeenCalled();
  });

  it("container not running: its own steps, then Update returns once the container runs", async () => {
    installAcpAgent.mockRejectedValue(new InstallAgentError("container is not running", "container_not_running"));
    const view = await renderScreen(incompatible(), info({ ...sandbox, ...allowed }));
    const { getByTestId, findByTestId, queryByTestId } = view;
    fireEvent.click(getByTestId("startup-error-update-restart"));
    expect((await findByTestId("startup-error-container-down")).textContent).toContain(
      "This Session's container isn't running",
    );
    const screen = getByTestId("startup-error-screen");
    expect(buttons(screen)).toEqual(["Start the container"]);
    expect(stepTitles(getByTestId)).toEqual(["Click Start the container", "Then update the adapter"]);
    expectStepsNameRenderedButtons(screen, stepTitles(getByTestId));
    expect(queryByTestId("adapter-install-error")).toBeNull();

    fireEvent.click(getByTestId("startup-error-start-container"));
    spawnCall();
    // The started container runs the adapter, which fails the version check again.
    view.rerender(<StartupErrorScreen detail={incompatible()} sessionId="s1" />);
    await waitFor(() => expect(buttons(screen)).toEqual(["Update in Sandbox & restart", "Check again"]));
    expect(queryByTestId("startup-error-container-down")).toBeNull();
    expect(queryByTestId("startup-error-fails-again")).toBeNull();
    expect(queryByTestId("adapter-install-error")).toBeNull();
  });

  it("a stopped container reported by install-info gets its steps before any click", async () => {
    const stopped = info({ ...sandbox, install_allowed: false, install_blocked: "container_not_running" });
    const view = await renderScreen(incompatible(), stopped);
    const { getByTestId, queryByTestId } = view;
    const screen = getByTestId("startup-error-screen");
    expect(getByTestId("startup-error-container-down").textContent).toContain("This Session's container isn't running");
    expect(buttons(screen)).toEqual(["Start the container"]);
    expect(stepTitles(getByTestId)).toEqual(["Click Start the container", "Then update the adapter"]);

    fireEvent.click(getByTestId("startup-error-start-container"));
    spawnCall();
    fetchAgentInstallInfo.mockResolvedValue(info({ ...sandbox, ...allowed }));
    view.rerender(<StartupErrorScreen detail={incompatible()} sessionId="s1" />);
    await waitFor(() => expect(buttons(screen)).toEqual(["Update in Sandbox & restart", "Check again"]));
    expect(queryByTestId("startup-error-container-down")).toBeNull();
    expect(getByTestId("startup-error-step-1").getAttribute("data-done")).toBe("true");
  });

  it("a stopped container leaves steps the Web dashboard can't run as they are", async () => {
    const stopped = { ...sandbox, install_allowed: false, install_blocked: "container_not_running" } as const;
    const cases: [string, IncompatibleAgentDetail, AgentInstallInfo, string][] = [
      [
        "manual-only",
        incompatible(false, OPENCODE),
        { ...opencode, ...stopped, container_install_command: null },
        "Run this inside this Session's container",
      ],
      [
        "protocol",
        { kind: "unsupported_protocol_version", expected: "V1", received: "V2" },
        info(stopped),
        "Install an adapter that speaks ACP protocol V1",
      ],
    ];
    for (const [label, detail, answer, stepOne] of cases) {
      const { getByTestId, queryByTestId } = await renderScreen(detail, answer);
      const screen = getByTestId("startup-error-screen");
      expect(queryByTestId("startup-error-container-down"), label).toBeNull();
      expect(buttons(screen), label).toEqual(["Check again"]);
      const titles = stepTitles(getByTestId);
      expect(titles, label).toEqual([stepOne, "Click Check again"]);
      expectStepsNameRenderedButtons(screen, titles);
      if (label === "manual-only") {
        expect(getByTestId("startup-error-install-command").textContent).toContain(OPENCODE_COMMAND);
      }
      cleanup();
    }
  });

  it("fails again: a version failure after the install says it changed nothing", async () => {
    installAcpAgent.mockResolvedValue(installOk());
    const view = await renderScreen(incompatible(), info({ ...sandbox, ...allowed }));
    fireEvent.click(view.getByTestId("startup-error-update-restart"));
    await waitFor(spawnCall);
    expect(view.queryByTestId("startup-error-fails-again")).toBeNull();
    view.rerender(<StartupErrorScreen detail={incompatible()} sessionId="s1" />);
    expect(view.getByTestId("startup-error-fails-again").textContent).toContain(
      "the install didn't change the version it reports",
    );
    expect(view.getByTestId("startup-error-install-command").textContent).toBe(IN_CONTAINER);
  });

  it.each([
    [false, "Host, where the Daemon runs"],
    [true, "This Session's container"],
  ])("Details (sandboxed=%s) starts closed, then holds the rationale and the Where row", async (sandboxed, where) => {
    const { getByTestId, queryByTestId } = await renderScreen(incompatible(), info(sandboxed ? sandbox : {}));
    const toggle = getByTestId("startup-error-details");
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    expect(queryByTestId("startup-error-details-body")).toBeNull();
    fireEvent.click(toggle);
    const body = getByTestId("startup-error-details-body").textContent ?? "";
    expect(body).toContain("memory_recall");
    expect(body).toContain(`Where${where}`);
    expect(body).toContain("initialize handshake");
    expect(body.includes("fixes new Sessions only")).toBe(sandboxed);
  });

  it.each<[string, IncompatibleAgentDetail, AgentInstallInfo]>([
    ["install off", incompatible(), info()],
    ["installs allowed", incompatible(), info(allowed)],
    ["sandboxed", incompatible(), info({ ...sandbox, ...allowed })],
    ["not npm-installable", { ...incompatible(false, OPENCODE), package_name: "opencode-acp" }, opencode],
    ["protocol", { kind: "unsupported_protocol_version", expected: "V1", received: "V2" }, info()],
  ])("keeps every line on a validated text colour: %s", async (_label, detail, answer) => {
    const { getByTestId } = await renderScreen(detail, answer);
    fireEvent.click(getByTestId("startup-error-details"));
    const screen = getByTestId("startup-error-screen");
    expectValidatedText(screen);
    expect(screen.querySelectorAll(".text-text-subtle").length).toBeGreaterThan(0);
  });
});
