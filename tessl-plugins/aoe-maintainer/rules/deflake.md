# Deflaking a test

Apply these when the task is to fix a flaky test. The "Tests" section of
`AGENTS.md` is canonical.

- State the causal precondition of the race: which event the test assumes has
  happened, and why it sometimes has not.
- Run a stress repro of N iterations of the one test, for example
  `cargo nextest run -E 'test(=<name>)' --stress-count <N>`. Show that it fails
  before the fix and passes after it, and put both counts in the PR body.
- Fix the race. Do not add retries, `#[ignore]`, fixed sleeps, or a longer
  timeout as the fix. Wait on a channel, barrier, or observable predicate with
  a deadline.
- One flaky test per PR.
