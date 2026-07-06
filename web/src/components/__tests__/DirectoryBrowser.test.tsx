// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import type { BrowseResponse } from "../../lib/types";

const browseFilesystem = vi.fn();
const getHomePath = vi.fn();

vi.mock("../../lib/api", () => ({
  browseFilesystem: (...args: unknown[]) => browseFilesystem(...args),
  getHomePath: () => getHomePath(),
}));

import { DirectoryBrowser } from "../DirectoryBrowser";

function response(entries: BrowseResponse["entries"]): BrowseResponse & { ok: boolean } {
  return { entries, has_more: false, ok: true };
}

function dir(name: string, path = `/home/user/${name}`) {
  return { name, path, is_dir: true, is_git_repo: false };
}

afterEach(() => {
  cleanup();
  window.localStorage.clear();
  browseFilesystem.mockReset();
  getHomePath.mockReset();
});

describe("DirectoryBrowser", () => {
  it("falls back to home when the initial path cannot be loaded", async () => {
    getHomePath.mockResolvedValue("/home/user");
    browseFilesystem.mockImplementation(async (path: string) => {
      if (path === "/missing") return { entries: [], has_more: false, ok: false };
      return response([dir("project")]);
    });

    render(<DirectoryBrowser initialPath="/missing" onSelect={vi.fn()} />);

    await expect(screen.findByRole("option", { name: /project/i })).resolves.toBeTruthy();
    expect(browseFilesystem).toHaveBeenNthCalledWith(1, "/missing", 100, undefined);
    expect(browseFilesystem).toHaveBeenNthCalledWith(2, "/home/user", 100, undefined);
  });

  it("ignores stale browse responses after a newer navigation finishes", async () => {
    getHomePath.mockResolvedValue("/home/user");
    let resolveSlow: ((value: BrowseResponse & { ok: boolean }) => void) | undefined;
    browseFilesystem.mockImplementation((path: string) => {
      if (path === "/home/user/slow") {
        return new Promise<BrowseResponse & { ok: boolean }>((resolve) => {
          resolveSlow = resolve;
        });
      }
      return Promise.resolve(response([dir("slow", "/home/user/slow")]));
    });

    render(<DirectoryBrowser onSelect={vi.fn()} />);

    await screen.findByRole("option", { name: /slow/i });
    fireEvent.click(screen.getByRole("option", { name: /slow/i }));
    fireEvent.click(screen.getByRole("button", { name: "user" }));

    await waitFor(() => {
      expect(browseFilesystem).toHaveBeenCalledWith("/home/user", 100, undefined);
    });

    expect(resolveSlow).toBeDefined();
    resolveSlow!(response([dir("stale-child", "/home/user/slow/stale-child")]));

    await waitFor(() => {
      expect(screen.queryByRole("option", { name: /stale-child/i })).toBeNull();
      expect(screen.getByRole("option", { name: /slow/i })).toBeTruthy();
    });
  });

  it("selects the current folder via 'Use this folder', even when it is not a git repo", async () => {
    getHomePath.mockResolvedValue("/home/user");
    browseFilesystem.mockResolvedValue(response([dir("project")]));
    const onSelect = vi.fn();

    render(<DirectoryBrowser onSelect={onSelect} />);

    await screen.findByRole("option", { name: /project/i });
    fireEvent.click(screen.getByRole("button", { name: /use this folder/i }));

    expect(onSelect).toHaveBeenCalledWith("/home/user");
  });

  it("requests filtered results from the server so entries past the first page can be found", async () => {
    getHomePath.mockResolvedValue("/home/user");
    const firstPage = Array.from({ length: 100 }, (_, i) => dir(`project-${i + 1}`));
    browseFilesystem.mockImplementation(async (_path: string, _limit: number, filter?: string) => {
      if (filter === "z") return response([dir("z-project")]);
      return { entries: firstPage, has_more: true, ok: true };
    });

    render(<DirectoryBrowser onSelect={vi.fn()} />);

    await screen.findByRole("option", { name: "project-1" });
    expect(screen.queryByRole("option", { name: /z-project/i })).toBeNull();

    fireEvent.change(screen.getByPlaceholderText("Type to filter..."), {
      target: { value: "z" },
    });

    await waitFor(() => {
      expect(browseFilesystem).toHaveBeenCalledWith("/home/user", 100, "z");
    });
    await expect(screen.findByRole("option", { name: /z-project/i })).resolves.toBeTruthy();
  });
});
