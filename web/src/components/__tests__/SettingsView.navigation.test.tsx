// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { SettingsView } from "../SettingsView";

const SCHEMA = [
  ["sandbox", "enabled_by_default", true],
  ["updates", "check_mode", false],
].map(([section, field, profile_overridable]) => ({
  section,
  field,
  label: String(field),
  widget: { kind: "toggle" },
  advanced: false,
  category: section,
  description: "",
  web_write: { policy: "allow" },
  profile_overridable,
  validation: { rule: "none" },
}));

vi.mock("../../lib/api", () => ({
  fetchProfiles: vi.fn(() => Promise.resolve([{ name: "main", is_default: true }])),
  fetchSettings: vi.fn(() => Promise.resolve({ sandbox: {}, updates: {} })),
  getSettingsSchema: vi.fn(() => Promise.resolve(SCHEMA)),
  updateProfileSettings: vi.fn(() => Promise.resolve(true)),
  updateSettings: vi.fn(() => Promise.resolve(true)),
  setDefaultProfile: vi.fn(() => Promise.resolve(true)),
}));

afterEach(() => {
  cleanup();
  localStorage.clear();
});

function renderView(tab: string | null) {
  const onSelectTab = vi.fn();
  const onShowList = vi.fn();
  render(
    <SettingsView
      onClose={() => {}}
      tab={tab}
      onSelectTab={onSelectTab}
      onShowList={onShowList}
      onServerAboutRefresh={() => {}}
    />,
  );
  return { onSelectTab, onShowList };
}

describe("SettingsView navigation", () => {
  it("without a tab lists grouped sections, remembering the pick for the next open", () => {
    const { onSelectTab } = renderView(null);
    const list = screen.getByTestId("settings-section-list");
    expect(within(list).getByText("Dashboard")).toBeTruthy();
    fireEvent.click(within(list).getByRole("button", { name: /Notifications/ }));
    expect(onSelectTab).toHaveBeenCalledWith("notifications");

    cleanup();
    renderView(null);
    expect(screen.getByRole("heading", { level: 2 }).textContent).toBe("Notifications");
  });

  it("on a tab page, the header returns to the section list", () => {
    const { onShowList } = renderView("sandbox");
    expect(screen.queryByTestId("settings-section-list")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: /All settings/ }));
    expect(onShowList).toHaveBeenCalled();
  });

  it.each([
    ["sandbox", "enabled_by_default", true],
    ["updates", "check_mode", false],
  ])("on %s, shows the profile picker only for profile-overridable fields", async (tab, field, shown) => {
    renderView(tab);
    // The field renders from the same schema, so the picker has had its chance.
    await screen.findByText(field);
    expect(!!screen.queryByTestId("settings-profile-picker")).toBe(shown);
  });
});
