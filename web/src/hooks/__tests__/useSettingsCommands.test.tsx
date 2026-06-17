// @vitest-environment jsdom
//
// Contract test for the per-setting command-palette entries (#2108). Asserts
// schema -> entry generation (local_only omitted), that writable toggles flip
// inline through the default profile, and that every other widget, elevation
// toggles, and read-only mode produce a jump instead of a write.

import { beforeEach, describe, expect, it, vi } from "vitest";
import { renderHook, waitFor } from "@testing-library/react";
import { useSettingsCommands } from "../useSettingsCommands";
import type { SettingsFieldDescriptor, SettingsWidget, SettingsWebWritePolicy } from "../../lib/types";

vi.mock("../../lib/api", () => ({
  getSettingsSchema: vi.fn(),
  fetchProfiles: vi.fn(),
  fetchSettings: vi.fn(),
  updateProfileSettings: vi.fn(),
}));
vi.mock("../../lib/toastBus", () => ({
  reportInfo: vi.fn(),
  reportError: vi.fn(),
}));

import { fetchProfiles, fetchSettings, getSettingsSchema, updateProfileSettings } from "../../lib/api";

function field(
  section: string,
  name: string,
  widget: SettingsWidget,
  web_write: SettingsWebWritePolicy,
  profile_overridable = true,
): SettingsFieldDescriptor {
  return {
    section,
    field: name,
    category: section,
    label: `${section}.${name}`,
    description: "",
    widget,
    web_write,
    profile_overridable,
    validation: { rule: "none" },
    advanced: false,
  };
}

const SCHEMA: SettingsFieldDescriptor[] = [
  field("session", "live_send", { kind: "toggle" }, { policy: "allow" }, true),
  field("worktree", "auto_cleanup", { kind: "toggle" }, { policy: "allow" }, false),
  field("security", "danger", { kind: "toggle" }, { policy: "requires_elevation", reason: "x" }),
  field("acp", "replay", { kind: "select", options: [] }, { policy: "allow" }),
  field("session", "secret", { kind: "toggle" }, { policy: "local_only", reason: "x" }),
];

beforeEach(() => {
  vi.clearAllMocks();
  vi.mocked(getSettingsSchema).mockResolvedValue(SCHEMA);
  vi.mocked(fetchProfiles).mockResolvedValue([{ name: "main", is_default: true }]);
  vi.mocked(fetchSettings).mockResolvedValue({
    session: { live_send: false },
    worktree: { auto_cleanup: true },
  } as never);
  vi.mocked(updateProfileSettings).mockResolvedValue(true);
});

function render(overrides: Partial<Parameters<typeof useSettingsCommands>[0]> = {}) {
  const onOpenSettingsTab = vi.fn();
  const hook = renderHook((args: Parameters<typeof useSettingsCommands>[0]) => useSettingsCommands(args), {
    initialProps: { open: true, readOnly: false, onOpenSettingsTab, ...overrides },
  });
  return { ...hook, onOpenSettingsTab };
}

describe("useSettingsCommands", () => {
  it("generates one Settings entry per writable field, omitting local_only", async () => {
    const { result } = render();
    await waitFor(() => expect(result.current.length).toBe(4));
    const ids = result.current.map((a) => a.id);
    expect(ids).toContain("setting:session.live_send");
    expect(ids).toContain("setting:worktree.auto_cleanup");
    expect(ids).toContain("setting:security.danger");
    expect(ids).toContain("setting:acp.replay");
    expect(ids).not.toContain("setting:session.secret");
    expect(result.current.every((a) => a.group === "Settings")).toBe(true);
  });

  it("flips a writable toggle inline through the default profile", async () => {
    const { result } = render();
    await waitFor(() => expect(result.current.length).toBe(4));
    const toggle = result.current.find((a) => a.id === "setting:session.live_send");
    expect(toggle?.subtitle).toBe("Off · main");
    toggle?.perform();
    await waitFor(() => expect(updateProfileSettings).toHaveBeenCalledWith("main", { session: { live_send: true } }));
  });

  it("labels a global-only toggle's scope as Global", async () => {
    const { result } = render();
    await waitFor(() => expect(result.current.length).toBe(4));
    const toggle = result.current.find((a) => a.id === "setting:worktree.auto_cleanup");
    expect(toggle?.subtitle).toBe("On · Global");
  });

  it("jumps for non-toggle widgets and elevation toggles, never writing", async () => {
    const { result, onOpenSettingsTab } = render();
    await waitFor(() => expect(result.current.length).toBe(4));
    result.current.find((a) => a.id === "setting:acp.replay")?.perform();
    expect(onOpenSettingsTab).toHaveBeenCalledWith("structured-view");
    result.current.find((a) => a.id === "setting:security.danger")?.perform();
    expect(onOpenSettingsTab).toHaveBeenCalledWith("security");
    expect(updateProfileSettings).not.toHaveBeenCalled();
  });

  it("turns every toggle into a jump in read-only mode", async () => {
    const { result, onOpenSettingsTab } = render({ readOnly: true });
    await waitFor(() => expect(result.current.length).toBe(4));
    result.current.find((a) => a.id === "setting:session.live_send")?.perform();
    expect(onOpenSettingsTab).toHaveBeenCalledWith("session");
    expect(updateProfileSettings).not.toHaveBeenCalled();
  });
});
