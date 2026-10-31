// @vitest-environment jsdom
import { act, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

// A failed fetch resolves null.
vi.mock("./api", () => ({ fetchAgents: vi.fn(() => Promise.resolve(null)) }));
import { fetchAgents } from "./api";

import { resolveAgentDisplayName, setAgentNames, useAgentDisplayNames } from "./agentDisplayName";
import type { AgentInfo } from "./types";

const known = new Map([["claude", "Claude Code"]]);

describe("resolveAgentDisplayName", () => {
  it.each([
    ["claude", "Claude Code"],
    ["my-agent", "my-agent"],
    // Custom registry keys that are also Object.prototype members keep their own name.
    ["constructor", "constructor"],
    ["toString", "toString"],
    ["__proto__", "__proto__"],
    [null, "the Agent"],
  ])("%s → %s", (agent, name) => {
    expect(resolveAgentDisplayName(agent, known)).toBe(name);
  });
});

describe("useAgentDisplayNames", () => {
  it("re-renders with the server's name once it arrives", () => {
    function Name() {
      const nameOf = useAgentDisplayNames();
      return <p>{nameOf("pi")}</p>;
    }
    render(<Name />);
    expect(screen.getByText("pi")).toBeTruthy();

    act(() => setAgentNames([{ name: "pi", display_name: "Pi Agent" } as AgentInfo]));
    expect(screen.getByText("Pi Agent")).toBeTruthy();
  });

  it("retries a failed load when connectivity returns", async () => {
    function Name() {
      const nameOf = useAgentDisplayNames();
      return <p>{nameOf("kimi")}</p>;
    }
    render(<Name />);
    await act(async () => {});
    expect(screen.getByText("kimi")).toBeTruthy();

    vi.mocked(fetchAgents).mockResolvedValueOnce([{ name: "kimi", display_name: "Kimi Code" } as AgentInfo]);
    await act(async () => {
      window.dispatchEvent(new Event("online"));
    });
    expect(screen.getByText("Kimi Code")).toBeTruthy();
  });
});
