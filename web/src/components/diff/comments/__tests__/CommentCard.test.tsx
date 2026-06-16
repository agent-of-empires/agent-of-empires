// @vitest-environment jsdom
//
// Coverage for CommentCard: saved view with range label, the stale chip,
// Edit -> CommentForm round-trip (onSave + back to view), and Delete.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { CommentCard } from "../CommentCard";
import type { AnchoredComment, DiffComment } from "../types";

function comment(over: Partial<DiffComment> = {}): DiffComment {
  return {
    id: "c1",
    filePath: "a.ts",
    side: "new",
    startLine: 3,
    endLine: 3,
    body: "needs a guard",
    capturedSnippet: "code",
    createdAt: "2025-01-01T00:00:00Z",
    ...over,
  };
}

function anchored(over: Partial<AnchoredComment> = {}, c: Partial<DiffComment> = {}): AnchoredComment {
  return { comment: comment(c), status: "active", contentChanged: false, ...over };
}

afterEach(cleanup);

describe("CommentCard", () => {
  it("renders the range, side, and body", () => {
    render(<CommentCard anchored={anchored()} onSave={() => {}} onDelete={() => {}} />);
    expect(screen.getByText(/line 3 \(new\)/)).toBeTruthy();
    expect(screen.getByText("needs a guard")).toBeTruthy();
  });

  it("shows a multi-line range and the stale chip", () => {
    render(
      <CommentCard
        anchored={anchored({ status: "stale" }, { startLine: 4, endLine: 9 })}
        onSave={() => {}}
        onDelete={() => {}}
      />,
    );
    expect(screen.getByText(/lines 4-9/)).toBeTruthy();
    expect(screen.getByText("stale")).toBeTruthy();
  });

  it("enters edit mode and saves through CommentForm", () => {
    const onSave = vi.fn();
    render(<CommentCard anchored={anchored()} onSave={onSave} onDelete={() => {}} />);
    fireEvent.click(screen.getByRole("button", { name: "Edit" }));

    const ta = screen.getByRole("textbox");
    fireEvent.change(ta, { target: { value: "rewritten" } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(onSave).toHaveBeenCalledWith("c1", "rewritten");
    // Back to the saved view.
    expect(screen.queryByRole("textbox")).toBeNull();
  });

  it("fires onDelete with the comment id", () => {
    const onDelete = vi.fn();
    render(<CommentCard anchored={anchored()} onSave={() => {}} onDelete={onDelete} />);
    fireEvent.click(screen.getByRole("button", { name: "Delete" }));
    expect(onDelete).toHaveBeenCalledWith("c1");
  });
});
