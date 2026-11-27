// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { BackgroundShell } from "../../lib/acpTypes";
import { ShellRow } from "./BackgroundShells";

const shell = (over: Partial<BackgroundShell>): BackgroundShell => ({
  taskId: "g-1",
  command: "",
  startedAt: null,
  endedAt: null,
  finished: false,
  ...over,
});

describe("ShellRow", () => {
  afterEach(cleanup);

  it("labels a task with no command by its id and opens it", () => {
    const onOpen = vi.fn();
    render(<ShellRow shell={shell({})} onOpen={onOpen} />);
    fireEvent.click(screen.getByText("g-1"));
    expect(onOpen).toHaveBeenCalledWith("g-1");
    expect(screen.getByText("running")).toBeTruthy();
  });

  it("prefers the command as the label", () => {
    render(<ShellRow shell={shell({ command: "sleep 5", finished: true })} onOpen={vi.fn()} />);
    expect(screen.getByText("sleep 5")).toBeTruthy();
    expect(screen.getByText("done")).toBeTruthy();
  });
});
