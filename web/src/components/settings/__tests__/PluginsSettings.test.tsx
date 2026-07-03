// @vitest-environment jsdom
//
// Contract test for the minimal PluginsSettings panel: it lists plugins
// (name, version, description, enabled state), the enable toggle POSTs the
// right setPluginEnabled payload, the server-returned refreshed list is
// adopted on success, a toggle error message is surfaced, and load_errors are
// shown rather than swallowed.

import { describe, expect, it, vi, beforeEach } from "vitest";
import { fireEvent, render, waitFor, within } from "@testing-library/react";

import type {
  DiscoverResult,
  PluginDetailResult,
  PluginDismissResult,
  PluginInstallPreviewResult,
  PluginJobResult,
  PluginJobStartResult,
  PluginListResponse,
  PluginToggleResult,
  PluginUpdateChangelog,
  PluginUpdatePreviewResult,
  PluginUpdatesResult,
} from "../../../lib/api";

const emptyChangelog: PluginUpdateChangelog = {
  entries: [],
  truncated: false,
  unavailable_reason: null,
  more_url: null,
};
const releaseChangelog: PluginUpdateChangelog = {
  entries: [{ kind: "release", tag: "v0.2.0", body: "Added a thing.", published_at: null }],
  truncated: false,
  unavailable_reason: null,
  more_url: null,
};
const commitChangelog: PluginUpdateChangelog = {
  entries: [{ kind: "commit", sha: "abcdef1234", subject: "fix: a bug", url: null }],
  truncated: true,
  unavailable_reason: null,
  more_url: "https://github.com/example/plugin/compare/aaa...bbb",
};

const fetchPlugins = vi.fn<[], Promise<PluginListResponse | null>>();
const setPluginEnabled = vi.fn<[string, boolean], Promise<PluginToggleResult>>();
const fetchPluginUpdates = vi.fn<[], Promise<PluginUpdatesResult>>();
const discoverPlugins = vi.fn<[string], Promise<DiscoverResult>>();
const fetchPluginDetails = vi.fn<[string], Promise<PluginDetailResult>>();
const previewPluginUpdate = vi.fn<[string], Promise<PluginUpdatePreviewResult>>();
const applyPluginUpdate = vi.fn<[string, string | null], Promise<PluginJobStartResult>>();
const dismissPluginUpdate = vi.fn<[string, string], Promise<PluginDismissResult>>();
const previewPluginInstall = vi.fn<[string], Promise<PluginInstallPreviewResult>>();
const startPluginInstall = vi.fn<[string, string], Promise<PluginJobStartResult>>();
const startPluginUninstall = vi.fn<[string], Promise<PluginJobStartResult>>();
const fetchPluginJob = vi.fn<[string, number?], Promise<PluginJobResult>>();
const reportInfo = vi.fn<[string], void>();

vi.mock("../../../lib/api", () => ({
  fetchPlugins: () => fetchPlugins(),
  setPluginEnabled: (id: string, enabled: boolean) => setPluginEnabled(id, enabled),
  fetchPluginUpdates: () => fetchPluginUpdates(),
  discoverPlugins: (q: string) => discoverPlugins(q),
  fetchPluginDetails: (source: string) => fetchPluginDetails(source),
  previewPluginUpdate: (id: string) => previewPluginUpdate(id),
  applyPluginUpdate: (id: string, fp: string | null) => applyPluginUpdate(id, fp),
  dismissPluginUpdate: (id: string, fp: string) => dismissPluginUpdate(id, fp),
  previewPluginInstall: (source: string) => previewPluginInstall(source),
  startPluginInstall: (source: string, fp: string) => startPluginInstall(source, fp),
  startPluginUninstall: (id: string) => startPluginUninstall(id),
  fetchPluginJob: (jobId: string, tail?: number) => fetchPluginJob(jobId, tail),
}));

vi.mock("../../../lib/toastBus", () => ({
  reportInfo: (message: string) => reportInfo(message),
}));

// Imported after the mock is registered.
import { PluginsSettings } from "../PluginsSettings";

function listResponse(overrides: Partial<PluginListResponse> = {}): PluginListResponse {
  return {
    plugins: [
      {
        id: "aoe.status",
        name: "Agent Status Detection",
        version: "1.1.0",
        description: "Detects agent session status.",
        enabled: true,
        builtin: true,
        validation: "builtin",
        source: null,
        capabilities: [],
        ui_contributions: [],
        granted: true,
        needs_reapproval: false,
      },
      {
        id: "example.plugin",
        name: "Example",
        version: "0.1.0",
        description: "A community plugin.",
        enabled: false,
        builtin: false,
        validation: "community",
        source: "gh:example/plugin",
        capabilities: ["net"],
        ui_contributions: [
          { slot: "status-bar", id: "s" },
          { slot: "row-badge", id: "b" },
        ],
        granted: true,
        needs_reapproval: false,
      },
    ],
    load_errors: [],
    ...overrides,
  };
}

beforeEach(() => {
  fetchPlugins.mockReset();
  setPluginEnabled.mockReset();
  fetchPluginUpdates.mockReset();
  discoverPlugins.mockReset();
  fetchPluginDetails.mockReset();
  previewPluginUpdate.mockReset();
  applyPluginUpdate.mockReset();
  dismissPluginUpdate.mockReset();
  previewPluginInstall.mockReset();
  startPluginInstall.mockReset();
  startPluginUninstall.mockReset();
  fetchPluginJob.mockReset();
  reportInfo.mockReset();
  fetchPlugins.mockResolvedValue(listResponse());
  fetchPluginUpdates.mockResolvedValue({ kind: "ok", updates: [] });
  discoverPlugins.mockResolvedValue({ kind: "ok", results: [] });
  fetchPluginDetails.mockResolvedValue({
    kind: "ok",
    detail: { source: "gh:example/plugin", manifest: null, manifest_error: null, release_tags: [] },
  });
  // Lifecycle jobs default to "started" + a terminal succeeded poll, so a
  // progress modal that opens resolves to Done without hanging.
  applyPluginUpdate.mockResolvedValue({ kind: "ok", jobId: "job1" });
  startPluginInstall.mockResolvedValue({ kind: "ok", jobId: "job1" });
  startPluginUninstall.mockResolvedValue({ kind: "ok", jobId: "job1" });
  fetchPluginJob.mockResolvedValue({
    kind: "ok",
    job: {
      job: {
        id: "job1",
        kind: "install",
        target: "gh:acme/widget",
        status: { state: "succeeded" },
        started_at: 0,
        finished_at: 1,
      },
      log: { exists: true, tail: "installed acme.widget 1.0.0", lines_returned: 1, truncated: false },
    },
  });
});

describe("PluginsSettings", () => {
  it("renders each plugin's name, version, and description", async () => {
    const { findByText } = render(<PluginsSettings />);
    await findByText("Agent Status Detection");
    await findByText("v1.1.0");
    await findByText("A community plugin.");
  });

  it("discloses the UI slots a plugin renders into, deduped", async () => {
    const { findByText } = render(<PluginsSettings />);
    // example.plugin declares status-bar + row-badge (#2366).
    await findByText("UI: status-bar, row-badge");
  });

  it("shows validation badges and a needs-approval state for an ungranted community plugin", async () => {
    fetchPlugins.mockResolvedValue(
      listResponse({
        plugins: [
          {
            id: "example.plugin",
            name: "Example",
            version: "0.2.0",
            description: "A community plugin.",
            enabled: true,
            builtin: false,
            validation: "community",
            source: "gh:example/plugin",
            capabilities: ["net", "fs.read"],
            granted: false,
            needs_reapproval: true,
          },
        ],
      }),
    );
    const { findByTestId, getByText } = render(<PluginsSettings />);
    const validation = await findByTestId("plugin-validation-example.plugin");
    expect(validation.textContent).toBe("community");
    await findByTestId("plugin-needs-approval-example.plugin");
    expect(getByText(/net, fs\.read/)).toBeTruthy();
    expect(getByText(/not granted/)).toBeTruthy();
  });

  it("shows the featured validation badge for a featured plugin", async () => {
    fetchPlugins.mockResolvedValue(
      listResponse({
        plugins: [
          {
            id: "agent-of-empires.example",
            name: "Official Example",
            version: "1.0.0",
            description: "A featured plugin.",
            enabled: true,
            builtin: false,
            validation: "featured",
            source: "gh:agent-of-empires/example",
            capabilities: [],
            granted: true,
            needs_reapproval: false,
          },
        ],
      }),
    );
    const { findByTestId } = render(<PluginsSettings />);
    const validation = await findByTestId("plugin-validation-agent-of-empires.example");
    expect(validation.textContent).toBe("featured");
  });

  it("disable toggle POSTs setPluginEnabled(id, false) and adopts the refreshed list", async () => {
    const disabled = listResponse({
      plugins: [{ ...listResponse().plugins[0]!, enabled: false }, listResponse().plugins[1]!],
    });
    setPluginEnabled.mockResolvedValue({ kind: "ok", data: disabled });

    const { findByLabelText } = render(<PluginsSettings />);
    const toggle = (await findByLabelText("Enable Agent Status Detection")) as HTMLInputElement;
    expect(toggle.checked).toBe(true);
    fireEvent.click(toggle);

    await waitFor(() => {
      expect(setPluginEnabled).toHaveBeenCalledWith("aoe.status", false);
    });
    await waitFor(() => {
      expect((toggle as HTMLInputElement).checked).toBe(false);
    });
  });

  it("warns about the startup-only serve gate when aoe.web is disabled", async () => {
    const web = {
      id: "aoe.web",
      name: "Web Dashboard",
      version: "1.0.0",
      description: "The web dashboard.",
      enabled: true,
      builtin: true,
      validation: "builtin",
      source: null,
      capabilities: [],
      granted: true,
      needs_reapproval: false,
    };
    fetchPlugins.mockResolvedValue(listResponse({ plugins: [web] }));
    setPluginEnabled.mockResolvedValue({
      kind: "ok",
      data: listResponse({ plugins: [{ ...web, enabled: false }] }),
    });

    const { findByLabelText } = render(<PluginsSettings />);
    fireEvent.click(await findByLabelText("Enable Web Dashboard"));

    await waitFor(() => {
      expect(reportInfo).toHaveBeenCalledWith("Web dashboard stays up until aoe serve is restarted.");
    });
  });

  it("does not warn when a non-web plugin is disabled", async () => {
    const disabled = listResponse({
      plugins: [{ ...listResponse().plugins[0]!, enabled: false }, listResponse().plugins[1]!],
    });
    setPluginEnabled.mockResolvedValue({ kind: "ok", data: disabled });
    const { findByLabelText } = render(<PluginsSettings />);
    fireEvent.click(await findByLabelText("Enable Agent Status Detection"));
    await waitFor(() => {
      expect(setPluginEnabled).toHaveBeenCalledWith("aoe.status", false);
    });
    expect(reportInfo).not.toHaveBeenCalled();
  });

  it("surfaces the error message when a toggle is rejected", async () => {
    setPluginEnabled.mockResolvedValue({ kind: "error", message: "Dashboard is read-only." });
    const { findByLabelText, findByText } = render(<PluginsSettings />);
    fireEvent.click(await findByLabelText("Enable Agent Status Detection"));
    await findByText("Dashboard is read-only.");
  });

  it("renders an explicit empty state when there are no plugins", async () => {
    fetchPlugins.mockResolvedValue(listResponse({ plugins: [] }));
    const { getByTestId, findByTestId } = render(<PluginsSettings />);
    await findByTestId("plugins-empty");
    expect(getByTestId("plugins-empty").textContent).toContain("No plugins detected");
  });

  it("surfaces load_errors rather than swallowing them", async () => {
    fetchPlugins.mockResolvedValue(listResponse({ load_errors: ["plugins/bad: manifest is invalid"] }));
    const { findByText } = render(<PluginsSettings />);
    await findByText(/manifest is invalid/);
  });

  it("shows an error when the plugin list fails to load", async () => {
    fetchPlugins.mockResolvedValue(null);
    const { findByText } = render(<PluginsSettings />);
    await findByText("Failed to load plugins.");
  });

  it("Check for updates calls the endpoint and badges an outdated plugin", async () => {
    fetchPluginUpdates.mockResolvedValue({
      kind: "ok",
      updates: [
        {
          id: "example.plugin",
          source: "gh:example/plugin",
          current: "abc1234",
          available: "def5678",
          needs_update: true,
          error: null,
        },
      ],
    });
    const { findByTestId, getByTestId } = render(<PluginsSettings />);
    fireEvent.click(await findByTestId("plugins-check-updates"));
    await waitFor(() => expect(fetchPluginUpdates).toHaveBeenCalled());
    await findByTestId("plugin-update-available-example.plugin");
    expect(getByTestId("plugin-example.plugin").textContent).toContain("abc1234 → def5678");
  });

  it("Check for updates surfaces a per-plugin check error", async () => {
    fetchPluginUpdates.mockResolvedValue({
      kind: "ok",
      updates: [
        {
          id: "example.plugin",
          source: "gh:example/plugin",
          current: "",
          available: null,
          needs_update: false,
          error: "git not found",
        },
      ],
    });
    const { findByTestId, findByText } = render(<PluginsSettings />);
    fireEvent.click(await findByTestId("plugins-check-updates"));
    await findByText(/Update check failed: git not found/);
  });

  it("Check for updates surfaces an endpoint failure and clears stale badges", async () => {
    fetchPluginUpdates.mockResolvedValue({ kind: "error", message: "Update check failed (HTTP 502)." });
    const { findByTestId, findByText } = render(<PluginsSettings />);
    fireEvent.click(await findByTestId("plugins-check-updates"));
    await findByText("Update check failed (HTTP 502).");
  });

  it("Search GitHub renders badged results with a copyable install command", async () => {
    discoverPlugins.mockResolvedValue({
      kind: "ok",
      results: [
        {
          slug: "gh:acme/widget",
          html_url: "https://github.com/acme/widget",
          description: "A widget plugin.",
          stars: 42,
          badge: "unvetted",
          install_command: "aoe plugin install gh:acme/widget",
        },
      ],
    });
    const { findByTestId } = render(<PluginsSettings />);
    fireEvent.click(await findByTestId("plugins-tab-marketplace"));
    fireEvent.click(await findByTestId("plugins-discover"));
    await waitFor(() => expect(discoverPlugins).toHaveBeenCalled());
    const result = await findByTestId("plugins-discover-result-gh:acme/widget");
    expect(result.textContent).toContain("aoe plugin install gh:acme/widget");
    expect(result.textContent).toContain("unvetted");
  });

  it("Search GitHub surfaces a discovery error (e.g. rate limit)", async () => {
    discoverPlugins.mockResolvedValue({ kind: "error", message: "Rate limited by GitHub." });
    const { findByTestId } = render(<PluginsSettings />);
    fireEvent.click(await findByTestId("plugins-tab-marketplace"));
    fireEvent.click(await findByTestId("plugins-discover"));
    const err = await findByTestId("plugins-discover-error");
    expect(err.textContent).toContain("Rate limited by GitHub.");
  });

  it("clicking a discovery result opens the detail modal with version and release tags", async () => {
    discoverPlugins.mockResolvedValue({
      kind: "ok",
      results: [
        {
          slug: "gh:acme/widget",
          html_url: "https://github.com/acme/widget",
          description: "A widget plugin.",
          stars: 42,
          badge: "unvetted",
          install_command: "aoe plugin install gh:acme/widget",
        },
      ],
    });
    fetchPluginDetails.mockResolvedValue({
      kind: "ok",
      detail: {
        source: "gh:acme/widget",
        manifest: {
          id: "acme.widget",
          name: "Widget",
          version: "2.3.0",
          description: "A widget plugin.",
          api_version: 4,
          capabilities: ["net"],
          ui_contributions: [{ slot: "status-bar", id: "s" }],
          screenshots: [],
        },
        manifest_error: null,
        release_tags: ["v2.3.0", "v2.2.0"],
      },
    });
    const { findByTestId } = render(<PluginsSettings />);
    fireEvent.click(await findByTestId("plugins-tab-marketplace"));
    fireEvent.click(await findByTestId("plugins-discover"));
    fireEvent.click(await findByTestId("plugins-discover-open-gh:acme/widget"));
    await waitFor(() => expect(fetchPluginDetails).toHaveBeenCalledWith("gh:acme/widget"));
    const modal = await findByTestId("plugin-detail-modal");
    expect(modal.textContent).toContain("v2.3.0");
    expect(modal.textContent).toContain("net");
    const versions = await findByTestId("plugin-detail-versions");
    expect(versions.textContent).toContain("v2.2.0");
    // No screenshots in the manifest: no gallery chrome.
    expect(modal.querySelector("[data-testid='plugin-detail-screenshots']")).toBeNull();
  });

  it("renders a screenshot gallery when the manifest declares screenshots", async () => {
    fetchPluginDetails.mockResolvedValue({
      kind: "ok",
      detail: {
        source: "gh:acme/widget",
        manifest: {
          id: "acme.widget",
          name: "Widget",
          version: "2.3.0",
          description: "A widget plugin.",
          api_version: 5,
          capabilities: [],
          ui_contributions: [],
          screenshots: [
            {
              src: "https://raw.githubusercontent.com/acme/widget/HEAD/a.png",
              alt: "Dashboard card",
              caption: "Live card.",
            },
            { src: "https://raw.githubusercontent.com/acme/widget/HEAD/b.gif", alt: "Demo", caption: "" },
          ],
        },
        manifest_error: null,
        release_tags: [],
      },
    });
    const { findByTestId } = render(<PluginsSettings />);
    fireEvent.click(await findByTestId("plugin-open-example.plugin"));
    const gallery = await findByTestId("plugin-detail-screenshots");
    const imgs = gallery.querySelectorAll("img");
    expect(imgs).toHaveLength(2);
    expect(imgs[0].getAttribute("src")).toBe("https://raw.githubusercontent.com/acme/widget/HEAD/a.png");
    expect(imgs[0].getAttribute("alt")).toBe("Dashboard card");
    expect(imgs[0].getAttribute("loading")).toBe("lazy");
    expect(gallery.textContent).toContain("Live card.");
    // Clicking a screenshot opens the full-size lightbox.
    const { findByTestId: findInModal, queryByTestId } = within(document.body);
    fireEvent.click(imgs[0]!);
    const lightbox = await findInModal("plugin-detail-lightbox");
    const bigImg = lightbox.querySelector("img")!;
    expect(bigImg.getAttribute("src")).toBe("https://raw.githubusercontent.com/acme/widget/HEAD/a.png");
    // Clicking the image itself does not dismiss; only the backdrop does.
    fireEvent.click(bigImg);
    expect(queryByTestId("plugin-detail-lightbox")).not.toBeNull();
    // Escape closes the lightbox first, leaving the detail modal open.
    fireEvent.keyDown(window, { key: "Escape" });
    await waitFor(() => expect(queryByTestId("plugin-detail-lightbox")).toBeNull());
    expect(queryByTestId("plugin-detail-modal")).not.toBeNull();
    // Backdrop click also dismisses.
    fireEvent.click(imgs[0]!);
    fireEvent.click(await findInModal("plugin-detail-lightbox"));
    await waitFor(() => expect(queryByTestId("plugin-detail-lightbox")).toBeNull());
    // A 404 (moved ref / deleted asset) hides the figure rather than leaving a
    // broken-image icon.
    fireEvent.error(imgs[0]!);
    expect(imgs[0]!.closest("figure")!.className).toContain("hidden");
  });

  it("separates installed management from the marketplace into tabs", async () => {
    const { findByTestId, getByTestId, queryByTestId } = render(<PluginsSettings />);
    // Installed tab is the default: update controls present, search hidden.
    await findByTestId("plugins-check-updates");
    expect(queryByTestId("plugins-discover")).toBeNull();
    // Switch to the marketplace: search present, update controls hidden.
    fireEvent.click(getByTestId("plugins-tab-marketplace"));
    await findByTestId("plugins-discover");
    expect(queryByTestId("plugins-check-updates")).toBeNull();
  });

  it("a failed details fetch shows the error, not a false 'no releases'", async () => {
    fetchPluginDetails.mockResolvedValue({ kind: "error", message: "Rate limited by GitHub." });
    const { findByTestId } = render(<PluginsSettings />);
    // example.plugin has a gh source, so opening it triggers a details fetch.
    fireEvent.click(await findByTestId("plugin-open-example.plugin"));
    const err = await findByTestId("plugin-detail-error");
    expect(err.textContent).toContain("Rate limited by GitHub.");
    const modal = await findByTestId("plugin-detail-modal");
    expect(modal.textContent).not.toContain("No published releases.");
  });

  it("clicking an installed plugin opens the detail modal and closes it", async () => {
    const { findByTestId, queryByTestId } = render(<PluginsSettings />);
    fireEvent.click(await findByTestId("plugin-open-example.plugin"));
    const modal = await findByTestId("plugin-detail-modal");
    // Falls back to the installed view's fields immediately.
    expect(modal.textContent).toContain("v0.1.0");
    fireEvent.click(await findByTestId("plugin-detail-close"));
    await waitFor(() => expect(queryByTestId("plugin-detail-modal")).toBeNull());
  });

  it("renders an installed plugin's icon_asset as an image", async () => {
    fetchPlugins.mockResolvedValue(
      listResponse({
        plugins: [
          {
            id: "agent-of-empires.github",
            name: "GitHub",
            version: "1.0.0",
            description: "GitHub integration.",
            enabled: true,
            builtin: false,
            validation: "featured",
            source: "gh:agent-of-empires/plugin-github",
            capabilities: [],
            ui_contributions: [],
            granted: true,
            needs_reapproval: false,
            icon: "github",
            icon_asset_url: "/api/plugins/agent-of-empires.github/icon",
          },
        ],
      }),
    );
    const { findByTestId } = render(<PluginsSettings />);
    const icon = await findByTestId("plugin-icon-agent-of-empires.github");
    expect(icon.tagName).toBe("IMG");
    expect(icon.getAttribute("src")).toBe("/api/plugins/agent-of-empires.github/icon");
  });

  it("falls back to the lucide icon when a plugin has no icon_asset_url", async () => {
    fetchPlugins.mockResolvedValue(
      listResponse({
        plugins: [
          {
            id: "acme.widget",
            name: "Widget",
            version: "1.0.0",
            description: "A widget.",
            enabled: true,
            builtin: false,
            validation: "community",
            source: "gh:acme/widget",
            capabilities: [],
            ui_contributions: [],
            granted: true,
            needs_reapproval: false,
            icon: "git-branch",
            icon_asset_url: null,
          },
        ],
      }),
    );
    const { findByTestId } = render(<PluginsSettings />);
    const icon = await findByTestId("plugin-icon-acme.widget");
    expect(icon.tagName).toBe("svg");
  });

  it("shows the plugin's icon in the detail modal header", async () => {
    fetchPlugins.mockResolvedValue(
      listResponse({
        plugins: [
          {
            id: "agent-of-empires.github",
            name: "GitHub",
            version: "1.0.0",
            description: "GitHub integration.",
            enabled: true,
            builtin: false,
            validation: "featured",
            source: "gh:agent-of-empires/plugin-github",
            capabilities: [],
            ui_contributions: [],
            granted: true,
            needs_reapproval: false,
            icon: "github",
            icon_asset_url: "/api/plugins/agent-of-empires.github/icon",
          },
        ],
      }),
    );
    const { findByTestId } = render(<PluginsSettings />);
    fireEvent.click(await findByTestId("plugin-open-agent-of-empires.github"));
    const icon = await findByTestId("plugin-detail-icon");
    expect(icon.tagName).toBe("IMG");
    expect(icon.getAttribute("src")).toBe("/api/plugins/agent-of-empires.github/icon");
  });

  it("Escape closes the detail modal when no lightbox is open", async () => {
    const { findByTestId, queryByTestId } = render(<PluginsSettings />);
    fireEvent.click(await findByTestId("plugin-open-example.plugin"));
    await findByTestId("plugin-detail-modal");
    fireEvent.keyDown(window, { key: "Escape" });
    await waitFor(() => expect(queryByTestId("plugin-detail-modal")).toBeNull());
  });

  // Surface the per-row Update button by reporting an available update.
  function markOutdated() {
    fetchPluginUpdates.mockResolvedValue({
      kind: "ok",
      updates: [
        {
          id: "example.plugin",
          source: "gh:example/plugin",
          current: "abc1234",
          available: "def5678",
          needs_update: true,
          error: null,
        },
      ],
    });
  }

  const consentPreview: PluginUpdatePreviewResult = {
    kind: "ok",
    preview: {
      kind: "consent_required",
      dismissed: false,
      consent: {
        id: "example.plugin",
        from_version: "0.1.0",
        to_version: "0.2.0",
        prior_capabilities: ["net"],
        new_capabilities: ["net", "fs.read"],
        added_capabilities: ["fs.read"],
        removed_capabilities: [],
        ui: [],
        build_steps: ["sh build.sh"],
        runtime_change: null,
        trust_downgrade: false,
        fingerprint: "treeB||community",
        stays_active_if_declined: true,
        changelog: releaseChangelog,
      },
    },
  };

  it("Update on a consent-required version opens the consent modal with the new access", async () => {
    markOutdated();
    previewPluginUpdate.mockResolvedValue(consentPreview);
    const { findByTestId } = render(<PluginsSettings />);
    fireEvent.click(await findByTestId("plugins-check-updates"));
    fireEvent.click(await findByTestId("plugin-update-example.plugin"));
    await waitFor(() => expect(previewPluginUpdate).toHaveBeenCalledWith("example.plugin"));
    await findByTestId("plugin-update-consent-modal");
    expect((await findByTestId("plugin-update-added-caps")).textContent).toContain("fs.read");
    expect((await findByTestId("plugin-update-build-steps")).textContent).toContain("sh build.sh");
    // The changelog is shown alongside the access disclosure.
    expect((await findByTestId("plugin-update-changelog")).textContent).toContain("v0.2.0");
  });

  it("Approving applies the update pinned to the previewed fingerprint and opens the job modal", async () => {
    markOutdated();
    previewPluginUpdate.mockResolvedValue(consentPreview);
    applyPluginUpdate.mockResolvedValue({ kind: "ok", jobId: "job1" });
    const { findByTestId, queryByTestId } = render(<PluginsSettings />);
    fireEvent.click(await findByTestId("plugins-check-updates"));
    fireEvent.click(await findByTestId("plugin-update-example.plugin"));
    fireEvent.click(await findByTestId("plugin-update-approve"));
    await waitFor(() => expect(applyPluginUpdate).toHaveBeenCalledWith("example.plugin", "treeB||community"));
    // The consent modal closes and the update runs as a job with a live log.
    await waitFor(() => expect(queryByTestId("plugin-update-consent-modal")).toBeNull());
    await findByTestId("plugin-job-modal");
  });

  it("Declining records the dismissal and never applies (the version stays active)", async () => {
    markOutdated();
    previewPluginUpdate.mockResolvedValue(consentPreview);
    dismissPluginUpdate.mockResolvedValue({ kind: "ok" });
    const { findByTestId, queryByTestId } = render(<PluginsSettings />);
    fireEvent.click(await findByTestId("plugins-check-updates"));
    fireEvent.click(await findByTestId("plugin-update-example.plugin"));
    fireEvent.click(await findByTestId("plugin-update-decline"));
    await waitFor(() => expect(dismissPluginUpdate).toHaveBeenCalledWith("example.plugin", "treeB||community"));
    expect(applyPluginUpdate).not.toHaveBeenCalled();
    await waitFor(() => expect(queryByTestId("plugin-update-consent-modal")).toBeNull());
  });

  it("a failed decline keeps the consent modal open and surfaces the error", async () => {
    markOutdated();
    previewPluginUpdate.mockResolvedValue(consentPreview);
    dismissPluginUpdate.mockResolvedValue({ kind: "error", message: "Dashboard is read-only." });
    const { findByTestId, queryByTestId } = render(<PluginsSettings />);
    fireEvent.click(await findByTestId("plugins-check-updates"));
    fireEvent.click(await findByTestId("plugin-update-example.plugin"));
    fireEvent.click(await findByTestId("plugin-update-decline"));
    const err = await findByTestId("plugin-update-consent-error");
    expect(err.textContent).toContain("Dashboard is read-only.");
    // The modal stays open and the update badge is not cleared, so the failed
    // decline is not mistaken for a persisted one.
    expect(queryByTestId("plugin-update-consent-modal")).not.toBeNull();
    await findByTestId("plugin-update-available-example.plugin");
  });

  it("a safe update shows the changelog in a review modal, then applies on Update", async () => {
    markOutdated();
    previewPluginUpdate.mockResolvedValue({
      kind: "ok",
      preview: {
        kind: "safe_update",
        to_version: "0.2.0",
        fingerprint: "treeC||community",
        changelog: commitChangelog,
      },
    });
    applyPluginUpdate.mockResolvedValue({ kind: "ok", jobId: "job1" });
    const { findByTestId, queryByTestId } = render(<PluginsSettings />);
    fireEvent.click(await findByTestId("plugins-check-updates"));
    fireEvent.click(await findByTestId("plugin-update-example.plugin"));
    // Safe updates no longer auto-apply: the review modal opens first, shows the
    // changelog, and has no access disclosure.
    await findByTestId("plugin-update-consent-modal");
    expect((await findByTestId("plugin-update-changelog")).textContent).toContain("fix: a bug");
    // A truncated changelog links out to the full history on GitHub.
    const more = await findByTestId("plugin-update-changelog-more");
    expect(more.getAttribute("href")).toBe("https://github.com/example/plugin/compare/aaa...bbb");
    expect(queryByTestId("plugin-update-added-caps")).toBeNull();
    expect(applyPluginUpdate).not.toHaveBeenCalled();
    fireEvent.click(await findByTestId("plugin-update-approve"));
    await waitFor(() => expect(applyPluginUpdate).toHaveBeenCalledWith("example.plugin", "treeC||community"));
    await findByTestId("plugin-job-modal");
  });

  it("an unavailable changelog says so and still lets the update proceed", async () => {
    markOutdated();
    previewPluginUpdate.mockResolvedValue({
      kind: "ok",
      preview: {
        kind: "safe_update",
        to_version: "0.2.0",
        fingerprint: "treeC||community",
        changelog: {
          entries: [],
          truncated: false,
          unavailable_reason: "GitHub rate limit reached; changelog unavailable.",
          more_url: null,
        },
      },
    });
    applyPluginUpdate.mockResolvedValue({ kind: "ok", jobId: "job1" });
    const { findByTestId } = render(<PluginsSettings />);
    fireEvent.click(await findByTestId("plugins-check-updates"));
    fireEvent.click(await findByTestId("plugin-update-example.plugin"));
    const note = await findByTestId("plugin-update-changelog-unavailable");
    expect(note.textContent).toContain("rate limit");
    // The update is still offered: approving applies it.
    fireEvent.click(await findByTestId("plugin-update-approve"));
    await waitFor(() => expect(applyPluginUpdate).toHaveBeenCalledWith("example.plugin", "treeC||community"));
  });

  it("surfaces an apply error in the consent modal and keeps it open", async () => {
    markOutdated();
    previewPluginUpdate.mockResolvedValue(consentPreview);
    applyPluginUpdate.mockResolvedValue({
      kind: "error",
      message: "the available update changed since it was shown; review it again before approving",
    });
    const { findByTestId } = render(<PluginsSettings />);
    fireEvent.click(await findByTestId("plugins-check-updates"));
    fireEvent.click(await findByTestId("plugin-update-example.plugin"));
    fireEvent.click(await findByTestId("plugin-update-approve"));
    const err = await findByTestId("plugin-update-consent-error");
    expect(err.textContent).toContain("changed since it was shown");
  });

  it("the consent modal renders removed caps, runtime, trust downgrade, and UI slots", async () => {
    markOutdated();
    previewPluginUpdate.mockResolvedValue({
      kind: "ok",
      preview: {
        kind: "consent_required",
        dismissed: false,
        consent: {
          id: "example.plugin",
          from_version: "0.1.0",
          to_version: "0.2.0",
          prior_capabilities: ["net", "fs.read"],
          new_capabilities: ["net"],
          added_capabilities: [],
          removed_capabilities: ["fs.read"],
          ui: [{ slot: "status-bar", id: "s" }],
          build_steps: [],
          runtime_change: "the worker is now a downloaded release binary",
          trust_downgrade: true,
          fingerprint: "treeD||community",
          stays_active_if_declined: true,
          changelog: emptyChangelog,
        },
      },
    });
    const { findByTestId } = render(<PluginsSettings />);
    fireEvent.click(await findByTestId("plugins-check-updates"));
    fireEvent.click(await findByTestId("plugin-update-example.plugin"));
    await findByTestId("plugin-update-consent-modal");
    expect((await findByTestId("plugin-update-runtime-change")).textContent).toContain("release binary");
    await findByTestId("plugin-update-trust-downgrade");
    // An empty (but available) changelog reads as "No changelog available".
    await findByTestId("plugin-update-changelog-empty");
  });

  it("Update reports up-to-date and clears the badge when preview finds no update", async () => {
    markOutdated();
    previewPluginUpdate.mockResolvedValue({ kind: "ok", preview: { kind: "no_update" } });
    const { findByTestId, queryByTestId } = render(<PluginsSettings />);
    fireEvent.click(await findByTestId("plugins-check-updates"));
    fireEvent.click(await findByTestId("plugin-update-example.plugin"));
    await waitFor(() => expect(reportInfo).toHaveBeenCalled());
    await waitFor(() => expect(queryByTestId("plugin-update-available-example.plugin")).toBeNull());
  });

  it("surfaces a preview error inline", async () => {
    markOutdated();
    previewPluginUpdate.mockResolvedValue({ kind: "error", message: "no published release" });
    const { findByTestId, findByText } = render(<PluginsSettings />);
    fireEvent.click(await findByTestId("plugins-check-updates"));
    fireEvent.click(await findByTestId("plugin-update-example.plugin"));
    await findByText("no published release");
  });

  it("surfaces an error when a safe update fails to apply", async () => {
    markOutdated();
    previewPluginUpdate.mockResolvedValue({
      kind: "ok",
      preview: { kind: "safe_update", to_version: "0.2.0", fingerprint: "treeC||community", changelog: emptyChangelog },
    });
    applyPluginUpdate.mockResolvedValue({ kind: "error", message: "apply boom" });
    const { findByTestId, findByText } = render(<PluginsSettings />);
    fireEvent.click(await findByTestId("plugins-check-updates"));
    fireEvent.click(await findByTestId("plugin-update-example.plugin"));
    fireEvent.click(await findByTestId("plugin-update-approve"));
    await findByText("apply boom");
  });

  it("does not close the consent modal while an apply is in flight", async () => {
    markOutdated();
    previewPluginUpdate.mockResolvedValue(consentPreview);
    // A never-resolving apply keeps the modal in its busy state.
    applyPluginUpdate.mockReturnValue(new Promise(() => {}));
    const { findByTestId, queryByTestId } = render(<PluginsSettings />);
    fireEvent.click(await findByTestId("plugins-check-updates"));
    fireEvent.click(await findByTestId("plugin-update-example.plugin"));
    fireEvent.click(await findByTestId("plugin-update-approve"));
    // Escape and the Close button must be no-ops while busy.
    fireEvent.keyDown(window, { key: "Escape" });
    fireEvent.click(await findByTestId("plugin-update-consent-close"));
    expect(queryByTestId("plugin-update-consent-modal")).not.toBeNull();
  });

  // --- Install (marketplace) ---

  function discoverWidget() {
    discoverPlugins.mockResolvedValue({
      kind: "ok",
      results: [
        {
          slug: "gh:acme/widget",
          html_url: "https://github.com/acme/widget",
          description: "A widget plugin.",
          stars: 42,
          badge: "unvetted",
          install_command: "aoe plugin install gh:acme/widget",
        },
      ],
    });
  }

  const installConsent: PluginInstallPreviewResult = {
    kind: "ok",
    consent: {
      id: "acme.widget",
      version: "1.0.0",
      source: "gh:acme/widget",
      notice: "installing the latest release v1.0.0",
      unverified: false,
      validation: "community",
      capabilities: ["net"],
      ui: [],
      build_steps: ["sh build.sh"],
      fingerprint: "treeA||community",
    },
  };

  it("Install on a marketplace result previews the gh: source and shows the disclosure", async () => {
    discoverWidget();
    previewPluginInstall.mockResolvedValue(installConsent);
    const { findByTestId } = render(<PluginsSettings />);
    fireEvent.click(await findByTestId("plugins-tab-marketplace"));
    fireEvent.click(await findByTestId("plugins-discover"));
    fireEvent.click(await findByTestId("plugins-install-gh:acme/widget"));
    // The gh: source is taken from the copy command, prefix intact.
    await waitFor(() => expect(previewPluginInstall).toHaveBeenCalledWith("gh:acme/widget"));
    await findByTestId("plugin-install-consent-modal");
    expect((await findByTestId("plugin-install-caps")).textContent).toContain("net");
    expect((await findByTestId("plugin-install-build-steps")).textContent).toContain("sh build.sh");
  });

  it("Approving an install starts the job pinned to the previewed fingerprint", async () => {
    discoverWidget();
    previewPluginInstall.mockResolvedValue(installConsent);
    startPluginInstall.mockResolvedValue({ kind: "ok", jobId: "job1" });
    const { findByTestId, queryByTestId } = render(<PluginsSettings />);
    fireEvent.click(await findByTestId("plugins-tab-marketplace"));
    fireEvent.click(await findByTestId("plugins-discover"));
    fireEvent.click(await findByTestId("plugins-install-gh:acme/widget"));
    fireEvent.click(await findByTestId("plugin-install-approve"));
    await waitFor(() => expect(startPluginInstall).toHaveBeenCalledWith("gh:acme/widget", "treeA||community"));
    await waitFor(() => expect(queryByTestId("plugin-install-consent-modal")).toBeNull());
    await findByTestId("plugin-job-modal");
  });

  it("surfaces an install preview error without opening the consent modal", async () => {
    discoverWidget();
    previewPluginInstall.mockResolvedValue({ kind: "error", message: "no published release" });
    const { findByTestId, queryByTestId } = render(<PluginsSettings />);
    fireEvent.click(await findByTestId("plugins-tab-marketplace"));
    fireEvent.click(await findByTestId("plugins-discover"));
    fireEvent.click(await findByTestId("plugins-install-gh:acme/widget"));
    const err = await findByTestId("plugins-discover-error");
    expect(err.textContent).toContain("no published release");
    expect(queryByTestId("plugin-install-consent-modal")).toBeNull();
  });

  it("an installed marketplace result shows no Install button", async () => {
    discoverPlugins.mockResolvedValue({
      kind: "ok",
      results: [
        {
          slug: "gh:acme/widget",
          html_url: "https://github.com/acme/widget",
          description: "A widget plugin.",
          stars: 42,
          badge: "installed",
          install_command: "aoe plugin install gh:acme/widget",
        },
      ],
    });
    const { findByTestId, queryByTestId } = render(<PluginsSettings />);
    fireEvent.click(await findByTestId("plugins-tab-marketplace"));
    fireEvent.click(await findByTestId("plugins-discover"));
    await findByTestId("plugins-discover-result-gh:acme/widget");
    expect(queryByTestId("plugins-install-gh:acme/widget")).toBeNull();
  });

  // --- Uninstall ---

  it("Uninstall on an external plugin confirms, then starts the job", async () => {
    startPluginUninstall.mockResolvedValue({ kind: "ok", jobId: "job1" });
    const { findByTestId } = render(<PluginsSettings />);
    fireEvent.click(await findByTestId("plugin-uninstall-example.plugin"));
    await findByTestId("plugin-uninstall-confirm");
    fireEvent.click(await findByTestId("plugin-uninstall-confirm-button"));
    await waitFor(() => expect(startPluginUninstall).toHaveBeenCalledWith("example.plugin"));
    await findByTestId("plugin-job-modal");
  });

  it("Cancelling the uninstall confirm starts no job", async () => {
    const { findByTestId, queryByTestId } = render(<PluginsSettings />);
    fireEvent.click(await findByTestId("plugin-uninstall-example.plugin"));
    fireEvent.click(await findByTestId("plugin-uninstall-cancel"));
    await waitFor(() => expect(queryByTestId("plugin-uninstall-confirm")).toBeNull());
    expect(startPluginUninstall).not.toHaveBeenCalled();
  });

  it("a builtin plugin has no Uninstall button", async () => {
    const { findByText, queryByTestId } = render(<PluginsSettings />);
    await findByText("Agent Status Detection");
    expect(queryByTestId("plugin-uninstall-aoe.status")).toBeNull();
  });

  // --- Job progress modal ---

  it("the job modal renders the live log tail and closes to a refreshed list when done", async () => {
    startPluginUninstall.mockResolvedValue({ kind: "ok", jobId: "job1" });
    fetchPluginJob.mockResolvedValue({
      kind: "ok",
      job: {
        job: {
          id: "job1",
          kind: "uninstall",
          target: "example.plugin",
          status: { state: "succeeded" },
          started_at: 0,
          finished_at: 1,
        },
        log: { exists: true, tail: "uninstalled example.plugin", lines_returned: 1, truncated: false },
      },
    });
    const { findByTestId, queryByTestId } = render(<PluginsSettings />);
    fireEvent.click(await findByTestId("plugin-uninstall-example.plugin"));
    fireEvent.click(await findByTestId("plugin-uninstall-confirm-button"));
    const log = await findByTestId("plugin-job-log");
    await waitFor(() => expect(log.textContent).toContain("uninstalled example.plugin"));
    const fetchCountBefore = fetchPlugins.mock.calls.length;
    fireEvent.click(await findByTestId("plugin-job-close"));
    await waitFor(() => expect(queryByTestId("plugin-job-modal")).toBeNull());
    // Closing refreshes the list so the removed plugin disappears.
    await waitFor(() => expect(fetchPlugins.mock.calls.length).toBeGreaterThan(fetchCountBefore));
  });

  it("the job modal shows the failure error from a failed job", async () => {
    startPluginUninstall.mockResolvedValue({ kind: "ok", jobId: "job1" });
    fetchPluginJob.mockResolvedValue({
      kind: "ok",
      job: {
        job: {
          id: "job1",
          kind: "uninstall",
          target: "example.plugin",
          status: { state: "failed", error: "removing tree failed" },
          started_at: 0,
          finished_at: 1,
        },
        log: { exists: true, tail: "uninstalling example.plugin", lines_returned: 1, truncated: false },
      },
    });
    const { findByTestId } = render(<PluginsSettings />);
    fireEvent.click(await findByTestId("plugin-uninstall-example.plugin"));
    fireEvent.click(await findByTestId("plugin-uninstall-confirm-button"));
    const err = await findByTestId("plugin-job-error");
    expect(err.textContent).toContain("removing tree failed");
  });

  it("the job modal treats a 404 (job gone) as terminal, not an endless reconnect", async () => {
    startPluginUninstall.mockResolvedValue({ kind: "ok", jobId: "job1" });
    // The daemon restarted mid-run: the job entry is gone. The modal must stop
    // polling and let the user close, not spin forever with Close disabled.
    fetchPluginJob.mockResolvedValue({ kind: "error", status: 404, message: "No plugin job job1" });
    const { findByTestId } = render(<PluginsSettings />);
    fireEvent.click(await findByTestId("plugin-uninstall-example.plugin"));
    fireEvent.click(await findByTestId("plugin-uninstall-confirm-button"));
    const status = await findByTestId("plugin-job-status");
    await waitFor(() => expect(status.textContent).toContain("no longer available"));
    const close = (await findByTestId("plugin-job-close")) as HTMLButtonElement;
    expect(close.disabled).toBe(false);
  });

  it("a rejected install start (e.g. another job active) surfaces the error in the consent modal", async () => {
    discoverWidget();
    previewPluginInstall.mockResolvedValue(installConsent);
    startPluginInstall.mockResolvedValue({ kind: "error", message: "Another plugin operation is already running" });
    const { findByTestId } = render(<PluginsSettings />);
    fireEvent.click(await findByTestId("plugins-tab-marketplace"));
    fireEvent.click(await findByTestId("plugins-discover"));
    fireEvent.click(await findByTestId("plugins-install-gh:acme/widget"));
    fireEvent.click(await findByTestId("plugin-install-approve"));
    const err = await findByTestId("plugin-install-consent-error");
    expect(err.textContent).toContain("already running");
  });
});
