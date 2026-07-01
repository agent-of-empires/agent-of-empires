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
  it("fetches the blob and opens it in a new tab", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: true, blob: async () => new Blob(["x"]) }));
    const open = vi.fn();
    vi.stubGlobal("open", open);
    await openArtifactInNewTab(URL_ANY);
    expect(fetch).toHaveBeenCalledWith(URL_ANY);
    expect(open).toHaveBeenCalledWith("blob:mock-url", "_blank", "noopener,noreferrer");
  });

  it("does not open a tab when the fetch fails", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: false, status: 500 }));
    const open = vi.fn();
    vi.stubGlobal("open", open);
    await openArtifactInNewTab(URL_ANY);
    expect(open).not.toHaveBeenCalled();
  });
});
