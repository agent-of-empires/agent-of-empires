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
  it("adds a new object_list item as an editable draft, persisting only once required fields are filled", async () => {
    const onSave = vi.fn().mockResolvedValue(true);
    render(<SchemaSection section="plugin:acme.cron" schema={SCHEMA} values={{ jobs: [] }} onSaveField={onSave} />);

    fireEvent.click(screen.getByText("Add item"));

    // The new item renders and is editable, but its required fields (agent,
    // schedule) are empty, so nothing is persisted yet.
    await waitFor(() => expect(screen.getByText("Item 1")).toBeTruthy());
    expect(onSave).not.toHaveBeenCalled();

    // Filling every required field promotes the draft to a persisted save.
    // TextField holds a local buffer while focused and commits on blur; the
    // native select commits on change.
    const cron = screen.getByPlaceholderText("0 9 * * 1-5");
    fireEvent.focus(cron);
    fireEvent.change(cron, { target: { value: "0 9 * * 1-5" } });
    fireEvent.blur(cron);
    await waitFor(() => expect(screen.getByText("Claude Code")).toBeTruthy());
    fireEvent.change(screen.getByRole("combobox"), { target: { value: "claude-code" } });

    await waitFor(() => expect(onSave).toHaveBeenCalled());
    const lastCall = onSave.mock.calls.at(-1)!;
    const [sec, field, value] = lastCall;
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

  it("reorders two valid items and persists the swap", async () => {
    const onSave = vi.fn().mockResolvedValue(true);
    render(
      <SchemaSection
        section="plugin:acme.cron"
        schema={SCHEMA}
        values={{
          jobs: [
            { id: "id-1", agent: "codex", schedule: "0 9 * * 1-5" },
            { id: "id-2", agent: "claude-code", schedule: "0 17 * * 1-5" },
          ],
        }}
        onSaveField={onSave}
      />,
    );

    // Moving the first item down swaps the order; both items stay valid, so the
    // reordered array persists.
    fireEvent.click(screen.getAllByRole("button", { name: "Move down" })[0]!);
    await waitFor(() => expect(onSave).toHaveBeenCalledTimes(1));
    const [, , value] = onSave.mock.calls[0]!;
    expect((value as { id: string }[]).map((it) => it.id)).toEqual(["id-2", "id-1"]);
  });

  it("renders every item field widget kind and the top-level cron/dynamic_select", async () => {
    const onSave = vi.fn().mockResolvedValue(true);
    const schema: SettingsFieldDescriptor[] = [
      {
        section: "plugin:acme.cron",
        field: "when",
        category: "Plugins",
        label: "When",
        description: "",
        web_write: ALLOW,
        profile_overridable: false,
        validation: { rule: "cron" },
        advanced: false,
        widget: { kind: "cron" },
      },
      {
        section: "plugin:acme.cron",
        field: "who",
        category: "Plugins",
        label: "Who",
        description: "",
        web_write: ALLOW,
        profile_overridable: false,
        validation: { rule: "str" },
        advanced: false,
        widget: { kind: "dynamic_select", source: "acp_agents" },
      },
      {
        section: "plugin:acme.cron",
        field: "rows",
        category: "Plugins",
        label: "Rows",
        description: "",
        web_write: ALLOW,
        profile_overridable: false,
        validation: NONE,
        advanced: false,
        widget: {
          kind: "object_list",
          id_field: "id",
          fields: [
            { field: "on", label: "On", required: false, widget: { kind: "toggle" }, validation: { rule: "bool" } },
            { field: "n", label: "N", required: false, widget: { kind: "number" }, validation: { rule: "range_i64" } },
            {
              field: "pick",
              label: "Pick",
              required: false,
              widget: { kind: "select", options: ["a", "b"] },
              validation: { rule: "one_of", options: ["a", "b"] },
            },
            { field: "note", label: "Note", required: false, widget: { kind: "text" }, validation: { rule: "str" } },
          ],
        },
      },
    ];

    render(
      <SchemaSection
        section="plugin:acme.cron"
        schema={schema}
        values={{ when: "0 9 * * 1-5", who: "codex", rows: [{ id: "r1", on: true, n: 2, pick: "a", note: "hi" }] }}
        onSaveField={onSave}
      />,
    );

    // Top-level cron + dynamic_select render, and the object_list item exposes
    // every nested widget kind.
    expect(screen.getByDisplayValue("0 9 * * 1-5")).toBeTruthy();
    expect(screen.getByText("On")).toBeTruthy();
    expect(screen.getByText("N")).toBeTruthy();
    expect(screen.getByText("Pick")).toBeTruthy();
    expect(screen.getByText("Note")).toBeTruthy();
    await waitFor(() => expect(screen.getByText("Claude Code")).toBeTruthy());
  });

  it("re-syncs its working copy when the persisted items change externally", async () => {
    const onSave = vi.fn().mockResolvedValue(true);
    const { rerender } = render(
      <SchemaSection section="plugin:acme.cron" schema={SCHEMA} values={{ jobs: [] }} onSaveField={onSave} />,
    );
    expect(screen.queryByText("Item 1")).toBeNull();

    // An external update to the persisted value flows into the rendered list.
    rerender(
      <SchemaSection
        section="plugin:acme.cron"
        schema={SCHEMA}
        values={{ jobs: [{ id: "id-1", agent: "codex", schedule: "0 9 * * 1-5" }] }}
        onSaveField={onSave}
      />,
    );
    await waitFor(() => expect(screen.getByText("Item 1")).toBeTruthy());
  });
});
