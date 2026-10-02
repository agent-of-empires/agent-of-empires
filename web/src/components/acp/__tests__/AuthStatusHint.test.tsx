// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";

import { AuthStatusHint } from "../ComposerControls";
import type { AuthStatus } from "../../../lib/acpTypes";

afterEach(cleanup);

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

  it("gives the known logged-out state its own treatment", () => {
    render(<AuthStatusHint authStatus={{ kind: "none", label: "Not logged in" }} />);
    const el = chip()!;
    expect(el.textContent).toContain("Not logged in");
    expect(el.getAttribute("data-auth-kind")).toBe("none");
    expect(el.className).toContain("status-error-text");
    // It reports the identity, so it must not claim a credential failed.
    expect(el.getAttribute("aria-label")).not.toMatch(/invalid|expired/i);
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
  });
});
