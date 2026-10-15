// @vitest-environment jsdom

import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { SettingsSearch } from "../SettingsSearch";
import type { SettingsFieldDescriptor } from "../../../lib/types";
import { descriptor } from "./fixtures";

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
  it("filters to matching settings and jumps with the resolved tab on select", () => {
    const onJump = vi.fn();
    render(<SettingsSearch schema={SCHEMA} loading={false} onJump={onJump} />);

    const input = screen.getByPlaceholderText("Search settings...");
    fireEvent.change(input, { target: { value: "tool" } });

    const hit = screen.getByTestId("settings-search-hit-acp-show_tool_durations");
    expect(hit).toBeTruthy();
    expect(screen.queryByTestId("settings-search-hit-theme-name")).toBeNull();

    fireEvent.click(hit);
    expect(onJump).toHaveBeenCalledTimes(1);
    expect(onJump).toHaveBeenCalledWith(
      expect.objectContaining({ section: "acp", field: "show_tool_durations", tab: "structured-view" }),
    );
  });
});
