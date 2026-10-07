// @vitest-environment jsdom
//
// ArtifactImage + openInNewTab fetch session files through the
// authed global fetch and hand back blob object URLs (see #2587). Pin the
// load path, the failure fallback, and the new-tab open.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, waitFor } from "@testing-library/react";

import { ArtifactImage } from "../artifactMedia";
import { openInNewTab } from "../../../lib/openInNewTab";

const URL_ANY = "/api/sessions/s1/artifacts/shot.png";

beforeEach(() => {
  vi.stubGlobal("URL", {
    createObjectURL: vi.fn(() => "blob:mock-url"),
    revokeObjectURL: vi.fn(),
  });
});

afterEach(() => {
  cleanup();
  vi.useRealTimers();
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

describe("openInNewTab", () => {
  it("opens the tab synchronously, then points it at the blob URL", async () => {
    let resolveFetch!: (response: Response) => void;
    const pendingFetch = new Promise<Response>((resolve) => {
      resolveFetch = resolve;
    });
    vi.stubGlobal("fetch", vi.fn().mockReturnValue(pendingFetch));
    const tab = { location: { href: "" }, close: vi.fn() };
    const open = vi.fn(() => tab);
    vi.stubGlobal("open", open);
    const opening = openInNewTab(URL_ANY);
    try {
      expect(open).toHaveBeenCalledWith("about:blank", "_blank");
      expect(fetch).toHaveBeenCalledWith(URL_ANY);
      expect(tab.location.href).toBe("");
    } finally {
      resolveFetch(new Response("x"));
      await opening;
    }
    expect(tab.location.href).toBe("blob:mock-url");
    expect(tab.close).not.toHaveBeenCalled();
  });

  it("closes the pre-opened tab when the fetch fails", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: false, status: 500 }));
    const tab = { location: { href: "" }, close: vi.fn() };
    const open = vi.fn(() => tab);
    vi.stubGlobal("open", open);
    await openInNewTab(URL_ANY);
    expect(tab.close).toHaveBeenCalled();
    expect(tab.location.href).toBe("");
  });

  it("falls back to a direct blob open when the sync tab is popup-blocked", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response("x")));
    // Popup blocked: the initial about:blank open returns null.
    const open = vi
      .fn()
      .mockReturnValueOnce(null)
      .mockReturnValue({ location: { href: "" }, close: vi.fn() });
    vi.stubGlobal("open", open);
    await openInNewTab(URL_ANY);
    expect(open).toHaveBeenNthCalledWith(1, "about:blank", "_blank");
    expect(open).toHaveBeenNthCalledWith(2, "blob:mock-url", "_blank", "noopener,noreferrer");
  });

  it.each([
    ["the given name", "report.html", "report.html"],
    ["the URL's file name", undefined, "status page.html"],
  ])("saves an attachment under %s and closes the pre-opened tab", async (_, name, expected) => {
    vi.useFakeTimers({ toFake: ["setTimeout"] });
    const response = new Response("<h1>hi</h1>", { headers: { "Content-Disposition": "attachment" } });
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(response));
    const tab = { location: { href: "" }, close: vi.fn() };
    const open = vi.fn(() => tab);
    vi.stubGlobal("open", open);
    const click = vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(() => {});
    expect(await openInNewTab("/api/sessions/s1/artifacts/sub/status%20page.html", name)).toEqual({ ok: true });
    expect(tab.close).toHaveBeenCalled();
    expect(tab.location.href).toBe("");
    const link = click.mock.contexts[0] as HTMLAnchorElement;
    expect(link.href).toBe("blob:mock-url");
    expect(link.download).toBe(expected);
    // Revoked later, so the browser can still read the blob for the download.
    expect(URL.revokeObjectURL).not.toHaveBeenCalled();
    vi.runAllTimers();
    expect(URL.revokeObjectURL).toHaveBeenCalledWith("blob:mock-url");
  });
});
