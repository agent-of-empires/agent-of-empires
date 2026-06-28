// @vitest-environment jsdom
//
// Coverage for TrashedWorkerStoppedBanner (#2489): the read-only banner shown
// in the structured view when a trashed session's worker is stopped. The
// variant selection is covered by workerStoppedBanner.ts tests; this pins the
// banner's own render (copy + testid keyed by session id).

import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";

import { TrashedWorkerStoppedBanner } from "../StructuredView";

afterEach(cleanup);

describe("TrashedWorkerStoppedBanner (#2489)", () => {
  it("renders the trash notice keyed by session id", () => {
    render(<TrashedWorkerStoppedBanner sessionId="sess-9" />);
    expect(screen.getByTestId("acp-trashed-banner-sess-9")).toBeTruthy();
    expect(screen.getByText("Session in trash")).toBeTruthy();
    expect(screen.getByText(/read-only/)).toBeTruthy();
  });
});
