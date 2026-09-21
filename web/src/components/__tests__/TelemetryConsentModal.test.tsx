// @vitest-environment jsdom

import { describe, expect, it, vi } from "vitest";
import { fireEvent, render } from "@testing-library/react";

import { TelemetryConsentModal } from "../TelemetryConsentModal";

describe("TelemetryConsentModal", () => {
  it("Enable telemetry reports opt-in", () => {
    const onChoose = vi.fn();
    const { getByText } = render(<TelemetryConsentModal onChoose={onChoose} />);
    fireEvent.click(getByText("Enable telemetry"));
    expect(onChoose).toHaveBeenCalledWith(true);
  });

  it("Not now reports decline", () => {
    const onChoose = vi.fn();
    const { getByText } = render(<TelemetryConsentModal onChoose={onChoose} />);
    fireEvent.click(getByText("Not now"));
    expect(onChoose).toHaveBeenCalledWith(false);
  });
});
