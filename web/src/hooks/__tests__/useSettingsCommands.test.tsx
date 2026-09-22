// @vitest-environment jsdom

import { beforeEach, describe, expect, it, vi } from "vitest";
import { act, renderHook, waitFor } from "@testing-library/react";
import { useSettingsCommands } from "../useSettingsCommands";
import type { SettingsFieldDescriptor, SettingsWidget, SettingsWebWritePolicy } from "../../lib/types";

vi.mock("../../lib/api", () => ({
  getSettingsSchema: vi.fn(),
  fetchProfiles: vi.fn(),
  fetchSettings: vi.fn(),
  updateProfileSettings: vi.fn(),
  updateSettings: vi.fn(),
}));
vi.mock("../../lib/toastBus", () => ({
  reportInfo: vi.fn(),
  reportError: vi.fn(),
}));

import { fetchProfiles, fetchSettings, getSettingsSchema, updateProfileSettings, updateSettings } from "../../lib/api";

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
  field("telemetry", "enabled", { kind: "toggle" }, { policy: "allow" }, false),
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
  vi.mocked(updateSettings).mockResolvedValue(true);
});

async function render(overrides: Partial<Parameters<typeof useSettingsCommands>[0]> = {}) {
  const onOpenSettingsTab = vi.fn();
  const hook = renderHook((args: Parameters<typeof useSettingsCommands>[0]) => useSettingsCommands(args), {
    initialProps: { open: true, readOnly: false, onOpenSettingsTab, ...overrides },
  });
  await waitFor(() => expect(hook.result.current.length).toBe(5));
  const action = (id: string) => hook.result.current.find((a) => a.id === id);
  await waitFor(() =>
    expect(action("setting:worktree.auto_cleanup")?.subtitle).toBe(
      overrides.readOnly ? "Opens settings · worktree" : "On · Global",
    ),
  );
  return { ...hook, onOpenSettingsTab, action };
}

describe("useSettingsCommands", () => {
  it("opens settings while values load and toggles only the loaded value", async () => {
    let resolveSettings!: (value: Awaited<ReturnType<typeof fetchSettings>>) => void;
    vi.mocked(fetchSettings).mockReturnValueOnce(
      new Promise((resolve) => {
        resolveSettings = resolve;
      }),
    );
    const onOpenSettingsTab = vi.fn();
    const { result } = renderHook(() => useSettingsCommands({ open: true, readOnly: false, onOpenSettingsTab }));
    const toggle = () => result.current.find((action) => action.id === "setting:worktree.auto_cleanup");
    await waitFor(() => expect(toggle()).toBeDefined());
    expect(toggle()?.subtitle).toBe("Opens settings · worktree");
    toggle()?.perform();
    expect(onOpenSettingsTab).toHaveBeenCalledWith("worktree");
    expect(updateSettings).not.toHaveBeenCalled();
    expect(updateProfileSettings).not.toHaveBeenCalled();

    await act(async () => resolveSettings({ worktree: { auto_cleanup: true } } as never));
    await waitFor(() => expect(toggle()?.subtitle).toBe("On · Global"));
    toggle()?.perform();
    await waitFor(() => expect(updateSettings).toHaveBeenCalledWith({ worktree: { auto_cleanup: false } }));
  });

  it("generates one Settings entry per writable field, omitting local_only", async () => {
    const { result } = await render();
    const ids = result.current.map((a) => a.id);
    expect(ids).toContain("setting:session.live_send");
    expect(ids).toContain("setting:worktree.auto_cleanup");
    expect(ids).toContain("setting:security.danger");
    expect(ids).toContain("setting:acp.replay");
    expect(ids).not.toContain("setting:session.secret");
    expect(result.current.every((a) => a.group === "Settings")).toBe(true);
  });

  it("does not reuse cached values while refreshing or reopening for another profile", async () => {
    for (const reopen of [false, true]) {
      const { action, rerender, unmount, onOpenSettingsTab } = await render();
      const fetchCount = vi.mocked(fetchSettings).mock.calls.length;
      let resolveSettings!: (value: Awaited<ReturnType<typeof fetchSettings>>) => void;
      vi.mocked(fetchSettings).mockReturnValueOnce(
        new Promise((resolve) => {
          resolveSettings = resolve;
        }),
      );
      vi.mocked(fetchProfiles).mockResolvedValueOnce([{ name: "alternate", is_default: true }]);
      if (reopen) {
        rerender({ open: false, readOnly: false, onOpenSettingsTab });
        rerender({ open: true, readOnly: false, onOpenSettingsTab });
      } else {
        action("setting:worktree.auto_cleanup")?.perform();
      }
      await waitFor(() => expect(fetchSettings).toHaveBeenCalledTimes(fetchCount + 1));
      expect(fetchSettings).toHaveBeenLastCalledWith("alternate");
      expect(action("setting:session.live_send")?.subtitle).toBe("Opens settings · session");
      const saveCount = vi.mocked(updateProfileSettings).mock.calls.length;
      action("setting:session.live_send")?.perform();
      expect(onOpenSettingsTab).toHaveBeenCalledWith("session");
      expect(updateProfileSettings).toHaveBeenCalledTimes(saveCount);

      await act(async () =>
        resolveSettings({ session: { live_send: true }, worktree: { auto_cleanup: false } } as never),
      );
      await waitFor(() => expect(action("setting:session.live_send")?.subtitle).toBe("On · alternate"));
      action("setting:session.live_send")?.perform();
      await waitFor(() => expect(updateProfileSettings).toHaveBeenCalledTimes(saveCount + 1));
      expect(updateProfileSettings).toHaveBeenLastCalledWith("alternate", { session: { live_send: false } });
      unmount();
    }
  });

  it("flips a writable toggle inline through the default profile", async () => {
    const { result } = await render();
    const toggle = result.current.find((a) => a.id === "setting:session.live_send");
    expect(toggle?.subtitle).toBe("Off · main");
    toggle?.perform();
    await waitFor(() => expect(updateProfileSettings).toHaveBeenCalledWith("main", { session: { live_send: true } }));
  });

  it("saves a global-only toggle at the scope named in its subtitle", async () => {
    const { result } = await render();
    const toggle = result.current.find((a) => a.id === "setting:worktree.auto_cleanup");
    expect(toggle?.subtitle).toBe("On · Global");
    toggle?.perform();
    await waitFor(() => expect(updateSettings).toHaveBeenCalledWith({ worktree: { auto_cleanup: false } }));
    expect(updateProfileSettings).not.toHaveBeenCalled();
  });

  it("opens settings for non-toggle widgets, elevation, and telemetry consent", async () => {
    const { onOpenSettingsTab, action } = await render();
    action("setting:acp.replay")?.perform();
    expect(onOpenSettingsTab).toHaveBeenCalledWith("structured-view");
    action("setting:security.danger")?.perform();
    expect(onOpenSettingsTab).toHaveBeenCalledWith("security");
    action("setting:telemetry.enabled")?.perform();
    expect(onOpenSettingsTab).toHaveBeenCalledWith("telemetry");
    expect(updateProfileSettings).not.toHaveBeenCalled();
    expect(updateSettings).not.toHaveBeenCalled();
  });

  it("turns every toggle into a jump in read-only mode", async () => {
    const { onOpenSettingsTab, action } = await render({ readOnly: true });
    action("setting:session.live_send")?.perform();
    expect(onOpenSettingsTab).toHaveBeenCalledWith("session");
    expect(updateProfileSettings).not.toHaveBeenCalled();
  });
});
