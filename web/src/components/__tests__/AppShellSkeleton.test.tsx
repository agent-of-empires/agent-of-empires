// @vitest-environment jsdom

import { describe, expect, it } from "vitest";
import { render } from "@testing-library/react";

import { AppShellSkeleton, MainPaneSkeleton } from "../AppShellSkeleton";

describe("AppShellSkeleton", () => {
  it("renders the TopBar strip and a sidebar column (md+) around the main pane", () => {
    const { container } = render(<AppShellSkeleton />);
    // TopBar-height strip using the elevated nav surface.
    expect(container.querySelector(".h-12.bg-surface-850")).not.toBeNull();
    // Sidebar silhouette is present but hidden below md.
    const sidebar = container.querySelector(".hidden.md\\:flex");
    expect(sidebar).not.toBeNull();
    // Placeholder blocks pulse only when motion is allowed.
    expect(container.querySelectorAll(".motion-safe\\:animate-pulse").length).toBeGreaterThan(0);
  });
});

describe("MainPaneSkeleton", () => {
  it("renders ragged placeholder lines that fill the pane", () => {
    const { container } = render(<MainPaneSkeleton />);
    const root = container.firstElementChild as HTMLElement;
    expect(root.className).toContain("animate-fade-in");
    expect(root.className).toContain("h-full");
    expect(container.querySelectorAll(".rounded-md.bg-surface-800").length).toBeGreaterThan(1);
  });
});
