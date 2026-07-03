// @vitest-environment jsdom
//
// Behavioral coverage for the rebuilt per-agent structured-view defaults widget
// (#2631): dropdowns populated from the recall catalog, free-text fallback when
// the catalog is empty, unverified preservation of stale values, per-model
// thinking overrides, and pruning empty agent entries out of the saved map.

import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { AcpDefaultsWidget } from "../AcpDefaultsWidget";
import * as api from "../../../lib/api";

// FormFields' SelectField/TextField render the label as a sibling, not
// associated by id, so findByLabelText misses them; locate the control by its
// label's container instead (the same pattern the session-tab test uses).
async function controlByLabel<T extends Element>(label: string, tag: string): Promise<T> {
  const labelEl = await screen.findByText(label);
  const control = labelEl.parentElement?.querySelector(tag);
  expect(control).toBeTruthy();
  return control as T;
}

vi.mock("../../../lib/api", () => ({
  fetchAgents: vi.fn(),
  fetchAcpOptionCatalog: vi.fn(),
}));

const DESCRIPTOR = {
  section: "session",
  field: "acp_defaults",
  category: "Session",
  label: "Structured View Defaults",
  description: "",
  widget: { kind: "custom" as const, id: "acp-defaults" },
  web_write: { policy: "allow" as const },
  profile_overridable: true,
  validation: { rule: "none" as const },
  advanced: false,
};

function agent(name: string, acp_capable = true) {
  return {
    kind: "builtin",
    name,
    binary: name,
    host_only: false,
    installed: true,
    install_hint: "",
    oneshot_capable: false,
    acp_capable,
    acp_installed: acp_capable,
  };
}

function catalog(agents: Record<string, unknown>) {
  return { version: 1, agents };
}

const OPENCODE_ENTRY = {
  updated_at: "2026-07-03T00:00:00Z",
  options: [
    {
      id: "model",
      name: "Model",
      category: "model",
      current_value: "",
      options: [
        { value: "openai/gpt-5.5", name: "GPT-5.5" },
        { value: "anthropic/opus", name: "Opus" },
      ],
    },
    {
      id: "thought_level",
      name: "Thinking",
      category: "thought_level",
      current_value: "",
      options: [
        { value: "low", name: "Low" },
        { value: "high", name: "High" },
      ],
    },
  ],
};

beforeEach(() => {
  vi.mocked(api.fetchAgents).mockResolvedValue([agent("opencode")] as never);
  vi.mocked(api.fetchAcpOptionCatalog).mockResolvedValue(catalog({ opencode: OPENCODE_ENTRY }) as never);
});

it("renders a model dropdown from the catalog and saves the selection", async () => {
  const save = vi.fn();
  render(<AcpDefaultsWidget descriptor={DESCRIPTOR} value={{}} save={save} />);
  const select = await controlByLabel<HTMLSelectElement>("Default model", "select");
  // Adapter default + two advertised models.
  expect(Array.from(select.options).map((o) => o.value)).toEqual(["", "openai/gpt-5.5", "anthropic/opus"]);
  fireEvent.change(select, { target: { value: "anthropic/opus" } });
  expect(save).toHaveBeenCalledWith({ opencode: { model: "anthropic/opus" } });
});

it("falls back to a free-text input when the catalog has no options for the agent", async () => {
  vi.mocked(api.fetchAcpOptionCatalog).mockResolvedValue(catalog({}) as never);
  const save = vi.fn();
  render(<AcpDefaultsWidget descriptor={DESCRIPTOR} value={{}} save={save} />);
  const input = await controlByLabel<HTMLInputElement>("Default model", "input");
  expect(input.tagName).toBe("INPUT");
  fireEvent.focus(input);
  fireEvent.change(input, { target: { value: "custom/model" } });
  fireEvent.blur(input);
  expect(save).toHaveBeenCalledWith({ opencode: { model: "custom/model" } });
});

it("keeps a saved value not in the catalog as an unverified option", async () => {
  const save = vi.fn();
  render(
    <AcpDefaultsWidget descriptor={DESCRIPTOR} value={{ opencode: { model: "openai/gpt-9-preview" } }} save={save} />,
  );
  const select = await controlByLabel<HTMLSelectElement>("Default model", "select");
  expect(select.value).toBe("openai/gpt-9-preview");
  expect(Array.from(select.options).map((o) => o.textContent)).toContain("openai/gpt-9-preview (unverified)");
});

it("adds a per-model thinking override from catalog models", async () => {
  const save = vi.fn();
  render(<AcpDefaultsWidget descriptor={DESCRIPTOR} value={{}} save={save} />);
  const adder = (await screen.findByLabelText("Add per-model thinking override for opencode")) as HTMLSelectElement;
  fireEvent.change(adder, { target: { value: "openai/gpt-5.5" } });
  // Defaults to the first advertised thinking level.
  expect(save).toHaveBeenCalledWith({
    opencode: { effort_by_model: { "openai/gpt-5.5": "low" } },
  });
});

it("prunes an agent entry from the map when its last value is cleared", async () => {
  const save = vi.fn();
  render(<AcpDefaultsWidget descriptor={DESCRIPTOR} value={{ opencode: { model: "openai/gpt-5.5" } }} save={save} />);
  const select = await controlByLabel<HTMLSelectElement>("Default model", "select");
  fireEvent.change(select, { target: { value: "" } });
  expect(save).toHaveBeenCalledWith({});
});

describe("no acp-capable agents", () => {
  it("shows an empty-state message", async () => {
    vi.mocked(api.fetchAgents).mockResolvedValue([agent("claude", false)] as never);
    render(<AcpDefaultsWidget descriptor={DESCRIPTOR} value={{}} save={vi.fn()} />);
    await waitFor(() => expect(screen.getByText("No ACP-capable agents detected.")).toBeTruthy());
  });
});
