---
name: aoe-runtime-lifecycle
description: Review an Agent of Empires change for state that goes wrong when a long-lived runtime object changes state while work is pending, such as a Session turn, agent or plugin worker, tmux pane, WebSocket, TUI view or image pull being cancelled, refused, replaced, restarted or ended. Use as one lens in a code review run.
---

# Review lens: AoE runtime lifecycle

Agent of Empires keeps long-lived runtime objects whose state changes under pending work: Sessions and their turns, ACP and plugin workers, tmux panes, WebSocket connections, TUI views and mounts, queued prompts and image pulls. Review the state a change adds or relies on for what happens to it at each transition.

## Scope

- **Transitions** Whether the new state, buffer, flag, cache or spawned task is still correct after each transition its owner can make while it is pending: cancel, disconnect, refusal, replacement by a newer instance, Daemon restart and recovery, the pane or worker ending, the selection moving away.
- **Identity** Whether pending work, suppression, cached results and recorded provenance are keyed to the instance they belong to (turn, launch generation, worker, transcript entry) rather than to a name, text, position or prefix a later instance can share.
- **Ownership** Whether each input, prompt or request has exactly one owner across the transition, so it is not lost, sent twice, merged with another, or delivered to a surface the user no longer sees. Key repeats and input that arrives before its target is ready count.
- **Check and act** Whether a guard (read-only, Ignore, liveness, namespace, consent) and the action it permits read the same snapshot, whether a lock is held across work that can wait indefinitely, and whether a probe that fails, times out or returns a partial answer is read as a definite "absent", "idle", "dead" or "empty" that lets a destructive or state-advancing action proceed.

## Method

Start at the state the diff introduces or begins to rely on, and name the runtime object that owns it. List that object's transitions from the code that drives it, not from the diff. For each transition, walk the pending state forward: who still holds it, what reads it next, and what the user sees. For each comparison that decides whether two things are the same, check what makes them distinct. For each guard, follow its probe into the helper that answers it and check whether an error or timeout comes back as the same value as a confirmed negative, such as a `None`, `false` or empty result. For a spawned task, receiver or pending mount, find every place that drops or cancels it and check that together they cover every way the user can leave. For keyboard and paste input, follow an event from the event loop to its handler, including key repeats and events that arrive while the target view is still opening or after the selection has changed.

## Threshold

Report when a reachable ordering of user input, Daemon events or process exits leaves a user-visible wrong result: lost, duplicated or misdirected input or prompts, a state shown that no longer holds, an action applied to something other than what was on screen, or a Session stuck until restart.

Do not report:
- Stored-data upgrades and migrations; `aoe-invariants` owns them.
- The cost of locking, retries or timeouts under load; `review-scale-and-resilience` owns it.
- Test code, including fixture cleanup, and tests that fail to pin a race; `aoe-test-evidence` owns them.
- Keyboard reachability, focus placement and containment, Escape layering and narrow-width clipping; `aoe-keyboard-focus` owns them.
- Orderings that need two independent unlikely failures, or that the code before the change already allowed and this change only moves.

## Reporting

- Name the state, the transition, the order of events that triggers the fault, and what the user sees.
- State the fix: the identity to key on, the single owner to route through, the snapshot or lock to share between check and action, or the cleanup to run on the transition.
