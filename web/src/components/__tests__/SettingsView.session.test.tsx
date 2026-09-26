// @vitest-environment jsdom
// Schema-driven Session and Structured view rows persist through the profile settings path, as in the TUI.

import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SettingsView } from "../SettingsView";
import * as api from "../../lib/api";

const PROFILES = [{ name: "main", is_default: true }];

const descriptor = (section: string, field: string, label: string, widget: Record<string, unknown>) => ({
  section,
  field,
  label,
  widget,
  category: section,
  description: "",
  web_write: { policy: "allow" },
  profile_overridable: true,
  validation: { rule: "none" },
  advanced: false,
});

const SESSION_SCHEMA = [
  descriptor("session", "auto_stop_idle_secs", "Auto-stop idle sessions (s)", { kind: "number", min: 0 }),
  descriptor("acp", "acp_defaults", "Structured View Defaults", { kind: "custom", id: "acp-defaults" }),
  descriptor("session", "show_diagnostics_pane", "Show system health strip", { kind: "toggle" }),
  descriptor("session", "smart_rename", "Smart Session Rename", { kind: "toggle" }),
  descriptor("session", "row_tag", "Row Tag", {
    kind: "select",
    options: ["none", "auto", "profile", "sandbox", "branch"].map((value) => ({ value, label: value })),
  }),
];

vi.mock("../../lib/api", () => ({
  fetchProfiles: vi.fn(() => Promise.resolve(PROFILES)),
  fetchPlugins: vi.fn(() => Promise.resolve(null)),
  fetchSettings: vi.fn(),
  getSettingsSchema: vi.fn(() => Promise.resolve(SESSION_SCHEMA)),
  updateProfileSettings: vi.fn(() => Promise.resolve(true)),
  setDefaultProfile: vi.fn(() => Promise.resolve(true)),
  createProfile: vi.fn(() => Promise.resolve(true)),
  renameProfile: vi.fn(() => Promise.resolve(true)),
  deleteProfile: vi.fn(() => Promise.resolve(true)),
  fetchAgents: vi.fn(() => Promise.resolve([])),
  fetchAcpOptionCatalog: vi.fn(() => Promise.resolve({ version: 1, agents: {} })),
}));

async function renderTab(tab: string, session: Record<string, unknown> = {}, waitFor: string) {
  vi.mocked(api.fetchSettings).mockResolvedValue({ session, acp: {}, sandbox: {}, worktree: {} } as never);
  const onSettingsRefresh = vi.fn();
  const view = render(
    <SettingsView
      onClose={() => {}}
      tab={tab}
      onSelectTab={() => {}}
      onServerAboutRefresh={() => {}}
      onSettingsRefresh={onSettingsRefresh}
    />,
  );
  await screen.findByText(waitFor);
  return { ...view, onSettingsRefresh };
}

/** The switch on the toggle row whose caption is `label`. The caption is plain
 *  text beside the control, not a `<label>`, and several ancestors share its
 *  text, so match the leaf and walk up to the row. Not by position: the section
 *  holds several switches in schema order. */
function toggleByLabel(container: HTMLElement, label: string): HTMLButtonElement {
  const caption = Array.from(container.querySelectorAll("div")).find(
    (el) => el.children.length === 0 && el.textContent === label,
  );
  const button = caption?.closest("div.justify-between")?.querySelector("button[role=switch]");
  expect(button).toBeTruthy();
  return button as HTMLButtonElement;
}

function commit(input: HTMLInputElement | HTMLTextAreaElement, value: string) {
  fireEvent.focus(input);
  fireEvent.change(input, { target: { value } });
  fireEvent.blur(input);
}

const autoStopInput = (container: HTMLElement) =>
  Array.from(container.querySelectorAll("label"))
    .find((l) => l.textContent === "Auto-stop idle sessions (s)")!
    .parentElement!.querySelector('input[type="number"]') as HTMLInputElement;

const expectSaved = (patch: Record<string, unknown>) =>
  waitFor(() => expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", patch));

describe("Session tab", () => {
  beforeEach(() => vi.clearAllMocks());

  it("renders and persists auto_stop_idle_secs (#1690)", async () => {
    const { container } = await renderTab("session", { auto_stop_idle_secs: 1800 }, "Auto-stop idle sessions (s)");
    await waitFor(() => expect(autoStopInput(container).value).toBe("1800"));
    commit(autoStopInput(container), "7200");
    await expectSaved({ session: { auto_stop_idle_secs: 7200 } });
  });

  it("persists smart_rename without refreshing app settings", async () => {
    const { container, onSettingsRefresh } = await renderTab("session", { smart_rename: true }, "Smart Session Rename");
    fireEvent.click(toggleByLabel(container, "Smart Session Rename"));
    await expectSaved({ session: { smart_rename: false } });
    expect(onSettingsRefresh).not.toHaveBeenCalled();
  });

  it("refreshes app-level settings after saving row_tag", async () => {
    const { container, onSettingsRefresh } = await renderTab("session", { row_tag: "branch" }, "Row Tag");
    const select = Array.from(container.querySelectorAll("select")).find((s) =>
      Array.from(s.options).some((o) => o.value === "sandbox"),
    )!;
    fireEvent.change(select, { target: { value: "none" } });
    await expectSaved({ session: { row_tag: "none" } });
    expect(onSettingsRefresh).toHaveBeenCalledTimes(1);
  });

  it("persists acp.acp_defaults on the Structured view tab through the raw-JSON fold", async () => {
    const { container } = await renderTab("structured-view", {}, "Structured View Defaults");
    fireEvent.click(screen.getByText("Advanced: edit raw JSON"));
    commit(container.querySelector("textarea")!, '{"opencode":{"model":"openai/gpt-5.5","effort":"high"}}');
    await expectSaved({ acp: { acp_defaults: { opencode: { model: "openai/gpt-5.5", effort: "high" } } } });
  });
});
