import { describe, expect, it } from "vitest";

import { descriptor } from "../components/settings/__tests__/fixtures";
import { settingTags } from "./settingRow";

describe("settingTags", () => {
  it.each([
    ["a plain row", descriptor({ section: "session", field: "f", label: "L" }), []],
    ["restart", descriptor({ section: "logging", field: "output", label: "L", restart: true }), ["Needs restart"]],
    [
      "web_only",
      descriptor({ section: "session", field: "show_session_colors", label: "L", web_only: true }),
      ["Web dashboard only"],
    ],
    ["tui_only", descriptor({ section: "session", field: "f", label: "L", tui_only: true }), ["Terminal app only"]],
    ["section acp", descriptor({ section: "acp", field: "f", label: "L" }), ["Structured view only"]],
    ["section systemd", descriptor({ section: "systemd", field: "f", label: "L" }), ["Linux only"]],
    ["a section only named like acp", descriptor({ section: "plugin:acp", field: "f", label: "L" }), []],
    [
      "several, in display order",
      descriptor({ section: "systemd", field: "f", label: "L", restart: true, tui_only: true }),
      ["Needs restart", "Terminal app only", "Linux only"],
    ],
    [
      "false flags",
      descriptor({ section: "session", field: "f", label: "L", restart: false, web_only: false, tui_only: false }),
      [],
    ],
  ])("%s", (_, d, tags) => {
    expect(settingTags(d)).toEqual(tags);
  });
});
