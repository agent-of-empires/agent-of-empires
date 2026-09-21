// @vitest-environment jsdom

import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SchemaSection } from "../SchemaSection";
import type { SettingsFieldDescriptor, SettingsObjectField } from "../../../lib/types";

const SECTION = "plugin:acme.cron";
const RESOLVE_URL = "/api/plugins/acme.cron/settings/options/resolve";

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

function descriptor(field: string, widget: SettingsFieldDescriptor["widget"]): SettingsFieldDescriptor {
  return {
    section: SECTION,
    field,
    category: "Plugins",
    label: field,
    description: "",
    web_write: { policy: "allow" },
    profile_overridable: false,
    validation: { rule: "none" },
    advanced: false,
    widget,
  } as SettingsFieldDescriptor;
}

const itemField = (field: string, widget: SettingsObjectField["widget"], required = false) =>
  ({ field, label: field, required, widget, validation: { rule: "str" } }) as SettingsObjectField;
const jobs = (...fields: SettingsObjectField[]) => [
  descriptor("jobs", { kind: "object_list", id_field: "id", fields }),
];

const CRON_JOBS = jobs(
  itemField("agent", { kind: "dynamic_select", source: "acp_agents" }, true),
  itemField("schedule", { kind: "cron" }, true),
);
const MULTI_JOBS = jobs(itemField("projects", { kind: "dynamic_multi_select", source: "projects" }));

function renderSection(schema: SettingsFieldDescriptor[], values: Record<string, unknown>) {
  const onSave = vi.fn().mockResolvedValue(true);
  const utils = render(<SchemaSection section={SECTION} schema={schema} values={values} onSaveField={onSave} />);
  const lastValue = async () => {
    await waitFor(() => expect(onSave).toHaveBeenCalled());
    return onSave.mock.calls.at(-1)![2];
  };
  return { onSave, lastValue, ...utils };
}

const job = (id: string, agent = "codex", schedule = "0 9 * * 1-5") => ({ id, agent, schedule });

describe("object_list", () => {
  it("keeps a new item as a local draft until its required fields are filled", async () => {
    const { onSave, lastValue } = renderSection(CRON_JOBS, { jobs: [] });
    fireEvent.click(screen.getByText("Add item"));
    await screen.findByText("Item 1");
    expect(onSave).not.toHaveBeenCalled();

    const cron = screen.getByPlaceholderText("0 9 * * 1-5");
    fireEvent.focus(cron);
    fireEvent.change(cron, { target: { value: "0 9 * * 1-5" } });
    fireEvent.blur(cron);
    await screen.findByText("Claude Code");
    fireEvent.change(screen.getByRole("combobox"), { target: { value: "claude-code" } });

    const value = (await lastValue()) as { id: string }[];
    expect(onSave.mock.calls.at(-1)!.slice(0, 2)).toEqual([SECTION, "jobs"]);
    expect(value).toHaveLength(1);
    expect(typeof value[0]!.id).toBe("string");
  });

  it("feeds an item's dynamic_select from the plugin's resolver endpoint", async () => {
    renderSection(CRON_JOBS, { jobs: [job("id-1")] });
    await waitFor(() =>
      expect(fetchMock).toHaveBeenCalledWith(RESOLVE_URL, expect.objectContaining({ method: "POST" })),
    );
    expect(await screen.findByText("Claude Code")).toBeTruthy();
  });

  it("removes and reorders items", async () => {
    const removed = renderSection(CRON_JOBS, { jobs: [job("id-1")] });
    fireEvent.click(screen.getByRole("button", { name: "Remove item" }));
    await waitFor(() => expect(removed.onSave).toHaveBeenCalledWith(SECTION, "jobs", []));
    removed.unmount();

    const moved = renderSection(CRON_JOBS, { jobs: [job("id-1"), job("id-2", "claude-code", "0 17 * * 1-5")] });
    fireEvent.click(screen.getAllByRole("button", { name: "Move down" })[0]!);
    expect(((await moved.lastValue()) as { id: string }[]).map((it) => it.id)).toEqual(["id-2", "id-1"]);
  });

  it("renders every item widget kind plus top-level cron and dynamic_select", async () => {
    const schema = [
      descriptor("when", { kind: "cron" }),
      descriptor("who", { kind: "dynamic_select", source: "acp_agents" }),
      ...jobs(
        itemField("On", { kind: "toggle" }),
        itemField("N", { kind: "number" }),
        itemField("Pick", { kind: "select", options: ["a", "b"] } as never),
        itemField("Note", { kind: "text" }),
      ),
    ];
    renderSection(schema, {
      when: "0 9 * * 1-5",
      who: "codex",
      jobs: [{ id: "r1", On: true, N: 2, Pick: "a", Note: "hi" }],
    });
    expect(screen.getByDisplayValue("0 9 * * 1-5")).toBeTruthy();
    for (const label of ["On", "N", "Pick", "Note"]) expect(screen.getByText(label)).toBeTruthy();
    expect(await screen.findByText("Claude Code")).toBeTruthy();
  });

  it("re-syncs its working copy when persisted items change externally", async () => {
    const { rerender, onSave } = renderSection(CRON_JOBS, { jobs: [] });
    expect(screen.queryByText("Item 1")).toBeNull();
    rerender(
      <SchemaSection section={SECTION} schema={CRON_JOBS} values={{ jobs: [job("id-1")] }} onSaveField={onSave} />,
    );
    expect(await screen.findByText("Item 1")).toBeTruthy();
  });
});

describe("dynamic_multi_select", () => {
  it.each([
    [[], ["claude-code"]],
    [["claude-code", "ghost"], ["ghost"]],
  ])("toggling Claude Code on %j persists %j, keeping unavailable values", async (projects, expected) => {
    const { lastValue } = renderSection(MULTI_JOBS, { jobs: [{ id: "j1", projects }] });
    await screen.findByText("Claude Code");
    if (projects.includes("ghost")) expect(screen.getByText("ghost (unavailable)")).toBeTruthy();
    fireEvent.click(screen.getByLabelText("Claude Code"));
    expect(((await lastValue()) as { projects: string[] }[])[0]!.projects).toEqual(expected);
  });

  it("resolves depends_on from sibling item values", async () => {
    const schema = jobs(
      itemField("agent", { kind: "dynamic_select", source: "acp_agents" }),
      itemField("models", { kind: "dynamic_multi_select", source: "acp_models", depends_on: ["agent"] }),
    );
    renderSection(schema, { jobs: [{ id: "j1", agent: "opencode", models: [] }] });
    await waitFor(() =>
      expect(
        fetchMock.mock.calls.some(([url, init]) => url === RESOLVE_URL && String(init.body).includes("opencode")),
      ).toBe(true),
    );
    expect(screen.getByText("models")).toBeTruthy();
  });
});
