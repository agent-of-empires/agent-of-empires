// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SuggestionsSetupHint } from "../SuggestionsSetupHint";
import { safeGetItem } from "../../lib/safeStorage";
import type { SuggestionsSnapshot } from "../../lib/types";

const api = vi.hoisted(() => ({
  fetchSuggestions: vi.fn(),
  discoverPlugins: vi.fn(),
  previewPluginInstall: vi.fn(),
  startPluginInstall: vi.fn(),
  runSuggestionChecks: vi.fn(),
}));

vi.mock("../../lib/api", () => api);
vi.mock("../settings/PluginInstallConsentModal", () => ({
  PluginInstallConsentModal: ({ onApprove }: { onApprove: () => void }) => (
    <button onClick={onApprove}>approve-consent</button>
  ),
}));
vi.mock("../settings/PluginJobProgressModal", () => ({
  PluginJobProgressModal: ({ onClose }: { onClose: () => void }) => <button onClick={onClose}>close-job</button>,
}));

const github = {
  slug: "agent-of-empires/plugin-github",
  html_url: "https://github.com/agent-of-empires/plugin-github",
  description: "Assigned issues and review requests.",
  stars: 1,
  badge: "featured" as const,
  suggestions: true,
  install_command: "aoe plugin install gh:agent-of-empires/plugin-github",
  source_avatar_url: "",
};

const empty: SuggestionsSnapshot = { providers: [], items: [] };
const withProvider = (checks: NonNullable<SuggestionsSnapshot["providers"][0]["checks"]>, status = "ok" as const) => ({
  items: [],
  providers: [{ id: "github", name: "GitHub", status, sources: [], checks }],
});
const check = (id: string, status: "ok" | "fail", extra = {}) => ({
  id,
  label: `Label ${id}`,
  status,
  detail: "",
  fix: "",
  ...extra,
});

const renderHint = (props = {}) => {
  const onOpenMarketplace = vi.fn();
  const onFinished = vi.fn();
  render(<SuggestionsSetupHint onOpenMarketplace={onOpenMarketplace} onFinished={onFinished} {...props} />);
  return { onOpenMarketplace, onFinished };
};

beforeEach(() => {
  window.localStorage.clear();
  api.fetchSuggestions.mockResolvedValue(empty);
  api.discoverPlugins.mockResolvedValue({ kind: "ok", results: [github] });
});
afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("first-run hint", () => {
  it("shows when a featured provider exists and none is installed, and opens the sheet", async () => {
    renderHint();
    fireEvent.click(await screen.findByTestId("suggestions-hint-open"));
    expect(screen.getByTestId("suggestions-setup-sheet")).toBeTruthy();
    expect(screen.getByText("Install 1 plugin")).toBeTruthy();
  });

  it("stays away when a provider is installed, without touching GitHub discovery", async () => {
    api.fetchSuggestions.mockResolvedValue(withProvider([]));
    renderHint();
    await waitFor(() => expect(api.fetchSuggestions).toHaveBeenCalled());
    expect(screen.queryByTestId("suggestions-hint")).toBeNull();
    expect(api.discoverPlugins).not.toHaveBeenCalled();
  });

  it("only offers featured providers that declare suggestions, else renders the fallback", async () => {
    api.discoverPlugins.mockResolvedValue({
      kind: "ok",
      results: [
        { ...github, suggestions: false },
        { ...github, slug: "x/y", badge: "unvetted" },
      ],
    });
    renderHint({ fallback: <span>no-sources</span> });
    expect(await screen.findByText("no-sources")).toBeTruthy();
    expect(screen.queryByTestId("suggestions-hint")).toBeNull();
  });

  it("persists the dismissal through the synced key", async () => {
    renderHint();
    fireEvent.click(await screen.findByTestId("suggestions-hint-dismiss"));
    expect(screen.queryByTestId("suggestions-hint")).toBeNull();
    expect(safeGetItem("aoe-suggestions-hint-dismissed")).toBe("1");
  });

  it("is not shown, nor does it hit discovery, once dismissed", async () => {
    window.localStorage.setItem("aoe-suggestions-hint-dismissed", "1");
    renderHint();
    await waitFor(() => expect(api.fetchSuggestions).toHaveBeenCalled());
    expect(screen.queryByTestId("suggestions-hint")).toBeNull();
    expect(api.discoverPlugins).not.toHaveBeenCalled();
  });

  it("shows a banner when discovery fails and recovers on retry", async () => {
    api.discoverPlugins.mockResolvedValueOnce({ kind: "error", message: "rate limited" });
    renderHint();
    const banner = await screen.findByTestId("suggestions-hint-error");
    expect(banner.textContent).toContain("rate limited");
    fireEvent.click(screen.getByText("Try again"));
    expect(await screen.findByTestId("suggestions-hint")).toBeTruthy();
  });
});

describe("setup sheet", () => {
  const openSheet = async () => {
    const handlers = renderHint();
    fireEvent.click(await screen.findByTestId("suggestions-hint-open"));
    return handlers;
  };

  it("opens the Marketplace from the last row", async () => {
    const { onOpenMarketplace } = await openSheet();
    fireEvent.click(screen.getByTestId("suggestions-setup-marketplace"));
    expect(onOpenMarketplace).toHaveBeenCalled();
    expect(screen.queryByTestId("suggestions-setup-sheet")).toBeNull();
  });

  it("installs the checked plugin, then shows a failed check with its fix and re-runs on Check again", async () => {
    api.previewPluginInstall.mockResolvedValue({
      kind: "ok",
      consent: { source: "gh:agent-of-empires/plugin-github", id: "g", fingerprint: "f" },
    });
    api.startPluginInstall.mockResolvedValue({ kind: "ok", jobId: "j1" });
    api.runSuggestionChecks.mockResolvedValue({ ok: true, data: undefined });
    const { onFinished } = await openSheet();

    fireEvent.click(screen.getByTestId("suggestions-setup-install"));
    expect(api.previewPluginInstall).toHaveBeenCalledWith("gh:agent-of-empires/plugin-github");
    fireEvent.click(await screen.findByText("approve-consent"));
    expect(api.startPluginInstall).toHaveBeenCalledWith("gh:agent-of-empires/plugin-github", "f");

    api.fetchSuggestions.mockResolvedValue(
      withProvider(
        [
          check("cli", "ok"),
          check("auth", "fail", { fix: "Run gh auth login where the Daemon runs." }),
          check("repos", "ok"),
        ],
        "pending" as never,
      ),
    );
    fireEvent.click(await screen.findByText("close-job"));

    const auth = await screen.findByTestId("suggestions-setup-check-auth");
    expect(auth.textContent).toContain("Run gh auth login where the Daemon runs.");
    expect(screen.getByTestId("suggestions-setup-check-repos").dataset.status).toBe("not-run");

    fireEvent.click(screen.getByTestId("suggestions-setup-recheck"));
    await waitFor(() => expect(api.runSuggestionChecks).toHaveBeenCalled());
    fireEvent.click(screen.getByTestId("suggestions-setup-later"));
    expect(onFinished).toHaveBeenCalled();
  });

  it("finishes on 'Suggestions are on' once the provider reports and every check passes", async () => {
    api.previewPluginInstall.mockResolvedValue({ kind: "ok", consent: { source: "s", id: "g", fingerprint: "f" } });
    api.startPluginInstall.mockResolvedValue({ kind: "ok", jobId: "j1" });
    const { onFinished } = await openSheet();
    fireEvent.click(screen.getByTestId("suggestions-setup-install"));
    fireEvent.click(await screen.findByText("approve-consent"));
    api.fetchSuggestions.mockResolvedValue(withProvider([check("cli", "ok")]));
    fireEvent.click(await screen.findByText("close-job"));

    fireEvent.click(await screen.findByTestId("suggestions-setup-show"));
    expect(onFinished).toHaveBeenCalled();
  });

  it("handles an empty featured list without an install action", async () => {
    // Rendered directly: the hint itself hides when nothing is featured.
    const { SuggestionsSetupSheet } = await import("../SuggestionsSetupSheet");
    render(<SuggestionsSetupSheet sources={[]} onClose={vi.fn()} onFinished={vi.fn()} onOpenMarketplace={vi.fn()} />);
    expect(screen.getByTestId("suggestions-setup-empty")).toBeTruthy();
    expect((screen.getByTestId("suggestions-setup-install") as HTMLButtonElement).disabled).toBe(true);
  });
});
