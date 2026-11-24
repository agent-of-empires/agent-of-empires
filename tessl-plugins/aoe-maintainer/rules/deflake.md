# Deflaking a test

Apply these when the task is to fix a flaky test: a Rust test under nextest, a
Vitest test, or a Playwright spec. The "Tests" sections of `AGENTS.md` and
`web/AGENTS.md` are canonical.

- State the causal precondition of the race: which event the test assumes has
  happened, and why it sometimes has not.
- Run a stress repro of N iterations of the one test, under load like CI's:
  - Rust: `cargo nextest run -E 'test(=<name>)' --stress-count <N>`. nextest
    never runs copies of one test in parallel, so `--test-threads` alone adds
    no load; widen the filter to the rest of its binary
    (`-E 'binary_id(<id>)'`) or run CPU load beside it.
  - Vitest, from `web/`: `npx vitest run <file> -t '<name>' --repeats <N>`.
  - Playwright, from `web/`: `npx playwright test <spec>:<line>
    --repeat-each <N> --retries 0 --workers <M>`, with
    `--config=playwright.live.config.ts` for a live spec. CI retries mocked
    specs once, so `--retries 0` is what exposes the flake.

  Show that it fails before the fix and passes after it, and put both counts
  in the PR body.
- Fix the race. Do not add retries, skips (`#[ignore]`, `.skip`, `.fixme`),
  fixed sleeps (`sleep`, `setTimeout`, `waitForTimeout`), or a longer timeout
  as the fix. Wait on a channel, barrier, or observable predicate with a
  deadline; in web tests, a web-first assertion or `waitFor` on the state the
  test needs.
- One flaky test per PR.
