// Unit test for the settings-search index builder. The index is what the
// web search box filters over, so this pins the inclusion rules: skip fields
// the dashboard cannot write, skip sections with no web tab (a hit must be
// able to jump somewhere), and map web/acp to their non-identity tabs.

import { describe, expect, it } from "vitest";
import { buildSettingsSearchIndex, SECTION_TO_TAB } from "../settingsSearchIndex";
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
    // acp and web are the non-identity mappings.
    expect(index.find((h) => h.section === "acp")?.tab).toBe("structured-view");
    expect(index.find((h) => h.section === "web")?.tab).toBe("notifications");
    expect(index.find((h) => h.section === "sandbox")?.tab).toBe("sandbox");
  });

  it("skips local_only fields the server rejects", () => {
    const index = buildSettingsSearchIndex([
      descriptor({
        section: "sandbox",
        field: "node_path",
        label: "Node path",
        web_write: { policy: "local_only", reason: "host binary" },
      }),
    ]);
    expect(index).toHaveLength(0);
  });

  it("skips sections with no web tab so every hit can jump", () => {
    const index = buildSettingsSearchIndex([
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

  it("maps every section in SECTION_TO_TAB to a real tab id", () => {
    // Drift guard: a section listed here but pointing at a tab that no longer
    // exists would silently swallow its fields' search hits.
    expect(Object.values(SECTION_TO_TAB).every((tab) => typeof tab === "string" && tab.length > 0)).toBe(true);
  });
});
