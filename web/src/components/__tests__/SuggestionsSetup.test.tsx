// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SuggestionsSetupHint } from "../SuggestionsSetupHint";
import { SuggestionsSetupSheet } from "../SuggestionsSetupSheet";
import { safeGetItem } from "../../lib/safeStorage";
import { clearFeaturedSuggestionSources } from "../../lib/featuredSuggestionSources";
import type { SuggestionProvider, SuggestionsSnapshot } from "../../lib/types";

const api = vi.hoisted(() => ({
  fetchSuggestions: vi.fn(),
  discoverPlugins: vi.fn(),
  previewPluginInstall: vi.fn(),
  startPluginInstall: vi.fn(),
  runSuggestionChecks: vi.fn(),
  refreshSuggestions: vi.fn(),
}));
const hydration = vi.hoisted(() => ({ done: true, listeners: [] as Array<() => void> }));

vi.mock("../../lib/api", () => api);
vi.mock("../../lib/webUiHydration", () => ({
  isWebUiStateHydrated: () => hydration.done,
  onWebUiStateHydrated: (listener: () => void) => {
    hydration.listeners.push(listener);
    return () => {
      hydration.listeners = hydration.listeners.filter((l) => l !== listener);
    };
  },
}));
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
const linear = {
  ...github,
  slug: "agent-of-empires/plugin-linear",
  install_command: "aoe plugin install gh:agent-of-empires/plugin-linear",
};

const prefs = { homepage: true, sidebar: false, refresh_minutes: 30, provider_order: [] };
const snapshot = (
  providers: SuggestionProvider[],
  over: Partial<SuggestionsSnapshot["prefs"]> = {},
): SuggestionsSnapshot => ({
  providers,
  items: [],
  prefs: { ...prefs, ...over },
});
const check = (id: string, status: "ok" | "fail", extra = {}) => ({
  id,
  label: `Label ${id}`,
  status,
  detail: "",
  fix: "",
  ...extra,
});
const provider = (over: Partial<SuggestionProvider> = {}): SuggestionProvider => ({
  id: "github",
  name: "GitHub",
  status: "ok",
  sources: [],
  checks: [],
  ...over,
});

const renderHint = (props = {}) => {
  const onOpenMarketplace = vi.fn();
  const onFinished = vi.fn();
  const view = render(
    <SuggestionsSetupHint onOpenMarketplace={onOpenMarketplace} onFinished={onFinished} {...props} />,
  );
  return { onOpenMarketplace, onFinished, ...view };
};

beforeEach(() => {
  window.localStorage.clear();
  clearFeaturedSuggestionSources();
  hydration.done = true;
  hydration.listeners = [];
  api.fetchSuggestions.mockResolvedValue(snapshot([]));
  api.discoverPlugins.mockResolvedValue({ kind: "ok", results: [github] });
});
afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.clearAllMocks();
});

describe("first-run hint", () => {
  it("names the featured source, and opens the sheet", async () => {
    renderHint();
    const hint = await screen.findByTestId("suggestions-hint");
    expect(hint.textContent).toContain("from your GitHub issues and PRs?");
    fireEvent.click(screen.getByTestId("suggestions-hint-open"));
    expect(screen.getByTestId("suggestions-setup-sheet")).toBeTruthy();
    expect(screen.getByText("Install 1 plugin")).toBeTruthy();
    expect(screen.getByTestId(`suggestions-setup-option-${github.slug}`).textContent).toContain("GitHub");
  });

  it.each([
    ["a provider is installed", () => snapshot([provider()])],
    ["the homepage section is off", () => snapshot([], { homepage: false })],
  ])("stays away, without touching GitHub discovery, when %s", async (_name, snap) => {
    api.fetchSuggestions.mockResolvedValue(snap());
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

  it("waits for hydration before reading the dismissal, so another Device's dismissal wins", async () => {
    hydration.done = false;
    renderHint();
    expect(api.fetchSuggestions).not.toHaveBeenCalled();
    window.localStorage.setItem("aoe-suggestions-hint-dismissed", "1");
    act(() => hydration.listeners.forEach((l) => l()));
    await act(async () => {});
    expect(api.fetchSuggestions).not.toHaveBeenCalled();
    expect(api.discoverPlugins).not.toHaveBeenCalled();
    expect(screen.queryByTestId("suggestions-hint")).toBeNull();
  });

  it("shows after hydration when nothing was dismissed", async () => {
    hydration.done = false;
    renderHint();
    expect(screen.queryByTestId("suggestions-hint")).toBeNull();
    act(() => hydration.listeners.forEach((l) => l()));
    expect(await screen.findByTestId("suggestions-hint")).toBeTruthy();
  });

  it("looks sources up once per page load, shared across mounts", async () => {
    const first = renderHint();
    await screen.findByTestId("suggestions-hint");
    first.unmount();
    renderHint();
    await screen.findByTestId("suggestions-hint");
    expect(api.discoverPlugins).toHaveBeenCalledTimes(1);
  });

  it("uses the caller's provider knowledge instead of fetching the snapshot", async () => {
    renderHint({ noProvider: true, ignoreDismissed: true });
    await screen.findByTestId("suggestions-hint");
    expect(api.fetchSuggestions).not.toHaveBeenCalled();
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
  const openSheet = async (props = {}) => {
    const handlers = renderHint(props);
    fireEvent.click(await screen.findByTestId("suggestions-hint-open"));
    return handlers;
  };
  const approveInstall = async () => {
    fireEvent.click(screen.getByTestId("suggestions-setup-install"));
    fireEvent.click(await screen.findByText("approve-consent"));
  };
  const consent = (source: string) => ({ kind: "ok", consent: { source, id: "g", fingerprint: `f-${source}` } });

  beforeEach(() => {
    api.previewPluginInstall.mockImplementation(async (source: string) => consent(source));
    api.startPluginInstall.mockResolvedValue({ kind: "ok", jobId: "j1" });
    api.runSuggestionChecks.mockResolvedValue({ ok: true, data: undefined });
    api.refreshSuggestions.mockResolvedValue({ ok: true, data: undefined });
  });

  it("opens the Marketplace from the last row", async () => {
    const { onOpenMarketplace } = await openSheet();
    fireEvent.click(screen.getByTestId("suggestions-setup-marketplace"));
    expect(onOpenMarketplace).toHaveBeenCalled();
    expect(screen.queryByTestId("suggestions-setup-sheet")).toBeNull();
  });

  it("installs every checked plugin in turn", async () => {
    api.discoverPlugins.mockResolvedValue({ kind: "ok", results: [github, linear] });
    await openSheet();
    await approveInstall();
    expect(api.previewPluginInstall).toHaveBeenCalledWith("gh:agent-of-empires/plugin-github");
    fireEvent.click(await screen.findByText("close-job"));
    await waitFor(() => expect(api.previewPluginInstall).toHaveBeenCalledWith("gh:agent-of-empires/plugin-linear"));
    expect(screen.queryByTestId("suggestions-setup-checks")).toBeNull();
  });

  it("unlocks the sheet when an install preview fails", async () => {
    api.previewPluginInstall.mockResolvedValueOnce({ kind: "error", message: "rate limit" });
    await openSheet();
    fireEvent.click(screen.getByTestId("suggestions-setup-install"));
    await waitFor(() => expect(screen.getByTestId("suggestions-setup-close")).not.toHaveProperty("disabled", true));
    expect(screen.getByTestId("suggestions-setup-install")).toHaveProperty("disabled", false);
  });

  it("shows a failed check with its fix, stops polling, and re-runs on Check again", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const { onFinished } = await openSheet();
    await approveInstall();
    api.fetchSuggestions.mockResolvedValue(
      snapshot([
        provider({
          status: "pending",
          checks: [
            check("cli", "ok"),
            check("auth", "fail", { fix: "Run gh auth login where the Daemon runs." }),
            check("repos", "ok"),
          ],
        }),
      ]),
    );
    fireEvent.click(await screen.findByText("close-job"));

    const auth = await screen.findByTestId("suggestions-setup-check-auth");
    expect(auth.textContent).toContain("Run gh auth login where the Daemon runs.");
    expect(screen.getByTestId("suggestions-setup-check-repos").dataset.status).toBe("not-run");

    const calls = api.fetchSuggestions.mock.calls.length;
    await act(() => vi.advanceTimersByTimeAsync(6000));
    expect(api.fetchSuggestions).toHaveBeenCalledTimes(calls);

    fireEvent.click(screen.getByTestId("suggestions-setup-recheck"));
    await waitFor(() => expect(api.runSuggestionChecks).toHaveBeenCalled());
    await waitFor(() => expect(api.fetchSuggestions.mock.calls.length).toBeGreaterThan(calls));
    fireEvent.click(screen.getByTestId("suggestions-setup-later"));
    expect(onFinished).toHaveBeenCalled();
  });

  it("draws a provider's fetch error as a failed row, and does not call it done", async () => {
    await openSheet();
    await approveInstall();
    api.fetchSuggestions.mockResolvedValue(
      snapshot([provider({ status: "error", error: "rate limit exceeded", checks: [check("cli", "ok")] })]),
    );
    fireEvent.click(await screen.findByText("close-job"));

    const row = await screen.findByTestId("suggestions-setup-check-github-fetch");
    expect(row.textContent).toContain("rate limit exceeded");
    expect(screen.queryByTestId("suggestions-setup-show")).toBeNull();
    fireEvent.click(screen.getByTestId("suggestions-setup-recheck"));
    await waitFor(() => expect(api.refreshSuggestions).toHaveBeenCalled());
  });

  it("finishes on 'Suggestions are on' once the provider has fetched and every check passes", async () => {
    const { onFinished } = await openSheet();
    await approveInstall();
    api.fetchSuggestions.mockResolvedValue(snapshot([provider({ checks: [check("cli", "ok")] })]));
    fireEvent.click(await screen.findByText("close-job"));

    fireEvent.click(await screen.findByTestId("suggestions-setup-show"));
    expect(onFinished).toHaveBeenCalled();
  });

  it("treats closing after the install as finishing, so the hint stops offering it", async () => {
    const { onFinished } = await openSheet();
    await approveInstall();
    api.fetchSuggestions.mockResolvedValue(snapshot([provider({ status: "pending", checks: [check("cli", "ok")] })]));
    fireEvent.click(await screen.findByText("close-job"));
    await screen.findByTestId("suggestions-setup-checks");

    fireEvent.click(screen.getByTestId("suggestions-setup-close"));
    expect(onFinished).toHaveBeenCalled();
    expect(screen.queryByTestId("suggestions-hint")).toBeNull();
  });

  it("handles an empty featured list without an install action", () => {
    render(<SuggestionsSetupSheet sources={[]} onClose={vi.fn()} onFinished={vi.fn()} onOpenMarketplace={vi.fn()} />);
    expect(screen.getByTestId("suggestions-setup-empty")).toBeTruthy();
    expect((screen.getByTestId("suggestions-setup-install") as HTMLButtonElement).disabled).toBe(true);
  });
});
