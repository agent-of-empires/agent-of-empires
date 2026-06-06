import { describe, expect, it } from "vitest";
import { ACP_CAPABLE_TOOLS, isAcpCapable } from "./acpCapableTools";

describe("ACP_CAPABLE_TOOLS", () => {
  it("includes cursor as a built-in structured-view-capable tool", () => {
    expect(ACP_CAPABLE_TOOLS.has("cursor")).toBe(true);
    expect(isAcpCapable("cursor", undefined)).toBe(true);
  });
});
