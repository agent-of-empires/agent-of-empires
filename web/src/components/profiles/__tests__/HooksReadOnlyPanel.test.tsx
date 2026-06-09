// @vitest-environment jsdom
//
// The hooks panel is a security boundary surface: it must render lifecycle
// hooks read-only (no inputs/controls) and explain why they cannot be
// edited from the dashboard. These tests pin the three-state rendering and
// the read-only invariant.

import { describe, expect, it } from "vitest";
import { render } from "@testing-library/react";
import { HooksReadOnlyPanel } from "../HooksReadOnlyPanel";
import { buildEffectiveHooks } from "../../../lib/profileHooks";

function mount(profile: Parameters<typeof buildEffectiveHooks>[0], global: Parameters<typeof buildEffectiveHooks>[1]) {
  return render(<HooksReadOnlyPanel groups={buildEffectiveHooks(profile, global)} />);
}

describe("HooksReadOnlyPanel", () => {
  it("renders the explain-why note about remote code execution", () => {
    const { getByText } = mount({}, {});
    expect(getByText(/remote code execution/i)).toBeTruthy();
  });

  it("exposes no editable controls (read-only invariant)", () => {
    const { container } = mount({ on_create: ["echo hi"] }, { on_launch: ["echo global"] });
    expect(container.querySelectorAll("input").length).toBe(0);
    expect(container.querySelectorAll("textarea").length).toBe(0);
    expect(container.querySelectorAll("button").length).toBe(0);
    expect(container.querySelectorAll("select").length).toBe(0);
  });

  it("shows a profile override command with the override badge", () => {
    const { getByText } = mount({ on_create: ["echo hi"] }, {});
    expect(getByText("echo hi")).toBeTruthy();
    expect(getByText("Profile override")).toBeTruthy();
  });

  it("labels an inherited global command", () => {
    const { getByText } = mount({}, { on_launch: ["echo global"] });
    expect(getByText("echo global")).toBeTruthy();
    expect(getByText("Inherited from global")).toBeTruthy();
  });

  it("labels an explicit empty override as overridden-to-none", () => {
    const { getByText } = mount({ on_destroy: [] }, { on_destroy: ["docker compose down"] });
    expect(getByText("Overridden: none")).toBeTruthy();
  });
});
