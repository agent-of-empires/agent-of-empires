// @vitest-environment jsdom
//
// Pins the web settings search box: it stays closed until you type, filters
// the schema-backed settings, and emits the chosen hit (with its resolved jump
// tab) through onJump so SettingsView can switch tabs and scroll to the field.

import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { SettingsSearch } from "../SettingsSearch";
import type { SettingsFieldDescriptor } from "../../../lib/types";

const ALLOW = { policy: "allow" } as const;
const NONE = { rule: "none" } as const;

function descriptor(
  over: Partial<SettingsFieldDescriptor> & Pick<SettingsFieldDescriptor, "section" | "field" | "label">,
): SettingsFieldDescriptor {
  return {
    category: "Sandbox",
    description: "",
    widget: { kind: "toggle" },
    web_write: ALLOW,
    profile_overridable: true,
    validation: NONE,
    advanced: false,
    ...over,
  };
}

const SCHEMA: SettingsFieldDescriptor[] = [
  descriptor({ section: "theme", field: "name", label: "Theme", category: "Theme" }),
  descriptor({
    section: "acp",
    field: "show_tool_durations",
    label: "Show tool-call durations",
    category: "Structured view",
  }),
];

describe("SettingsSearch", () => {
  // This repo's component tests do not load jest-dom, so assertions use plain
  // DOM presence (queryBy -> null) instead of toBeInTheDocument.
  it("shows no result list until the user types", () => {
    render(<SettingsSearch schema={SCHEMA} loading={false} onJump={vi.fn()} />);
    expect(screen.queryByText("Theme")).toBeNull();
    expect(screen.queryByText("No matching settings")).toBeNull();
  });

  it("filters to matching settings and jumps with the resolved tab on select", () => {
    const onJump = vi.fn();
    render(<SettingsSearch schema={SCHEMA} loading={false} onJump={onJump} />);

    const input = screen.getByPlaceholderText("Search settings...");
    fireEvent.change(input, { target: { value: "tool" } });

    // The matching hit shows; the unrelated one is filtered out.
    const hit = screen.getByTestId("settings-search-hit-acp-show_tool_durations");
    expect(hit).toBeTruthy();
    expect(screen.queryByTestId("settings-search-hit-theme-name")).toBeNull();

    fireEvent.click(hit);
    expect(onJump).toHaveBeenCalledTimes(1);
    expect(onJump).toHaveBeenCalledWith(
      expect.objectContaining({ section: "acp", field: "show_tool_durations", tab: "structured-view" }),
    );
  });

  it("disables the input while the schema is loading", () => {
    render(<SettingsSearch schema={[]} loading={true} onJump={vi.fn()} />);
    expect((screen.getByPlaceholderText("Loading settings...") as HTMLInputElement).disabled).toBe(true);
  });
});
