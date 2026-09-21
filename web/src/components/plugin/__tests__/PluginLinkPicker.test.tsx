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
  it.each([
    ["digit key", () => fireEvent.keyDown(document, { key: "2" }), links[1]!.href],
    ["click", () => fireEvent.click(screen.getByText("a: PR #1")), links[0]!.href],
  ])("opens the chosen link on %s and closes", (_, trigger, href) => {
    const { open, onClose } = setup();
    trigger();
    expect(open).toHaveBeenCalledWith(href, "_blank", "noopener,noreferrer");
    expect(onClose).toHaveBeenCalled();
  });

  it("closes on Escape without opening", () => {
    const { open, onClose } = setup();
    fireEvent.keyDown(document, { key: "Escape" });
    expect(open).not.toHaveBeenCalled();
    expect(onClose).toHaveBeenCalled();
  });

  it.each([{ key: "5" }, { key: "1", ctrlKey: true }, { key: "1", metaKey: true }, { key: "1", altKey: true }])(
    "ignores %j",
    (event) => {
      const { open, onClose } = setup();
      fireEvent.keyDown(document, event);
      expect(open).not.toHaveBeenCalled();
      expect(onClose).not.toHaveBeenCalled();
    },
  );
});
