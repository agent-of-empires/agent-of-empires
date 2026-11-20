// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { PromptBlockedNote } from "../PromptBlockedNote";

const updateSettings = vi.fn();
vi.mock("../../lib/api", () => ({ updateSettings: (...a: unknown[]) => updateSettings(...a) }));

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

const LONG =
  "Claude Code is asking whether to trust this folder, so the prompt was not typed. Turn on `pre_trust_agent_folders` ...";

describe("PromptBlockedNote", () => {
  it("an untrusted folder: plain reason, Turn on pre-trust after a confirmation, then Try again", async () => {
    updateSettings.mockResolvedValue("committed");
    const onTryAgain = vi.fn();
    render(
      <PromptBlockedNote
        label="Prompt not sent"
        code="agent_folder_not_trusted"
        message={LONG}
        onTryAgain={onTryAgain}
      />,
    );
    expect(screen.getByRole("alert").textContent).toBe(
      "Prompt not sent: Claude Code is asking whether to trust this folder, so the prompt was not typed.",
    );
    fireEvent.click(screen.getByRole("button", { name: "Turn on pre-trust" }));
    expect(updateSettings).not.toHaveBeenCalled();
    expect(screen.getByText(/also runs the repo's own \.claude\/settings\.json hooks/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Turn on" }));
    await waitFor(() => expect(updateSettings).toHaveBeenCalledWith({ session: { pre_trust_agent_folders: true } }));
    fireEvent.click(await screen.findByRole("button", { name: "Try again" }));
    expect(onTryAgain).toHaveBeenCalled();
  });

  it("without Try again it says how to go on, and a failed save says so", async () => {
    updateSettings.mockResolvedValueOnce("rejected").mockResolvedValueOnce("committed");
    render(<PromptBlockedNote label="Initial prompt not sent" code="agent_folder_not_trusted" message={LONG} />);
    fireEvent.click(screen.getByRole("button", { name: "Turn on pre-trust" }));
    fireEvent.click(screen.getByRole("button", { name: "Turn on" }));
    expect(await screen.findByText("Couldn't turn on pre-trust.")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Turn on pre-trust" }));
    fireEvent.click(screen.getByRole("button", { name: "Turn on" }));
    expect(await screen.findByText(/Pre-trust is on: new Sessions start trusted/)).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Try again" })).toBeNull();
  });

  it("Bypass Permissions and unknown codes have no action", () => {
    const { rerender } = render(
      <PromptBlockedNote
        label="Prompt not sent"
        code="agent_bypass_not_accepted"
        message="long"
        onTryAgain={vi.fn()}
      />,
    );
    expect(screen.getByRole("alert").textContent).toBe(
      "Prompt not sent: Claude Code is waiting for Bypass Permissions to be accepted once. Open the Session and accept it.",
    );
    expect(screen.queryByRole("button")).toBeNull();
    rerender(<PromptBlockedNote label="Prompt not sent" code="something_new" message="the agent was not ready" />);
    expect(screen.getByRole("alert").textContent).toBe("Prompt not sent: the agent was not ready");
  });

  it("read-only shows the reason without actions", () => {
    render(<PromptBlockedNote label="Prompt not sent" code="agent_folder_not_trusted" message={LONG} disabled />);
    expect(screen.queryByRole("button")).toBeNull();
  });
});
