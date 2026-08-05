// @vitest-environment jsdom
//
// RTL mount of the real <Composer>, driving the `/` slash-command popover to
// verify the provenance badge (#3052): a command backed by a skill renders
// its source pill inline with the label, and a plain non-skill command (or
// one with no resolvable skill) renders no badge at all. The live Playwright
// suite (tests/live/acp-stories/composer-slash-pick-no-arg.spec.ts) drives
// the same popover against a real backend; this jsdom mount is what lifts
// the badge-rendering branch in the merged coverage report.

import { describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { AssistantRuntimeProvider, useExternalStoreRuntime, type ThreadMessageLike } from "@assistant-ui/react";
import { afterEach, beforeEach } from "vitest";

import { Composer } from "./Composer";
import { buildSkillIndex, type SkillIndex } from "../../lib/skillProvenance";
import type { AvailableCommand } from "../../lib/acpTypes";

vi.mock("./useFilesIndex", () => ({
  useFilesIndex: () => ({ files: [] }),
  // The trigger popover's own query-detection drives which items are
  // offered; the identity filter keeps this test independent of fuzzyFilter's
  // ranking behavior.
  fuzzyFilter: <T,>(items: T[]) => items,
}));
vi.mock("./SessionConfigControls", () => ({ SessionConfigControls: () => null }));
vi.mock("./SwitchAgentModal", () => ({ SwitchAgentModal: () => null }));
vi.mock("../../hooks/useMobileKeyboard", () => ({ useMobileKeyboard: () => ({ keyboardOpen: false }) }));
vi.mock("../../hooks/useFocusTerminalTarget", () => ({ useFocusTerminalTarget: () => {} }));
vi.mock("../../lib/agentProfileContext", () => ({
  useAgentProfile: () => ({ clearAliases: [], capabilities: { legacyModeFallback: false } }),
}));
vi.mock("../../lib/acpDrafts", () => ({
  getDraft: () => "",
  setDraft: () => {},
  clearDraft: () => {},
  clearDraftAttachments: () => {},
}));
vi.mock("./useDictationBurstGuard", () => ({
  useDictationBurstGuard: () => ({
    observeInputType: () => {},
    shouldSuppressUpstream: () => false,
    flushOnBlur: () => {},
  }),
}));

const { skillIndexRef } = vi.hoisted(() => ({
  skillIndexRef: { current: { labelsByKey: new Map<string, Set<string>>() } as SkillIndex },
}));
vi.mock("../../hooks/useSkillIndex", () => ({
  useSkillIndex: () => skillIndexRef.current,
}));

const COMMANDS: AvailableCommand[] = [
  { name: "aoe-review", description: "Run the review skill", accepts_input: false },
  { name: "help", description: "Show help", accepts_input: false },
];

function Harness() {
  const runtime = useExternalStoreRuntime<ThreadMessageLike>({
    messages: [],
    isRunning: false,
    convertMessage: (m) => m,
    onNew: async () => {},
  });
  return (
    <AssistantRuntimeProvider runtime={runtime}>
      <Composer
        sessionId="s-slash-provenance"
        currentAgent={"claude" as never}
        availableModes={[] as never}
        currentModeId={null as never}
        legacyMode={null as never}
        configOptions={[] as never}
        pendingConfigOption={null as never}
        setConfigOption={() => {}}
        sessionUsage={null as never}
        availableCommands={COMMANDS}
        connected={true}
        turnActive={false}
        queuedCount={0}
        enqueuePrompt={() => {}}
        promptCapabilities={null}
        pendingAttachments={[]}
        setPendingAttachments={() => {}}
      />
    </AssistantRuntimeProvider>
  );
}

beforeEach(() => {
  // jsdom has no matchMedia; a never-matching stub yields the desktop code
  // path. Stubbed (not assigned directly) so it does not leak into later
  // test files.
  vi.stubGlobal(
    "matchMedia",
    vi.fn().mockImplementation((query: string) => ({
      matches: false,
      media: query,
      onchange: null,
      addListener: () => {},
      removeListener: () => {},
      addEventListener: () => {},
      removeEventListener: () => {},
      dispatchEvent: () => false,
    })),
  );
  // "aoe-review" is a skill (single source "Claude"); "help" is not indexed
  // at all, so it must render with no badge.
  skillIndexRef.current = buildSkillIndex({
    roots: [
      { id: "claude-user", label: "Claude", relativePath: ".claude/skills", consumers: ["claude"], legacy: false },
    ],
    skills: [
      {
        directory: "aoe-review",
        name: "aoe-review",
        description: "",
        provenance: { kind: "external", root: "claude-user" },
        provenanceLabel: "external:claude-user",
        writable: false,
      },
    ],
  });
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

function getComposer(): HTMLTextAreaElement {
  return screen.getByRole("textbox") as HTMLTextAreaElement;
}

describe("<Composer> slash popover provenance badge (#3052)", () => {
  it("badges a skill-backed command and leaves a plain command unbadged", async () => {
    render(<Harness />);
    const ta = getComposer();
    fireEvent.change(ta, { target: { value: "/" } });

    const options = await waitFor(() => {
      const opts = screen.getAllByRole("option");
      expect(opts.length).toBeGreaterThan(0);
      return opts;
    });

    const reviewOption = options.find((o) => o.textContent?.includes("/aoe-review"));
    const helpOption = options.find((o) => o.textContent?.includes("/help"));
    expect(reviewOption).toBeDefined();
    expect(helpOption).toBeDefined();
    expect(reviewOption!.textContent).toContain("Claude");
    expect(helpOption!.textContent).not.toContain("Claude");
  });
});
