// @vitest-environment jsdom
//
// FullFileViewer contract (#1810, #4003): the whole-file view for a file with
// no diff against the base. It renders through the shared `@pierre/diffs` file
// renderer, so what is worth pinning here is the handoff, that the file name
// and text reach the renderer unchanged, that line numbers are left enabled,
// and that a file switch re-keys the view instead of reusing the previous
// file's instance.
//
// The renderer manipulates the DOM and spins up workers, neither of which runs
// under jsdom, so it is mocked with a stand-in that surfaces what it was
// handed (the same approach as the DiffFileViewer specs). Real rendering,
// including the gutter itself, is covered by the live Playwright suite.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render } from "@testing-library/react";
import { FullFileViewer } from "../FullFileViewer";

vi.mock("../../../hooks/useShikiTheme", () => ({
  useShikiTheme: () => ({ theme: "github-dark", appearance: "dark" }),
}));

vi.mock("../pierre/DiffWorkerPoolProvider", () => ({
  DiffWorkerPoolProvider: ({ children }: { children: React.ReactNode }) => <>{children}</>,
}));

vi.mock("@pierre/diffs/react", () => ({
  Virtualizer: ({ children }: { children: React.ReactNode }) => <div data-testid="virtualizer">{children}</div>,
  File: ({
    file,
    options,
  }: {
    file: { name: string; contents: string };
    options: { theme: string; disableLineNumbers?: boolean };
  }) => (
    <div
      data-testid="pierre-file"
      data-name={file.name}
      data-theme={options.theme}
      data-line-numbers={String(options.disableLineNumbers !== true)}
    >
      {file.contents}
    </div>
  ),
}));

afterEach(cleanup);

describe("FullFileViewer", () => {
  it("hands the file name and text to the renderer with line numbers enabled", () => {
    const { getByTestId } = render(<FullFileViewer content={"a\nb\n"} filePath="src/a.ts" />);
    const rendered = getByTestId("pierre-file");
    expect(rendered.dataset.name).toBe("src/a.ts");
    expect(rendered.textContent).toBe("a\nb\n");
    expect(rendered.dataset.lineNumbers).toBe("true");
    expect(rendered.dataset.theme).toBe("github-dark");
  });

  it("re-keys the view on a file switch so the previous file's instance is not reused", () => {
    const { getByTestId, rerender } = render(<FullFileViewer content="first" filePath="src/a.ts" />);
    const first = getByTestId("virtualizer");
    rerender(<FullFileViewer content="second" filePath="src/b.ts" />);
    const second = getByTestId("virtualizer");
    expect(second).not.toBe(first);
    expect(getByTestId("pierre-file").textContent).toBe("second");
  });

  it("re-keys when the same path's content changes, so rows are remeasured", () => {
    const { getByTestId, rerender } = render(<FullFileViewer content={"a\nb"} filePath="src/a.ts" />);
    const first = getByTestId("virtualizer");
    rerender(<FullFileViewer content={"a\nb\nc"} filePath="src/a.ts" />);
    expect(getByTestId("virtualizer")).not.toBe(first);
  });

  it("re-keys on an equal-length edit, which a length-only key would miss", () => {
    const { getByTestId, rerender } = render(<FullFileViewer content={"a\nb"} filePath="src/a.ts" />);
    const first = getByTestId("virtualizer");
    rerender(<FullFileViewer content={"ab\n"} filePath="src/a.ts" />);
    expect(getByTestId("virtualizer")).not.toBe(first);
  });

  it("keeps the view mounted when nothing changed", () => {
    const { getByTestId, rerender } = render(<FullFileViewer content={"a\nb"} filePath="src/a.ts" />);
    const first = getByTestId("virtualizer");
    rerender(<FullFileViewer content={"a\nb"} filePath="src/a.ts" />);
    expect(getByTestId("virtualizer")).toBe(first);
  });
});
