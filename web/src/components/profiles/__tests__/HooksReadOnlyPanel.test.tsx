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

  it.each<[string, Hooks, Hooks, string[]]>([
    ["a profile override", { on_create: ["echo hi"] }, {}, ["echo hi", "Profile override"]],
    ["an inherited global command", {}, { on_launch: ["echo global"] }, ["echo global", "Inherited from global"]],
    ["an explicit empty override", { on_destroy: [] }, { on_destroy: ["docker compose down"] }, ["Overridden: none"]],
  ])("labels %s", (_, profile, global, texts) => {
    const { getByText } = render(<HooksReadOnlyPanel groups={buildEffectiveHooks(profile, global)} />);
    for (const text of texts) expect(getByText(text)).toBeTruthy();
  });
});
