import { describe, expect, it } from "vitest";
import { buildSettingsSearchIndex } from "../settingsSearchIndex";
import { descriptor } from "./fixtures";

describe("buildSettingsSearchIndex", () => {
  it("includes schema-backed writable fields and resolves the jump tab", () => {
    const index = buildSettingsSearchIndex([
      descriptor({ section: "sandbox", field: "enabled_by_default", label: "Enabled by Default" }),
      descriptor({ section: "acp", field: "show_tool_durations", label: "Show tool-call durations" }),
      descriptor({ section: "web", field: "notify_on_idle", label: "Notify on idle" }),
    ]);

    expect(index.map((h) => `${h.section}.${h.field}`)).toEqual([
      "sandbox.enabled_by_default",
      "acp.show_tool_durations",
      "web.notify_on_idle",
    ]);
    expect(index.find((h) => h.section === "acp")?.tab).toBe("structured-view");
    expect(index.find((h) => h.section === "web")?.tab).toBe("notifications");
    expect(index.find((h) => h.section === "sandbox")?.tab).toBe("sandbox");
  });

  it("skips local_only fields and sections with no web tab so every hit can jump", () => {
    const index = buildSettingsSearchIndex([
      descriptor({
        section: "sandbox",
        field: "node_path",
        label: "Node path",
        web_write: { policy: "local_only", reason: "host binary" },
      }),
      descriptor({ section: "diff", field: "context_lines", label: "Context lines" }),
      descriptor({ section: "made_up", field: "x", label: "X" }),
      descriptor({ section: "tmux", field: "prefix", label: "Prefix" }),
    ]);
    expect(index.map((h) => h.section)).toEqual(["tmux"]);
  });

  it("packs label, description, section, and field into the searchable text", () => {
    const [hit] = buildSettingsSearchIndex([
      descriptor({
        section: "session",
        field: "max_concurrent_workers",
        label: "Max Concurrent Workers",
        description: "How many agents run at once",
      }),
    ]);
    expect(hit.searchText).toBe("Max Concurrent Workers How many agents run at once session max_concurrent_workers");
  });
});
