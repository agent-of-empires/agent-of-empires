// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi, beforeEach } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
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
  updateProfileSettings: vi.fn(() => Promise.resolve("committed" as const)),
  updateSettings: vi.fn(() => Promise.resolve("committed" as const)),
  setDefaultProfile: vi.fn(() => Promise.resolve(true)),
}));

afterEach(() => {
  cleanup();
  localStorage.clear();
});

function renderView(tab: string | null, field?: string) {
  const onSelectTab = vi.fn();
  const onShowList = vi.fn();
  render(
    <SettingsView
      onClose={() => {}}
      tab={tab}
      onSelectTab={onSelectTab}
      onShowList={onShowList}
      onServerAboutRefresh={() => {}}
      field={field}
    />,
  );
  return { onSelectTab, onShowList };
}

import { resetSettingsStore } from "../../lib/settingsStore";

beforeEach(resetSettingsStore);

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

  describe("?field= deep link", () => {
    const highlighted = () => document.querySelector(".animate-settings-highlight");

    it("highlights and scrolls to the row on the open tab", async () => {
      const scrollIntoView = vi.fn();
      Element.prototype.scrollIntoView = scrollIntoView;
      renderView("sandbox", "sandbox.enabled_by_default");
      await waitFor(() => expect(scrollIntoView).toHaveBeenCalled());
      expect(highlighted()?.getAttribute("data-settings-field")).toBe("sandbox.enabled_by_default");
    });

    it("refocuses when the same field returns after it was dropped (browser Back)", async () => {
      const scrollIntoView = vi.fn();
      Element.prototype.scrollIntoView = scrollIntoView;
      const view = (field?: string) => (
        <SettingsView
          onClose={() => {}}
          tab="sandbox"
          onSelectTab={() => {}}
          onServerAboutRefresh={() => {}}
          field={field}
        />
      );
      const { rerender } = render(view("sandbox.enabled_by_default"));
      await waitFor(() => expect(scrollIntoView).toHaveBeenCalledTimes(1));
      rerender(view(undefined));
      rerender(view("sandbox.enabled_by_default"));
      await waitFor(() => expect(scrollIntoView).toHaveBeenCalledTimes(2));
    });

    it.each([["sandbox.nope"], ["updates.check_mode"]])("opens the tab without error for %s", async (field) => {
      renderView("sandbox", field);
      await screen.findByText("enabled_by_default");
      expect(highlighted()).toBeNull();
    });
  });
});
