// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import { lazyView } from "../lazyView";

describe("lazyView", () => {
  it("shows the fallback while loading, then the component with its props", async () => {
    let resolve!: (c: (p: { name: string }) => React.ReactElement) => void;
    const View = lazyView(
      () => new Promise<(p: { name: string }) => React.ReactElement>((r) => (resolve = r)),
      <div>loading</div>,
    );
    render(<View name="aoe" />);
    expect(screen.getByText("loading")).toBeTruthy();
    resolve(({ name }) => <div>hello {name}</div>);
    expect(await screen.findByText("hello aoe")).toBeTruthy();
    expect(screen.queryByText("loading")).toBeNull();
  });

  it("keeps a failed chunk load local to the view", async () => {
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    const View = lazyView(() => Promise.reject(new Error("chunk failed")), <div>loading</div>);
    render(
      <>
        <div>sibling</div>
        <View />
      </>,
    );
    expect(await screen.findByText("Couldn't load this view.")).toBeTruthy();
    expect(screen.getByText("sibling")).toBeTruthy();
    errSpy.mockRestore();
  });
});
