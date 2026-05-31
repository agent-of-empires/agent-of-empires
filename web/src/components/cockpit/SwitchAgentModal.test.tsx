// @vitest-environment jsdom
//
// Modal-side contract for the agent-switch flow (#1281 / #1282). The
// same dialog drives two triggers: the rate-limit recovery path
// ("rate_limit") and an explicit user-initiated switch ("manual"). The
// component fans out to three API helpers in lib/api; the test mocks
// them so each assertion pins one slice of behaviour:
//   - confirm fires switchCockpitAgent then fetchContextPrimer, in
//     that order, then onPrefill with the framed handoff text;
//   - the recorded reason matches the trigger (rate_limited vs manual);
//   - cancel / Escape do NOT touch switchCockpitAgent;
//   - the recap and unprocessed_prompt slots show up in the prefill in
//     the expected positions;
//   - the manual trigger swaps the copy and drops the codex preference.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  cleanup,
  fireEvent,
  render,
  waitFor,
} from "@testing-library/react";

import { SwitchAgentModal } from "./SwitchAgentModal";

vi.mock("../../lib/api", () => ({
  fetchCockpitAgents: vi.fn(),
  switchCockpitAgent: vi.fn(),
  fetchContextPrimer: vi.fn(),
}));

import {
  fetchCockpitAgents,
  fetchContextPrimer,
  switchCockpitAgent,
} from "../../lib/api";

const mockFetchAgents = vi.mocked(fetchCockpitAgents);
const mockSwitch = vi.mocked(switchCockpitAgent);
const mockPrimer = vi.mocked(fetchContextPrimer);

beforeEach(() => {
  vi.clearAllMocks();
  mockFetchAgents.mockResolvedValue([
    { name: "claude", description: "Claude (Sonnet)", command: "claude-agent-acp" },
    { name: "codex", description: "OpenAI Codex", command: "codex-acp" },
    { name: "opencode", description: "OpenCode", command: "opencode-acp" },
  ]);
  mockSwitch.mockResolvedValue({
    session_id: "s-1",
    agent: "codex",
    before_seq: 41,
    switch_seq: 42,
    status: "switched",
  });
  mockPrimer.mockResolvedValue({
    primer: "user: hi\nagent: hello",
    included_event_count: 2,
    included_turn_count: 1,
    truncated: false,
    max_chars: 4_000,
    unprocessed_prompt: "deploy the thing",
  });
});

afterEach(() => {
  cleanup();
});

function mount(props?: Partial<React.ComponentProps<typeof SwitchAgentModal>>) {
  const onClose = vi.fn();
  const onPrefill = vi.fn();
  const utils = render(
    <SwitchAgentModal
      open
      sessionId="s-1"
      currentAgent="claude"
      onClose={onClose}
      onPrefill={onPrefill}
      trigger="rate_limit"
      {...props}
    />,
  );
  return { onClose, onPrefill, ...utils };
}

describe("SwitchAgentModal (rate_limit)", () => {
  it("filters out the current agent and preselects codex", async () => {
    const { container, findByText } = mount();
    await findByText(/Continue in codex/);
    const radios = Array.from(
      container.querySelectorAll<HTMLInputElement>(
        "input[name=cockpit-agent-target]",
      ),
    );
    const values = radios.map((r) => r.value);
    expect(values).toEqual(expect.arrayContaining(["codex", "opencode"]));
    expect(values).not.toContain("claude");
    const checked = radios.find((r) => r.checked);
    expect(checked?.value).toBe("codex");
  });

  it("falls back to the first remaining agent when codex isn't installed", async () => {
    mockFetchAgents.mockResolvedValue([
      { name: "claude", description: "Claude", command: "claude-agent-acp" },
      { name: "opencode", description: "OpenCode", command: "opencode-acp" },
    ]);
    const { findByText } = mount();
    await findByText(/Continue in opencode/);
  });

  it("hands off via switchCockpitAgent + fetchContextPrimer and prefills", async () => {
    const { findByText, onPrefill, onClose } = mount();
    const confirm = await findByText(/Continue in codex/);
    fireEvent.click(confirm);
    await waitFor(() => expect(mockSwitch).toHaveBeenCalledTimes(1));
    // reason "rate_limited" so the transcript divider reads correctly.
    expect(mockSwitch).toHaveBeenCalledWith("s-1", "codex", null, "rate_limited");
    await waitFor(() => expect(mockPrimer).toHaveBeenCalledTimes(1));
    // Primer must be invoked with before_seq from the switch response
    // (41), not switch_seq, so the recap excludes the AgentSwitched
    // event itself.
    expect(mockPrimer.mock.calls[0]?.[1]).toBe(41);

    await waitFor(() => expect(onPrefill).toHaveBeenCalledTimes(1));
    const prefilled = onPrefill.mock.calls[0]?.[0] as string;
    expect(prefilled).toContain("CONTEXT HANDOFF");
    expect(prefilled).toContain("rate-limited");
    expect(prefilled).toContain("claude");
    expect(prefilled).toContain("codex");
    expect(prefilled).toContain("user: hi");
    expect(prefilled).toContain("deploy the thing");
    expect(prefilled.indexOf("user: hi")).toBeLessThan(
      prefilled.indexOf("deploy the thing"),
    );
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("does not call switchCockpitAgent when the user cancels", async () => {
    const { findByText, onClose } = mount();
    await findByText(/Continue in codex/);
    fireEvent.click(await findByText("Cancel"));
    expect(mockSwitch).not.toHaveBeenCalled();
    expect(mockPrimer).not.toHaveBeenCalled();
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("closes on Escape without dispatching a switch", async () => {
    const { findByText, onClose } = mount();
    await findByText(/Continue in codex/);
    fireEvent.keyDown(document, { key: "Escape" });
    expect(onClose).toHaveBeenCalledTimes(1);
    expect(mockSwitch).not.toHaveBeenCalled();
  });

  it("surfaces a server error and keeps the modal open", async () => {
    mockSwitch.mockRejectedValue(new Error("boom"));
    const { findByText, onPrefill, onClose } = mount();
    fireEvent.click(await findByText(/Continue in codex/));
    const alert = await findByText(/boom/);
    expect(alert.textContent).toMatch(/boom/);
    expect(onPrefill).not.toHaveBeenCalled();
    expect(onClose).not.toHaveBeenCalled();
  });

  it("surfaces fetchCockpitAgents rejection in the modal error slot", async () => {
    mockFetchAgents.mockRejectedValue(new Error("agents fetch broke"));
    const { findByText, onPrefill } = mount();
    const alert = await findByText(/agents fetch broke/);
    expect(alert.textContent).toMatch(/agents fetch broke/);
    expect(mockSwitch).not.toHaveBeenCalled();
    expect(onPrefill).not.toHaveBeenCalled();
  });

  it("surfaces a generic message when switchCockpitAgent returns null", async () => {
    // The api helper returns null on 4xx/5xx without throwing (fetchJson
    // semantics). Modal must not crash and must show a clear message.
    mockSwitch.mockResolvedValue(null);
    const { findByText, onPrefill, onClose } = mount();
    fireEvent.click(await findByText(/Continue in codex/));
    const alert = await findByText(/server returned no response/i);
    expect(alert.textContent).toMatch(/server returned no response/i);
    expect(mockPrimer).not.toHaveBeenCalled();
    expect(onPrefill).not.toHaveBeenCalled();
    expect(onClose).not.toHaveBeenCalled();
  });

  it("clicking a non-preselected radio updates the confirm-button target", async () => {
    const { container, findByText } = mount();
    await findByText(/Continue in codex/);
    const opencodeRadio = container.querySelector<HTMLInputElement>(
      "input[name=cockpit-agent-target][value=opencode]",
    );
    expect(opencodeRadio).not.toBeNull();
    fireEvent.click(opencodeRadio!);
    await findByText(/Continue in opencode/);
  });

  it("renders an install hint when no alternative agents are registered", async () => {
    mockFetchAgents.mockResolvedValue([
      { name: "claude", description: "claude", command: "claude-agent-acp" },
    ]);
    const { findByText } = mount();
    await findByText(/No alternative cockpit agents are registered/i);
  });
});

describe("SwitchAgentModal (manual)", () => {
  it("uses 'Switch to' copy and no codex preference", async () => {
    // Manual trigger has no preferred direction: it preselects the first
    // remaining entry (claude filtered out -> codex is first here, but
    // the label proves the manual copy path, not a rate-limit fallback).
    const { container, findByText, queryByText } = mount({ trigger: "manual" });
    await findByText(/Switch to/);
    expect(queryByText(/Continue in/)).toBeNull();
    const checked = Array.from(
      container.querySelectorAll<HTMLInputElement>(
        "input[name=cockpit-agent-target]",
      ),
    ).find((r) => r.checked);
    // First remaining agent after filtering out the current one.
    expect(checked?.value).toBe("codex");
  });

  it("records reason 'manual' and frames the recap as a plain switch", async () => {
    const { findByText, onPrefill } = mount({ trigger: "manual" });
    fireEvent.click(await findByText(/Switch to codex/));
    await waitFor(() => expect(mockSwitch).toHaveBeenCalledTimes(1));
    expect(mockSwitch).toHaveBeenCalledWith("s-1", "codex", null, "manual");
    await waitFor(() => expect(onPrefill).toHaveBeenCalledTimes(1));
    const prefilled = onPrefill.mock.calls[0]?.[0] as string;
    expect(prefilled).toContain("switched from claude to codex");
    expect(prefilled).not.toContain("rate-limited");
  });

  it("preselects the first remaining agent (no codex bias) on manual switch", async () => {
    mockFetchAgents.mockResolvedValue([
      { name: "claude", description: "Claude", command: "claude-agent-acp" },
      { name: "opencode", description: "OpenCode", command: "opencode-acp" },
      { name: "codex", description: "OpenAI Codex", command: "codex-acp" },
    ]);
    const { findByText } = mount({ trigger: "manual" });
    // opencode comes before codex in the list, so it wins without the
    // rate-limit codex preference.
    await findByText(/Switch to opencode/);
  });
});
