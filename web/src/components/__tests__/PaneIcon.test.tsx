// @vitest-environment jsdom
import { fireEvent, render } from "@testing-library/react";
import { GitBranch } from "lucide-react";
import { describe, expect, it } from "vitest";

import { PaneIcon } from "../PaneIcon";

describe("PaneIcon", () => {
  it("renders the icon_asset image, outranking the resolved fallback icon", async () => {
    const { findByTestId } = render(
      <PaneIcon icon={GitBranch} iconAssetUrl="/api/plugins/acme.widget/icon" className="size-4" testId="icon" />,
    );
    const icon = await findByTestId("icon");
    expect(icon.tagName).toBe("IMG");
    expect(icon.getAttribute("src")).toBe("/api/plugins/acme.widget/icon");
  });

  it("falls back to the resolved icon component when there is no icon_asset", () => {
    const { getByTestId } = render(<PaneIcon icon={GitBranch} className="size-4" testId="icon" />);
    expect(getByTestId("icon").tagName).toBe("svg");
  });

  it("falls back to the icon on load failure, then retries a later working URL", async () => {
    const { findByTestId, getByTestId, rerender } = render(
      <PaneIcon icon={GitBranch} iconAssetUrl="/first.png" className="size-4" testId="icon" />,
    );
    fireEvent.error(getByTestId("icon"));
    expect(getByTestId("icon").tagName).toBe("svg");

    rerender(<PaneIcon icon={GitBranch} iconAssetUrl="/second.png" className="size-4" testId="icon" />);
    const icon = await findByTestId("icon");
    expect(icon.tagName).toBe("IMG");
    expect(icon.getAttribute("src")).toBe("/second.png");
  });
});
