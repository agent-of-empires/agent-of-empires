// @vitest-environment jsdom
//
// Switch-account dialog: names come from the server, confirm posts the restart
// route through switchSessionAccount, and a refusal is shown as the server worded it.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { SwitchAccountModal } from "./SwitchAccountModal";

vi.mock("../../lib/api", () => ({
  fetchAccountTargets: vi.fn(),
  switchSessionAccount: vi.fn(),
}));

import { fetchAccountTargets, switchSessionAccount } from "../../lib/api";

const mockTargets = vi.mocked(fetchAccountTargets);
const mockSwitch = vi.mocked(switchSessionAccount);

const authStatus = {
  kind: "account" as const,
  label: "Claude Max",
  account: { email: "someone@example.com", plan: "Max" },
};

function renderModal(props: Partial<React.ComponentProps<typeof SwitchAccountModal>> = {}) {
  const onClose = vi.fn();
  render(
    <SwitchAccountModal
      open
      sessionId="s1"
      currentTool="claude-a"
      authStatus={authStatus}
      onClose={onClose}
      {...props}
    />,
  );
  return onClose;
}

beforeEach(() => {
  vi.clearAllMocks();
  mockTargets.mockResolvedValue(["claude-b", "claude-c"]);
  mockSwitch.mockResolvedValue({ ok: true });
});
afterEach(cleanup);

describe("SwitchAccountModal", () => {
  it("lists the names, states the carry and cache cost, and shows only label and plan", async () => {
    renderModal();
    expect(await screen.findByLabelText("claude-b")).toBeTruthy();
    expect(screen.getByLabelText("claude-c")).toBeTruthy();
    const dialog = screen.getByRole("dialog");
    expect(dialog.textContent).toContain("copied into the other account's store");
    expect(dialog.textContent).toContain("uncached on the new account");
    expect(dialog.textContent).toContain("claude-a");
    expect(dialog.textContent).toContain("Claude Max");
    expect(dialog.textContent).not.toContain("someone@example.com");
  });

  it("confirms the picked account and closes on success", async () => {
    const onClose = renderModal();
    fireEvent.click(await screen.findByLabelText("claude-c"));
    fireEvent.click(screen.getByTestId("switch-account-confirm"));
    await waitFor(() => expect(onClose).toHaveBeenCalled());
    expect(mockSwitch).toHaveBeenCalledWith("s1", "claude-c");
  });

  it("shows the server's refusal and stays open", async () => {
    mockSwitch.mockResolvedValue({
      ok: false,
      message: "Cannot switch to 'claude-b' without losing the conversation.",
    });
    const onClose = renderModal();
    await screen.findByLabelText("claude-b");
    fireEvent.click(screen.getByTestId("switch-account-confirm"));
    expect((await screen.findByTestId("switch-account-error")).textContent).toContain(
      "without losing the conversation",
    );
    expect(onClose).not.toHaveBeenCalled();
  });

  it.each([
    [[], "No other account can take this conversation over"],
    [null, "Couldn't list the accounts"],
  ])("with targets %j explains and offers no confirm", async (targets, text) => {
    mockTargets.mockResolvedValue(targets);
    renderModal();
    expect((await screen.findByTestId("switch-account-empty")).textContent).toContain(text);
    expect((screen.getByTestId("switch-account-confirm") as HTMLButtonElement).disabled).toBe(true);
  });

  it("uses names the caller already loaded without fetching", async () => {
    renderModal({ targets: ["claude-b"] });
    expect(await screen.findByLabelText("claude-b")).toBeTruthy();
    expect(mockTargets).not.toHaveBeenCalled();
  });

  it("Escape and Cancel close without a request", async () => {
    const onClose = renderModal();
    await screen.findByLabelText("claude-b");
    fireEvent.keyDown(document, { key: "Escape" });
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(onClose).toHaveBeenCalledTimes(2);
    expect(mockSwitch).not.toHaveBeenCalled();
  });
});
