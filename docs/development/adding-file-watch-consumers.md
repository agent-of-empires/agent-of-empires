# Adding a FileWatchService consumer

`FileWatchService` (`src/file_watch.rs`) is the per-process file-event primitive that surfaces kernel filesystem notifications and in-process Local writes through a single dispatcher with per-`(SubscriptionId, path)` trailing-edge debounce. This guide is the canonical recipe for new consumers.

## Per-process single-instance rule

Each process constructs exactly one live `FileWatchService` at bootstrap (`Arc<Self>`) and threads the clone through every consumer. `FileWatchService::new()` is the canonical constructor; do not call it from arbitrary call sites. Tests outside the crate go through `test_support::new_filewatch` to make the rule discoverable.

The daemon constructs its instance in `start_server` (`src/server/mod.rs`) and threads it through `Storage::new` and the per-profile disk-watch subscriptions. Short-lived CLI subprocesses and the native TUI use `Storage::new_unwatched` (a noop service): they have no in-process subscriber, and any change they persist still reaches the daemon through its kernel watcher. Per-process live instances are independent because cross-process subscribers always go through the kernel watcher anyway.

## Consumer recipe

```rust
let config_path = profile_dir.join("config.toml");
let spec = WatchSpec {
    dir: profile_dir,                            // canonicalized inside subscribe_channel
    matcher: FileMatcher::Exact(config_path),    // Exact(PathBuf); see also AnyOf(Vec<PathBuf>)
    debounce: Some(Duration::from_millis(100)),  // None disables debouncing
};
let (mut rx, handle) = svc.subscribe_channel(spec, capacity)?;
// keep `handle` alive for the consumer's lifetime; drop = unsubscribe
let forwarder = task_util::spawn_supervised(
    "your.consumer.forwarder",
    PanicPolicy::Log,
    async move {
        while rx.recv().await.is_some() {
            // collapse N events into one signal; the consumer pulls latest
            // state from disk on its next tick.
            dirty.store(true, Ordering::Release);
        }
    },
);
```

### Capacity

Bounded mpsc receiver, sized to the worst-case burst between consumer ticks. The dispatcher uses `try_send` and drops with a rate-limited `debug!` on full. Consumers that latch into a single `Arc<AtomicBool>` (pattern below) tolerate drops because the latch is idempotent; downstream consumers that need every event must size capacity to absorb worst-case bursts plus headroom.

### Capacity rule of thumb

- AtomicBool latch consumer (storage / config kicks): 4-16 is plenty
- Per-event consumer (none today): size to peak burst within a tick budget

### Debounce

Per-`(SubscriptionId, path)` trailing-edge debounce in `Duration` granularity. Common values: 75 ms (storage; kernel echo of `Storage::update` rename) and 100 ms (config; vim-style write+rename burst). The passive-status pipeline (`docs/development/internals/sessions.md`) is a canonical `Storage::update` producer: the daemon's `status_poll_loop` writes one row per profile per 2s tick when transitions occur, so a `sessions.json` subscriber's `FileWatchService` event stream is dominated by passive-status writes under a busy set of sessions.

### Lifetime of `SubscriptionHandle`

RAII guard. Dropping it deregisters the subscription from the dispatcher's registry, closes the source channel, and decrements the directory's refcount (unwatching when it hits zero). The pattern for consumer entries is `struct WatchEntry { handle: SubscriptionHandle, forwarder: AbortHandle }` plus a `drop_watch_entry(entry)` helper that drops `handle` first (closes channel, forwarder exits naturally on next `recv` returning `None`), then `forwarder.abort()` as a fast-path safeguard for an in-flight `try_send`.

### Forwarder vs direct loop

A short forwarder task that latches a flag is the canonical pattern. Direct `match rx.try_recv()` in a tick body works but couples the consumer to the receiver lifetime. The forwarder isolates the receiver's drop semantics from the consumer's tick.

## Diagnostics

### Dispatcher death

The dispatcher's exit (panic, channel close, service drop) flips a sticky `dispatcher_dead` latch and logs once with the exit reason. After this, `subscribe_channel` returns `WatchError::DispatcherDead` and `notify_local_change` becomes a silent no-op. Consumers should fall back to polling or the heartbeat reload path on a `DispatcherDead` error.

### `AOE_FILE_WATCH=off`

Operator escape hatch: setting this env to `off` makes `FileWatchService::new` return a noop service instead of starting the kernel watcher. Useful when watcher-related issues are suspected; consumers degrade to their polling fallback. Surfaced in the daemon and TUI bootstrap; not part of `Config` because it is a runtime kill-switch, not a tunable setting.

## See also

- `src/file_watch.rs` rustdoc on the types
- `src/server/mod.rs::disk_watcher_consumer`: daemon storage-mirror consumer
- `src/server/mod.rs::build_disk_watch_entry` / `drop_disk_watch_entry`: the canonical handle + forwarder lifetime pattern
- `src/logging.rs::watch_runtime_filter`: runtime log-filter consumer
