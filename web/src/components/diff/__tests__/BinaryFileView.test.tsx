// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { BinaryFileView } from "../BinaryFileView";
import * as openInNewTab from "../../../lib/openInNewTab";

const RAW = "/api/sessions/s1/file/raw?path=";

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

describe("BinaryFileView", () => {
  it.each(["a.png", "dir/a.JPG", "a.jpeg", "a.gif", "a.webp", "a.avif", "a.bmp", "a.ico"])(
    "renders %s inline through the authenticated fetch",
    async (path) => {
      vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: true, blob: async () => new Blob(["x"]) }));
      render(<BinaryFileView sessionId="s1" filePath={path} />);
      const img = await screen.findByRole("img");
      expect(img.getAttribute("src")).toBe("blob:mock-url");
      expect(fetch).toHaveBeenCalledWith(`${RAW}${new URLSearchParams({ path }).toString().slice(5)}`);
      expect(screen.queryByRole("button", { name: "Download" })).toBeNull();
    },
  );

  it("offers a download instead of fetching for other binary types", () => {
    vi.stubGlobal("fetch", vi.fn());
    const open = vi.spyOn(openInNewTab, "openInNewTab").mockResolvedValue({ ok: true });
    render(<BinaryFileView sessionId="s1" filePath="/tmp/archive.zip" />);
    expect(screen.queryByRole("img")).toBeNull();
    expect(fetch).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Download" }));
    expect(open).toHaveBeenCalledWith("/api/sessions/s1/file/raw?path=%2Ftmp%2Farchive.zip", "archive.zip", true);
  });

  it("treats svg as a download, since the server never serves it inline", () => {
    vi.stubGlobal("fetch", vi.fn());
    render(<BinaryFileView sessionId="s1" filePath="logo.svg" />);
    expect(screen.queryByRole("img")).toBeNull();
    expect(screen.getByRole("button", { name: "Download" })).toBeTruthy();
  });

  it("falls back to the download button when the image fails to load", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: false, status: 404 }));
    render(<BinaryFileView sessionId="s1" filePath="gone.png" />);
    await waitFor(() => {
      expect(screen.getByRole("button", { name: "Download" })).toBeTruthy();
    });
    expect(screen.queryByRole("img")).toBeNull();
  });

  it("falls back to the download button when the bytes fetch but do not decode", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: true, blob: async () => new Blob(["not an image"]) }));
    render(<BinaryFileView sessionId="s1" filePath="broken.png" />);
    fireEvent.error(await screen.findByRole("img"));
    expect(screen.getByRole("button", { name: "Download" })).toBeTruthy();
    expect(screen.queryByRole("img")).toBeNull();
  });

  it("shows when a download failed", async () => {
    vi.spyOn(openInNewTab, "openInNewTab").mockResolvedValue({ ok: false, status: 404 });
    render(<BinaryFileView sessionId="s1" filePath="a.zip" />);
    fireEvent.click(screen.getByRole("button", { name: "Download" }));
    await screen.findByText("Download failed");
  });
});
