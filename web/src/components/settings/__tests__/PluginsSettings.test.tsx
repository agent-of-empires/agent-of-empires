// @vitest-environment jsdom

import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";

import type {
  PluginInstallPreviewResult,
  PluginJobResult,
  PluginListResponse,
  PluginUpdateChangelog,
  PluginUpdatePreviewResult,
  PluginUpdateStatus,
  PluginView,
} from "../../../lib/api";

const api = vi.hoisted(() => ({
  fetchPlugins: vi.fn(),
  setPluginEnabled: vi.fn(),
  fetchPluginUpdates: vi.fn(),
  discoverPlugins: vi.fn(),
  fetchPluginDetails: vi.fn(),
  previewPluginUpdate: vi.fn(),
  applyPluginUpdate: vi.fn(),
  dismissPluginUpdate: vi.fn(),
  previewPluginInstall: vi.fn(),
  startPluginInstall: vi.fn(),
  startPluginUninstall: vi.fn(),
  fetchPluginJob: vi.fn(),
}));
const reportInfo = vi.hoisted(() => vi.fn());
vi.mock("../../../lib/api", () => api);
vi.mock("../../../lib/toastBus", () => ({ reportInfo }));

import { PluginsSettings } from "../PluginsSettings";

const changelog = (over: Partial<PluginUpdateChangelog> = {}): PluginUpdateChangelog => ({
  entries: [],
  truncated: false,
  unavailable_reason: null,
  more_url: null,
  ...over,
});

const STATUS: PluginView = {
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
} as PluginView;
const EXAMPLE: PluginView = {
  ...STATUS,
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
};

const list = (plugins: PluginView[] = [STATUS, EXAMPLE], load_errors: string[] = []): PluginListResponse => ({
  plugins,
  load_errors,
});

const jobResult = (state: "succeeded" | "failed", tail: string, error?: string): PluginJobResult =>
  ({
    kind: "ok",
    job: {
      job: {
        id: "job1",
        kind: "uninstall",
        target: "example.plugin",
        status: error ? { state, error } : { state },
        started_at: 0,
        finished_at: 1,
      },
      log: { exists: true, tail, lines_returned: 1, truncated: false },
    },
  }) as PluginJobResult;

const manifest = (over: Record<string, unknown> = {}) => ({
  id: "acme.widget",
  name: "Widget",
  version: "2.3.0",
  description: "A widget plugin.",
  api_version: 4,
  capabilities: ["net"],
  ui_contributions: [{ slot: "status-bar", id: "s" }],
  screenshots: [],
  ...over,
});

const details = (over: Record<string, unknown> = {}) =>
  api.fetchPluginDetails.mockResolvedValue({
    kind: "ok",
    detail: { source: "gh:acme/widget", manifest: manifest(), manifest_error: null, release_tags: [], ...over },
  });

const WIDGET = {
  slug: "gh:acme/widget",
  html_url: "https://github.com/acme/widget",
  description: "A widget plugin.",
  stars: 42,
  badge: "unvetted",
  install_command: "aoe plugin install gh:acme/widget",
};

beforeEach(() => {
  Object.values(api).forEach((fn) => fn.mockReset());
  reportInfo.mockReset();
  api.fetchPlugins.mockResolvedValue(list());
  api.fetchPluginUpdates.mockResolvedValue({ kind: "ok", updates: [] });
  api.discoverPlugins.mockResolvedValue({ kind: "ok", results: [WIDGET] });
  api.fetchPluginDetails.mockResolvedValue({
    kind: "ok",
    detail: { source: "gh:example/plugin", manifest: null, manifest_error: null, release_tags: [] },
  });
  api.applyPluginUpdate.mockResolvedValue({ kind: "ok", jobId: "job1" });
  api.startPluginInstall.mockResolvedValue({ kind: "ok", jobId: "job1" });
  api.startPluginUninstall.mockResolvedValue({ kind: "ok", jobId: "job1" });
  api.fetchPluginJob.mockResolvedValue(jobResult("succeeded", "done"));
});

const click = async (testId: string) => fireEvent.click(await screen.findByTestId(testId));

function renderWith(plugins?: PluginView[], loadErrors?: string[]) {
  if (plugins || loadErrors) api.fetchPlugins.mockResolvedValue(list(plugins, loadErrors));
  return render(<PluginsSettings />);
}

describe("installed plugins", () => {
  it("renders name, version, description, deduped UI slots, and builtin has no Uninstall", async () => {
    renderWith();
    for (const text of ["Agent Status Detection", "v1.1.0", "A community plugin.", "UI: status-bar, row-badge"]) {
      expect(await screen.findByText(text)).toBeTruthy();
    }
    expect(screen.queryByTestId("plugin-uninstall-aoe.status")).toBeNull();
  });

  it("shows a needs-approval state for an ungranted plugin", async () => {
    renderWith([{ ...EXAMPLE, capabilities: ["net", "fs.read"], granted: false, needs_reapproval: true }]);
    await screen.findByTestId("plugin-needs-approval-example.plugin");
    expect(screen.getByText(/net, fs\.read/)).toBeTruthy();
    expect(screen.getByText(/not granted/)).toBeTruthy();
  });

  it("renders load errors", async () => {
    renderWith([STATUS], ["plugins/bad: manifest is invalid"]);
    expect(await screen.findByText(/manifest is invalid/)).toBeTruthy();
  });

  it("shows an error when the list fails to load", async () => {
    api.fetchPlugins.mockResolvedValue(null);
    render(<PluginsSettings />);
    expect(await screen.findByText("Failed to load plugins.")).toBeTruthy();
  });

  it("disabling adopts the refreshed list; only the web plugin gets a restart notice", async () => {
    const web = { ...STATUS, id: "aoe.web", name: "Web Dashboard" };
    renderWith([STATUS, web]);
    for (const [plugin, rest] of [
      [STATUS, [web]],
      [web, [{ ...STATUS, enabled: false }]],
    ] as const) {
      api.setPluginEnabled.mockResolvedValue({ kind: "ok", data: list([...rest, { ...plugin, enabled: false }]) });
      const toggle = (await screen.findByLabelText(`Enable ${plugin.name}`)) as HTMLInputElement;
      expect(toggle.checked).toBe(true);
      fireEvent.click(toggle);
      await waitFor(() => expect(toggle.checked).toBe(false));
      expect(api.setPluginEnabled).toHaveBeenLastCalledWith(plugin.id, false);
      expect(reportInfo.mock.calls.length > 0).toBe(plugin === web);
    }
    expect(reportInfo).toHaveBeenCalledWith("Web dashboard stays up until aoe serve is restarted.");
  });

  it("surfaces a rejected toggle", async () => {
    api.setPluginEnabled.mockResolvedValue({ kind: "error", message: "Dashboard is read-only." });
    renderWith();
    fireEvent.click(await screen.findByLabelText("Enable Agent Status Detection"));
    expect(await screen.findByText("Dashboard is read-only.")).toBeTruthy();
  });
});

describe("detail modal", () => {
  it("opens from an installed row with fallback fields, header icon, and closes", async () => {
    renderWith([{ ...EXAMPLE, icon: "github", icon_asset_url: "/api/plugins/x/icon" }]);
    await click("plugin-open-example.plugin");
    const modal = await screen.findByTestId("plugin-detail-modal");
    expect(modal.textContent).toContain("v0.1.0");
    expect((await screen.findByTestId("plugin-detail-icon")).getAttribute("src")).toBe("/api/plugins/x/icon");
    await click("plugin-detail-close");
    await waitFor(() => expect(screen.queryByTestId("plugin-detail-modal")).toBeNull());
    await click("plugin-open-example.plugin");
    await screen.findByTestId("plugin-detail-modal");
    fireEvent.keyDown(window, { key: "Escape" });
    await waitFor(() => expect(screen.queryByTestId("plugin-detail-modal")).toBeNull());
  });

  it("shows a details fetch error instead of claiming no releases", async () => {
    api.fetchPluginDetails.mockResolvedValue({ kind: "error", message: "Rate limited by GitHub." });
    renderWith();
    await click("plugin-open-example.plugin");
    expect((await screen.findByTestId("plugin-detail-error")).textContent).toContain("Rate limited by GitHub.");
    expect(screen.getByTestId("plugin-detail-modal").textContent).not.toContain("No published releases.");
  });

  it("opens from a discovery result with version and release tags", async () => {
    details({ release_tags: ["v2.3.0", "v2.2.0"] });
    renderWith();
    await click("plugins-tab-marketplace");
    await click("plugins-discover");
    await click("plugins-discover-open-gh:acme/widget");
    await waitFor(() => expect(api.fetchPluginDetails).toHaveBeenCalledWith("gh:acme/widget"));
    const modal = await screen.findByTestId("plugin-detail-modal");
    expect(modal.textContent).toContain("v2.3.0");
    expect(modal.textContent).toContain("net");
    expect((await screen.findByTestId("plugin-detail-versions")).textContent).toContain("v2.2.0");
    expect(screen.queryByTestId("plugin-detail-screenshots")).toBeNull();
  });

  it("renders a screenshot gallery with a dismissible lightbox", async () => {
    const a = "https://raw.githubusercontent.com/acme/widget/HEAD/a.png";
    details({
      manifest: manifest({
        screenshots: [
          { src: a, alt: "Dashboard card", caption: "Live card." },
          { src: "https://raw.githubusercontent.com/acme/widget/HEAD/b.gif", alt: "Demo", caption: "" },
        ],
      }),
    });
    renderWith();
    await click("plugin-open-example.plugin");
    const gallery = await screen.findByTestId("plugin-detail-screenshots");
    const imgs = gallery.querySelectorAll("img");
    expect(imgs).toHaveLength(2);
    expect([imgs[0]!.getAttribute("src"), imgs[0]!.getAttribute("alt"), imgs[0]!.getAttribute("loading")]).toEqual([
      a,
      "Dashboard card",
      "lazy",
    ]);
    expect(gallery.textContent).toContain("Live card.");

    const body = within(document.body);
    fireEvent.click(imgs[0]!);
    const bigImg = (await body.findByTestId("plugin-detail-lightbox")).querySelector("img")!;
    expect(bigImg.getAttribute("src")).toBe(a);
    fireEvent.click(bigImg);
    expect(body.queryByTestId("plugin-detail-lightbox")).not.toBeNull();
    fireEvent.keyDown(window, { key: "Escape" });
    await waitFor(() => expect(body.queryByTestId("plugin-detail-lightbox")).toBeNull());
    expect(body.queryByTestId("plugin-detail-modal")).not.toBeNull();
    fireEvent.click(imgs[0]!);
    fireEvent.click(await body.findByTestId("plugin-detail-lightbox"));
    await waitFor(() => expect(body.queryByTestId("plugin-detail-lightbox")).toBeNull());
    fireEvent.error(imgs[0]!);
    expect(imgs[0]!.closest("figure")!.className).toContain("hidden");
  });
});

describe("updates", () => {
  const outdated = (status: Partial<PluginUpdateStatus>) =>
    api.fetchPluginUpdates.mockResolvedValue({
      kind: "ok",
      updates: [
        {
          id: "example.plugin",
          source: "gh:example/plugin",
          current: "abc1234",
          available: "def5678",
          needs_update: true,
          error: null,
          ...status,
        },
      ],
    });
  const consentPreview = (over: Record<string, unknown> = {}): PluginUpdatePreviewResult =>
    ({
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
          changelog: changelog({
            entries: [{ kind: "release", tag: "v0.2.0", body: "Added a thing.", published_at: null }],
          }),
          ...over,
        },
      },
    }) as PluginUpdatePreviewResult;
  const safePreview = (log = changelog()): PluginUpdatePreviewResult => ({
    kind: "ok",
    preview: { kind: "safe_update", to_version: "0.2.0", fingerprint: "treeC||community", changelog: log },
  });

  async function openReview(preview: PluginUpdatePreviewResult | { kind: "error"; message: string }) {
    outdated({});
    api.previewPluginUpdate.mockResolvedValue(preview);
    renderWith();
    await click("plugins-check-updates");
    await click("plugin-update-example.plugin");
  }

  it("Check for updates badges an outdated plugin with its versions", async () => {
    outdated({});
    renderWith();
    await click("plugins-check-updates");
    await screen.findByTestId("plugin-update-available-example.plugin");
    expect(screen.getByTestId("plugin-example.plugin").textContent).toContain("abc1234 → def5678");
  });

  it("Check for updates surfaces a per-plugin error", async () => {
    outdated({ current: "", available: null, needs_update: false, error: "git not found" });
    renderWith();
    await click("plugins-check-updates");
    expect(await screen.findByText(/Update check failed: git not found/)).toBeTruthy();
  });

  it("a consent-required update discloses the new access and changelog, then applies pinned to its fingerprint", async () => {
    await openReview(consentPreview());
    await waitFor(() => expect(api.previewPluginUpdate).toHaveBeenCalledWith("example.plugin"));
    expect((await screen.findByTestId("plugin-update-added-caps")).textContent).toContain("fs.read");
    expect(screen.getByTestId("plugin-update-build-steps").textContent).toContain("sh build.sh");
    expect(screen.getByTestId("plugin-update-changelog").textContent).toContain("v0.2.0");
    await click("plugin-update-approve");
    await waitFor(() => expect(api.applyPluginUpdate).toHaveBeenCalledWith("example.plugin", "treeB||community"));
    await waitFor(() => expect(screen.queryByTestId("plugin-update-consent-modal")).toBeNull());
    await screen.findByTestId("plugin-job-modal");
  });

  it("renders removed caps, runtime change, trust downgrade, and an empty changelog", async () => {
    await openReview(
      consentPreview({
        added_capabilities: [],
        removed_capabilities: ["fs.read"],
        ui: [{ slot: "status-bar", id: "s" }],
        build_steps: [],
        runtime_change: "the worker is now a downloaded release binary",
        trust_downgrade: true,
        changelog: changelog(),
      }),
    );
    expect((await screen.findByTestId("plugin-update-runtime-change")).textContent).toContain("release binary");
    await screen.findByTestId("plugin-update-trust-downgrade");
    await screen.findByTestId("plugin-update-changelog-empty");
  });

  it.each([
    [{ kind: "ok" }, false],
    [{ kind: "error", message: "Dashboard is read-only." }, true],
  ])("declining with %j never applies; a failure keeps the modal and badge", async (result, failed) => {
    api.dismissPluginUpdate.mockResolvedValue(result);
    await openReview(consentPreview());
    await click("plugin-update-decline");
    await waitFor(() => expect(api.dismissPluginUpdate).toHaveBeenCalledWith("example.plugin", "treeB||community"));
    expect(api.applyPluginUpdate).not.toHaveBeenCalled();
    if (failed) {
      expect((await screen.findByTestId("plugin-update-consent-error")).textContent).toContain("read-only");
      expect(screen.getByTestId("plugin-update-consent-modal")).toBeTruthy();
      await screen.findByTestId("plugin-update-available-example.plugin");
    } else {
      await waitFor(() => expect(screen.queryByTestId("plugin-update-consent-modal")).toBeNull());
    }
  });

  it("a safe update reviews a truncated changelog before applying", async () => {
    await openReview(
      safePreview(
        changelog({
          entries: [{ kind: "commit", sha: "abcdef1234", subject: "fix: a bug", url: null }],
          truncated: true,
          more_url: "https://github.com/example/plugin/compare/aaa...bbb",
        }),
      ),
    );
    expect((await screen.findByTestId("plugin-update-changelog")).textContent).toContain("fix: a bug");
    expect(screen.getByTestId("plugin-update-changelog-more").getAttribute("href")).toBe(
      "https://github.com/example/plugin/compare/aaa...bbb",
    );
    expect(screen.queryByTestId("plugin-update-added-caps")).toBeNull();
    expect(api.applyPluginUpdate).not.toHaveBeenCalled();
    await click("plugin-update-approve");
    await waitFor(() => expect(api.applyPluginUpdate).toHaveBeenCalledWith("example.plugin", "treeC||community"));
    await screen.findByTestId("plugin-job-modal");
  });

  it("an apply error stays in the open review modal", async () => {
    api.applyPluginUpdate.mockResolvedValue({ kind: "error", message: "changed since it was shown" });
    await openReview(consentPreview());
    await click("plugin-update-approve");
    expect((await screen.findByTestId("plugin-update-consent-error")).textContent).toContain(
      "changed since it was shown",
    );
  });

  it("no update reports up to date and clears the badge", async () => {
    await openReview({ kind: "ok", preview: { kind: "no_update" } });
    await waitFor(() => expect(reportInfo).toHaveBeenCalled());
    await waitFor(() => expect(screen.queryByTestId("plugin-update-available-example.plugin")).toBeNull());
  });

  it("surfaces a preview error inline", async () => {
    await openReview({ kind: "error", message: "no published release" });
    expect(await screen.findByText("no published release")).toBeTruthy();
  });

  it("does not close while an apply is in flight", async () => {
    api.applyPluginUpdate.mockReturnValue(new Promise(() => {}));
    await openReview(consentPreview());
    await click("plugin-update-approve");
    fireEvent.keyDown(window, { key: "Escape" });
    await click("plugin-update-consent-close");
    expect(screen.queryByTestId("plugin-update-consent-modal")).not.toBeNull();
  });
});

describe("marketplace", () => {
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

  async function search() {
    renderWith();
    await click("plugins-tab-marketplace");
    await click("plugins-discover");
  }

  it("Install previews the gh: source, discloses access, and starts the pinned job", async () => {
    api.previewPluginInstall.mockResolvedValue(installConsent);
    await search();
    await click("plugins-install-gh:acme/widget");
    await waitFor(() => expect(api.previewPluginInstall).toHaveBeenCalledWith("gh:acme/widget"));
    expect((await screen.findByTestId("plugin-install-caps")).textContent).toContain("net");
    expect(screen.getByTestId("plugin-install-build-steps").textContent).toContain("sh build.sh");
    await click("plugin-install-approve");
    await waitFor(() => expect(api.startPluginInstall).toHaveBeenCalledWith("gh:acme/widget", "treeA||community"));
    await waitFor(() => expect(screen.queryByTestId("plugin-install-consent-modal")).toBeNull());
    await screen.findByTestId("plugin-job-modal");
  });

  it("surfaces a preview error without opening the consent modal", async () => {
    api.previewPluginInstall.mockResolvedValue({ kind: "error", message: "no published release" });
    await search();
    await click("plugins-install-gh:acme/widget");
    expect((await screen.findByTestId("plugins-discover-error")).textContent).toContain("no published release");
    expect(screen.queryByTestId("plugin-install-consent-modal")).toBeNull();
  });

  it("surfaces a rejected install start in the consent modal", async () => {
    api.previewPluginInstall.mockResolvedValue(installConsent);
    api.startPluginInstall.mockResolvedValue({ kind: "error", message: "Another plugin operation is already running" });
    await search();
    await click("plugins-install-gh:acme/widget");
    await click("plugin-install-approve");
    expect((await screen.findByTestId("plugin-install-consent-error")).textContent).toContain("already running");
  });
});

describe("uninstall and job progress", () => {
  async function uninstall() {
    renderWith();
    await click("plugin-uninstall-example.plugin");
    await screen.findByTestId("plugin-uninstall-confirm");
  }

  it("cancelling starts no job; confirming follows the job log and closes to a refreshed list", async () => {
    api.fetchPluginJob.mockResolvedValue(jobResult("succeeded", "uninstalled example.plugin"));
    await uninstall();
    await click("plugin-uninstall-cancel");
    await waitFor(() => expect(screen.queryByTestId("plugin-uninstall-confirm")).toBeNull());
    expect(api.startPluginUninstall).not.toHaveBeenCalled();
    await click("plugin-uninstall-example.plugin");
    await click("plugin-uninstall-confirm-button");
    await waitFor(() => expect(api.startPluginUninstall).toHaveBeenCalledWith("example.plugin"));
    const log = await screen.findByTestId("plugin-job-log");
    await waitFor(() => expect(log.textContent).toContain("uninstalled example.plugin"));
    const before = api.fetchPlugins.mock.calls.length;
    await click("plugin-job-close");
    await waitFor(() => expect(screen.queryByTestId("plugin-job-modal")).toBeNull());
    await waitFor(() => expect(api.fetchPlugins.mock.calls.length).toBeGreaterThan(before));
  });

  it("shows a failed job's error", async () => {
    api.fetchPluginJob.mockResolvedValue(jobResult("failed", "uninstalling", "removing tree failed"));
    await uninstall();
    await click("plugin-uninstall-confirm-button");
    expect((await screen.findByTestId("plugin-job-error")).textContent).toContain("removing tree failed");
  });

  it("treats a vanished job (404) as terminal", async () => {
    api.fetchPluginJob.mockResolvedValue({ kind: "error", status: 404, message: "No plugin job job1" });
    await uninstall();
    await click("plugin-uninstall-confirm-button");
    const status = await screen.findByTestId("plugin-job-status");
    await waitFor(() => expect(status.textContent).toContain("no longer available"));
    expect(((await screen.findByTestId("plugin-job-close")) as HTMLButtonElement).disabled).toBe(false);
  });
});
