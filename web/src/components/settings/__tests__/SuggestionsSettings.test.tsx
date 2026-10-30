// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { SuggestionsSettings } from "../SuggestionsSettings";
import { useSuggestionsSnapshot } from "../useSuggestionsSnapshot";
import { expandTemplate } from "../../../lib/suggestionTemplate";
import type { SuggestionsSnapshot } from "../../../lib/types";

const api = vi.hoisted(() => ({
  fetchSuggestions: vi.fn(),
  updateSuggestionPrefs: vi.fn(),
  updateSuggestionSource: vi.fn(),
  fetchSuggestionTemplates: vi.fn(),
  putSuggestionTemplate: vi.fn(),
  resetSuggestionTemplate: vi.fn(),
  fetchSuggestionMappings: vi.fn(),
  putSuggestionMapping: vi.fn(),
  deleteSuggestionMapping: vi.fn(),
  fetchProjects: vi.fn(),
}));
vi.mock("../../../lib/api", () => api);

const ok = { ok: true, data: undefined };
const snapshot = (): SuggestionsSnapshot => ({
  prefs: { homepage: true, sidebar: false, refresh_minutes: 15, provider_order: ["gh", "lin"] },
  providers: [
    {
      id: "gh",
      name: "GitHub",
      status: "ok",
      enabled: true,
      updated_at: Math.floor(Date.now() / 1000) - 120,
      sources: [
        {
          criterion: "review-requested",
          label: "Review requests",
          description: "Review requested",
          enabled: true,
          variables: ["pr-number", "repo", "org", "branch"],
        },
        { criterion: "ci-failed", label: "CI failures", description: "", enabled: false, variables: ["branch"] },
      ],
    },
    { id: "lin", name: "Linear", status: "error", error: "API key rejected.", enabled: true, sources: [] },
  ],
  items: [
    {
      id: "1",
      provider_id: "gh",
      title: "T",
      reason: "r",
      source_url: "",
      source_ref: "x",
      preview: "",
      criterion: "review-requested",
      mapping_key: "github.com/o/r",
      variables: { "pr-number": "4562" },
      draft: { title: "T", prompt: "plugin prompt" },
      dismissed: true,
    },
  ],
});

function Harness({ readOnly, onOpenSuggestions }: { readOnly: boolean; onOpenSuggestions: () => void }) {
  const { snapshot, failed, reload } = useSuggestionsSnapshot();
  return (
    <SuggestionsSettings
      snapshot={snapshot}
      failed={failed}
      reload={reload}
      readOnly={readOnly}
      onOpenSuggestions={onOpenSuggestions}
    />
  );
}

function open(props: { readOnly?: boolean } = {}) {
  api.fetchSuggestions.mockResolvedValue(snapshot());
  api.fetchSuggestionTemplates.mockResolvedValue([]);
  api.fetchSuggestionMappings.mockResolvedValue([]);
  api.fetchProjects.mockResolvedValue([{ name: "aoe", path: "/p/aoe", scope: "global", pinned: false }]);
  for (const f of [
    api.updateSuggestionPrefs,
    api.updateSuggestionSource,
    api.putSuggestionTemplate,
    api.resetSuggestionTemplate,
    api.putSuggestionMapping,
    api.deleteSuggestionMapping,
  ]) {
    f.mockResolvedValue(ok);
  }
  const onOpenSuggestions = vi.fn();
  render(<Harness readOnly={props.readOnly ?? false} onOpenSuggestions={onOpenSuggestions} />);
  return onOpenSuggestions;
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("SuggestionsSettings", () => {
  it("shows provider status and writes prefs, sources and order", async () => {
    const openFull = open();
    await screen.findByText("Plugin · updated 2 min ago");
    expect(screen.getByText(/Couldn't refresh: API key rejected\./)).toBeTruthy();
    expect(screen.getByText("1 hidden for this Profile.")).toBeTruthy();

    fireEvent.click(screen.getByRole("switch", { name: "Show in sidebar" }));
    await waitFor(() => expect(api.updateSuggestionPrefs).toHaveBeenCalledWith({ sidebar: true }));
    fireEvent.click(screen.getByRole("switch", { name: "Show Linear suggestions" }));
    await waitFor(() => expect(api.updateSuggestionPrefs).toHaveBeenCalledWith({ provider_enabled: { lin: false } }));
    fireEvent.change(screen.getByLabelText("Refresh every"), { target: { value: "60" } });
    await waitFor(() => expect(api.updateSuggestionPrefs).toHaveBeenCalledWith({ refresh_minutes: 60 }));

    fireEvent.click(screen.getByLabelText("GitHub: CI failures"));
    await waitFor(() => expect(api.updateSuggestionSource).toHaveBeenCalledWith("gh", "ci-failed", true));

    fireEvent.keyDown(screen.getByLabelText("Provider GitHub"), { key: "ArrowDown", altKey: true });
    await waitFor(() => expect(api.updateSuggestionPrefs).toHaveBeenCalledWith({ provider_order: ["lin", "gh"] }));
    fireEvent.keyDown(screen.getByLabelText("Provider GitHub"), { key: "ArrowUp", altKey: true });
    expect(api.updateSuggestionPrefs).not.toHaveBeenCalledWith({ provider_order: ["gh", "lin"] });

    fireEvent.click(screen.getByRole("button", { name: "Show" }));
    expect(openFull).toHaveBeenCalledWith(true);
    expect((screen.getByRole("switch", { name: "Start without review" }) as HTMLButtonElement).disabled).toBe(true);
  });

  it("disables every control when read-only", async () => {
    open({ readOnly: true });
    await screen.findByText("Plugin · updated 2 min ago");
    for (const sw of screen.getAllByRole("switch")) expect((sw as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByLabelText("Refresh every") as HTMLSelectElement).disabled).toBe(true);
  });

  it("edits a template: chip inserts at the cursor, blur saves, preview expands, reset deletes", async () => {
    open();
    fireEvent.click(await screen.findByRole("button", { name: "Edit" }));
    const field = (await screen.findByLabelText("GitHub review-requested template")) as HTMLTextAreaElement;
    expect(screen.getByText("Uses the plugin's own prompt.")).toBeTruthy();
    expect(screen.getByText("Preview: plugin prompt")).toBeTruthy();

    fireEvent.change(field, { target: { value: "/aoe-review #" } });
    field.setSelectionRange(13, 13);
    fireEvent.click(screen.getByRole("button", { name: "{pr-number}" }));
    expect(field.value).toBe("/aoe-review #{pr-number}");
    expect(screen.getByText("Preview: /aoe-review #4562")).toBeTruthy();

    fireEvent.blur(field);
    await waitFor(() =>
      expect(api.putSuggestionTemplate).toHaveBeenCalledWith("gh", "review-requested", "/aoe-review #{pr-number}"),
    );

    fireEvent.change(field, { target: { value: "x {nope}" } });
    expect(screen.getByText("{nope}").className).toContain("underline");
    expect(screen.getByText("Preview: x {nope}")).toBeTruthy();
    fireEvent.change(field, { target: { value: "" } });
    expect(screen.getByText("Preview: starts with an empty prompt, so you type the first message.")).toBeTruthy();

    expect(screen.getByText("CI failures from GitHub won't be suggested.")).toBeTruthy();
  });

  it("keeps typed text when saving fails and locks reorder while a patch is in flight", async () => {
    open();
    let finish: (v: typeof ok) => void = () => {};
    api.updateSuggestionPrefs.mockReturnValue(new Promise((resolve) => (finish = resolve)));
    const github = await screen.findByLabelText("Provider GitHub");
    fireEvent.keyDown(github, { key: "ArrowDown", altKey: true });
    await waitFor(() => expect(api.updateSuggestionPrefs).toHaveBeenCalledTimes(1));
    fireEvent.keyDown(github, { key: "ArrowDown", altKey: true });
    expect(api.updateSuggestionPrefs).toHaveBeenCalledTimes(1);
    finish(ok);
    await waitFor(() => expect(screen.getByLabelText("Drag GitHub to reorder")).toHaveProperty("disabled", false));
    cleanup();

    open();
    api.putSuggestionTemplate.mockResolvedValue({ ok: false, error: "too long", stale: false });
    fireEvent.click(await screen.findByRole("button", { name: "Edit" }));
    const field = (await screen.findByLabelText("GitHub review-requested template")) as HTMLTextAreaElement;
    fireEvent.change(field, { target: { value: "typed" } });
    fireEvent.blur(field);
    expect((await screen.findByRole("alert")).textContent).toContain("too long");
    expect(field.value).toBe("typed");
  });

  it("maps a source to a Project and clears it", async () => {
    open();
    fireEvent.click(await screen.findByRole("button", { name: "Edit" }));
    const row = (await screen.findByText("github.com/o/r")).closest("li") as HTMLElement;
    expect(within(row).getByText("no mapping: the wizard asks")).toBeTruthy();
    const select = within(row).getByLabelText("Map github.com/o/r") as HTMLSelectElement;
    await waitFor(() => expect(select.disabled).toBe(false));
    fireEvent.change(select, { target: { value: "/p/aoe" } });
    await waitFor(() => expect(api.putSuggestionMapping).toHaveBeenCalledWith("gh", "github.com/o/r", ["/p/aoe"]));
  });
});

describe("expandTemplate", () => {
  it("substitutes known names once and leaves the rest as typed", () => {
    expect(expandTemplate("{a} {b} {a{c}}", { a: "{b}", c: "1" })).toBe("{b} {b} {a1}");
  });
});
