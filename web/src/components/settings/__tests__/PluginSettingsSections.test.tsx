// @vitest-environment jsdom

import { beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";

vi.mock("../../../lib/api", () => ({
  updateSettings: vi.fn(),
  fetchSettings: vi.fn(),
}));

import { fetchSettings, updateSettings } from "../../../lib/api";
import type { SaveOutcome } from "../../../lib/api";
import { loadSettings, resetSettingsStore, saveField, useProfileSettings } from "../../../lib/settingsStore";
import { PluginSettingsSections } from "../PluginSettingsSections";
import type { SettingsFieldDescriptor } from "../../../lib/types";

beforeEach(() => {
  resetSettingsStore();
  vi.mocked(updateSettings).mockReset().mockResolvedValue("committed");
  vi.mocked(fetchSettings)
    .mockReset()
    .mockReturnValue(new Promise(() => {}));
});

const ALLOW = { policy: "allow" } as const;
const NONE = { rule: "none" } as const;

const SCHEMA: SettingsFieldDescriptor[] = [
  // A core section is ignored by this component.
  {
    section: "theme",
    field: "idle_decay_minutes",
    category: "Theme",
    label: "Idle Decay",
    description: "",
    widget: { kind: "number" },
    web_write: ALLOW,
    profile_overridable: true,
    validation: NONE,
    advanced: false,
  },
  {
    section: "plugin:acme.kit",
    field: "enabled",
    category: "Plugins",
    label: "Enabled",
    description: "",
    widget: { kind: "toggle" },
    web_write: ALLOW,
    profile_overridable: false,
    validation: NONE,
    advanced: false,
    default: true,
  },
  {
    section: "plugin:acme.kit",
    field: "retries",
    category: "Plugins",
    label: "Retries",
    description: "",
    widget: { kind: "number" },
    web_write: ALLOW,
    profile_overridable: false,
    validation: { rule: "range_u64", min: 0, max: 5 },
    advanced: false,
    default: 3,
  },
];

const LIST_SCHEMA: SettingsFieldDescriptor[] = [
  {
    section: "plugin:acme.kit",
    field: "tags",
    category: "Plugins",
    label: "Tags",
    description: "",
    widget: { kind: "list" },
    web_write: ALLOW,
    profile_overridable: false,
    validation: NONE,
    advanced: false,
  },
];

describe("PluginSettingsSections", () => {
  it("renders only plugin sections, seeding the manifest default until a value is stored", () => {
    const { rerender } = render(
      <PluginSettingsSections schema={SCHEMA} settings={{ plugins: {} }} onSaveField={async () => true} />,
    );
    expect(screen.getByText("acme.kit")).toBeTruthy();
    expect(screen.queryByText("Idle Decay")).toBeNull();
    expect(screen.getByDisplayValue("3")).toBeTruthy();
    rerender(
      <PluginSettingsSections
        schema={SCHEMA}
        settings={{ plugins: { "acme.kit": { settings: { retries: 4 } } } }}
        onSaveField={async () => true}
      />,
    );
    expect(screen.getByDisplayValue("4")).toBeTruthy();
  });

  it("saves a field under its plugin:<id> section", async () => {
    const onSaveField = vi.fn(async () => true);
    render(<PluginSettingsSections schema={SCHEMA} settings={{ plugins: {} }} onSaveField={onSaveField} />);
    fireEvent.click(screen.getByRole("switch"));
    await waitFor(() => expect(onSaveField).toHaveBeenCalledWith("plugin:acme.kit", "enabled", false));
  });
});

describe("plugin list editing through the settings store", () => {
  const stored = (tags: string[]) => ({ plugins: { "acme.kit": { settings: { tags } } } });
  const held = () => {
    let release: (outcome: SaveOutcome) => void = () => {};
    const promise = new Promise<SaveOutcome>((resolve) => (release = resolve));
    return { promise, release };
  };
  const lastPatch = () => vi.mocked(updateSettings).mock.calls.at(-1)![0];

  function Harness() {
    const { settings, error } = useProfileSettings("main");
    return (
      <>
        <PluginSettingsSections
          schema={LIST_SCHEMA}
          settings={settings}
          onSaveField={(section, field, value) =>
            saveField("main", section, field, value, () => updateSettings({ [section]: { [field]: value } }, "main"))
          }
        />
        {error && <p role="alert">{error}</p>}
      </>
    );
  }

  async function mount(tags: string[]) {
    vi.mocked(fetchSettings).mockResolvedValueOnce(stored(tags));
    await act(async () => {
      await loadSettings("main");
    });
    const view = render(<Harness />);
    await waitFor(() => expect(screen.getByText(tags[0]!)).toBeTruthy());
    return view;
  }

  const removeFirst = () => fireEvent.click(screen.getAllByTitle(/^Remove /)[0]!);

  it("sends a second edit after the first PATCH, built on the first", async () => {
    const first = held();
    vi.mocked(updateSettings).mockReturnValueOnce(first.promise);
    await mount(["a", "b", "c"]);

    fireEvent.click(screen.getByTitle("Edit b"));
    removeFirst();
    await waitFor(() => expect(lastPatch()).toEqual({ "plugin:acme.kit": { tags: ["b", "c"] } }));
    fireEvent.change(await screen.findByDisplayValue("b"), { target: { value: "x" } });
    fireEvent.click(screen.getByText("Save"));

    expect(updateSettings).toHaveBeenCalledTimes(1);
    first.release("committed");
    await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(2));
    expect(lastPatch()).toEqual({ "plugin:acme.kit": { tags: ["x", "c"] } });
  });

  it("shows the saved list again when the server rejects the save", async () => {
    const failing = held();
    vi.mocked(updateSettings).mockReturnValueOnce(failing.promise);
    await mount(["a", "b"]);
    vi.mocked(fetchSettings).mockResolvedValueOnce(stored(["a", "b"]));
    removeFirst();
    await waitFor(() => expect(screen.queryByText("a")).toBeNull());

    failing.release("rejected");
    await waitFor(() => expect(screen.getByText("a")).toBeTruthy());
    expect(screen.getByRole("alert").textContent).toMatch(/Failed to save/);
  });

  it("keeps an acknowledged removal when a later queued save is rejected", async () => {
    const removal = held();
    const edit = held();
    vi.mocked(updateSettings).mockReturnValueOnce(removal.promise).mockReturnValueOnce(edit.promise);
    await mount(["a", "b", "c"]);
    vi.mocked(fetchSettings).mockResolvedValueOnce(stored(["b", "c"]));

    fireEvent.click(screen.getByTitle("Edit b"));
    removeFirst();
    fireEvent.change(await screen.findByDisplayValue("b"), { target: { value: "x" } });
    fireEvent.click(screen.getByText("Save"));
    removal.release("committed");
    await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(2));
    edit.release("rejected");

    await waitFor(() => expect(screen.getByText("b")).toBeTruthy());
    expect(screen.queryByText("a")).toBeNull();
    fireEvent.click(screen.getByTitle("Edit c"));
    fireEvent.change(screen.getByDisplayValue("c"), { target: { value: "y" } });
    fireEvent.click(screen.getByText("Save"));
    await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(3));
    expect(lastPatch()).toEqual({ "plugin:acme.kit": { tags: ["b", "y"] } });
  });

  it("builds the next edit on a removal still in flight after the list remounts", async () => {
    const removal = held();
    vi.mocked(updateSettings).mockReturnValueOnce(removal.promise);
    const first = await mount(["a", "b", "c"]);
    removeFirst();
    await waitFor(() => expect(lastPatch()).toEqual({ "plugin:acme.kit": { tags: ["b", "c"] } }));

    first.unmount();
    render(<Harness />);
    await waitFor(() => expect(screen.queryByText("a")).toBeNull());
    removeFirst();

    expect(updateSettings).toHaveBeenCalledTimes(1);
    removal.release("committed");
    await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(2));
    expect(lastPatch()).toEqual({ "plugin:acme.kit": { tags: ["c"] } });
  });

  it("keeps a committed removal across a remount while a refetch has not landed", async () => {
    const first = await mount(["a", "b", "c"]);
    removeFirst();
    await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(1));

    // Nothing refetched: the cache still shows the optimistic list when the tab reopens.
    first.unmount();
    render(<Harness />);
    expect(screen.queryByText("a")).toBeNull();
    removeFirst();
    await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(2));
    expect(lastPatch()).toEqual({ "plugin:acme.kit": { tags: ["c"] } });
  });

  it("warns and keeps the edit when a write never landed and every fetch shows the old list", async () => {
    vi.mocked(updateSettings).mockResolvedValue("uncertain");
    await mount(["a", "b", "c"]);
    vi.mocked(fetchSettings).mockResolvedValue(stored(["a", "b", "c"]));

    removeFirst();

    await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(3));
    await waitFor(() =>
      expect(screen.getByRole("alert").textContent).toMatch(/Could not confirm plugin:acme\.kit\.tags/),
    );
    expect(screen.queryByText("a")).toBeNull();

    // A late commit and a later edit cannot restore `a`: the edit builds on the kept list.
    removeFirst();
    await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(4));
    expect(lastPatch()).toEqual({ "plugin:acme.kit": { tags: ["c"] } });
  });
});
