// @vitest-environment jsdom
//
// Contract test for the CityHall export panel. The panel is the only settings
// surface that hands an admin a file to carry to another system, so what matters
// is that nothing is offered before a bundle exists, that a failed export
// surfaces the server's reason rather than a blank pane, and that the download
// really carries the fetched bytes.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, waitFor } from "@testing-library/react";

const fetchCityHallBundle = vi.fn<[], Promise<string>>();

vi.mock("../../../lib/api", () => ({
  fetchCityHallBundle: () => fetchCityHallBundle(),
}));

// Imported after the mock is registered.
import { CityHallSettings } from "../CityHallSettings";

const BUNDLE = 'schema_version = 1\n\n[[projects]]\nname = "demo"\nremote = "https://example.com/demo.git"\n';

beforeEach(() => {
  fetchCityHallBundle.mockReset();
});

// In cleanup rather than at the end of each test body: a failing assertion
// before the restore would otherwise leak a replaced URL/navigator into
// unrelated tests.
afterEach(() => {
  vi.unstubAllGlobals();
});

describe("CityHallSettings contract", () => {
  /// Download and Copy would produce an empty file before a bundle is fetched.
  it("offers nothing to take away until a bundle has been generated", () => {
    const { queryByText, getByTestId } = render(<CityHallSettings />);
    expect(getByTestId("cityhall-export").textContent).toBe("Generate bundle");
    expect(queryByText(/Download/)).toBeNull();
    expect(queryByText("Copy")).toBeNull();
  });

  it("renders the fetched bundle and offers it for download", async () => {
    fetchCityHallBundle.mockResolvedValue(BUNDLE);
    const { getByTestId, findByText, container } = render(<CityHallSettings />);

    fireEvent.click(getByTestId("cityhall-export"));
    await findByText(/Download cityhall.toml/);
    // The document itself is shown, so an admin can see what they are about to
    // hand over.
    expect(container.querySelector("pre")?.textContent).toContain('name = "demo"');
    // A second run replaces rather than appends.
    expect(getByTestId("cityhall-export").textContent).toBe("Regenerate");
  });

  it("downloads exactly the fetched bytes", async () => {
    fetchCityHallBundle.mockResolvedValue(BUNDLE);
    const blobs: Blob[] = [];
    // jsdom implements neither createObjectURL nor an anchor click that
    // navigates, so record what would have been handed to the browser.
    const createObjectURL = vi.fn((blob: Blob) => {
      blobs.push(blob);
      return "blob:stub";
    });
    vi.stubGlobal("URL", {
      ...URL,
      createObjectURL,
      revokeObjectURL: vi.fn(),
    });

    const { getByTestId, findByText } = render(<CityHallSettings />);
    fireEvent.click(getByTestId("cityhall-export"));
    const download = await findByText(/Download cityhall.toml/);
    fireEvent.click(download);

    expect(blobs).toHaveLength(1);
    expect(await blobs[0].text()).toBe(BUNDLE);
    expect(blobs[0].type).toBe("application/toml");
  });

  it("copies exactly the fetched bundle to the clipboard", async () => {
    fetchCityHallBundle.mockResolvedValue(BUNDLE);
    const writeText = vi.fn<(text: string) => Promise<void>>().mockResolvedValue(undefined);
    vi.stubGlobal("navigator", { ...navigator, clipboard: { writeText } });

    const { getByTestId, findByText } = render(<CityHallSettings />);
    fireEvent.click(getByTestId("cityhall-export"));
    fireEvent.click(await findByText("Copy"));

    await findByText("Copied");
    expect(writeText).toHaveBeenCalledWith(BUNDLE);
  });

  /// The clipboard needs a secure context and can be denied outright. Download
  /// still works, so the denial is swallowed on purpose; what must not happen is
  /// an unhandled rejection or a "Copied" label that lies.
  it("stays usable when the clipboard is denied", async () => {
    fetchCityHallBundle.mockResolvedValue(BUNDLE);
    const writeText = vi.fn<(text: string) => Promise<void>>().mockRejectedValue(new Error("denied"));
    vi.stubGlobal("navigator", { ...navigator, clipboard: { writeText } });

    const { getByTestId, findByText, queryByText } = render(<CityHallSettings />);
    fireEvent.click(getByTestId("cityhall-export"));
    fireEvent.click(await findByText("Copy"));

    // Wait on the rejected call, not on the absence of "Copied": that label is
    // absent before the click too, so waiting for it would pass even if the
    // handler never ran.
    await waitFor(() => {
      expect(writeText).toHaveBeenCalledWith(BUNDLE);
    });
    expect(queryByText("Copied")).toBeNull();
    // Download is the fallback, so it has to survive the failed copy.
    expect(await findByText(/Download cityhall.toml/)).toBeTruthy();
  });

  /// The endpoint refuses CityHall client mode with a 403 whose message says so.
  /// Swallowing it would leave an admin staring at an empty panel.
  it("surfaces the server's reason when the export fails", async () => {
    fetchCityHallBundle.mockRejectedValue(new Error("This action is disabled in CityHall client mode"));
    const { getByTestId, findByText, queryByText } = render(<CityHallSettings />);

    fireEvent.click(getByTestId("cityhall-export"));
    await findByText(/disabled in CityHall client mode/);
    expect(queryByText(/Download/)).toBeNull();
  });

  /// A failed retry must not leave the previous bundle on screen looking current.
  it("clears a stale bundle when a later export fails", async () => {
    fetchCityHallBundle.mockResolvedValueOnce(BUNDLE);
    const { getByTestId, findByText, container } = render(<CityHallSettings />);
    fireEvent.click(getByTestId("cityhall-export"));
    await findByText(/Download cityhall.toml/);

    fetchCityHallBundle.mockRejectedValueOnce(new Error("Export failed"));
    fireEvent.click(getByTestId("cityhall-export"));
    await findByText("Export failed");
    await waitFor(() => {
      expect(container.querySelector("pre")).toBeNull();
    });
  });
});
