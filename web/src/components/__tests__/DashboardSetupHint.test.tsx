// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";

import { Dashboard } from "../Dashboard";

vi.mock("../SuggestionsSetupHint", () => ({ SuggestionsSetupHint: () => <p data-testid="setup-hint" /> }));

afterEach(cleanup);

describe("Dashboard first-run suggestions hint", () => {
  it.each([
    ["is shown by default", {}, true],
    ["is hidden on a read-only dashboard", { readOnly: true }, false],
    ["is hidden in CityHall client mode", { canManageProjects: false }, false],
    ["is hidden during the onboarding tour", { tourActive: true }, false],
    ["is hidden when the Marketplace cannot be opened", { onOpenSuggestionsMarketplace: undefined }, false],
  ])("%s", (_name, props, shown) => {
    render(
      <Dashboard
        sessions={[]}
        onSelectSession={vi.fn()}
        onNewSession={vi.fn()}
        onCloneFromUrl={vi.fn()}
        onToggleSidebar={vi.fn()}
        onOpenSuggestionsMarketplace={vi.fn()}
        {...props}
      />,
    );
    expect(screen.queryByTestId("setup-hint") !== null).toBe(shown);
  });
});
