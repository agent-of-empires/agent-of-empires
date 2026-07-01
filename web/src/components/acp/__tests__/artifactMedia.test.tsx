// @vitest-environment jsdom
//
// ArtifactImage + openArtifactInNewTab fetch session artifacts through the
// authed global fetch and hand back blob object URLs (see #2587). Pin the
// load path, the failure fallback, and the new-tab open.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, waitFor } from "@testing-library/react";

import { ArtifactImage } from "../artifactMedia";
import { openArtifactInNewTab } from "../../../lib/artifacts";

const URL_ANY = "/api/sessions/s1/artifacts/shot.png";

beforeEach(() => {
  vi.stubGlobal("URL", {
    createObjectURL: vi.fn(() => "blob:mock-url"),
    revokeObjectURL: vi.fn(),
  });
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("ArtifactImage", () => {
  it("fetches the artifact and renders it as an <img> from the blob URL", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: true, blob: async () => new Blob(["x"]) }));
    const { container } = render(<ArtifactImage url={URL_ANY} alt="a shot" />);
    // Placeholder until the bytes load.
    expect(container.querySelector("span.acp-inert-path")).not.toBeNull();
    await waitFor(() => {
      const img = container.querySelector("img.acp-artifact-image");
      expect(img).not.toBeNull();
      expect(img?.getAttribute("src")).toBe("blob:mock-url");
    });
    expect(fetch).toHaveBeenCalledWith(URL_ANY);
  });

  it("keeps the alt text as inert placeholder when the fetch fails", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: false, status: 404 }));
    const { container } = render(<ArtifactImage url={URL_ANY} alt="a shot" />);
    // Give the rejected promise a tick; it must not become an <img>.
    await waitFor(() => {
      expect(container.querySelector("span.acp-inert-path")?.textContent).toBe("a shot");
    });
    expect(container.querySelector("img")).toBeNull();
  });
});

describe("openArtifactInNewTab", () => {
  it("opens the tab synchronously, then points it at the blob URL", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: true, blob: async () => new Blob(["x"]) }));
    const tab = { location: { href: "" }, close: vi.fn() };
    const open = vi.fn(() => tab);
    vi.stubGlobal("open", open);
    await openArtifactInNewTab(URL_ANY);
    // Tab opened synchronously (before the fetch) to keep the user gesture.
    expect(open).toHaveBeenCalledWith("about:blank", "_blank");
    expect(fetch).toHaveBeenCalledWith(URL_ANY);
    expect(tab.location.href).toBe("blob:mock-url");
    expect(tab.close).not.toHaveBeenCalled();
  });

  it("closes the pre-opened tab when the fetch fails", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: false, status: 500 }));
    const tab = { location: { href: "" }, close: vi.fn() };
    const open = vi.fn(() => tab);
    vi.stubGlobal("open", open);
    await openArtifactInNewTab(URL_ANY);
    expect(tab.close).toHaveBeenCalled();
    expect(tab.location.href).toBe("");
  });

  it("falls back to a direct blob open when the sync tab is popup-blocked", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: true, blob: async () => new Blob(["x"]) }));
    // Popup blocked: the initial about:blank open returns null.
    const open = vi
      .fn()
      .mockReturnValueOnce(null)
      .mockReturnValue({ location: { href: "" }, close: vi.fn() });
    vi.stubGlobal("open", open);
    await openArtifactInNewTab(URL_ANY);
    expect(open).toHaveBeenNthCalledWith(1, "about:blank", "_blank");
    expect(open).toHaveBeenNthCalledWith(2, "blob:mock-url", "_blank", "noopener,noreferrer");
  });
});
