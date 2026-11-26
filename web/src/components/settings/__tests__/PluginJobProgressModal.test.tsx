// @vitest-environment jsdom

import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

import type { PluginJob, PluginJobState } from "../../../lib/api";

const fetchPluginJob = vi.fn();

vi.mock("../../../lib/api", () => ({
  fetchPluginJob: (id: string) => fetchPluginJob(id),
}));

import { PluginJobProgressModal } from "../PluginJobProgressModal";

function job(status: PluginJobState): PluginJob {
  return {
    job: { id: "j1", kind: "install", target: "p", status, started_at: 1, finished_at: 2 },
    log: { tail: "" },
  } as PluginJob;
}

beforeEach(() => fetchPluginJob.mockReset());

describe("PluginJobProgressModal onClose", () => {
  it.each([
    { status: { state: "failed", error: "boom" } as PluginJobState, expected: false },
    { status: { state: "succeeded" } as PluginJobState, expected: true },
  ])("passes $expected when the job $status.state", async ({ status, expected }) => {
    fetchPluginJob.mockResolvedValue({ kind: "ok", job: job(status) });
    const onClose = vi.fn();
    render(<PluginJobProgressModal jobId="j1" title="t" onClose={onClose} />);
    const done = await screen.findByTestId("plugin-job-done");
    fireEvent.click(done);
    expect(onClose).toHaveBeenCalledExactlyOnceWith(expected);
  });
});
