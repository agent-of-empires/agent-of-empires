// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SuggestionMappingWizard } from "../SuggestionMappingWizard";
import type { SuggestionItem } from "../../lib/types";

const api = vi.hoisted(() => ({
  fetchSuggestionMappings: vi.fn(),
  fetchSuggestionProjects: vi.fn(),
  putSuggestionMapping: vi.fn(),
}));
vi.mock("../../lib/api", () => api);

const item = (mapping_key: string): SuggestionItem => ({
  id: mapping_key,
  provider_id: "gh",
  title: "",
  reason: "",
  source_url: "",
  source_ref: "",
  preview: "",
  criterion: "assigned",
  mapping_key,
  draft: { title: "", prompt: "" },
  dismissed: false,
});

const provider = { id: "gh", name: "GitHub", status: "ok" as const, enabled: true, sources: [] };

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

async function open(onClose = vi.fn(), onSaved = vi.fn()) {
  api.fetchSuggestionMappings.mockResolvedValue([{ provider_id: "gh", mapping_key: "acme/done", projects: ["/w/x"] }]);
  api.fetchSuggestionProjects.mockResolvedValue([
    { name: "web", path: "/w/web", origin: "acme/web" },
    { name: "api", path: "/w/api", origin: null },
    { name: "other", path: "/w/other", origin: null },
  ]);
  render(
    <SuggestionMappingWizard
      items={[item("acme/web"), item("acme/api"), item("acme/lost"), item("acme/done")]}
      providers={[provider]}
      onClose={onClose}
      onSaved={onSaved}
    />,
  );
  await screen.findByLabelText("Map acme/web");
  return { onClose, onSaved };
}

describe("SuggestionMappingWizard", () => {
  it("pre-checks origin matches only and saves the checked rows in one action", async () => {
    api.putSuggestionMapping.mockResolvedValue({ ok: true, data: {} });
    const { onClose, onSaved } = await open();

    expect(screen.queryByLabelText("Map acme/done")).toBeNull();
    expect((screen.getByLabelText("Map acme/web") as HTMLInputElement).checked).toBe(true);
    expect((screen.getByLabelText("Map acme/api") as HTMLInputElement).checked).toBe(false);
    const lost = screen.getByLabelText("Map acme/lost") as HTMLInputElement;
    expect(lost.disabled).toBe(true);

    fireEvent.click(screen.getByLabelText("Map acme/api"));
    fireEvent.change(screen.getByLabelText("Project for acme/lost"), { target: { value: "/w/other" } });
    expect(lost.checked).toBe(true);
    fireEvent.change(screen.getByLabelText("Project for acme/web"), { target: { value: "/w/api" } });

    fireEvent.click(screen.getByRole("button", { name: "Save 3 mappings" }));
    await waitFor(() => expect(onClose).toHaveBeenCalled());
    expect(api.putSuggestionMapping.mock.calls).toEqual([
      ["gh", "acme/api", ["/w/api"]],
      ["gh", "acme/lost", ["/w/other"]],
      ["gh", "acme/web", ["/w/api"]],
    ]);
    expect(onSaved).toHaveBeenCalledTimes(1);
  });

  it("keeps the wizard open and reports a failed save", async () => {
    api.putSuggestionMapping.mockResolvedValue({ ok: false, error: "nope" });
    const { onClose, onSaved } = await open();
    fireEvent.click(screen.getByRole("button", { name: "Save 1 mapping" }));
    expect((await screen.findByRole("alert")).textContent).toContain("acme/web: nope");
    expect(onClose).not.toHaveBeenCalled();
    expect(onSaved).not.toHaveBeenCalled();
  });
});
