// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { TrashReadFailed } from "../TrashReadFailed";

describe("TrashReadFailed", () => {
  afterEach(cleanup);

  it("offers Retry", () => {
    const onRetry = vi.fn();
    render(<TrashReadFailed onRetry={onRetry} />);
    fireEvent.click(screen.getByRole("button", { name: "Retry" }));
    expect(onRetry).toHaveBeenCalledTimes(1);
  });
});
