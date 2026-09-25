// @vitest-environment jsdom

import { fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { PluginLinkPicker } from "../PluginLinkPicker";

const links = [
  { href: "https://github.com/o/a/pull/1", label: "a: PR #1" },
  { href: "https://github.com/o/b/pull/2", label: "b: PR #2" },
];

function setup() {
  const open = vi.spyOn(window, "open").mockReturnValue(null);
  const onClose = vi.fn();
  render(<PluginLinkPicker links={links} onClose={onClose} />);
  return { open, onClose };
}

afterEach(() => {
  vi.restoreAllMocks();
});

describe("PluginLinkPicker", () => {
  it("opens the chosen link on a digit key or click, without an opener, and closes", () => {
    const { open, onClose } = setup();
    fireEvent.keyDown(document, { key: "2" });
    expect(open).toHaveBeenLastCalledWith(links[1]!.href, "_blank", "noopener,noreferrer");
    fireEvent.click(screen.getByText("a: PR #1"));
    expect(open).toHaveBeenLastCalledWith(links[0]!.href, "_blank", "noopener,noreferrer");
    expect(onClose).toHaveBeenCalledTimes(2);
  });

  it("ignores out-of-range and modified digits, then closes on Escape without opening", () => {
    const { open, onClose } = setup();
    for (const event of [
      { key: "5" },
      { key: "1", ctrlKey: true },
      { key: "1", metaKey: true },
      { key: "1", altKey: true },
    ]) {
      fireEvent.keyDown(document, event);
    }
    expect(onClose).not.toHaveBeenCalled();
    fireEvent.keyDown(document, { key: "Escape" });
    expect(open).not.toHaveBeenCalled();
    expect(onClose).toHaveBeenCalled();
  });
});
