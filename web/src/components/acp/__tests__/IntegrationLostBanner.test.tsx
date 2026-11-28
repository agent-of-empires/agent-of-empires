// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";

import type { IntegrationLostReason } from "../../../lib/acpTypes";
import { IntegrationLostBanner } from "../IntegrationLostBanner";

afterEach(cleanup);

describe("IntegrationLostBanner", () => {
  it.each<[IntegrationLostReason, string]>([
    [{ kind: "contract_violation", detail: "bad key" }, "The integration broke the protocol: bad key."],
    [{ kind: "hold_overflow" }, "The integration held back too many messages."],
    [{ kind: "lease_overflow" }, "The integration left too many requests open."],
    [{ kind: "proxy_exited" }, "The integration process exited."],
  ])("words %j", (reason, line) => {
    render(<IntegrationLostBanner reason={reason} />);
    expect(screen.getByRole("alert").textContent).toContain("Agent integration lost");
    expect(screen.getByTestId("integration-lost-line").textContent).toBe(`${line} Messages you send stay queued.`);
  });
});
