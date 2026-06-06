import { describe, expect, it } from "vitest";

import type { ConfigOptionDescriptor } from "../../lib/acpTypes";
import {
  configOptionsForComposerControls,
  showModePickerForComposerControls,
} from "./configOptionsForComposerControls";

function option(
  id: string,
  category: ConfigOptionDescriptor["category"],
): ConfigOptionDescriptor {
  return {
    id,
    name: id,
    category,
    current_value: "current",
    options: [{ value: "current", name: "Current" }],
  };
}

describe("configOptionsForComposerControls", () => {
  it("removes generic model config for Cursor sessions", () => {
    const options = [
      option("model", "model"),
      option("fast", "model_config"),
      option("effort", "thought_level"),
      option("mode", "mode"),
    ];

    expect(
      configOptionsForComposerControls(options, {
        currentAgent: "cursor-agent",
        sessionTool: "cursor",
      }).map((o) => o.id),
    ).toEqual(["effort", "mode"]);
  });

  it("keeps model config for non-Cursor sessions", () => {
    const options = [option("model", "model")];

    expect(
      configOptionsForComposerControls(options, {
        currentAgent: "opencode",
        sessionTool: "opencode",
      }).map((o) => o.id),
    ).toEqual(["model"]);
  });

  it("hides the generic mode picker for Cursor sessions", () => {
    expect(
      showModePickerForComposerControls({
        currentAgent: "cursor-agent",
        sessionTool: "cursor",
      }),
    ).toBe(false);
  });

  it("keeps the generic mode picker for non-Cursor sessions", () => {
    expect(
      showModePickerForComposerControls({
        currentAgent: "opencode",
        sessionTool: "opencode",
      }),
    ).toBe(true);
  });
});
