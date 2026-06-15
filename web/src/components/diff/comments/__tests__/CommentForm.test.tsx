// @vitest-environment jsdom
//
// Coverage for the inline diff CommentForm: range label, empty-body Save
// gating, Cmd/Ctrl+Enter save, Esc cancel, and the Cancel/Save buttons.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { CommentForm } from "../CommentForm";

afterEach(cleanup);

describe("CommentForm", () => {
  it("labels a single line vs a range", () => {
    const { rerender } = render(
      <CommentForm startLine={5} endLine={5} side="new" onSave={() => {}} onCancel={() => {}} />,
    );
    expect(screen.getByText(/Commenting on line 5 \(new\)/)).toBeTruthy();

    rerender(<CommentForm startLine={5} endLine={8} side="old" onSave={() => {}} onCancel={() => {}} />);
    expect(screen.getByText(/Commenting on lines 5-8 \(old\)/)).toBeTruthy();
  });

  it("disables Save until the body is non-empty", () => {
    render(<CommentForm startLine={1} endLine={1} side="new" onSave={() => {}} onCancel={() => {}} />);
    const save = screen.getByRole("button", { name: "Save" }) as HTMLButtonElement;
    expect(save.disabled).toBe(true);

    fireEvent.change(screen.getByRole("textbox"), { target: { value: "  looks good  " } });
    expect(save.disabled).toBe(false);

    const onSave = vi.fn();
    cleanup();
    render(<CommentForm startLine={1} endLine={1} side="new" onSave={onSave} onCancel={() => {}} />);
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "  trimmed  " } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(onSave).toHaveBeenCalledWith("trimmed");
  });

  it("saves on Cmd/Ctrl+Enter and cancels on Esc", () => {
    const onSave = vi.fn();
    const onCancel = vi.fn();
    render(<CommentForm startLine={1} endLine={1} side="new" onSave={onSave} onCancel={onCancel} />);
    const ta = screen.getByRole("textbox");

    fireEvent.change(ta, { target: { value: "ship it" } });
    fireEvent.keyDown(ta, { key: "Enter", metaKey: true });
    expect(onSave).toHaveBeenCalledWith("ship it");

    fireEvent.keyDown(ta, { key: "Escape" });
    expect(onCancel).toHaveBeenCalledOnce();
  });

  it("does not save an empty body on Cmd+Enter", () => {
    const onSave = vi.fn();
    render(<CommentForm startLine={1} endLine={1} side="new" onSave={onSave} onCancel={() => {}} />);
    fireEvent.keyDown(screen.getByRole("textbox"), { key: "Enter", ctrlKey: true });
    expect(onSave).not.toHaveBeenCalled();
  });
});
