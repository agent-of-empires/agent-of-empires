// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { CommentForm } from "../CommentForm";

afterEach(cleanup);

describe("CommentForm", () => {
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

  it("saves a non-empty body on Cmd/Ctrl+Enter and cancels on Esc", () => {
    const onSave = vi.fn();
    const onCancel = vi.fn();
    render(<CommentForm startLine={1} endLine={1} side="new" onSave={onSave} onCancel={onCancel} />);
    const ta = screen.getByRole("textbox");
    fireEvent.keyDown(ta, { key: "Enter", ctrlKey: true });
    expect(onSave).not.toHaveBeenCalled();

    fireEvent.change(ta, { target: { value: "ship it" } });
    fireEvent.keyDown(ta, { key: "Enter", metaKey: true });
    expect(onSave).toHaveBeenCalledWith("ship it");

    fireEvent.keyDown(ta, { key: "Escape" });
    expect(onCancel).toHaveBeenCalledOnce();
  });
});
