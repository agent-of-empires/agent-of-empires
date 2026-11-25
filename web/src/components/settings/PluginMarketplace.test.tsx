// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { PluginMarketplace } from "./PluginMarketplace";
import type { PluginMarketplaceState } from "./pluginFlows";

afterEach(cleanup);

describe("PluginMarketplace", () => {
  it.each([
    ["keyCode 229", { key: "Enter", keyCode: 229 }],
    ["isComposing", { key: "Enter", isComposing: true }],
  ])("does not search on Enter while an IME composes (%s)", (_n, init) => {
    const discover = vi.fn();
    const market = {
      query: "kanban",
      setQuery: vi.fn(),
      results: [],
      error: null,
      discovering: false,
      previewingSource: null,
      discover,
      install: vi.fn(),
    } as unknown as PluginMarketplaceState;
    render(<PluginMarketplace market={market} installedSources={new Set()} onOpenDetail={vi.fn()} />);
    const input = screen.getByTestId("plugins-discover-query");
    fireEvent.keyDown(input, init);
    expect(discover).not.toHaveBeenCalled();
    fireEvent.keyDown(input, { key: "Enter" });
    expect(discover).toHaveBeenCalledTimes(1);
  });
});
