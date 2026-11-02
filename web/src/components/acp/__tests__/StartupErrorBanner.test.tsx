// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, waitFor, within } from "@testing-library/react";

import type { AgentInstallInfo } from "../../../lib/api";
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
function stubApi(routes: { info?: Reply; spawn?: Reply; install?: Reply; log?: Reply }) {
  const spy = vi.fn(async (url: string) => {
    const reply =
      (url.includes("/acp/install-info") && routes.info) ||
      (url.includes("/acp/spawn") && routes.spawn) ||
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
}: { message?: string; onSwitchToTerminal?: () => void; switchable?: boolean } = {}) {
  return render(
    <StartupErrorBanner
      sessionId="s-1"
      message={message}
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
      texts: ["Run this inside the container, then Retry", "sandbox image update available"],
      absent: ["where the Daemon runs"],
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
});

describe("StartupErrorBanner auth remediation", () => {
  it.each<{ agent: string; info: AgentInstallInfo; present: string[]; absent: string[] }>([
    {
      agent: "claude",
      info: claude(),
      present: ["claude /login", "ANTHROPIC_API_KEY"],
      absent: ["codex login"],
    },
    {
      agent: "codex",
      info: claude({ agent: "codex", display_name: "Codex" }),
      present: ["codex login", "OPENAI_API_KEY"],
      absent: ["claude /login", "ANTHROPIC_API_KEY"],
    },
    {
      agent: "opencode",
      info: opencode,
      present: ["OpenCode isn't signed in"],
      absent: ["claude /login", "codex login"],
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
    ["install_disabled", 'Turn on "Allow agent install from web" in the aoe TUI settings (Global, Acp tab, Advanced)'],
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

  it("opens Details when the respawn after a successful install fails", async () => {
    stubApi({
      info: json(claude({ install_allowed: true })),
      install: json({ success: true, exit_code: 0, stdout: "", stderr: "", recovered_sessions: 0 }),
      spawn: json("spawn failed: capacity full", 500),
    });
    const { findByTestId } = renderBanner();
    fireEvent.click(await findByTestId("startup-error-banner-install"));
    const details = await findByTestId("startup-error-banner-details-body");
    await waitFor(() => expect(details.textContent).toContain("Retry failed: capacity full"));
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
  it("says what happens next once the respawn is accepted", async () => {
    stubApi({ info: json(claude()), spawn: json({}) });
    const { getByTestId, findByText, queryByTestId } = renderBanner();
    fireEvent.click(getByTestId("startup-error-banner-retry"));
    await findByText("Retrying. Events will appear when Claude starts.");
    expect(queryByTestId("startup-error-banner-details-body")).toBeNull();
  });
});

describe("StartupErrorBanner Details", () => {
  it("starts closed, then holds the raw error and the agent log", async () => {
    stubApi({ info: json(claude()) });
    const { getByTestId, queryByTestId } = renderBanner();
    const toggle = getByTestId("startup-error-banner-details");
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    expect(queryByTestId("startup-error-banner-details-body")).toBeNull();
    fireEvent.click(toggle);
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    const body = getByTestId("startup-error-banner-details-body");
    expect(body.textContent).toContain(SPAWN_MSG);
    expect(within(body).getByTestId("acp-agent-log-toggle")).toBeTruthy();
  });

  const cause = SPAWN_MSG.split(": ").slice(1).join(": ");
  // Past the 200 characters the retry error is cut to for display.
  const longCause = `acp client error: agent spawn failed: runner control attach failed at /home/me/.agent-of-empires/acp-workers/${"a".repeat(120)}.control.sock: runner closed before Hello`;
  it.each([
    ["the same error", SPAWN_MSG, `spawn failed: ${cause}`, "Retry failed: same error"],
    [
      "the same error after a reason code",
      SPAWN_MSG,
      `worker_not_ready: spawn failed: ${cause}`,
      "Retry failed: same error",
    ],
    [
      "the same long error",
      `Failed to start structured view agent "claude": ${longCause}`,
      `spawn failed: ${longCause}`,
      "Retry failed: same error",
    ],
    ["a new error", SPAWN_MSG, "spawn failed: capacity full", "Retry failed: capacity full"],
    [
      "a JSON refusal",
      SPAWN_MSG,
      JSON.stringify({ error: "session_archived", message: "session is archived; unarchive it first" }),
      "Retry failed: session is archived; unarchive it first",
    ],
  ])("a retry failing with %s opens Details without the HTTP status", async (_label, message, body, line) => {
    stubApi({ info: json(claude()), spawn: json(body, 500) });
    const { getByTestId, findByTestId } = renderBanner({ message });
    fireEvent.click(getByTestId("startup-error-banner-retry"));
    const details = await findByTestId("startup-error-banner-details-body");
    await waitFor(() => expect(details.textContent).toContain(line));
    expect(details.textContent).not.toContain("500");
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
