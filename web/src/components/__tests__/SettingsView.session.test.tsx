// @vitest-environment jsdom
//
// Behavioral coverage for the Session tab's "Auto-stop idle sessions" number
// field (#1690): committing a value persists `session.auto_stop_idle_secs`
// through the normal profile-settings path, the same wiring the TUI uses.
//
// The end-to-end persist-through-reload path lives in live Playwright at
// web/tests/live/settings-persistence-session.spec.ts.

import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SettingsView } from "../SettingsView";
import * as api from "../../lib/api";

const PROFILES = [{ name: "main", is_default: true }];

vi.mock("../../lib/api", () => ({
  fetchProfiles: vi.fn(() => Promise.resolve(PROFILES)),
  fetchSettings: vi.fn(() =>
    Promise.resolve({ session: {}, cockpit: {}, sandbox: {}, worktree: {} }),
  ),
  updateProfileSettings: vi.fn(() => Promise.resolve(true)),
  setCockpitMaster: vi.fn(() => Promise.resolve(true)),
  setDefaultProfile: vi.fn(() => Promise.resolve(true)),
  createProfile: vi.fn(() => Promise.resolve(true)),
  renameProfile: vi.fn(() => Promise.resolve(true)),
  deleteProfile: vi.fn(() => Promise.resolve(true)),
}));

const SERVER_ABOUT = {
  cockpit_master_enabled: true,
  cockpit_show_tool_durations: true,
  cockpit_queue_drain_mode: "combined" as const,
  cockpit_max_concurrent_resumes: 4,
};

function numberInputByLabel(
  container: HTMLElement,
  label: string,
): HTMLInputElement {
  const labels = Array.from(container.querySelectorAll("label"));
  const match = labels.find((l) => l.textContent === label);
  const input = match?.parentElement?.querySelector('input[type="number"]');
  expect(input).toBeTruthy();
  return input as HTMLInputElement;
}

function commit(input: HTMLInputElement, value: string) {
  fireEvent.focus(input);
  fireEvent.change(input, { target: { value } });
  fireEvent.blur(input);
}

describe("Session tab auto-stop idle field", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("persists session.auto_stop_idle_secs through the profile path", async () => {
    const { container } = render(
      <SettingsView
        onClose={() => {}}
        tab="session"
        onSelectTab={() => {}}
        serverAbout={SERVER_ABOUT as never}
        onServerAboutRefresh={() => {}}
      />,
    );
    await screen.findByText("Auto-stop idle sessions (s)");

    commit(numberInputByLabel(container, "Auto-stop idle sessions (s)"), "7200");

    await waitFor(() =>
      expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
        session: { auto_stop_idle_secs: 7200 },
      }),
    );
  });
});
