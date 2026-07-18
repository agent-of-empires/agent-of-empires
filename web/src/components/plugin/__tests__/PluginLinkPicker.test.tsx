// @vitest-environment jsdom

import { fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { PluginLinkPicker } from "../PluginLinkPicker";

const links = [
  { href: "https://github.com/o/a/pull/1", label: "a: PR #1" },
  { href: "https://github.com/o/b/pull/2", label: "b: PR #2" },
];

afterEach(() => {
  vi.restoreAllMocks();
});

describe("PluginLinkPicker", () => {
  it("opens the numbered link on its digit key and closes", () => {
    const open = vi.spyOn(window, "open").mockReturnValue(null);
    const onClose = vi.fn();
    render(<PluginLinkPicker links={links} onClose={onClose} />);
    expect(screen.getByText("a: PR #1")).toBeTruthy();

    fireEvent.keyDown(document, { key: "2" });
    expect(open).toHaveBeenCalledWith("https://github.com/o/b/pull/2", "_blank", "noopener,noreferrer");
    expect(onClose).toHaveBeenCalled();
  });

  it("opens on click", () => {
    const open = vi.spyOn(window, "open").mockReturnValue(null);
    const onClose = vi.fn();
    render(<PluginLinkPicker links={links} onClose={onClose} />);
    fireEvent.click(screen.getByText("a: PR #1"));
    expect(open).toHaveBeenCalledWith("https://github.com/o/a/pull/1", "_blank", "noopener,noreferrer");
    expect(onClose).toHaveBeenCalled();
  });

  it("closes on Escape without opening", () => {
    const open = vi.spyOn(window, "open").mockReturnValue(null);
    const onClose = vi.fn();
    render(<PluginLinkPicker links={links} onClose={onClose} />);
    fireEvent.keyDown(document, { key: "Escape" });
    expect(open).not.toHaveBeenCalled();
    expect(onClose).toHaveBeenCalled();
  });

  it("ignores a digit past the last link", () => {
    const open = vi.spyOn(window, "open").mockReturnValue(null);
    const onClose = vi.fn();
    render(<PluginLinkPicker links={links} onClose={onClose} />);
    fireEvent.keyDown(document, { key: "5" });
    expect(open).not.toHaveBeenCalled();
    expect(onClose).not.toHaveBeenCalled();
  });

  it("ignores a modified digit so browser shortcuts are not hijacked", () => {
    const open = vi.spyOn(window, "open").mockReturnValue(null);
    const onClose = vi.fn();
    render(<PluginLinkPicker links={links} onClose={onClose} />);
    fireEvent.keyDown(document, { key: "1", ctrlKey: true });
    fireEvent.keyDown(document, { key: "1", metaKey: true });
    fireEvent.keyDown(document, { key: "1", altKey: true });
    expect(open).not.toHaveBeenCalled();
    expect(onClose).not.toHaveBeenCalled();
  });
});
