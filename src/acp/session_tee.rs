//! Per-session tracing tee: mirrors session-scoped events into each
//! session's `acp-workers/<id>.log` so `aoe acp logs --session <id>`
//! surfaces the daemon's watchdog/cancel breadcrumbs, not just the
//! startup marker plus agent stderr. Additive: events still flow to the
//! shared `debug.log`. See issue #1864.
//!
//! Capture. Events are routed by their `session` (or rare `session_id`)
//! field. A `tracing::info_span!("acp_session", session = %id)` wraps
//! each daemon per-session connection task, so events that do not set the
//! field explicitly still inherit it through the span scope. The layer
//! reads the event's own fields first, then walks the span scope.
//!
//! I/O. Synchronous best-effort writes through a `SizeRotatingWriter` per
//! session, mirroring the shared `debug.log` writer (same rare-rotation
//! stall profile, so no new failure class). Writers are bounded by count
//! and the least-recently-used one is evicted, so a long-lived daemon
//! does not leak file handles. No background thread, no channel: dropping
//! a breadcrumb during a spike would lose exactly the diagnostics this
//! exists to capture.
//!
//! Re-entrancy. The writer never emits tracing; events with target
//! `acp.tee` are skipped so any future self-reporting cannot loop.

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex, MutexGuard};

use tracing::field::{Field, Visit};
use tracing::span::Attributes;
use tracing::{Event, Id, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

use crate::logging::{RotationPolicy, SizeRotatingWriter};
use crate::session::config::RotationKind;

// The layer is installed generically (un-boxed) in the daemon subscriber
// stack so its `Layer<S> for any S` impl resolves against the full layered
// subscriber type, which a `Box<dyn Layer<Registry>>` could not name.

/// Span name carrying the session id for scope-based capture. Entered at
/// the daemon per-session connection task so nested events inherit the
/// `session` field even when they do not set it explicitly.
pub const SESSION_SPAN: &str = "acp_session";

/// Target reserved for the tee's own diagnostics; skipped on the event
/// path to prevent re-entrancy.
const TEE_TARGET: &str = "acp.tee";

/// Cap on simultaneously-open per-session log files. Realistic concurrent
/// session counts sit well under this; the bound only matters for a
/// daemon that churns through many sessions over days.
const MAX_OPEN_SESSION_LOGS: usize = 64;
const PER_SESSION_MAX_BYTES: u64 = 10 * 1024 * 1024;
const PER_SESSION_KEEP: u8 = 2;

/// Cached session id stored in a span's extensions on creation, so the
/// per-event scope walk is a pointer chase rather than a field re-visit.
struct SessionTag(String);

pub struct SessionTeeLayer {
    writers: Mutex<WriterCache>,
}

struct WriterCache {
    map: HashMap<String, Entry>,
    /// Monotonic counter stamping each access; the smallest stamp is the
    /// least-recently-used entry to evict when the cap is reached.
    tick: u64,
}

struct Entry {
    writer: Arc<Mutex<SizeRotatingWriter>>,
    last_used: u64,
}

impl Default for SessionTeeLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionTeeLayer {
    pub fn new() -> Self {
        Self {
            writers: Mutex::new(WriterCache {
                map: HashMap::new(),
                tick: 0,
            }),
        }
    }

    /// Resolve (or open) the per-session writer, updating LRU bookkeeping.
    /// Returns `None` when the session id is unsafe or the file cannot be
    /// opened; the caller silently drops the line in that case.
    fn writer_for(&self, session: &str) -> Option<Arc<Mutex<SizeRotatingWriter>>> {
        let mut cache = lock(&self.writers);
        cache.tick += 1;
        let now = cache.tick;
        if let Some(entry) = cache.map.get_mut(session) {
            entry.last_used = now;
            return Some(entry.writer.clone());
        }
        let path = crate::acp::worker_registry::log_path_for(session).ok()?;
        // At capacity: evict the oldest entry whose writer is idle
        // (`strong_count == 1`, only the cache holds it). Evicting a writer
        // still in flight on another thread would let this call open a
        // second `SizeRotatingWriter` on the same file and race its appends
        // and rotation. If every cached writer is in flight, drop this event
        // rather than open a racing writer.
        if cache.map.len() >= MAX_OPEN_SESSION_LOGS {
            let evict = cache
                .map
                .iter()
                .filter(|(_, e)| Arc::strong_count(&e.writer) == 1)
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone())?;
            cache.map.remove(&evict);
        }
        let policy = RotationPolicy {
            kind: RotationKind::Size,
            max_size_bytes: PER_SESSION_MAX_BYTES,
            keep_count: PER_SESSION_KEEP,
        };
        let writer = Arc::new(Mutex::new(SizeRotatingWriter::new(path, policy).ok()?));
        cache.map.insert(
            session.to_string(),
            Entry {
                writer: writer.clone(),
                last_used: now,
            },
        );
        Some(writer)
    }
}

impl<S> Layer<S> for SessionTeeLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        if attrs.metadata().name() != SESSION_SPAN {
            return;
        }
        let mut v = SessionVisitor::default();
        attrs.record(&mut v);
        if let Some(session) = v.session {
            if let Some(span) = ctx.span(id) {
                span.extensions_mut().insert(SessionTag(session));
            }
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        if event.metadata().target() == TEE_TARGET {
            return;
        }
        // Collect the renderable line and any explicit session field in a
        // single pass, then fall back to span-scope inheritance.
        let mut visitor = LineVisitor::default();
        event.record(&mut visitor);
        let session = match visitor.session.clone() {
            Some(s) => s,
            None => match session_from_scope(event, &ctx) {
                Some(s) => s,
                None => return,
            },
        };
        let Some(writer) = self.writer_for(&session) else {
            return;
        };
        let line = visitor.format(event);
        let mut w = lock(&writer);
        let _ = w.write_all(line.as_bytes());
        let _ = w.flush();
    }
}

fn session_from_scope<S>(event: &Event<'_>, ctx: &Context<'_, S>) -> Option<String>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let scope = ctx.event_scope(event)?;
    for span in scope.from_root() {
        if let Some(tag) = span.extensions().get::<SessionTag>() {
            return Some(tag.0.clone());
        }
    }
    None
}

/// Lock that never panics on poison: a writer panic must not propagate
/// out of the tracing event path. Recovers the inner guard instead.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Debug-formatted values arrive quoted (`"abc"`); strip surrounding
/// quotes so `session` values pass `log_path_for` validation and the
/// rendered line reads cleanly. Mirrors `StageRecorder` in `deletion.rs`.
fn unquote(s: &str) -> String {
    s.trim_matches('"').to_string()
}

#[derive(Default)]
struct SessionVisitor {
    session: Option<String>,
}

impl SessionVisitor {
    fn capture(&mut self, field: &Field, value: String) {
        match field.name() {
            "session" => self.session = Some(value),
            "session_id" if self.session.is_none() => self.session = Some(value),
            _ => {}
        }
    }
}

impl Visit for SessionVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.capture(field, value.to_string());
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.capture(field, unquote(&format!("{value:?}")));
    }
}

/// Visitor that both finds the session id and collects the renderable
/// message plus remaining fields for the per-session line. The session
/// field itself is not echoed into the line: the file is already
/// per-session, so repeating it would be noise.
#[derive(Default)]
struct LineVisitor {
    session: Option<String>,
    message: Option<String>,
    kv: String,
}

impl LineVisitor {
    fn push(&mut self, field: &Field, value: String) {
        match field.name() {
            "message" => self.message = Some(value),
            "session" => self.session = Some(value),
            "session_id" => {
                if self.session.is_none() {
                    self.session = Some(value);
                }
            }
            name => {
                self.kv.push(' ');
                self.kv.push_str(name);
                self.kv.push('=');
                self.kv.push_str(&value);
            }
        }
    }

    fn format(&self, event: &Event<'_>) -> String {
        let meta = event.metadata();
        let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
        let msg = self.message.as_deref().unwrap_or("");
        format!(
            "{ts}  {} {}: {}{}\n",
            meta.level(),
            meta.target(),
            msg,
            self.kv
        )
    }
}

impl Visit for LineVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.push(field, value.to_string());
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.push(field, unquote(&format!("{value:?}")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::TempDir;
    use tracing::subscriber::with_default;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::Registry;

    /// Point `get_app_dir` at a throwaway home so `log_path_for` resolves
    /// under a temp dir. Restoration runs from an RAII guard so a panicking
    /// `f()` cannot leak the temp env into later serialized tests.
    fn with_temp_home<F: FnOnce()>(f: F) {
        struct EnvGuard {
            home: Option<std::ffi::OsString>,
            xdg: Option<std::ffi::OsString>,
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                // SAFETY: tests are serialized via `#[serial]`.
                unsafe {
                    match self.home.take() {
                        Some(v) => std::env::set_var("HOME", v),
                        None => std::env::remove_var("HOME"),
                    }
                    match self.xdg.take() {
                        Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                        None => std::env::remove_var("XDG_CONFIG_HOME"),
                    }
                }
            }
        }

        let tmp = TempDir::new().unwrap();
        let _guard = EnvGuard {
            home: std::env::var_os("HOME"),
            xdg: std::env::var_os("XDG_CONFIG_HOME"),
        };
        // SAFETY: tests are serialized via `#[serial]`; the guard restores
        // the originals on scope exit, including an unwind.
        unsafe {
            std::env::set_var("HOME", tmp.path());
            std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));
        }
        f();
    }

    fn read_log(session: &str) -> String {
        let p = crate::acp::worker_registry::log_path_for(session).unwrap();
        std::fs::read_to_string(p).unwrap_or_default()
    }

    #[test]
    #[serial]
    fn routes_event_with_session_field_to_its_file() {
        with_temp_home(|| {
            let sub = Registry::default().with(SessionTeeLayer::new());
            with_default(sub, || {
                tracing::warn!(target: "acp.protocol", session = %"sess-a", "watchdog fired");
            });
            let body = read_log("sess-a");
            assert!(body.contains("watchdog fired"), "got: {body}");
            assert!(body.contains("acp.protocol"), "got: {body}");
        });
    }

    #[test]
    #[serial]
    fn no_cross_session_leakage() {
        with_temp_home(|| {
            let sub = Registry::default().with(SessionTeeLayer::new());
            with_default(sub, || {
                tracing::info!(target: "acp.protocol", session = %"sess-x", "x only");
                tracing::info!(target: "acp.protocol", session = %"sess-y", "y only");
            });
            let x = read_log("sess-x");
            let y = read_log("sess-y");
            assert!(x.contains("x only") && !x.contains("y only"), "x log: {x}");
            assert!(y.contains("y only") && !y.contains("x only"), "y log: {y}");
        });
    }

    #[test]
    #[serial]
    fn drops_event_without_session() {
        with_temp_home(|| {
            let sub = Registry::default().with(SessionTeeLayer::new());
            with_default(sub, || {
                tracing::info!(target: "acp.protocol", "no session here");
            });
            let dir = crate::acp::worker_registry::workers_dir().unwrap();
            let count = std::fs::read_dir(&dir).map(|rd| rd.count()).unwrap_or(0);
            assert_eq!(count, 0, "a sessionless event must not create a log file");
        });
    }

    #[test]
    #[serial]
    fn inherits_session_from_span_scope() {
        with_temp_home(|| {
            let sub = Registry::default().with(SessionTeeLayer::new());
            with_default(sub, || {
                let span = tracing::info_span!("acp_session", session = %"sess-span");
                let _g = span.enter();
                // Event carries no explicit `session` field; it must be
                // attributed via the enclosing span scope.
                tracing::warn!(target: "acp.protocol", "inherited via span");
            });
            let body = read_log("sess-span");
            assert!(body.contains("inherited via span"), "got: {body}");
        });
    }

    #[test]
    #[serial]
    fn skips_tee_target_to_avoid_reentrancy() {
        with_temp_home(|| {
            let sub = Registry::default().with(SessionTeeLayer::new());
            with_default(sub, || {
                tracing::warn!(target: "acp.tee", session = %"sess-z", "internal");
            });
            assert!(
                read_log("sess-z").is_empty(),
                "events on the acp.tee target must be skipped"
            );
        });
    }
}
