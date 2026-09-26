// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render } from "@testing-library/react";
import { FullFileViewer } from "../FullFileViewer";

vi.mock("../../../hooks/useShikiTheme", () => ({
  useShikiTheme: () => ({ theme: "github-dark", appearance: "dark" }),
}));

vi.mock("../pierre/DiffWorkerPoolProvider", () => ({
  DiffWorkerPoolProvider: ({ children }: { children: React.ReactNode }) => <>{children}</>,
}));

// Stubbed: the renderer touches the DOM and spins up workers, neither of
// which runs under jsdom. The stand-in surfaces what it was handed; the real
// gutter is covered by the live Playwright suite.
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

  it("re-keys the view when the path or content changes, including equal-length edits", () => {
    const view = (content: string, filePath = "src/a.ts") => <FullFileViewer content={content} filePath={filePath} />;
    const { getByTestId, rerender } = render(view("a\nb"));
    const step = (content: string, filePath: string | undefined, remounts: boolean) => {
      const before = getByTestId("virtualizer");
      rerender(view(content, filePath));
      if (remounts) expect(getByTestId("virtualizer")).not.toBe(before);
      else expect(getByTestId("virtualizer")).toBe(before);
    };
    step("a\nb", undefined, false);
    step("a\nb\nc", undefined, true);
    step("ab\nc\n", undefined, true);
    step("second", "src/b.ts", true);
    expect(getByTestId("pierre-file").textContent).toBe("second");
  });
});
