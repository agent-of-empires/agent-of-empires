// @vitest-environment jsdom

import { describe, expect, it, vi } from "vitest";
import { fireEvent, render } from "@testing-library/react";

import { TelemetryConsentModal } from "../TelemetryConsentModal";

describe("TelemetryConsentModal", () => {
  it("reports opt-in from Enable telemetry and decline from Not now", () => {
    const onChoose = vi.fn();
    const { getByText } = render(<TelemetryConsentModal onChoose={onChoose} />);
    fireEvent.click(getByText("Enable telemetry"));
    fireEvent.click(getByText("Not now"));
    expect(onChoose.mock.calls).toEqual([[true], [false]]);
  });
});
