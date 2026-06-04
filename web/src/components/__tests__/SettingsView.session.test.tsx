// @vitest-environment jsdom
//
// Behavioral coverage for the Session tab's "Auto-stop idle sessions" number
// field (#1690): a persisted value renders into the field, and committing a
// value persists `session.auto_stop_idle_secs` through the normal
// profile-settings path, the same wiring the TUI uses.

import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SettingsView } from "../SettingsView";
import * as api from "../../lib/api";

const PROFILES = [{ name: "main", is_default: true }];

vi.mock("../../lib/api", () => ({
  fetchProfiles: vi.fn(() => Promise.resolve(PROFILES)),
  fetchSettings: vi.fn(() =>
    Promise.resolve({ session: {}, acp: {}, sandbox: {}, worktree: {} }),
  ),
  updateProfileSettings: vi.fn(() => Promise.resolve(true)),
  setDefaultProfile: vi.fn(() => Promise.resolve(true)),
  createProfile: vi.fn(() => Promise.resolve(true)),
  renameProfile: vi.fn(() => Promise.resolve(true)),
  deleteProfile: vi.fn(() => Promise.resolve(true)),
}));

const SERVER_ABOUT = {
  acp_show_tool_durations: true,
  acp_queue_drain_mode: "combined" as const,
  acp_max_concurrent_resumes: 4,
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

function textareaByLabel(container: HTMLElement, label: string): HTMLTextAreaElement {
  const labels = Array.from(container.querySelectorAll("label"));
  const match = labels.find((l) => l.textContent === label);
  const textarea = match?.parentElement?.querySelector("textarea");
  expect(textarea).toBeTruthy();
  return textarea as HTMLTextAreaElement;
}

function commitTextarea(input: HTMLTextAreaElement, value: string) {
  fireEvent.focus(input);
  fireEvent.change(input, { target: { value } });
  fireEvent.blur(input);
}

describe("Session tab auto-stop idle field", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    // clearAllMocks resets call history but not implementations, so restore
    // the empty-settings default here to isolate tests that override it.
    vi.mocked(api.fetchSettings).mockResolvedValue({
      session: {},
      acp: {},
      sandbox: {},
      worktree: {},
    } as never);
  });

  it("renders the persisted auto_stop_idle_secs value into the field", async () => {
    vi.mocked(api.fetchSettings).mockResolvedValue({
      session: { auto_stop_idle_secs: 1800 },
      acp: {},
      sandbox: {},
      worktree: {},
    } as never);

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

    await waitFor(() =>
      expect(
        numberInputByLabel(container, "Auto-stop idle sessions (s)").value,
      ).toBe("1800"),
    );
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

  it("persists session.acp_defaults through the profile path", async () => {
    const { container } = render(
      <SettingsView
        onClose={() => {}}
        tab="session"
        onSelectTab={() => {}}
        serverAbout={SERVER_ABOUT as never}
        onServerAboutRefresh={() => {}}
      />,
    );
    await screen.findByText("Structured view defaults");

    commitTextarea(
      textareaByLabel(container, "Structured view defaults"),
      '{"opencode":{"model":"openai/gpt-5.5","effort":"high"}}',
    );

    await waitFor(() =>
      expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
        session: {
          acp_defaults: {
            opencode: { model: "openai/gpt-5.5", effort: "high" },
          },
        },
      }),
    );
  });
});
