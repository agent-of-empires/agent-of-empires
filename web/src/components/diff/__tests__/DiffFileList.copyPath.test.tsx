// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { DiffFileList } from "../DiffFileList";
import type { RepoBase, RichDiffFile } from "../../../lib/types";

const files: RichDiffFile[] = [
  {
    path: "src/app/foo.rs",
    old_path: null,
    status: "modified",
    additions: 1,
    deletions: 0,
  },
];

const perRepoBases: RepoBase[] = [{ base_branch: "main" }];

function stubClipboard() {
  Object.defineProperty(window, "isSecureContext", {
    value: true,
    configurable: true,
  });
  const writeText = vi.fn().mockResolvedValue(undefined);
  Object.defineProperty(navigator, "clipboard", {
    value: { writeText },
    configurable: true,
  });
  return writeText;
}

afterEach(() => {
  vi.restoreAllMocks();
  window.localStorage.clear();
});

describe("DiffFileList copy relative path", () => {
  it("right-clicking a file row offers Copy relative path and copies it", () => {
    const writeText = stubClipboard();
    render(
      <DiffFileList
        files={files}
        perRepoBases={perRepoBases}
        warning={null}
        selectedPath={null}
        selectedRepoName={undefined}
        loading={false}
        onSelectFile={() => {}}
      />,
    );

    const row = screen.getByText("foo.rs").closest("button");
    expect(row).not.toBeNull();
    fireEvent.contextMenu(row as HTMLElement);

    fireEvent.click(screen.getByText("Copy relative path"));
    expect(writeText).toHaveBeenCalledWith("src/app/foo.rs");
  });

  it("does not open the menu when right-clicking off a file row", () => {
    render(
      <DiffFileList
        files={files}
        perRepoBases={perRepoBases}
        warning={null}
        selectedPath={null}
        selectedRepoName={undefined}
        loading={false}
        onSelectFile={() => {}}
      />,
    );

    // The "Changes" header has no data-path ancestor, so the delegated handler
    // bails and no menu opens (the native menu is left to the browser).
    fireEvent.contextMenu(screen.getByText("Changes"));
    expect(screen.queryByText("Copy relative path")).toBeNull();
  });

  it("copies from a flat-list row too", () => {
    const writeText = stubClipboard();
    window.localStorage.setItem(
      "aoe-web-settings",
      JSON.stringify({ diffViewMode: "flat" }),
    );
    render(
      <DiffFileList
        files={files}
        perRepoBases={perRepoBases}
        warning={null}
        selectedPath={null}
        selectedRepoName={undefined}
        loading={false}
        onSelectFile={() => {}}
      />,
    );

    const row = screen.getByText("foo.rs").closest("button");
    // Sanity-check we're actually in flat mode: FlatList renders the dir
    // prefix inline on the row, so the test stays honest if the view-mode
    // wiring changes.
    expect(row?.textContent).toContain("src/app/");
    fireEvent.contextMenu(row as HTMLElement);
    fireEvent.click(screen.getByText("Copy relative path"));
    expect(writeText).toHaveBeenCalledWith("src/app/foo.rs");
  });
});
