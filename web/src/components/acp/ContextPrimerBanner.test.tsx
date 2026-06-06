// @vitest-environment jsdom
//
// Context-primer banner. When the structured view detects a context-reset
// (`session/load` failure with prior turns in SQLite), this banner
// offers the user a recap fetched from
// `GET /api/sessions/:id/acp/context-primer`. The component owns
// the loading/error transients and the Insert vs Dismiss routing.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, waitFor } from "@testing-library/react";

vi.mock("../../lib/api", () => ({
  fetchContextPrimer: vi.fn(),
}));

import { ContextPrimerBanner } from "./ContextPrimerBanner";
import { fetchContextPrimer } from "../../lib/api";

const mockFetch = vi.mocked(fetchContextPrimer);

const AVAILABLE = { resetSeq: 7, reason: "session/load" };

function mount(
  props?: Partial<React.ComponentProps<typeof ContextPrimerBanner>>,
) {
  const onInsertPrimer = vi.fn();
  const onDismiss = vi.fn();
  const utils = render(
    <ContextPrimerBanner
      sessionId="s-1"
      available={AVAILABLE}
      onInsertPrimer={onInsertPrimer}
      onDismiss={onDismiss}
      {...props}
    />,
  );
  return { onInsertPrimer, onDismiss, ...utils };
}

beforeEach(() => {
  mockFetch.mockReset();
});

afterEach(() => {
  cleanup();
});

describe("ContextPrimerBanner", () => {
  it("renders nothing when `available` is null", () => {
    const { container } = mount({ available: null });
    expect(container.firstChild).toBeNull();
  });

  it("renders the banner copy and the Resume button when available is set", () => {
    const { getByText } = mount();
    expect(getByText(/Agent lost its prior model context/i)).toBeTruthy();
    expect(getByText(/Resume with prior context/i)).toBeTruthy();
  });

  it("renders the dismiss control with its aria-label", () => {
    const { getByLabelText } = mount();
    expect(getByLabelText("Dismiss context-reset banner")).toBeTruthy();
  });

  it("calls onDismiss when the × button is clicked", () => {
    const { getByLabelText, onDismiss } = mount();
    fireEvent.click(getByLabelText("Dismiss context-reset banner"));
    expect(onDismiss).toHaveBeenCalledTimes(1);
  });

  it("hits the primer endpoint with the session id and reset seq", async () => {
    mockFetch.mockResolvedValueOnce({
      primer: "user: hi\nagent: hello",
      included_event_count: 2,
      included_turn_count: 1,
      truncated: false,
      max_chars: 4000,
      unprocessed_prompt: null,
    });
    const { getByText } = mount();
    fireEvent.click(getByText(/Resume with prior context/i));
    await waitFor(() => expect(mockFetch).toHaveBeenCalledTimes(1));
    expect(mockFetch.mock.calls[0]?.[0]).toBe("s-1");
    expect(mockFetch.mock.calls[0]?.[1]).toBe(7);
  });

  it("inserts the fetched primer and triggers onDismiss on success", async () => {
    mockFetch.mockResolvedValueOnce({
      primer: "recap text",
      included_event_count: 4,
      included_turn_count: 2,
      truncated: false,
      max_chars: 4000,
      unprocessed_prompt: null,
    });
    const { getByText, onInsertPrimer, onDismiss } = mount();
    fireEvent.click(getByText(/Resume with prior context/i));
    await waitFor(() =>
      expect(onInsertPrimer).toHaveBeenCalledWith("recap text"),
    );
    expect(onDismiss).toHaveBeenCalledTimes(1);
  });

  it("surfaces an error when the primer endpoint returns null", async () => {
    mockFetch.mockResolvedValueOnce(null);
    const { getByText, findByRole, onInsertPrimer, onDismiss } = mount();
    fireEvent.click(getByText(/Resume with prior context/i));
    const alert = await findByRole("alert");
    expect(alert.textContent).toMatch(/Failed to fetch primer/i);
    expect(onInsertPrimer).not.toHaveBeenCalled();
    expect(onDismiss).not.toHaveBeenCalled();
  });

  it("surfaces a 'no transcript' message when primer is empty", async () => {
    mockFetch.mockResolvedValueOnce({
      primer: "",
      included_event_count: 0,
      included_turn_count: 0,
      truncated: false,
      max_chars: 4000,
      unprocessed_prompt: null,
    });
    const { getByText, findByRole, onInsertPrimer, onDismiss } = mount();
    fireEvent.click(getByText(/Resume with prior context/i));
    const alert = await findByRole("alert");
    expect(alert.textContent).toMatch(/No prior transcript/i);
    expect(onInsertPrimer).not.toHaveBeenCalled();
    expect(onDismiss).not.toHaveBeenCalled();
  });

  it("surfaces a network error on fetch rejection (not AbortError)", async () => {
    mockFetch.mockRejectedValueOnce(new Error("network down"));
    const { getByText, findByRole, onInsertPrimer } = mount();
    fireEvent.click(getByText(/Resume with prior context/i));
    const alert = await findByRole("alert");
    expect(alert.textContent).toMatch(/Network error/i);
    expect(onInsertPrimer).not.toHaveBeenCalled();
  });

  it("ignores an AbortError without surfacing an error message", async () => {
    const err = Object.assign(new Error("aborted"), { name: "AbortError" });
    mockFetch.mockRejectedValueOnce(err);
    const { getByText, queryByRole, onInsertPrimer } = mount();
    fireEvent.click(getByText(/Resume with prior context/i));
    await waitFor(() => expect(mockFetch).toHaveBeenCalled());
    // No alert role surfaces from an AbortError.
    await Promise.resolve();
    expect(queryByRole("alert")?.textContent ?? "").not.toMatch(
      /Network error/i,
    );
    expect(onInsertPrimer).not.toHaveBeenCalled();
  });

  it("clears prior error state when resetSeq changes", async () => {
    mockFetch.mockResolvedValueOnce(null);
    const { getByText, findByRole, rerender, queryByRole } = render(
      <ContextPrimerBanner
        sessionId="s-1"
        available={AVAILABLE}
        onInsertPrimer={vi.fn()}
        onDismiss={vi.fn()}
      />,
    );
    fireEvent.click(getByText(/Resume with prior context/i));
    await findByRole("alert");
    rerender(
      <ContextPrimerBanner
        sessionId="s-1"
        available={{ resetSeq: 8, reason: "new reset" }}
        onInsertPrimer={vi.fn()}
        onDismiss={vi.fn()}
      />,
    );
    expect(queryByRole("alert")).toBeNull();
  });
});
