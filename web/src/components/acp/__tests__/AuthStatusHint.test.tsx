// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

vi.mock("../../../lib/clipboard", () => ({ writeClipboard: vi.fn(() => Promise.resolve(true)) }));
vi.mock("../../../lib/api", () => ({ restartSession: vi.fn() }));

import { AuthStatusHint } from "../ComposerControls";
import type { SignInContext } from "../SignInPopover";
import type { AuthStatus } from "../../../lib/acpTypes";
import { restartSession } from "../../../lib/api";
import { writeClipboard } from "../../../lib/clipboard";
import { FOCUS_TERMINAL_EVENT, type FocusTerminalDetail } from "../../../lib/terminalFocus";

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
  vi.unstubAllGlobals();
});

const chip = () => screen.queryByTestId("composer-auth-status");

describe("AuthStatusHint", () => {
  it("renders nothing when the agent never reported", () => {
    // Silence means "not reported", which must not read as logged out.
    render(<AuthStatusHint authStatus={null} />);
    expect(chip()).toBeNull();
  });

  it.each([
    ["account", "Claude Max"],
    ["api_key", "Anthropic API key"],
    ["external", "AWS Bedrock"],
    // A kind added upstream after this code was written still renders.
    ["unknown", "Future Auth"],
  ])("shows the %s label without a warning tone", (kind, label) => {
    render(<AuthStatusHint authStatus={{ kind, label } as AuthStatus} />);
    const el = chip()!;
    expect(el.textContent).toContain(label);
    expect(el.className).not.toContain("status-error");
  });

  it("keeps the account email out of the rendered text", () => {
    render(
      <AuthStatusHint
        authStatus={{
          kind: "account",
          label: "Claude Max",
          account: { email: "someone@example.com", organization: "Acme", plan: "max" },
        }}
      />,
    );
    const el = chip()!;
    // The chip sits in every screenshot and screen share; details are hover only.
    expect(el.textContent).not.toContain("someone@example.com");
    expect(el.textContent).not.toContain("Acme");
    expect(el.getAttribute("aria-label")).toContain("someone@example.com");
    // Named and focusable, so keyboard and screen reader users reach the details.
    expect(screen.getByRole("img", { name: /someone@example\.com/ })).toBe(el);
    expect(el.tabIndex).toBe(0);
    fireEvent.focus(el);
    expect(screen.getByRole("tooltip").textContent).toContain("someone@example.com");
  });
});

describe("AuthStatusHint signed out", () => {
  const signIn = (over: Partial<SignInContext> = {}): SignInContext => ({
    sessionId: "s1",
    agent: "claude",
    agentName: "Claude Code",
    sandboxed: false,
    onOpenTerminal: vi.fn(),
    canRestart: true,
    ...over,
  });
  const renderSignedOut = (context?: SignInContext) =>
    render(<AuthStatusHint authStatus={{ kind: "none", label: "Not logged in" }} signIn={context} />);
  const trigger = () => screen.getByRole("button", { name: "Not logged in" });
  const dialog = () => screen.queryByRole("dialog");

  it("is a button that opens the Agent's sign-in steps", () => {
    renderSignedOut(signIn());
    const button = trigger();
    // A native button, so Enter and Space open it too.
    expect(button.tagName).toBe("BUTTON");
    expect(button.getAttribute("type")).toBe("button");
    expect(button.getAttribute("aria-haspopup")).toBe("dialog");
    expect(button.getAttribute("aria-expanded")).toBe("false");
    // It reports the identity, so it must not claim a credential failed.
    expect(button.getAttribute("aria-describedby")).toBeTruthy();
    expect(document.getElementById(button.getAttribute("aria-describedby")!)!.textContent).not.toMatch(
      /invalid|expired/i,
    );

    fireEvent.click(button);
    expect(button.getAttribute("aria-expanded")).toBe("true");
    // Focus enters the dialog, so a screen reader announces it.
    expect(document.activeElement).toBe(screen.getByRole("dialog", { name: "Claude Code isn't signed in" }));
    expect(screen.getByTestId("sign-in-command").textContent).toBe("claude auth login");
    expect(dialog()!.textContent).toContain("host terminal");
    expect(dialog()!.textContent).toContain("ANTHROPIC_API_KEY");
  });

  it("names no command for an Agent aoe has no steps for", () => {
    renderSignedOut(signIn({ agent: "opencode", agentName: "OpenCode" }));
    fireEvent.click(trigger());
    expect(screen.queryByTestId("sign-in-command")).toBeNull();
    expect(dialog()!.textContent).toContain("Sign in to OpenCode, then restart the Agent.");
  });

  it.each([
    ["Escape", true],
    ["an outside click", false],
  ])("closes on %s; only Escape puts focus back on the button", (_how, refocus) => {
    renderSignedOut(signIn());
    fireEvent.click(trigger());
    const copy = screen.getByRole("button", { name: "Copy command" });
    copy.focus();
    if (refocus) fireEvent.keyDown(copy, { key: "Escape" });
    else fireEvent.mouseDown(document.body);
    expect(dialog()).toBeNull();
    expect(document.activeElement === trigger()).toBe(refocus);
  });

  it("copies exactly the command it shows", async () => {
    renderSignedOut(signIn({ agent: "codex", agentName: "Codex", sandboxed: true }));
    fireEvent.click(trigger());
    const shown = screen.getByTestId("sign-in-command").textContent;
    fireEvent.click(screen.getByRole("button", { name: "Copy command" }));
    await screen.findByRole("button", { name: "Copied" });
    expect(writeClipboard).toHaveBeenCalledWith(shown);
    expect(shown).toBe("codex login --device-auth");
  });

  it.each([
    [true, "container"],
    [false, "host"],
  ])("Open Terminal in a Session sandboxed: %s opens its %s shell", (sandboxed, mode) => {
    const onOpenTerminal = vi.fn();
    renderSignedOut(signIn({ sandboxed, onOpenTerminal }));
    fireEvent.click(trigger());
    fireEvent.click(screen.getByRole("button", { name: "Open Terminal" }));
    expect(onOpenTerminal).toHaveBeenCalledWith(mode);
    expect(dialog()).toBeNull();
  });

  it.each([
    ["a server that allows both", signIn(), ["Open Terminal", "Restart Agent"]],
    // App omits the terminal and restart where read-only or CityHall mode refuses them.
    ["a read-only or CityHall server", signIn({ onOpenTerminal: undefined, canRestart: false }), []],
    ["no Structured view context", undefined, []],
  ])("offers the actions %s allows", (_label, context, actions) => {
    renderSignedOut(context);
    fireEvent.click(trigger());
    for (const name of ["Open Terminal", "Restart Agent"]) {
      expect(!!screen.queryByRole("button", { name })).toBe(actions.includes(name));
    }
  });

  it("Restart Agent restarts the Session once and says why it failed", async () => {
    let answer!: (r: { ok: boolean; message?: string }) => void;
    vi.mocked(restartSession).mockReturnValueOnce(new Promise((r) => (answer = r)));
    renderSignedOut(signIn());
    fireEvent.click(trigger());
    fireEvent.click(screen.getByRole("button", { name: "Restart Agent" }));
    const busy = screen.getByRole("button", { name: "Restarting…" }) as HTMLButtonElement;
    expect(busy.disabled).toBe(true);
    fireEvent.click(busy);
    expect(restartSession).toHaveBeenCalledTimes(1);
    expect(restartSession).toHaveBeenCalledWith("s1");
    await act(async () => answer({ ok: false, message: "session is busy" }));
    expect(screen.getByRole("status").textContent).toBe("Couldn't restart the Agent: session is busy");
    expect(screen.getByRole("button", { name: "Restart Agent" })).toBeTruthy();
  });

  it("a restart that lands after the panel closed leaves the reopened one open", async () => {
    let answer!: (r: { ok: boolean }) => void;
    vi.mocked(restartSession).mockReturnValueOnce(new Promise((r) => (answer = r)));
    renderSignedOut(signIn());
    fireEvent.click(trigger());
    fireEvent.click(screen.getByRole("button", { name: "Restart Agent" }));
    fireEvent.mouseDown(document.body);
    fireEvent.click(trigger());
    await act(async () => answer({ ok: true }));
    expect(dialog()).not.toBeNull();
  });

  it.each([
    ["a mouse", false, ["composer"]],
    // Focusing the composer would open the on-screen keyboard.
    ["touch", true, []],
  ])("closes once the restart is accepted, and with %s focuses the composer", async (_, coarse, focused) => {
    vi.stubGlobal("matchMedia", (query: string) => ({ matches: coarse && query === "(pointer: coarse)" }));
    const targets: string[] = [];
    const onFocus = (e: Event) => targets.push((e as CustomEvent<FocusTerminalDetail>).detail.target);
    window.addEventListener(FOCUS_TERMINAL_EVENT, onFocus);
    vi.mocked(restartSession).mockResolvedValueOnce({ ok: true });
    renderSignedOut(signIn());
    fireEvent.click(trigger());
    fireEvent.click(screen.getByRole("button", { name: "Restart Agent" }));
    await waitFor(() => expect(dialog()).toBeNull());
    window.removeEventListener(FOCUS_TERMINAL_EVENT, onFocus);
    expect(targets).toEqual(focused);
  });
});
