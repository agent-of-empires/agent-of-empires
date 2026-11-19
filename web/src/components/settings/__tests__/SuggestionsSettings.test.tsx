// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { SuggestionsSettings } from "../SuggestionsSettings";
import type { PresetFocus } from "../SuggestionPresetEditor";
import { useSuggestionsSnapshot } from "../useSuggestionsSnapshot";
import { expandTemplate } from "../../../lib/suggestionTemplate";
import type { SettingsFieldDescriptor, SuggestionPreset, SuggestionsSnapshot } from "../../../lib/types";
import { loadSettings, resetSettingsStore, saveField, useProfileSettings } from "../../../lib/settingsStore";

const api = vi.hoisted(() => ({
  fetchSuggestions: vi.fn(),
  fetchAgents: vi.fn(),
  fetchSuggestionPresets: vi.fn(),
  runSuggestionFix: vi.fn(),
  updateSuggestionPrefs: vi.fn(),
  updateSuggestionSource: vi.fn(),
  putSuggestionPreset: vi.fn(),
  resetSuggestionPreset: vi.fn(),
  fetchSuggestionMappings: vi.fn(),
  putSuggestionMapping: vi.fn(),
  deleteSuggestionMapping: vi.fn(),
  fetchProjects: vi.fn(),
  fetchSettings: vi.fn(),
  updateSettings: vi.fn(),
}));
vi.mock("../../../lib/api", () => api);

const ok = { ok: true, data: undefined };
const PRESET: SuggestionPreset = {
  agent: "claude",
  view: "structured",
  worktree: true,
  sandbox: false,
  yolo: false,
  prompt: "/aoe-review {pr-number}",
};
const PRESETS = [
  { provider_id: "gh", criterion: "review-requested", customized: false, preset: PRESET, defaults: PRESET },
  {
    provider_id: "gh",
    criterion: "ci-failed",
    customized: true,
    preset: { ...PRESET, view: "terminal", prompt: "" },
    defaults: PRESET,
  },
];
const snapshot = (): SuggestionsSnapshot => ({
  prefs: {
    homepage: true,
    sidebar: false,
    refresh_minutes: 15,
    show_source_ref: true,
    provider_order: ["gh", "lin"],
  },
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
        {
          criterion: "ci-failed",
          label: "CI failures",
          description: "",
          enabled: false,
          variables: ["branch"],
        },
        { criterion: "new-issues", label: "New issues", description: "", enabled: true, variables: [] },
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

function Harness({
  readOnly,
  onOpenSuggestions,
  presetFocus,
}: {
  readOnly: boolean;
  onOpenSuggestions: (dismissed: boolean) => void;
  presetFocus?: PresetFocus;
}) {
  const { snapshot, failed, reload } = useSuggestionsSnapshot();
  return (
    <SuggestionsSettings
      snapshot={snapshot}
      failed={failed}
      reload={reload}
      readOnly={readOnly}
      onOpenSuggestions={onOpenSuggestions}
      presetFocus={presetFocus}
    />
  );
}

function open(props: { readOnly?: boolean; presetFocus?: PresetFocus; presets?: typeof PRESETS } = {}) {
  api.fetchSuggestions.mockResolvedValue(snapshot());
  api.fetchSuggestionPresets.mockResolvedValue(props.presets ?? PRESETS);
  api.fetchAgents.mockResolvedValue([
    { name: "claude", display_name: "Claude Code", kind: "builtin", installed: true },
    { name: "codex", display_name: "Codex", kind: "builtin", installed: true },
  ]);
  api.fetchSuggestionMappings.mockResolvedValue([]);
  api.fetchProjects.mockResolvedValue([{ name: "aoe", path: "/p/aoe", scope: "global", pinned: false }]);
  for (const f of [
    api.updateSuggestionPrefs,
    api.updateSuggestionSource,
    api.putSuggestionPreset,
    api.resetSuggestionPreset,
    api.putSuggestionMapping,
    api.deleteSuggestionMapping,
  ]) {
    f.mockResolvedValue(ok);
  }
  const onOpenSuggestions = vi.fn();
  render(
    <Harness
      readOnly={props.readOnly ?? false}
      onOpenSuggestions={onOpenSuggestions}
      presetFocus={props.presetFocus}
    />,
  );
  return onOpenSuggestions;
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("SuggestionsSettings", () => {
  it("lists Sources and Kinds, and writes prefs, kinds and order", async () => {
    open();
    await screen.findByText("Updated 2 min ago");
    expect(screen.getByText(/Couldn't refresh: API key rejected\./)).toBeTruthy();
    expect(screen.getByText("Drag to set which source comes first, or Alt+↑/↓ on a focused row.")).toBeTruthy();

    fireEvent.click(screen.getByRole("switch", { name: "Show in the sidebar" }));
    await waitFor(() => expect(api.updateSuggestionPrefs).toHaveBeenCalledWith({ sidebar: true }));
    fireEvent.click(screen.getByRole("switch", { name: "Show where each one comes from" }));
    await waitFor(() => expect(api.updateSuggestionPrefs).toHaveBeenCalledWith({ show_source_ref: false }));
    fireEvent.click(screen.getByRole("switch", { name: "Show Linear suggestions" }));
    await waitFor(() => expect(api.updateSuggestionPrefs).toHaveBeenCalledWith({ provider_enabled: { lin: false } }));

    const kinds = screen.getByRole("region", { name: "GitHub kinds" });
    expect(within(kinds).getByText("Claude Code \u00b7 Structured view \u00b7 worktree")).toBeTruthy();
    expect(within(kinds).getByText("Not suggested")).toBeTruthy();
    fireEvent.click(within(kinds).getByRole("switch", { name: "Suggest CI failures" }));
    await waitFor(() => expect(api.updateSuggestionSource).toHaveBeenCalledWith("gh", "ci-failed", true));
  });

  it("reorders sources by keyboard and writes the Advanced settings", async () => {
    const openFull = open();
    await screen.findByText("Updated 2 min ago");
    // Only kinds whose text comes from other people carry the warning.
    expect(screen.getAllByText(/^From other people\./)).toHaveLength(1);

    fireEvent.keyDown(screen.getByLabelText("Source GitHub"), { key: "ArrowDown", altKey: true });
    await waitFor(() => expect(api.updateSuggestionPrefs).toHaveBeenCalledWith({ provider_order: ["lin", "gh"] }));
    fireEvent.keyDown(screen.getByLabelText("Source GitHub"), { key: "ArrowUp", altKey: true });
    expect(api.updateSuggestionPrefs).not.toHaveBeenCalledWith({ provider_order: ["gh", "lin"] });

    fireEvent.change(screen.getByLabelText("Check for new suggestions every"), { target: { value: "60" } });
    await waitFor(() => expect(api.updateSuggestionPrefs).toHaveBeenCalledWith({ refresh_minutes: 60 }));
    expect(screen.getByText("1 hidden.")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Show" }));
    expect(openFull).toHaveBeenCalledWith(true);
    // The old per-kind checkboxes, prompt templates page and unreviewed start are gone.
    expect(screen.queryByText(/Start without review/)).toBeNull();
    expect(screen.queryByText(/Prompt templates/)).toBeNull();
  });

  it("shows the agent-usage note and runs the plugin's fix", async () => {
    const snap = snapshot();
    snap.providers[0].uses_agent = true;
    snap.providers[1].fix = { label: "Fix in Linear settings" };
    api.fetchSuggestions.mockResolvedValue(snap);
    api.fetchAgents.mockResolvedValue([]);
    api.fetchSuggestionPresets.mockResolvedValue(PRESETS);
    api.runSuggestionFix.mockResolvedValue(ok);
    render(<Harness readOnly={false} onOpenSuggestions={vi.fn()} />);
    expect((await screen.findByText(/Uses your Agent's usage/)).textContent).toContain("Updated");
    expect(screen.getAllByText(/Uses your Agent's usage/)).toHaveLength(1);
    fireEvent.click(screen.getByRole("button", { name: "Fix in Linear settings" }));
    await waitFor(() => expect(api.runSuggestionFix).toHaveBeenCalledWith("lin"));
  });

  it("disables every control when read-only", async () => {
    open({ readOnly: true });
    await screen.findByText("Updated 2 min ago");
    for (const sw of screen.getAllByRole("switch")) expect((sw as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByLabelText("Check for new suggestions every") as HTMLSelectElement).disabled).toBe(true);
  });

  it("edits a kind's preset: the fields show it, the prompt previews, Save sends the whole preset", async () => {
    open();
    fireEvent.click(await screen.findByRole("button", { name: "Edit Review requests" }));
    await screen.findByText(/Edit \u00b7 Review requests/);
    expect((screen.getByLabelText("Agent") as HTMLSelectElement).value).toBe("claude");
    expect(screen.getByRole("radio", { name: "Structured" }).getAttribute("aria-checked")).toBe("true");
    expect(screen.getByText("Preview: /aoe-review 4562")).toBeTruthy();

    fireEvent.change(screen.getByLabelText("Agent"), { target: { value: "codex" } });
    fireEvent.click(screen.getByRole("radio", { name: "Terminal" }));
    fireEvent.click(screen.getByRole("switch", { name: "Sandbox" }));
    fireEvent.click(screen.getByRole("switch", { name: "Yolo mode" }));
    fireEvent.change(screen.getByLabelText("GitHub review-requested prompt"), { target: { value: "Review {repo}" } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() =>
      expect(api.putSuggestionPreset).toHaveBeenCalledWith("gh", "review-requested", {
        agent: "codex",
        view: "terminal",
        worktree: true,
        sandbox: true,
        yolo: true,
        prompt: "Review {repo}",
      }),
    );
    // Back on the list once the save and reload land.
    await screen.findByRole("button", { name: "Edit Review requests" });
  });

  it("a preset with no prompt uses the plugin's, and typing one replaces it", async () => {
    open({ presets: [{ ...PRESETS[0]!, preset: { ...PRESET, prompt: null } }, PRESETS[1]!] });
    fireEvent.click(await screen.findByRole("button", { name: "Edit Review requests" }));
    expect(await screen.findByText("Preview: plugin prompt")).toBeTruthy();
    expect(screen.getByText(/Uses GitHub's own prompt until you type one/)).toBeTruthy();
    fireEvent.change(screen.getByLabelText("GitHub review-requested prompt"), { target: { value: "Mine" } });
    expect(screen.getByRole("button", { name: "Use GitHub's prompt" })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() =>
      expect(api.putSuggestionPreset).toHaveBeenCalledWith(
        "gh",
        "review-requested",
        expect.objectContaining({ prompt: "Mine" }),
      ),
    );
  });

  it("Cancel leaves the preset alone, and Reset deletes a customized one", async () => {
    open();
    fireEvent.click(await screen.findByRole("button", { name: "Edit Review requests" }));
    fireEvent.click(await screen.findByRole("button", { name: "Cancel" }));
    expect(api.putSuggestionPreset).not.toHaveBeenCalled();

    fireEvent.click(await screen.findByRole("button", { name: "Edit CI failures" }));
    const reset = await screen.findByRole("button", { name: /^Reset to/ });
    fireEvent.click(reset);
    await waitFor(() => expect(api.resetSuggestionPreset).toHaveBeenCalledWith("gh", "ci-failed"));
  });

  it("keeps the editor open and shows the error when saving fails", async () => {
    open();
    api.putSuggestionPreset.mockResolvedValueOnce({ ok: false, error: "unknown agent", stale: false });
    fireEvent.click(await screen.findByRole("button", { name: "Edit Review requests" }));
    fireEvent.click(await screen.findByRole("button", { name: "Save" }));
    expect((await screen.findByRole("alert")).textContent).toContain("unknown agent");
    expect(screen.getByText(/Edit \u00b7 Review requests/)).toBeTruthy();
  });

  it("says so when the snapshot reload fails, and Retry reloads", async () => {
    open();
    await screen.findByText("Updated 2 min ago");
    api.fetchSuggestions.mockResolvedValueOnce(null);
    fireEvent.click(screen.getByRole("switch", { name: "Show in the sidebar" }));
    await screen.findByText(/Could not reload suggestions/);
    fireEvent.click(screen.getByRole("button", { name: "Retry" }));
    await waitFor(() => expect(screen.queryByText(/Could not reload suggestions/)).toBeNull());
  });

  it("opens straight on a focused preset and returns to the suggestions view", async () => {
    const openFull = open({
      presetFocus: { providerId: "gh", criterion: "review-requested", returnToSuggestions: true },
    });
    await screen.findByText(/Edit \u00b7 Review requests/);
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(openFull).toHaveBeenCalledWith(false);
  });

  it("opens the editor when a focus arrives after mount, once per focus", async () => {
    api.fetchSuggestions.mockResolvedValue(snapshot());
    api.fetchSuggestionPresets.mockResolvedValue(PRESETS);
    api.fetchAgents.mockResolvedValue([]);
    const focus = () => ({ providerId: "gh", criterion: "review-requested" });
    const view = render(<Harness readOnly={false} onOpenSuggestions={vi.fn()} />);
    await screen.findByText("Updated 2 min ago");
    view.rerender(<Harness readOnly={false} onOpenSuggestions={vi.fn()} presetFocus={focus()} />);
    fireEvent.click(await screen.findByRole("button", { name: "Cancel" }));
    await screen.findByText("Updated 2 min ago");
    // A fresh object with the same values (an App re-render) must not reopen it.
    view.rerender(<Harness readOnly={false} onOpenSuggestions={vi.fn()} presetFocus={focus()} />);
    expect(screen.queryByText(/Edit \u00b7 Review requests/)).toBeNull();
  });

  describe("Linked repos", () => {
    const openLinked = async () => {
      open();
      fireEvent.click(await screen.findByText("Advanced"));
      const row = screen.getByText("Linked repos").closest("div.flex") as HTMLElement;
      fireEvent.click(within(row).getByRole("button", { name: "Edit" }));
      return (await screen.findByLabelText("Link github.com/o/r")) as HTMLSelectElement;
    };

    it("links a repo to a Project", async () => {
      const select = await openLinked();
      expect(screen.getByText("0 repos linked to Projects.")).toBeTruthy();
      expect(screen.getByText("no Project yet")).toBeTruthy();
      await waitFor(() => expect(select.disabled).toBe(false));
      fireEvent.change(select, { target: { value: "/p/aoe" } });
      await waitFor(() => expect(api.putSuggestionMapping).toHaveBeenCalledWith("gh", "github.com/o/r", ["/p/aoe"]));
    });

    it("locks the controls until a save and its reload finish", async () => {
      let finish: (v: typeof ok) => void = () => {};
      const select = await openLinked();
      api.putSuggestionMapping.mockReturnValueOnce(new Promise((resolve) => (finish = resolve)));
      await waitFor(() => expect(select.disabled).toBe(false));
      fireEvent.change(select, { target: { value: "/p/aoe" } });
      await waitFor(() => expect(api.putSuggestionMapping).toHaveBeenCalled());
      expect(select.disabled).toBe(true);
      finish(ok);
      await waitFor(() => expect(select.disabled).toBe(false));
    });

    it("keeps the controls locked after a failed reload until Retry succeeds", async () => {
      const select = await openLinked();
      await waitFor(() => expect(select.disabled).toBe(false));
      api.fetchSuggestionMappings.mockResolvedValueOnce(null);
      fireEvent.change(select, { target: { value: "/p/aoe" } });
      await screen.findByText(/Could not reload linked repos/);
      expect(select.disabled).toBe(true);
      fireEvent.click(screen.getByRole("button", { name: "Retry" }));
      await waitFor(() => expect(select.disabled).toBe(false));
      expect(screen.queryByText(/Could not reload linked repos/)).toBeNull();
    });
  });
});

describe("provider source settings", () => {
  const field = (key: string, label: string, advanced = true): SettingsFieldDescriptor => ({
    section: "plugin:gh",
    field: key,
    category: "Plugins",
    label,
    description: "",
    widget: { kind: "text" },
    web_write: { policy: "allow" },
    profile_overridable: false,
    validation: { rule: "none" },
    advanced,
    default: "",
  });
  const SCHEMA = [
    field("suggestion_ignore", "Ignored GitHub suggestion sources"),
    field("token_path", "Token path", false),
  ];

  // Saves the way SettingsView's Plugins tab does: the settings store over PATCH /api/settings.
  function PluginHarness({ readOnly = false }: { readOnly?: boolean }) {
    const { snapshot, failed, reload } = useSuggestionsSnapshot();
    const { settings } = useProfileSettings("main");
    return (
      <SuggestionsSettings
        snapshot={snapshot}
        failed={failed}
        reload={reload}
        readOnly={readOnly}
        pluginSettings={{
          schema: SCHEMA,
          settings,
          onSaveField: (section, f, value) =>
            saveField("main", section, f, value, () => api.updateSettings({ [section]: { [f]: value } }, "main")),
        }}
      />
    );
  }

  it("shows each provider's suggestion_ settings unfolded and saves them as plugin settings", async () => {
    resetSettingsStore();
    api.fetchSuggestions.mockResolvedValue(snapshot());
    api.fetchSettings.mockResolvedValue({ plugins: { gh: { settings: { suggestion_ignore: "acme" } } } });
    api.updateSettings.mockResolvedValue("committed");
    await loadSettings("main");
    render(<PluginHarness />);

    const section = await screen.findByRole("region", { name: "GitHub source settings" });
    expect(screen.queryByRole("region", { name: "Linear source settings" })).toBeNull();
    expect(within(section).queryByText("Token path")).toBeNull();
    expect(within(section).queryByText("Advanced")).toBeNull();
    const input = within(section).getByDisplayValue("acme");
    fireEvent.focus(input);
    fireEvent.change(input, { target: { value: "acme, acme/widgets" } });
    fireEvent.blur(input);
    await waitFor(() =>
      expect(api.updateSettings).toHaveBeenCalledWith(
        { "plugin:gh": { suggestion_ignore: "acme, acme/widgets" } },
        "main",
      ),
    );
  });

  it("keeps a provider's source settings visible but disabled when read-only", async () => {
    resetSettingsStore();
    api.fetchSuggestions.mockResolvedValue(snapshot());
    api.fetchSettings.mockResolvedValue({ plugins: { gh: { settings: { suggestion_ignore: "acme" } } } });
    await loadSettings("main");
    render(<PluginHarness readOnly />);
    const section = await screen.findByRole("region", { name: "GitHub source settings" });
    expect(within(section).getByDisplayValue("acme").matches(":disabled")).toBe(true);
  });
});

describe("expandTemplate", () => {
  it("substitutes known names once and leaves the rest as typed", () => {
    expect(expandTemplate("{a} {b} {a{c}}", { a: "{b}", c: "1" })).toBe("{b} {b} {a1}");
  });

  it("leaves inherited object names as typed", () => {
    expect(expandTemplate("{toString} {constructor} {a}", { a: "1" })).toBe("{toString} {constructor} 1");
  });
});
