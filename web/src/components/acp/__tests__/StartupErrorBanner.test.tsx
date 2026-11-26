// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, waitFor, within } from "@testing-library/react";

import type { AgentInstallInfo } from "../../../lib/api";
import type { AcpWorkerState } from "../../../lib/types";
import { applyEvent, emptyAcpState } from "../../../lib/acpTypes";
import { SessionBanners } from "../SessionBanners";
import { StartupErrorBanner } from "../StartupErrorBanner";

const writeClipboard = vi.fn();
vi.mock("../../../lib/clipboard", () => ({
  writeClipboard: (text: string) => writeClipboard(text),
}));

const NATIVE_BINARY_MSG =
  'agent spawn failed: ACP connection failed: Internal error: { "details": "Claude Code native binary at /usr/lib/node_modules/.../claude exists but failed to launch." }';
const SPAWN_MSG =
  'Failed to start structured view agent "claude": acp client error: agent spawn failed: runner closed before Hello';
const SPAWN_PREFIX = 'Failed to start structured view agent "claude": acp client error: agent spawn failed: ';
const CLAUDE_NPM = "npm install -g @agentclientprotocol/claude-agent-acp@latest";

const claude = (over: Partial<AgentInstallInfo> = {}): AgentInstallInfo => ({
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
const opencode = claude({
  agent: "opencode",
  display_name: "OpenCode",
  install_command: "curl -fsSL https://opencode.ai/install | bash  (then `opencode acp`)",
  npm_installable: false,
  install_allowed: true,
});

type Reply = { status: number; body: unknown };
const json = (body: unknown, status = 200): Reply => ({ status, body });

/** Answers each endpoint the banner calls; unlisted ones 404. */
function stubApi(routes: { info?: Reply; spawn?: Reply; restart?: Reply; install?: Reply; log?: Reply }) {
  const spy = vi.fn(async (url: string, _init?: RequestInit) => {
    const reply =
      (url.includes("/acp/install-info") && routes.info) ||
      (url.includes("/acp/spawn") && routes.spawn) ||
      (url.includes("/acp/restart") && routes.restart) ||
      (url.includes("/acp/install-agent") && routes.install) ||
      (url.includes("/acp/worker-log") && routes.log) ||
      json("not found", 404);
    const text = typeof reply.body === "string" ? reply.body : JSON.stringify(reply.body);
    return new Response(text, { status: reply.status });
  });
  vi.stubGlobal("fetch", spy);
  return spy;
}
const calls = (spy: ReturnType<typeof stubApi>, path: string) =>
  spy.mock.calls.filter(([url]) => String(url).includes(path)).length;

function renderBanner({
  message = SPAWN_MSG,
  onSwitchToTerminal = vi.fn(),
  switchable = true,
  acpWorkerState = "absent",
  sentStopped = false,
  retriedError = null,
  restartEpoch = 0,
  onRetryStarted,
  onRetryFailed,
}: {
  message?: string;
  onSwitchToTerminal?: () => void;
  switchable?: boolean;
  acpWorkerState?: AcpWorkerState;
  sentStopped?: boolean;
  retriedError?: string | null;
  restartEpoch?: number;
  onRetryStarted?: (epoch: number, message: string) => void;
  onRetryFailed?: (epoch: number, detail: string) => void;
} = {}) {
  return render(
    <StartupErrorBanner
      sessionId="s-1"
      message={message}
      acpWorkerState={acpWorkerState}
      canRecover
      sentStopped={sentStopped}
      retriedError={retriedError}
      restartEpoch={restartEpoch}
      onRetryStarted={onRetryStarted}
      onRetryFailed={onRetryFailed}
      onSwitchToTerminal={switchable ? onSwitchToTerminal : undefined}
    />,
  );
}
const buttons = (container: HTMLElement) =>
  [...container.querySelectorAll("button")].map((b) => b.textContent).filter((t) => t !== "Copy" && t !== "Details");

beforeEach(() => writeClipboard.mockReset().mockResolvedValue(true));
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("StartupErrorBanner layout", () => {
  it.each<{
    label: string;
    info: AgentInstallInfo | null;
    message?: string;
    switchable?: boolean;
    headline: string;
    command: string | null;
    texts: string[];
    absent: string[];
    actions: string[];
  }>([
    {
      label: "install off: the Agent's own command, then Retry",
      info: claude(),
      headline: "Claude couldn't start in Structured view",
      command: CLAUDE_NPM,
      texts: ["connects AoE to Claude", "Run this where the Daemon runs, then Retry."],
      absent: ["inside the container"],
      actions: ["Retry", "Use Terminal view"],
    },
    {
      label: "the missing-binary error names its cause",
      info: claude(),
      message: `Failed to start structured view agent "claude": acp client error: agent spawn failed: entity not found (binary \`claude-agent-acp\` not found on the daemon's PATH or in any known node-manager bin dir)`,
      headline: "Claude couldn't start in Structured view",
      command: CLAUDE_NPM,
      texts: ["Claude's ACP adapter isn't installed where the Daemon runs."],
      absent: ["usually missing or out of date"],
      actions: ["Retry", "Use Terminal view"],
    },
    {
      label: "a stopped Session says so instead of offering an install",
      info: claude(),
      message:
        "Structured view worker was stopped (aoe acp stop); queued prompts wait until the session is started again.",
      headline: "Claude was stopped",
      command: null,
      texts: ["was stopped", "Retry"],
      absent: ["usually missing or out of date", CLAUDE_NPM],
      actions: ["Retry", "Use Terminal view"],
    },
    {
      label: "install on: Install and retry replaces the command",
      info: claude({ install_allowed: true }),
      headline: "Claude couldn't start in Structured view",
      command: null,
      texts: [],
      absent: ["Run this where the Daemon runs"],
      actions: ["Install and retry", "Retry", "Use Terminal view"],
    },
    {
      label: "not npm-installable: the command without its launch note",
      info: opencode,
      headline: "OpenCode couldn't start in Structured view",
      command: "curl -fsSL https://opencode.ai/install | bash",
      texts: ["then `opencode acp`", "Run this where the Daemon runs, then Retry."],
      absent: [],
      actions: ["Retry", "Use Terminal view"],
    },
    {
      label: "sandboxed with install on: installs in the container",
      info: claude({ sandboxed: true, install_allowed: true }),
      headline: "Claude couldn't start in Structured view",
      command: null,
      texts: [],
      absent: ["where the Daemon runs"],
      actions: ["Install in sandbox and retry", "Retry", "Use Terminal view"],
    },
    {
      label: "sandboxed with install off: no host command, the image refresh hint",
      info: claude({ sandboxed: true }),
      headline: "Claude couldn't start in Structured view",
      command: CLAUDE_NPM,
      texts: [
        "Run this inside the container, then Retry",
        'the dashboard\'s "Sandbox image update available" banner offers Update now to pull it, or shows the command to run.',
      ],
      absent: ["where the Daemon runs", "TUI"],
      actions: ["Retry", "Use Terminal view"],
    },
    {
      label: "no install info: the Agent from the message and doctor --fix",
      info: null,
      message: SPAWN_MSG.replace('"claude"', '"codex"'),
      headline: "codex couldn't start in Structured view",
      command: null,
      texts: ["aoe acp doctor --fix"],
      absent: [],
      actions: ["Retry", "Use Terminal view"],
    },
    {
      label: "a specific remediation keeps its text and offers no install",
      info: claude({ install_allowed: true }),
      message: NATIVE_BINARY_MSG,
      headline: "Claude couldn't start in Structured view",
      command: null,
      texts: ["Architecture mismatch", "dynamic loader", "bind-mounted into a container"],
      absent: ["aoe acp doctor --fix", "connects AoE to"],
      actions: ["Retry", "Use Terminal view"],
    },
    {
      label: "the short native-binary form gets the same remediation",
      info: claude({ install_allowed: true }),
      message: "ACP connection failed: native binary failed to launch",
      headline: "Claude couldn't start in Structured view",
      command: null,
      texts: ["Architecture mismatch"],
      absent: ["aoe acp doctor --fix", "connects AoE to"],
      actions: ["Retry", "Use Terminal view"],
    },
    {
      label: "a stale bundled copy names the doctor fix, not npm",
      info: claude({ install_allowed: true }),
      message: `${SPAWN_PREFIX}aoe's bundled ACP adapter \`claude-agent-acp\` predates this aoe build, so it is refused until it is reinstalled. Reinstall it with: aoe acp doctor --fix --adapter claude-agent-acp`,
      headline: "Claude couldn't start in Structured view",
      command: null,
      texts: ["older than this aoe build", "aoe acp doctor --fix --adapter claude-agent-acp"],
      absent: ["connects AoE to", "isn't installed"],
      actions: ["Retry", "Use Terminal view"],
    },
    {
      label: "a Node too old for the adapter says to upgrade Node",
      info: claude({ install_allowed: true }),
      message: `${SPAWN_PREFIX}\`claude-agent-acp\` needs Node 22.6 or newer to run its sources; found v22.1.0. Upgrade Node, or point AOE_ACP_NODE at a newer one, then retry`,
      headline: "Claude couldn't start in Structured view",
      command: null,
      texts: ["needs Node 22.6 or newer, and the Daemon found v22.1.0", "AOE_ACP_NODE", "won't help"],
      absent: ["connects AoE to", "isn't installed"],
      actions: ["Retry", "Use Terminal view"],
    },
    {
      label: "no switch handler: no Use Terminal view",
      info: claude(),
      switchable: false,
      headline: "Claude couldn't start in Structured view",
      command: CLAUDE_NPM,
      texts: [],
      absent: [],
      actions: ["Retry"],
    },
  ])("$label", async ({ info, message, switchable = true, headline, command, texts, absent, actions }) => {
    const spy = stubApi({ info: info ? json(info) : json("boom", 500) });
    const { container, findByText, queryByTestId, getByRole } = renderBanner({ message, switchable });
    await waitFor(() => expect(calls(spy, "/acp/install-info")).toBe(1));
    await findByText(headline);
    // Only the headline region announces; the actions and Details stay out of it.
    const alert = getByRole("alert");
    expect(alert.textContent).toContain(headline);
    expect(within(alert).queryAllByRole("button")).toHaveLength(0);
    await waitFor(() => expect(queryByTestId("startup-error-banner-command")?.textContent ?? null).toBe(command));
    for (const t of texts) expect(container.textContent).toContain(t);
    for (const t of absent) expect(container.textContent).not.toContain(t);
    expect(buttons(container)).toEqual(actions);
  });

  it("copies the command without its launch note", async () => {
    stubApi({ info: json(opencode) });
    const { findByText } = renderBanner();
    fireEvent.click(await findByText("Copy"));
    await findByText("Copied");
    expect(writeClipboard).toHaveBeenCalledWith("curl -fsSL https://opencode.ai/install | bash");
  });

  it("Use Terminal view hands off to the switch-view confirmation", async () => {
    stubApi({ info: json(claude()) });
    const onSwitch = vi.fn();
    const { findByTestId } = renderBanner({ onSwitchToTerminal: onSwitch });
    fireEvent.click(await findByTestId("startup-error-banner-terminal"));
    expect(onSwitch).toHaveBeenCalledTimes(1);
  });

  it.each([
    [true, "The message you sent was stopped. Send it again once Claude is ready."],
    [false, null],
  ])("with a restart's stopped message %s, says to send it again", async (sentStopped, caption) => {
    stubApi({ info: json(claude()) });
    const { findByText, queryByTestId } = renderBanner({ sentStopped });
    await findByText("Claude couldn't start in Structured view");
    expect(queryByTestId("startup-error-banner-sent-stopped")?.textContent ?? null).toBe(caption);
  });
});

describe("StartupErrorBanner auth remediation", () => {
  it.each<{ agent: string; info: AgentInstallInfo; present: string[]; absent: string[] }>([
    {
      agent: "claude",
      info: claude(),
      present: [
        "claude auth login",
        "host terminal",
        "Or set ANTHROPIC_API_KEY in the environment that runs aoe serve, restart aoe serve, then click Retry.",
      ],
      absent: ["codex login", "claude /login", "[sandbox] environment"],
    },
    {
      agent: "codex",
      info: claude({ agent: "codex", display_name: "Codex" }),
      present: ["codex login", "OPENAI_API_KEY"],
      absent: ["claude auth login", "ANTHROPIC_API_KEY", "--device-auth"],
    },
    {
      agent: "codex",
      info: claude({ agent: "codex", display_name: "Codex", sandboxed: true }),
      present: [
        "codex login --device-auth",
        "Container terminal",
        "Or set OPENAI_API_KEY in the environment that runs aoe serve, restart aoe serve, then click Retry.",
      ],
      absent: ["host terminal", "[sandbox] environment"],
    },
    {
      agent: "opencode",
      info: opencode,
      present: ["OpenCode isn't signed in", "Sign in to OpenCode"],
      absent: ["claude auth login", "codex login"],
    },
  ])("names the $agent login step, not another agent's", async ({ agent, info, present, absent }) => {
    stubApi({ info: json(info) });
    const message = `Failed to start structured view agent "${agent}": authentication required`;
    const { findByTestId } = renderBanner({ message });
    const banner = await findByTestId("startup-error-banner");
    await waitFor(() => {
      for (const text of present) expect(banner.textContent).toContain(text);
    });
    expect(banner.textContent).not.toContain("usually missing");
    for (const text of absent) expect(banner.textContent).not.toContain(text);
  });

  it("names no command while it can't tell host from Container", async () => {
    const spy = stubApi({});
    const message = 'Failed to start structured view agent "codex": authentication required';
    const { findByTestId } = renderBanner({ message });
    const banner = await findByTestId("startup-error-banner");
    await waitFor(() => expect(calls(spy, "/acp/install-info")).toBe(1));
    await act(async () => {});
    expect(banner.textContent).toContain("Sign in to codex, then click Retry");
    expect(banner.textContent).not.toContain("codex login");
  });
});

describe("StartupErrorBanner primary action", () => {
  // resolved.rs checks this label/fill pair per built-in theme; hover must not swap either colour.
  it("uses the theme's readable pair at rest and on hover", async () => {
    stubApi({ info: json(claude()) });
    const { findByTestId } = renderBanner();
    const classes = (await findByTestId("startup-error-banner-retry")).className.split(/\s+/);
    expect(classes).toEqual(expect.arrayContaining(["bg-status-error", "text-text-on-status-error"]));
    expect(classes.filter((c) => /^hover:(bg|text)-/.test(c))).toEqual([]);
    expect(classes).not.toContain("text-white");
  });

  // resolved.rs checks text-primary, text-subtle and status-error-text on the tint and on
  // surface-950; the raw secondary, dim and running colours fail there in light themes.
  it.each([
    [
      "a failed install with Details and a truncated log open",
      () =>
        stubApi({
          info: json(claude({ install_allowed: true })),
          install: json({
            success: false,
            exit_code: 243,
            stdout: "",
            stderr: "npm ERR! EACCES",
            recovered_sessions: 0,
          }),
          log: json({ exists: true, tail: "tail content", truncated: true }),
        }),
      async (view: ReturnType<typeof renderBanner>) => {
        fireEvent.click(await view.findByTestId("startup-error-banner-install"));
        await view.findByText("npm ERR! EACCES");
        fireEvent.click(view.getByTestId("startup-error-banner-details"));
        fireEvent.click(view.getByTestId("acp-agent-log-toggle"));
        await view.findByText("Log is large; showing the tail.");
      },
    ],
    [
      "a sandboxed Session's steps",
      () => stubApi({ info: json(claude({ sandboxed: true })) }),
      async (view: ReturnType<typeof renderBanner>) => {
        await view.findByTestId("startup-error-banner-sandbox-note");
      },
    ],
  ])("keeps every line on a validated text colour: %s", async (_label, stub, open) => {
    stub();
    const view = renderBanner();
    await open(view);
    const banner = view.getByTestId("startup-error-banner");
    expect(banner.querySelectorAll(".text-text-secondary, .text-text-dim, .text-status-running")).toHaveLength(0);
    expect(banner.querySelectorAll(".text-text-subtle").length).toBeGreaterThan(0);
  });
});

describe("StartupErrorBanner install", () => {
  it.each([
    ["npm_missing", "npm isn't on the Daemon's PATH"],
    ["not_npm_installable", "The Daemon can't install this ACP adapter with npm"],
    ["container_not_running", "The Session's sandbox container isn't running"],
    [
      "install_disabled",
      'Turn on "Let the Web dashboard install Agents" in the aoe TUI settings (Global, Acp tab, Advanced)',
    ],
    ["install_timeout", "`npm install -g` did not finish within 180s."],
  ])("a %s refusal shows its own message and doesn't respawn", async (code, text) => {
    const spy = stubApi({
      info: json(claude({ install_allowed: true })),
      install: json({ error: code, message: "`npm install -g` did not finish within 180s." }, 400),
    });
    const { findByTestId } = renderBanner();
    fireEvent.click(await findByTestId("startup-error-banner-install"));
    expect((await findByTestId("adapter-install-error")).textContent).toContain(text);
    expect(calls(spy, "/acp/spawn")).toBe(0);
  });

  it("shows npm's output when the install exits non-zero", async () => {
    stubApi({
      info: json(claude({ install_allowed: true })),
      install: json({ success: false, exit_code: 243, stdout: "", stderr: "npm ERR! EACCES", recovered_sessions: 0 }),
    });
    const { findByTestId, findByText } = renderBanner();
    fireEvent.click(await findByTestId("startup-error-banner-install"));
    const error = await findByTestId("adapter-install-error");
    expect(error.textContent).toContain("Install exited with code 243");
    // The token resolved.rs lifts to AA on the banner's tint, not the raw error hue.
    expect(error.classList).toContain("text-status-error-text");
    await findByText("npm ERR! EACCES");
  });

  it("hands the respawn after a successful install to the startup line, and its refusal on", async () => {
    stubApi({
      info: json(claude({ install_allowed: true })),
      install: json({ success: true, exit_code: 0, stdout: "", stderr: "", recovered_sessions: 0 }),
      spawn: json("spawn failed: capacity full", 500),
    });
    const onRetryStarted = vi.fn();
    const onRetryFailed = vi.fn();
    const { findByTestId } = renderBanner({ onRetryStarted, onRetryFailed });
    fireEvent.click(await findByTestId("startup-error-banner-install"));
    await waitFor(() => expect(onRetryFailed.mock.calls).toEqual([[0, "spawn failed: capacity full"]]));
    expect(onRetryStarted.mock.calls).toEqual([[0, SPAWN_MSG]]);
  });

  it("respawns once the install succeeds", async () => {
    const spy = stubApi({
      info: json(claude({ install_allowed: true })),
      install: json({ success: true, exit_code: 0, stdout: "added 1 package", stderr: "", recovered_sessions: 0 }),
      spawn: json({}),
    });
    const { findByTestId } = renderBanner();
    fireEvent.click(await findByTestId("startup-error-banner-install"));
    await waitFor(() => expect(calls(spy, "/acp/spawn")).toBe(1));
  });
});

describe("StartupErrorBanner Retry", () => {
  it.each([
    ["spawns when no Worker is left", "absent", "/acp/spawn"],
    ["restarts a Worker that hasn't settled, so it can't refuse the Retry", "running", "/acp/restart"],
  ] as const)("%s, handing the error to the startup line first", async (_label, acpWorkerState, path) => {
    const spy = stubApi({ info: json(claude()), spawn: json({}), restart: json({}) });
    const onRetryStarted = vi.fn(() => expect(calls(spy, path)).toBe(0));
    const onRetryFailed = vi.fn();
    const { getByTestId } = renderBanner({ acpWorkerState, restartEpoch: 3, onRetryStarted, onRetryFailed });
    fireEvent.click(getByTestId("startup-error-banner-retry"));
    await waitFor(() => expect(calls(spy, path)).toBe(1));
    expect(calls(spy, path === "/acp/spawn" ? "/acp/restart" : "/acp/spawn")).toBe(0);
    expect(onRetryStarted.mock.calls).toEqual([[3, SPAWN_MSG]]);
    expect(onRetryFailed).not.toHaveBeenCalled();
  });

  it.each([
    ["a plain refusal", "spawn failed: capacity full", "spawn failed: capacity full"],
    [
      "a JSON refusal, by its message",
      JSON.stringify({ error: "session_archived", message: "session is archived; unarchive it first" }),
      "session is archived; unarchive it first",
    ],
  ])("hands %s on without the HTTP status", async (_label, body, detail) => {
    stubApi({ info: json(claude()), spawn: json(body, 500) });
    const onRetryFailed = vi.fn();
    const { getByTestId } = renderBanner({ restartEpoch: 3, onRetryStarted: vi.fn(), onRetryFailed });
    fireEvent.click(getByTestId("startup-error-banner-retry"));
    await waitFor(() => expect(onRetryFailed.mock.calls).toEqual([[3, detail]]));
  });
});

describe("StartupErrorBanner Details", () => {
  it("starts closed and keeps opened startup details and logs through an establishing connection loss", async () => {
    const spy = stubApi({
      info: json(claude({ install_allowed: true })),
      log: json({ exists: true, tail: "bootstrap failed before input" }),
    });
    let state = { ...emptyAcpState(), startupError: SPAWN_MSG };
    const props = {
      sessionId: "s-1",
      startingAgent: null,
      canRestart: true,
      acpWorkerState: "absent" as const,
      trashedAt: null,
      archivedAt: null,
      snoozedUntil: null,
      dismissError: vi.fn(),
      onSwitchToTerminal: vi.fn(),
    };
    const { getByTestId, queryByTestId, findByTestId, rerender } = render(<SessionBanners {...props} state={state} />);
    const actions = [
      await findByTestId("startup-error-banner-install"),
      getByTestId("startup-error-banner-retry"),
      getByTestId("startup-error-banner-terminal"),
    ];
    for (const action of actions) expect(action.hasAttribute("disabled")).toBe(false);
    const toggle = getByTestId("startup-error-banner-details");
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    expect(queryByTestId("startup-error-banner-details-body")).toBeNull();
    fireEvent.click(toggle);
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    const body = getByTestId("startup-error-banner-details-body");
    expect(body.textContent).toContain(SPAWN_MSG);
    expect(within(body).getByTestId("acp-agent-log-toggle")).toBeTruthy();
    state = {
      ...state,
      connectionUnavailable: { source: "control", cause: "read_failed", phase: "establishing" },
    };
    rerender(<SessionBanners {...props} state={state} />);
    expect(getByTestId("startup-error-banner-details-body")).toBe(body);
    expect(getByTestId("startup-error-banner-details").getAttribute("aria-expanded")).toBe("true");
    for (const action of actions) {
      expect(action.hasAttribute("disabled")).toBe(true);
      action.click();
    }
    fireEvent.click(within(body).getByTestId("acp-agent-log-toggle"));
    await waitFor(() =>
      expect(within(body).getByTestId("acp-agent-log-pre").textContent).toContain("bootstrap failed before input"),
    );
    state = { ...state, connectionUnavailable: null };
    rerender(<SessionBanners {...props} state={state} />);
    expect(getByTestId("startup-error-banner-details-body")).toBe(body);
    expect(getByTestId("startup-error-banner-details").getAttribute("aria-expanded")).toBe("true");
    expect(within(body).getByTestId("acp-agent-log-pre").textContent).toContain("bootstrap failed before input");
    for (const action of actions) expect(action.hasAttribute("disabled")).toBe(false);
    expect(calls(spy, "/acp/worker-log")).toBe(1);
    expect(calls(spy, "/acp/spawn")).toBe(0);
    expect(calls(spy, "/acp/restart")).toBe(0);
    expect(calls(spy, "/acp/install-agent")).toBe(0);
    expect(props.onSwitchToTerminal).not.toHaveBeenCalled();
  });

  it.each([
    {
      label: "cold retained ownership uncertainty",
      canRestart: true,
      unavailable: { source: "runner_record", cause: "ownership_unconfirmed", phase: "establishing" },
    },
    {
      label: "cold post-establishment connection loss",
      canRestart: true,
      unavailable: { source: "control", cause: "read_failed", phase: "established" },
    },
    { label: "read-only recovery policy", canRestart: false, unavailable: null },
  ] as const)("keeps replayed startup diagnostics read-only during $label", async ({ canRestart, unavailable }) => {
    const spy = stubApi({
      info: json(claude({ install_allowed: true })),
      log: json({ exists: true, tail: "retained startup diagnostic" }),
    });
    const replayed = applyEvent(emptyAcpState(), {
      session_id: "s-1",
      seq: 1,
      event: { AgentStartupError: { message: SPAWN_MSG } },
    });
    const onSwitchToTerminal = vi.fn();
    const { getByTestId, findByTestId } = render(
      <SessionBanners
        sessionId="s-1"
        state={{ ...replayed, connectionUnavailable: unavailable }}
        startingAgent={null}
        canRestart={canRestart}
        acpWorkerState="running"
        trashedAt={null}
        archivedAt={null}
        snoozedUntil={null}
        dismissError={vi.fn()}
        onSwitchToTerminal={onSwitchToTerminal}
      />,
    );
    for (const action of [
      await findByTestId("startup-error-banner-install"),
      getByTestId("startup-error-banner-retry"),
      getByTestId("startup-error-banner-terminal"),
    ]) {
      expect(action.hasAttribute("disabled")).toBe(true);
      action.click();
    }
    fireEvent.click(getByTestId("startup-error-banner-details"));
    const body = getByTestId("startup-error-banner-details-body");
    expect(body.textContent).toContain(SPAWN_MSG);
    fireEvent.click(within(body).getByTestId("acp-agent-log-toggle"));
    await waitFor(() =>
      expect(within(body).getByTestId("acp-agent-log-pre").textContent).toContain("retained startup diagnostic"),
    );
    expect(calls(spy, "/acp/worker-log")).toBe(1);
    expect(calls(spy, "/acp/spawn")).toBe(0);
    expect(calls(spy, "/acp/restart")).toBe(0);
    expect(calls(spy, "/acp/install-agent")).toBe(0);
    expect(onSwitchToTerminal).not.toHaveBeenCalled();
  });

  it("copies the full error", async () => {
    stubApi({ info: json(claude()) });
    const { getByTestId, findByText } = renderBanner();
    fireEvent.click(getByTestId("startup-error-banner-details"));
    fireEvent.click(getByTestId("startup-error-banner-copy"));
    await findByText("Copied");
    expect(writeClipboard).toHaveBeenCalledWith(SPAWN_MSG);
  });
});

describe("StartupErrorBanner after a Retry", () => {
  const cause = SPAWN_MSG.split(": ").slice(1).join(": ");
  // Past the 200 characters a refusal is cut to for display.
  const longCause = `acp client error: agent spawn failed: runner control attach failed at /home/me/.agent-of-empires/acp-workers/${"a".repeat(120)}.control.sock: runner closed before Hello`;
  const SAME = "Same error as before.";
  it.each([
    ["the same error", SPAWN_MSG, `spawn failed: ${cause}`, SAME],
    ["the same error after a reason code", SPAWN_MSG, `worker_not_ready: spawn failed: ${cause}`, SAME],
    [
      "the same long error",
      `Failed to start structured view agent "claude": ${longCause}`,
      `spawn failed: ${longCause}`,
      SAME,
    ],
    ["the same error from a restart", SPAWN_MSG, `restart spawn failed: ${cause}`, SAME],
    [
      "the same error from a restart after a reason code",
      SPAWN_MSG,
      `worker_not_ready: restart spawn failed: ${cause}`,
      SAME,
    ],
    ["the same error as an event", SPAWN_MSG, SPAWN_MSG, SAME],
    ["a new error", SPAWN_MSG, "spawn failed: capacity full", "It failed differently this time: capacity full"],
    [
      "a new error over several lines, by its first",
      SPAWN_MSG,
      "spawn failed: capacity full\nat worker.rs:12",
      "It failed differently this time: capacity full",
    ],
  ])("%s says it still couldn't start and how the cause compares", async (_label, retriedError, message, line) => {
    stubApi({ info: json(claude()) });
    const { findByRole, getByTestId } = renderBanner({ message, retriedError });
    expect((await findByRole("heading")).textContent).toBe("Claude still couldn't start");
    expect(getByTestId("startup-error-banner-changed").textContent).toBe(line);
  });

  it("names neither before any Retry", async () => {
    stubApi({ info: json(claude()) });
    const { findByRole, queryByTestId } = renderBanner();
    expect((await findByRole("heading")).textContent).toBe("Claude couldn't start in Structured view");
    expect(queryByTestId("startup-error-banner-changed")).toBeNull();
  });
});

describe("AgentLogDisclosure", () => {
  const openLog = async (log: Reply) => {
    const spy = stubApi({ info: json(claude()), log });
    const view = renderBanner();
    fireEvent.click(view.getByTestId("startup-error-banner-details"));
    return { spy, ...view };
  };

  it("fetches the worker log only on first open and renders the tail", async () => {
    const { spy, getByTestId } = await openLog(
      json({ exists: true, tail: "ERROR acp.acp: spawn failed\nclaude execve: ENOEXEC" }),
    );
    expect(calls(spy, "/acp/worker-log")).toBe(0);
    fireEvent.click(getByTestId("acp-agent-log-toggle"));
    await waitFor(() => expect(getByTestId("acp-agent-log-pre").textContent).toContain("ENOEXEC"));
    expect(spy.mock.calls.find(([url]) => String(url).includes("/worker-log"))?.[0]).toContain(
      "/api/sessions/s-1/acp/worker-log?tail=200",
    );
  });

  it.each([
    ["No log output yet", json({ exists: false, tail: "" })],
    ["Log file exists but is empty", json({ exists: true, tail: "" })],
    ["Log is large; showing the tail", json({ exists: true, tail: "tail content", truncated: true })],
    ["Could not load log: Server returned 500. boom", json("boom", 500)],
  ])("renders %s", async (expected, log) => {
    const { getByTestId, container } = await openLog(log);
    fireEvent.click(getByTestId("acp-agent-log-toggle"));
    await waitFor(() => expect(container.textContent).toContain(expected));
  });

  it("hides the body on a second toggle and re-fetches on Refresh", async () => {
    const { spy, getByTestId, queryByTestId } = await openLog(json({ exists: true, tail: "abc" }));
    const toggle = getByTestId("acp-agent-log-toggle");
    fireEvent.click(toggle);
    await waitFor(() => expect(queryByTestId("acp-agent-log-pre")).not.toBeNull());
    fireEvent.click(getByTestId("acp-agent-log-refresh"));
    await waitFor(() => expect(calls(spy, "/acp/worker-log")).toBe(2));
    fireEvent.click(toggle);
    expect(queryByTestId("acp-agent-log-pre")).toBeNull();
  });
});

describe("StartupErrorBanner sandbox login", () => {
  const LOGIN_MESSAGE = "sandbox login required for claude: run `aoe sandbox login --tool claude`";
  const LOGIN_BODY = { login_id: "login-1", url: "https://login.example.test/authorize?code=true" };
  /** Answers the login endpoints, and 404s the rest the banner asks on mount. */
  const isStart = (url: string, init?: RequestInit) =>
    url.split("?")[0] === "/api/sandbox/login" && init?.method === "POST";
  function stubLogin() {
    const spy = vi.fn(async (url: string, init?: RequestInit) =>
      isStart(url, init)
        ? new Response(JSON.stringify(LOGIN_BODY), { status: 200 })
        : new Response("not found", { status: 404 }),
    );
    vi.stubGlobal("fetch", spy);
    return spy;
  }
  const loginCall = (spy: ReturnType<typeof stubLogin>) =>
    spy.mock.calls.find(([url, init]) => isStart(String(url), init));

  it("offers Log in only for the login-required cause", () => {
    stubApi({});
    const withCause = render(
      <StartupErrorBanner
        sessionId="s-1"
        acpWorkerState="absent"
        message={LOGIN_MESSAGE}
        cause={{ kind: "sandbox_login_required" }}
      />,
    );
    expect(withCause.queryByRole("button", { name: "Log in" })).not.toBeNull();
    withCause.unmount();

    const otherCause = render(
      <StartupErrorBanner
        sessionId="s-1"
        acpWorkerState="absent"
        message={LOGIN_MESSAGE}
        cause={{ kind: "container_missing" }}
      />,
    );
    expect(otherCause.queryByRole("button", { name: "Log in" })).toBeNull();
    otherCause.unmount();

    const noCause = renderBanner({ message: LOGIN_MESSAGE });
    expect(noCause.queryByRole("button", { name: "Log in" })).toBeNull();
  });

  it("opens the login dialog for the tool of the session that is waiting", async () => {
    const spy = stubLogin();
    const { getByRole, findByRole } = render(
      <StartupErrorBanner
        sessionId="s-1"
        acpWorkerState="absent"
        tool="claude-personal"
        message={LOGIN_MESSAGE}
        cause={{ kind: "sandbox_login_required" }}
      />,
    );
    fireEvent.click(getByRole("button", { name: "Log in" }));
    await findByRole("dialog");
    await waitFor(() => expect(loginCall(spy)).toBeDefined());
    expect(loginCall(spy)?.[1]).toEqual(
      expect.objectContaining({
        method: "POST",
        body: expect.stringMatching(/^\{"force":false,"tool":"claude-personal","lease":"[0-9a-f]{32}"\}$/),
      }),
    );
  });

  it("shows the server's message and logs in the command and profile it named", async () => {
    const message =
      "sandbox login required for personal: run `aoe sandbox login -p work --tool personal --command claude`";
    const spy = stubLogin();
    const { container, getByRole, findByRole } = render(
      <StartupErrorBanner
        sessionId="s-1"
        acpWorkerState="absent"
        tool="personal"
        message={message}
        cause={{ kind: "sandbox_login_required", command: "claude", profile: "work" }}
      />,
    );
    // The recovery command is the server's text, not one built here.
    expect(container.textContent).toContain(message);
    fireEvent.click(getByRole("button", { name: "Log in" }));
    await findByRole("dialog");
    await waitFor(() => expect(loginCall(spy)).toBeDefined());
    expect(loginCall(spy)?.[1]).toEqual(
      expect.objectContaining({
        method: "POST",
        body: expect.stringMatching(
          /^\{"force":false,"tool":"personal","command":"claude","profile":"work","lease":"[0-9a-f]{32}"\}$/,
        ),
      }),
    );
  });

  it("says the login is required, with the daemon's guidance and no adapter remediation", () => {
    stubApi({});
    const { container, getByRole } = render(
      <StartupErrorBanner
        sessionId="s-1"
        acpWorkerState="absent"
        message={LOGIN_MESSAGE}
        cause={{ kind: "sandbox_login_required" }}
      />,
    );
    expect(container.textContent).toContain("Sandbox login required");
    expect(container.textContent).toContain(LOGIN_MESSAGE);
    expect(container.textContent).not.toContain("couldn't start");
    expect(container.textContent).not.toContain("claude /login");
    expect(getByRole("button", { name: "Retry" })).toBeTruthy();
    expect(getByRole("button", { name: "Log in" })).toBeTruthy();
  });

  it("offers no Log in for a runtime that cannot run the login, and shows its refusal as is", () => {
    stubApi({});
    const refusal =
      "sandbox.shared_credential is separate_login, which is not supported with Apple Container; use host_seed or Docker/Podman";
    const { container, queryByRole } = render(
      <StartupErrorBanner
        sessionId="s-1"
        acpWorkerState="absent"
        message={refusal}
        cause={{ kind: "sandbox_login_unsupported" }}
      />,
    );
    expect(queryByRole("button", { name: "Log in" })).toBeNull();
    expect(container.textContent).toContain("Sandbox login not supported");
    expect(container.textContent).toContain(refusal);
    // The refusal is the guidance: the adapter and credentials remediation does not apply.
    expect(container.textContent).not.toContain("claude /login");
    expect(container.textContent).not.toContain("aoe acp doctor");
  });
});
