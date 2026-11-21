import { afterEach, describe, expect, it, vi } from "vitest";

const api = vi.hoisted(() => ({
  fetchSettings: vi.fn(),
  getProfileSettings: vi.fn(),
  updateSettings: vi.fn(),
}));
vi.mock("./api", () => api);

import {
  agentDefaultsHref,
  agentModelDefault,
  loadAgentModelDefault,
  saveAgentModelDefault,
  withDefaultModel,
} from "./agentModelDefault";

const settings = (acp_defaults: Record<string, unknown>) => ({ acp: { acp_defaults } });

afterEach(() => vi.clearAllMocks());

describe("agentModelDefault", () => {
  it.each([
    ["no entry: the Agent picks its own", {}, "claude", { model: null, pinned: false }],
    ["a configured model", { claude: { model: "opus" } }, "claude", { model: "opus", pinned: false }],
    [
      "a blank model reads as unset",
      { claude: { model: "  ", effort: "high" } },
      "claude",
      { model: null, pinned: false },
    ],
    ["a pin", { claude: { model: "opus", pin_model: true } }, "claude", { model: "opus", pinned: true }],
    ["a pin without a model is no pin", { claude: { pin_model: true } }, "claude", { model: null, pinned: false }],
    ["legacy claude-code reads claude", { claude: { model: "opus" } }, "claude-code", { model: "opus", pinned: false }],
    [
      "claude-code's own entry wins",
      { claude: { model: "opus" }, "claude-code": { model: "sonnet" } },
      "claude-code",
      { model: "sonnet", pinned: false },
    ],
  ])("%s", (_label, map, agent, want) => {
    expect(agentModelDefault(settings(map), agent)).toEqual(want);
  });
});

describe("withDefaultModel", () => {
  it.each([
    [
      "changes only the entry's model, keeping its other fields and other Agents",
      { claude: { effort: "high", mode: "plan", effort_by_model: { opus: "max" } }, codex: { model: "gpt" } },
      {},
      "claude",
      {
        claude: { effort: "high", mode: "plan", effort_by_model: { opus: "max" }, model: "sonnet" },
        codex: { model: "gpt" },
      },
    ],
    [
      "adds an entry the profile doesn't have, leaving inherited fields to global",
      {},
      {},
      "claude",
      { claude: { model: "sonnet" } },
    ],
    [
      "writes legacy claude-code into the claude entry it reads",
      {},
      { claude: { effort: "high" } },
      "claude-code",
      { claude: { model: "sonnet" } },
    ],
  ])("%s", (_label, override, resolved, agent, want) => {
    expect(withDefaultModel(settings(override), settings(resolved), agent, "sonnet")).toEqual(want);
  });
});

describe("saveAgentModelDefault", () => {
  it("writes the profile's own map back with only the model changed", async () => {
    api.getProfileSettings.mockResolvedValue(settings({ claude: { pin_model: false, mode: "plan" } }));
    api.fetchSettings.mockResolvedValue(
      settings({ claude: { effort: "high", mode: "plan" }, codex: { model: "gpt" } }),
    );
    api.updateSettings.mockResolvedValue("committed");
    expect(await saveAgentModelDefault("work", "claude", "sonnet")).toBe("committed");
    expect(api.getProfileSettings).toHaveBeenCalledWith("work");
    expect(api.updateSettings).toHaveBeenCalledExactlyOnceWith(
      { acp: { acp_defaults: { claude: { pin_model: false, mode: "plan", model: "sonnet" } } } },
      "work",
    );
  });

  it.each([
    ["the profile override", null, settings({})],
    ["the resolved settings", settings({}), null],
  ])("writes nothing when %s can't be read", async (_label, override, resolved) => {
    api.getProfileSettings.mockResolvedValue(override);
    api.fetchSettings.mockResolvedValue(resolved);
    expect(await saveAgentModelDefault("work", "claude", "sonnet")).toBe("rejected");
    expect(api.updateSettings).not.toHaveBeenCalled();
  });
});

describe("loadAgentModelDefault", () => {
  it("reads the Session's profile, resolved, and gives null when it can't", async () => {
    api.fetchSettings.mockResolvedValueOnce(settings({ claude: { model: "sonnet" } })).mockResolvedValueOnce(null);
    expect(await loadAgentModelDefault("work", "claude")).toEqual({ model: "sonnet", pinned: false });
    expect(api.fetchSettings).toHaveBeenCalledWith("work");
    expect(await loadAgentModelDefault("work", "claude")).toBeNull();
  });
});

describe("agentDefaultsHref", () => {
  it.each([
    ["before /api/about answers", null, null],
    ["read-only", { read_only: true, cityhall_mode: false }, null],
    ["CityHall", { read_only: false, cityhall_mode: true }, null],
    [
      "writable",
      { read_only: false, cityhall_mode: false },
      "/settings/structured-view?profile=my%20work&field=acp.acp_defaults",
    ],
  ])("%s", (_label, about, want) => {
    expect(agentDefaultsHref("my work", about as Parameters<typeof agentDefaultsHref>[1])).toBe(want);
  });
});
