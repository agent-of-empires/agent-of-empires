// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import { SettingsView } from "../SettingsView";

const PROFILES = [{ name: "main", is_default: true }];

vi.mock("../../lib/api", () => ({
  fetchProfiles: vi.fn(() => Promise.resolve(PROFILES)),
  fetchSettings: vi.fn(() =>
    Promise.resolve({ acp: {}, sandbox: {}, worktree: {} }),
  ),
  updateProfileSettings: vi.fn(() => Promise.resolve(true)),
  setDefaultProfile: vi.fn(() => Promise.resolve(true)),
  createProfile: vi.fn(() => Promise.resolve(true)),
  renameProfile: vi.fn(() => Promise.resolve(true)),
  deleteProfile: vi.fn(() => Promise.resolve(true)),
}));

afterEach(() => {
  window.localStorage.clear();
});

describe("SettingsView diff tab", () => {
  it("renders the Diff section (split + tree toggles)", async () => {
    render(
      <SettingsView
        onClose={() => {}}
        tab="diff"
        onSelectTab={vi.fn()}
        serverAbout={null}
        onServerAboutRefresh={() => {}}
      />,
    );

    expect(await screen.findByText("Side-by-side diff")).toBeTruthy();
    expect(screen.getByText("Tree file list")).toBeTruthy();
  });
});
