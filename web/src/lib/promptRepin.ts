// Pure decision for the structured view's "re-engage stick-to-bottom on
// submit" (#3993). DOM-free so it is unit-tested directly; StructuredView
// wires it to the reducer's `promptSeq` and the viewport (the same split as
// `autoLoadDecision` in historyScroll.ts).

export interface PromptRepinInput {
  /** `promptSeq` observed by the previous evaluation; `null` on mount. */
  seen: number | null;
  /** The reducer's monotonic count of prompts applied: bumped by this
   *  client's optimistic `user_prompt` action the moment Send is clicked, and
   *  by a `UserPromptSent` echo with no matching optimistic id (another
   *  device, a drained queue entry, a replay). One bump per prompt, whatever
   *  the path. */
  promptSeq: number;
  /** Whether the session socket has opened. `fetchReplay` lands the
   *  transcript (and its prompt bumps) before the WebSocket dials and
   *  `hasEverOpened` flips only in `onopen`, so a bump seen while this is
   *  false is hydration, not a submit — unless it is this client's own. */
  live: boolean;
  /** Whether this client has an optimistic prompt in flight
   *  (`inflightPromptIds` non-empty). A bump that arrives with one is a local
   *  submit whatever the socket state: `sendPrompt` dispatches the optimistic
   *  row before the POST, even before the first open, and that submit deserves
   *  its re-pin too. Replay bumps never carry an in-flight id. */
  localInflight: boolean;
}

export interface PromptRepinDecision {
  /** Value to carry forward as `seen`. */
  seen: number;
  /** True when the transcript should re-pin to the bottom now. */
  pin: boolean;
}

/** Re-pin exactly once per prompt dispatched after mount, when the socket is
 *  live or the prompt is this client's own. The mount pass never pins (the
 *  scroll-state restore owns the first pin, and a reader who reopened scrolled
 *  up must stay there); a reset that lowers the counter never pins either. */
export function promptRepinDecision(i: PromptRepinInput): PromptRepinDecision {
  if (i.seen === null) return { seen: i.promptSeq, pin: false };
  const advanced = i.promptSeq > i.seen;
  return { seen: i.promptSeq, pin: advanced && (i.live || i.localInflight) };
}
