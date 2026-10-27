// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";

import type { AgentInfo } from "../../lib/types";
import { setAgentNames } from "../../lib/agentDisplayName";
import { UsageLimitBanner, UsageLimitSection } from "./UsageLimitBanner";

vi.mock("../../lib/api", () => ({
  fetchAgents: vi.fn(),
  fetchAcpAgents: vi.fn(),
  switchAcpAgent: vi.fn(),
  fetchContextPrimer: vi.fn(),
}));

import { fetchAcpAgents, fetchAgents, fetchContextPrimer, switchAcpAgent } from "../../lib/api";

const NOW = new Date("2026-10-22T10:00:00Z");
const IN_2H = "2026-10-22T12:00:00Z";
const AGO = "2026-10-22T09:00:00Z";
const PARIS = "Internal error: You've hit your weekly limit · resets 4am (Europe/Paris)";
const at = (iso: string) => new Date(iso).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

// The copy's display names are the server's; a case overrides the list to change the switch targets.
beforeEach(() => {
  setAgentNames([agent("claude"), agent("codex")]);
  vi.mocked(fetchAgents).mockResolvedValue([agent("claude"), agent("codex")]);
});

type BannerProps = React.ComponentProps<typeof UsageLimitBanner>;

function mountBanner(overrides: Partial<BannerProps> = {}) {
  const props: BannerProps = {
    agent: "claude",
    rateLimit: { status: "limited", resets_at: IN_2H, kind: "usage" },
    autoResume: true,
    retriesExhausted: false,
    resumeState: "idle",
    resumeError: null,
    onResume: vi.fn(),
    settingsHref: "/settings/structured-view?profile=work",
    ...overrides,
  };
  render(
    <MemoryRouter>
      <UsageLimitBanner {...props} />
    </MemoryRouter>,
  );
  return { props, text: screen.getByTestId("usage-limit-banner").textContent ?? "" };
}

describe("UsageLimitBanner", () => {
  beforeEach(() => {
    vi.useFakeTimers({ toFake: ["Date"] });
    vi.setSystemTime(NOW);
  });

  it.each<[string, Partial<BannerProps>, (string | RegExp)[], (string | RegExp)[], string]>([
    [
      "an upcoming reset with auto-resume on",
      {},
      [
        "Claude Code usage limit reached",
        `Resets ${at(IN_2H)} · in about 2h`,
        "Picks up automatically",
        `Otherwise the limit lifts at ${at(IN_2H)}.`,
      ],
      [/Auto-resume is off/],
      "Try again",
    ],
    ["auto-resume unknown", { autoResume: undefined }, ["usage limit reached"], [/Auto-resume|Picks up/], "Try again"],
    [
      "a lifted limit with auto-resume off",
      { autoResume: false, rateLimit: { status: "limited", resets_at: AGO, kind: "usage" } },
      ["Claude Code usage limit lifted", `The limit reset at ${at(AGO)}.`, "Auto-resume is off."],
      [/Added more usage/],
      "Resume now",
    ],
    [
      "a lifted limit with auto-resume on",
      { rateLimit: { status: "limited", resets_at: AGO, kind: "usage" } },
      ["usage limit lifted", "Added more usage? Try again now."],
      [/Otherwise the limit lifts/],
      "Try again",
    ],
    [
      "no reported reset",
      { rateLimit: { status: PARIS, resets_at: null, kind: "usage" } },
      ["No reset time was reported."],
      [/weekly limit|Otherwise/],
      "Try again",
    ],
    [
      "a non-Claude Agent's own wording",
      {
        agent: "codex",
        rateLimit: {
          status: `ACP connection failed: ${PARIS}: {\n  "errorKind":"rate_limit"\n}`,
          resets_at: "not-a-timestamp",
          kind: "usage",
        },
      },
      ["Codex usage limit reached", "Codex said: You've hit your weekly limit · resets 4am (Europe/Paris)"],
      [/Invalid Date|errorKind|ACP connection failed|Internal error/],
      "Try again",
    ],
    [
      "an unknown Agent",
      { agent: null, rateLimit: { status: PARIS, resets_at: null, kind: "usage" } },
      ["Agent usage limit reached", "The Agent said: You've hit your weekly limit"],
      [/the Agent usage/],
      "Try again",
    ],
    [
      "a non-Claude Agent reporting only a fingerprint",
      { agent: "codex", rateLimit: { status: '{"errorKind":"rate_limit"}', resets_at: null, kind: "usage" } },
      ["No reset time was reported."],
      [/errorKind/],
      "Try again",
    ],
    [
      "exhausted retries",
      { retriesExhausted: true },
      ["Auto-resume stopped: the same prompt was re-sent too many times"],
      [/Picks up/],
      "Try again",
    ],
  ])("%s", (_label, overrides, present, absent, button) => {
    const { text } = mountBanner(overrides);
    for (const p of present) expect(text).toMatch(p);
    for (const a of absent) expect(text).not.toMatch(a);
    expect(screen.getByTestId("usage-limit-resume").textContent).toBe(button);
  });

  it.each([
    ["/settings/structured-view?profile=work", true],
    [null, false],
  ])("links Turn on to %s", (settingsHref, linked) => {
    mountBanner({ autoResume: false, settingsHref });
    const link = screen.queryByRole("link", { name: "Turn on" });
    expect(link?.getAttribute("href") ?? null).toBe(linked ? settingsHref : null);
  });

  it.each([
    ["retrying", "Resuming…", true, null],
    ["ok", "Resume requested", true, "New events should start streaming shortly."],
    ["failed", "Try again", false, "Resume failed: Server returned 500."],
  ] as const)("resume %s", (resumeState, label, disabled, note) => {
    const { text } = mountBanner({ resumeState, resumeError: "Server returned 500." });
    const button = screen.getByTestId("usage-limit-resume") as HTMLButtonElement;
    expect(button.textContent).toBe(label);
    expect(button.disabled).toBe(disabled);
    if (note) expect(text).toContain(note);
  });

  it("offers Switch Agent only with a handler and calls both actions", () => {
    const onSwitchAgent = vi.fn();
    const { props } = mountBanner({ onSwitchAgent });
    fireEvent.click(screen.getByRole("button", { name: "Switch Agent" }));
    fireEvent.click(screen.getByTestId("usage-limit-resume"));
    expect(onSwitchAgent).toHaveBeenCalledTimes(1);
    expect(props.onResume).toHaveBeenCalledTimes(1);
    cleanup();
    mountBanner();
    expect(screen.queryByRole("button", { name: "Switch Agent" })).toBeNull();
  });
});

const agent = (name: string, overrides: Partial<AgentInfo> = {}): AgentInfo => ({
  name,
  kind: "builtin",
  binary: name,
  host_only: false,
  installed: true,
  install_hint: "",
  acp_capable: true,
  acp_installed: true,
  display_name: { claude: "Claude Code", codex: "Codex" }[name] ?? name,
  ...overrides,
});

describe("UsageLimitSection", () => {
  const LIMITED = { status: "limited", resets_at: null, kind: "usage" };
  function mountSection(onPrefill = vi.fn()) {
    const section = (agent: string, rateLimit: typeof LIMITED | null) => (
      <MemoryRouter>
        <UsageLimitSection
          sessionId="s-1"
          agent={agent}
          rateLimit={rateLimit}
          autoResume
          retriesExhausted={false}
          settingsHref={null}
          onPrefill={onPrefill}
        />
      </MemoryRouter>
    );
    const view = render(section("claude", LIMITED));
    // What the switch itself does: a new Agent, and a Stopped that clears the limit.
    const switched = () => view.rerender(section("codex", null));
    return { onPrefill, switched };
  }

  it.each([
    ["its recap arrives", true],
    ["its recap request fails", false],
  ])("keeps the handoff modal after the switch clears the limit, until %s", async (_label, recapArrives) => {
    vi.mocked(fetchAgents).mockResolvedValue([agent("claude"), agent("codex")]);
    vi.mocked(fetchAcpAgents).mockResolvedValue(
      ["claude", "codex"].map((name) => ({ name, description: name, command: name })),
    );
    vi.mocked(switchAcpAgent).mockResolvedValue({
      session_id: "s-1",
      agent: "codex",
      before_seq: 41,
      switch_seq: 42,
      status: "switched",
    });
    let settle!: (ok: boolean) => void;
    vi.mocked(fetchContextPrimer).mockReturnValue(
      new Promise((resolve, reject) => {
        settle = (ok) =>
          ok
            ? resolve({
                primer: "user: hi",
                included_event_count: 1,
                included_turn_count: 1,
                truncated: false,
                max_chars: 4_000,
                unprocessed_prompt: "",
              })
            : reject(new Error("primer unavailable"));
      }),
    );
    const { onPrefill, switched } = mountSection();
    fireEvent.click(await screen.findByRole("button", { name: "Switch Agent" }));
    fireEvent.click(await screen.findByRole("button", { name: "Continue in Codex" }));
    await waitFor(() => expect(fetchContextPrimer).toHaveBeenCalled());

    switched();
    await act(async () => {});
    expect(screen.queryByTestId("usage-limit-banner")).toBeNull();
    expect(screen.getByRole("dialog").textContent).toContain("Switching...");
    // Still the handoff it started: from Claude, to Codex.
    const picks = Array.from(document.querySelectorAll<HTMLInputElement>("input[name=acp-agent-target]"));
    expect(picks.map((r) => [r.value, r.checked])).toEqual([
      ["claude", false],
      ["codex", true],
    ]);
    expect(screen.getByText("current · limit reached").closest("label")?.textContent).toContain("Claude");

    await act(async () => settle(recapArrives));
    if (recapArrives) {
      expect(onPrefill).toHaveBeenCalledWith(expect.stringContaining("user: hi"));
      expect(screen.queryByRole("dialog")).toBeNull();
    } else {
      expect(onPrefill).not.toHaveBeenCalled();
      expect(screen.getByRole("alert").textContent).toContain("primer unavailable");
    }
  });

  it("says why Switch Agent is missing when the Agent list fails", async () => {
    // fetchAgents resolves an empty list, never rejects, when /api/agents fails.
    vi.mocked(fetchAgents).mockResolvedValue([]);
    mountSection();
    expect(await screen.findByText(/Couldn't list the other Agents, so Switch Agent isn't offered/)).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Switch Agent" })).toBeNull();
  });

  it("offers only installed, allowed Agents other than the limited one", async () => {
    vi.mocked(fetchAgents).mockResolvedValue([
      agent("claude"),
      agent("codex"),
      agent("gemini", { acp_installed: false }),
      agent("opencode", { acp_allowed: false }),
    ]);
    vi.mocked(fetchAcpAgents).mockResolvedValue(
      ["claude", "codex", "gemini", "opencode"].map((name) => ({ name, description: name, command: name })),
    );
    mountSection();
    fireEvent.click(await screen.findByRole("button", { name: "Switch Agent" }));
    await screen.findByText("Continue in Codex");
    const offered = Array.from(document.querySelectorAll<HTMLInputElement>("input[name=acp-agent-target]"));
    expect(offered.map((r) => [r.value, r.checked, r.disabled])).toEqual([
      ["claude", false, true],
      ["codex", true, false],
    ]);
    expect(screen.getByText("current · limit reached")).not.toBeNull();
  });

  it("hides Switch Agent when no other Agent is installed", async () => {
    vi.mocked(fetchAgents).mockResolvedValue([agent("claude"), agent("codex", { acp_installed: false })]);
    mountSection();
    await waitFor(() => expect(fetchAgents).toHaveBeenCalled());
    await act(async () => {});
    expect(screen.queryByRole("button", { name: "Switch Agent" })).toBeNull();
  });
});
