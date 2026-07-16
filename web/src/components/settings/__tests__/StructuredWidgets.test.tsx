// @vitest-environment jsdom
//
// Contract test for the API v9 structured plugin settings widgets (#2897):
// object_list add/remove with a host-populated dynamic_select picker, and
// dynamic_select dependency-driven option reloads. Pins that the object_list
// generates a stable id on add, renders nested pickers fed by the resolver
// endpoint, and emits the full array through onSaveField.

import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SchemaSection } from "../SchemaSection";
import type { SettingsFieldDescriptor } from "../../../lib/types";

const ALLOW = { policy: "allow" } as const;
const NONE = { rule: "none" } as const;

// Stub the resolver endpoint so the picker has host options without a backend.
const fetchMock = vi.fn();
beforeEach(() => {
  fetchMock.mockReset();
  fetchMock.mockResolvedValue({
    ok: true,
    json: async () => ({
      options: [
        { value: "claude-code", label: "Claude Code" },
        { value: "codex", label: "Codex" },
      ],
    }),
  });
  vi.stubGlobal("fetch", fetchMock);
});

const SCHEMA: SettingsFieldDescriptor[] = [
  {
    section: "plugin:acme.cron",
    field: "jobs",
    category: "Plugins",
    label: "Scheduled jobs",
    description: "",
    web_write: ALLOW,
    profile_overridable: false,
    validation: NONE,
    advanced: false,
    widget: {
      kind: "object_list",
      id_field: "id",
      fields: [
        {
          field: "agent",
          label: "Agent",
          required: true,
          widget: { kind: "dynamic_select", source: "acp_agents" },
          validation: { rule: "non_empty_string" },
        },
        {
          field: "schedule",
          label: "Schedule",
          required: true,
          widget: { kind: "cron" },
          validation: { rule: "cron" },
        },
      ],
    },
  },
];

describe("structured plugin settings widgets", () => {
  it("adds an object_list item with a stable id and a host-populated picker", async () => {
    const onSave = vi.fn().mockResolvedValue(true);
    render(<SchemaSection section="plugin:acme.cron" schema={SCHEMA} values={{ jobs: [] }} onSaveField={onSave} />);

    fireEvent.click(screen.getByText("Add item"));

    // The whole array (one item with a generated stable id) is persisted.
    await waitFor(() => expect(onSave).toHaveBeenCalledTimes(1));
    const [sec, field, value] = onSave.mock.calls[0]!;
    expect(sec).toBe("plugin:acme.cron");
    expect(field).toBe("jobs");
    expect(Array.isArray(value)).toBe(true);
    expect((value as { id: string }[]).length).toBe(1);
    expect(typeof (value as { id: string }[])[0]!.id).toBe("string");
  });

  it("renders an item's dynamic_select from host-resolved options", async () => {
    const onSave = vi.fn().mockResolvedValue(true);
    render(
      <SchemaSection
        section="plugin:acme.cron"
        schema={SCHEMA}
        values={{ jobs: [{ id: "id-1", agent: "codex", schedule: "0 9 * * 1-5" }] }}
        onSaveField={onSave}
      />,
    );

    // The nested dynamic_select fetched its options from the resolver endpoint,
    // scoped to the plugin id, and rendered the host labels.
    await waitFor(() =>
      expect(fetchMock).toHaveBeenCalledWith(
        "/api/plugins/acme.cron/settings/options/resolve",
        expect.objectContaining({ method: "POST" }),
      ),
    );
    await waitFor(() => expect(screen.getByText("Claude Code")).toBeTruthy());
  });

  it("removes an object_list item", async () => {
    const onSave = vi.fn().mockResolvedValue(true);
    render(
      <SchemaSection
        section="plugin:acme.cron"
        schema={SCHEMA}
        values={{ jobs: [{ id: "id-1", agent: "codex", schedule: "0 9 * * 1-5" }] }}
        onSaveField={onSave}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "Remove item" }));
    await waitFor(() => expect(onSave).toHaveBeenCalledWith("plugin:acme.cron", "jobs", []));
  });
});
