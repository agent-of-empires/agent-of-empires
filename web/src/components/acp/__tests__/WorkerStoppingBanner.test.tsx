// @vitest-environment jsdom
//
// The stopping banner is the only signal that a session refuses prompts
// and resumes because its previous worker is not yet proven dead (#3487).

import { describe, expect, it } from "vitest";
import { render } from "@testing-library/react";

import { WorkerStoppingBanner } from "../StructuredView";

describe("WorkerStoppingBanner (#3487)", () => {
  it("tells the user the worker is stopping and nothing resumes yet", () => {
    const { container } = render(<WorkerStoppingBanner />);
    expect(container.textContent).toContain("Stopping structured view worker");
    expect(container.textContent).toContain("before anything can resume");
  });
});
