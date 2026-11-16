// @vitest-environment jsdom
//
// Switch-account dialog: names come from the server, confirm hands the pick to the
// caller and closes at once.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { stubMatchMedia } from "../../hooks/__tests__/fixtures";
import { FOCUS_TERMINAL_EVENT } from "../../lib/terminalFocus";
import { SwitchAccountModal } from "./SwitchAccountModal";

vi.mock("../../lib/api", () => ({
  fetchAccountTargets: vi.fn(),
}));

import { fetchAccountTargets } from "../../lib/api";

const mockTargets = vi.mocked(fetchAccountTargets);
const onSwitch = vi.fn();

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
      onSwitch={onSwitch}
      onClose={onClose}
      {...props}
    />,
  );
  return onClose;
}

beforeEach(() => {
  vi.clearAllMocks();
  mockTargets.mockResolvedValue(["claude-b", "claude-c"]);
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

  it("hands the picked account to the caller and closes at once", async () => {
    const onClose = renderModal();
    fireEvent.click(await screen.findByLabelText("claude-c"));
    fireEvent.click(screen.getByTestId("switch-account-confirm"));
    expect(onSwitch).toHaveBeenCalledWith("claude-c");
    expect(onClose).toHaveBeenCalledTimes(1);
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

  it("blocks confirming while an automatic failover is running", async () => {
    renderModal({ failoverTo: "claude-b" });
    await screen.findByLabelText("claude-b");
    expect(screen.getByTestId("switch-account-failover").textContent).toContain("claude-b");
    const confirm = screen.getByTestId("switch-account-confirm") as HTMLButtonElement;
    expect(confirm.disabled).toBe(true);
    fireEvent.click(confirm);
    expect(onSwitch).not.toHaveBeenCalled();
  });

  it("focuses Cancel while accounts load, then Confirm once it is enabled", async () => {
    let resolve!: (names: string[]) => void;
    mockTargets.mockReturnValue(new Promise((r) => (resolve = r)));
    renderModal();
    const cancel = screen.getByRole("button", { name: "Cancel" });
    await waitFor(() => expect(document.activeElement).toBe(cancel));
    resolve(["claude-b"]);
    await waitFor(() => expect(document.activeElement).toBe(screen.getByTestId("switch-account-confirm")));
  });

  it("wraps Tab and Shift+Tab inside the dialog", async () => {
    renderModal();
    await screen.findByLabelText("claude-b");
    const first = screen.getByLabelText("claude-b");
    const confirm = screen.getByTestId("switch-account-confirm");
    confirm.focus();
    fireEvent.keyDown(confirm, { key: "Tab" });
    expect(document.activeElement).toBe(first);
    fireEvent.keyDown(first, { key: "Tab", shiftKey: true });
    expect(document.activeElement).toBe(confirm);
  });

  it("Escape and Cancel close without a request", async () => {
    const onClose = renderModal();
    await screen.findByLabelText("claude-b");
    fireEvent.keyDown(document, { key: "Escape" });
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(onClose).toHaveBeenCalledTimes(2);
    expect(onSwitch).not.toHaveBeenCalled();
  });

  it.each([
    ["the opener is still there", true, false, []],
    ["the switch removed the opener", false, false, ["composer"]],
    ["the switch removed the opener on touch, where the composer would open the keyboard", false, true, []],
  ])("on close, restores focus when %s", async (_l, openerStays, coarse, expected) => {
    const originalMatchMedia = window.matchMedia;
    stubMatchMedia(coarse, "(pointer: coarse)");
    const opener = document.createElement("button");
    document.body.append(opener);
    opener.focus();
    const focusEvents: string[] = [];
    const onEvent = (e: Event) => focusEvents.push((e as CustomEvent).detail.target);
    window.addEventListener(FOCUS_TERMINAL_EVENT, onEvent);
    const { unmount } = render(
      <SwitchAccountModal
        open
        sessionId="s1"
        currentTool="claude-a"
        authStatus={null}
        onSwitch={onSwitch}
        onClose={vi.fn()}
      />,
    );
    await screen.findByLabelText("claude-b");
    if (!openerStays) opener.remove();
    unmount();
    window.removeEventListener(FOCUS_TERMINAL_EVENT, onEvent);
    window.matchMedia = originalMatchMedia;
    if (openerStays) {
      expect(document.activeElement).toBe(opener);
      opener.remove();
    }
    expect(focusEvents).toEqual(expected);
  });
});
