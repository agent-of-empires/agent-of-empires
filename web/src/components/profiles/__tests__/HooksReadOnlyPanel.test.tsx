// @vitest-environment jsdom

import { describe, expect, it } from "vitest";
import { render } from "@testing-library/react";
import { HooksReadOnlyPanel } from "../HooksReadOnlyPanel";
import { buildEffectiveHooks } from "../../../lib/profileHooks";

type Hooks = Parameters<typeof buildEffectiveHooks>[0];

describe("HooksReadOnlyPanel", () => {
  it("explains why hooks are read-only and exposes no controls", () => {
    const { container, getByText } = render(
      <HooksReadOnlyPanel groups={buildEffectiveHooks({ on_create: ["echo hi"] }, { on_launch: ["echo global"] })} />,
    );
    expect(getByText(/remote code execution/i)).toBeTruthy();
    expect(container.querySelectorAll("input, textarea, button, select")).toHaveLength(0);
  });

  it("labels profile overrides, inherited globals, and explicit empty overrides", () => {
    const cases: [Hooks, Hooks, string[]][] = [
      [{ on_create: ["echo hi"] }, {}, ["echo hi", "Profile override"]],
      [{}, { on_launch: ["echo global"] }, ["echo global", "Inherited from global"]],
      [{ on_destroy: [] }, { on_destroy: ["docker compose down"] }, ["Overridden: none"]],
    ];
    for (const [profile, global, texts] of cases) {
      const { getByText, unmount } = render(<HooksReadOnlyPanel groups={buildEffectiveHooks(profile, global)} />);
      for (const text of texts) expect(getByText(text)).toBeTruthy();
      unmount();
    }
  });
});
