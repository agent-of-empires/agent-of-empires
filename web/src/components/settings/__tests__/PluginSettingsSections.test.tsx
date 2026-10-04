// @vitest-environment jsdom

import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";

vi.mock("../../../lib/api", () => ({
  updateSettings: vi.fn().mockResolvedValue(true),
}));

import { updateSettings } from "../../../lib/api";
import { PluginSettingsSections } from "../PluginSettingsSections";
import type { SettingsFieldDescriptor } from "../../../lib/types";

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

describe("PluginSettingsSections", () => {
  it("renders only plugin sections, seeding the manifest default until a value is stored", () => {
    const { rerender } = render(
      <PluginSettingsSections schema={SCHEMA} settings={{ plugins: {} }} onSaved={() => {}} />,
    );
    expect(screen.getByText("acme.kit")).toBeTruthy();
    expect(screen.queryByText("Idle Decay")).toBeNull();
    expect(screen.getByDisplayValue("3")).toBeTruthy();
    rerender(
      <PluginSettingsSections
        schema={SCHEMA}
        settings={{ plugins: { "acme.kit": { settings: { retries: 4 } } } }}
        onSaved={() => {}}
      />,
    );
    expect(screen.getByDisplayValue("4")).toBeTruthy();
  });

  it("saves through the global PATCH with the plugin:<id> section", async () => {
    const onSaved = vi.fn();
    render(<PluginSettingsSections schema={SCHEMA} settings={{ plugins: {} }} onSaved={onSaved} />);
    fireEvent.click(screen.getByRole("switch"));
    await waitFor(() => {
      expect(updateSettings).toHaveBeenCalledWith({ "plugin:acme.kit": { enabled: false } });
    });
    expect(onSaved).toHaveBeenCalled();
  });
});

describe("PluginSettingsSections list editing while a save is in flight", () => {
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
  const stored = (tags: string[]) => ({ plugins: { "acme.kit": { settings: { tags } } } });
  const held = () => {
    let release: (ok: boolean) => void = () => {};
    const promise = new Promise<boolean>((resolve) => (release = resolve));
    return { promise, release };
  };
  const lastPatch = () => vi.mocked(updateSettings).mock.calls.at(-1)![0];

  it("builds a Save on the removal still waiting for its PATCH and refetch", async () => {
    vi.mocked(updateSettings).mockClear();
    const pendingPatch = held();
    vi.mocked(updateSettings).mockReturnValueOnce(pendingPatch.promise);
    render(<PluginSettingsSections schema={LIST_SCHEMA} settings={stored(["a", "b", "c"])} onSaved={() => {}} />);

    fireEvent.click(screen.getByTitle("Edit b"));
    fireEvent.click(screen.getAllByTitle(/^Remove /)[0]!);
    await waitFor(() => expect(lastPatch()).toEqual({ "plugin:acme.kit": { tags: ["b", "c"] } }));

    // The settings prop is still the old list: neither the PATCH nor the refetch has finished.
    fireEvent.change(await screen.findByDisplayValue("b"), { target: { value: "x" } });
    fireEvent.click(screen.getByText("Save"));

    // Saves of one field go out in order: the edit waits for the removal's PATCH.
    expect(updateSettings).toHaveBeenCalledTimes(1);
    pendingPatch.release(true);
    await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(2));
    expect(lastPatch()).toEqual({ "plugin:acme.kit": { tags: ["x", "c"] } });
  });

  it("reverts the shown list when the save fails", async () => {
    const failing = held();
    vi.mocked(updateSettings).mockReturnValueOnce(failing.promise);
    render(<PluginSettingsSections schema={LIST_SCHEMA} settings={stored(["a", "b"])} onSaved={() => {}} />);
    fireEvent.click(screen.getAllByTitle(/^Remove /)[0]!);
    await waitFor(() => expect(screen.queryByText("a")).toBeNull());
    failing.release(false);
    await waitFor(() => expect(screen.getByText("a")).toBeTruthy());
  });

  it("keeps an acknowledged save when a later queued save fails before the refetch lands", async () => {
    vi.mocked(updateSettings).mockClear();
    const removal = held();
    const edit = held();
    vi.mocked(updateSettings).mockReturnValueOnce(removal.promise).mockReturnValueOnce(edit.promise);
    // The settings prop never changes: the refetch for the acknowledged removal is still pending.
    render(<PluginSettingsSections schema={LIST_SCHEMA} settings={stored(["a", "b", "c"])} onSaved={() => {}} />);

    fireEvent.click(screen.getByTitle("Edit b"));
    fireEvent.click(screen.getAllByTitle(/^Remove /)[0]!);
    fireEvent.change(await screen.findByDisplayValue("b"), { target: { value: "x" } });
    fireEvent.click(screen.getByText("Save"));

    removal.release(true);
    await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(2));
    edit.release(false);

    // The removal was saved, so `a` must not reappear.
    await waitFor(() => expect(screen.getByText("b")).toBeTruthy());
    expect(screen.queryByText("a")).toBeNull();
    fireEvent.click(screen.getByTitle("Edit c"));
    fireEvent.change(screen.getByDisplayValue("c"), { target: { value: "y" } });
    fireEvent.click(screen.getByText("Save"));
    await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(3));
    expect(lastPatch()).toEqual({ "plugin:acme.kit": { tags: ["b", "y"] } });
  });

  it("keeps an acknowledged save when a save submitted after it fails before the refetch lands", async () => {
    vi.mocked(updateSettings).mockClear();
    const removal = held();
    const edit = held();
    vi.mocked(updateSettings).mockReturnValueOnce(removal.promise).mockReturnValueOnce(edit.promise);
    // The settings prop never changes: the refetch for the acknowledged removal is still pending.
    render(<PluginSettingsSections schema={LIST_SCHEMA} settings={stored(["a", "b", "c"])} onSaved={() => {}} />);

    fireEvent.click(screen.getAllByTitle(/^Remove /)[0]!);
    removal.release(true);
    await waitFor(() => expect(screen.queryByText("a")).toBeNull());

    // Only now is the edit submitted, with the removal already acknowledged and nothing in flight.
    fireEvent.click(screen.getByTitle("Edit b"));
    fireEvent.change(screen.getByDisplayValue("b"), { target: { value: "x" } });
    fireEvent.click(screen.getByText("Save"));
    await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(2));
    edit.release(false);

    await waitFor(() => expect(screen.getByText("b")).toBeTruthy());
    expect(screen.queryByText("a")).toBeNull();
    fireEvent.click(screen.getByTitle("Edit c"));
    fireEvent.change(screen.getByDisplayValue("c"), { target: { value: "y" } });
    fireEvent.click(screen.getByText("Save"));
    await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(3));
    expect(lastPatch()).toEqual({ "plugin:acme.kit": { tags: ["b", "y"] } });
  });

  it("shows the refetched settings once they arrive", async () => {
    const done = held();
    vi.mocked(updateSettings).mockReturnValueOnce(done.promise);
    const { rerender } = render(
      <PluginSettingsSections schema={LIST_SCHEMA} settings={stored(["a", "b"])} onSaved={() => {}} />,
    );
    fireEvent.click(screen.getAllByTitle(/^Remove /)[0]!);
    done.release(true);
    await waitFor(() => expect(screen.queryByText("a")).toBeNull());
    rerender(<PluginSettingsSections schema={LIST_SCHEMA} settings={stored(["z"])} onSaved={() => {}} />);
    expect(screen.getByText("z")).toBeTruthy();
    expect(screen.queryByText("b")).toBeNull();
  });
});
