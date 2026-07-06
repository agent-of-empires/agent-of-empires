// @vitest-environment jsdom
import { fireEvent, render } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { PluginIdentityIcon } from "../PluginIdentityIcon";

describe("PluginIdentityIcon", () => {
  it("falls back to the lucide icon, then retries a later working URL instead of staying stuck", async () => {
    const { findByTestId, getByTestId, rerender } = render(
      <PluginIdentityIcon icon="git-branch" iconAssetUrl="/first.png" testId="icon" />,
    );
    // The asset fails to load; the component falls back to the lucide icon
    // (a dynamically-loaded lucide component, hence findByTestId to await it).
    fireEvent.error(getByTestId("icon"));
    expect((await findByTestId("icon")).tagName).toBe("svg");

    // A later render swaps in a different (working) URL, e.g. the detail
    // modal's fallback route getting replaced by the resolved manifest URL.
    // The prior failure must not stick to the new URL.
    rerender(<PluginIdentityIcon icon="git-branch" iconAssetUrl="/second.png" testId="icon" />);
    const icon = await findByTestId("icon");
    expect(icon.tagName).toBe("IMG");
    expect(icon.getAttribute("src")).toBe("/second.png");
  });
});
