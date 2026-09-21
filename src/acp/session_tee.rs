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

/// Span name carrying the session id for scope-based capture.
pub const SESSION_SPAN: &str = "acp_session";

/// Target reserved for the tee's own diagnostics; skipped on the event
/// path to prevent re-entrancy.
const TEE_TARGET: &str = "acp.tee";

/// Cap on simultaneously-open per-session log files.
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
    fn writer_for(&self, session: &str) -> Option<Arc<Mutex<SizeRotatingWriter>>> {
        let mut cache = lock(&self.writers);
        cache.tick += 1;
        let now = cache.tick;
        if let Some(entry) = cache.map.get_mut(session) {
            entry.last_used = now;
            return Some(entry.writer.clone());
        }
        let path = crate::process::worker_registry::log_path_for(session).ok()?;
        // At capacity: evict the oldest entry whose writer is idle
        // (`strong_count == 1`, only the cache holds it).
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
/// out of the tracing event path.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Debug-formatted values arrive quoted (`"abc"`); strip surrounding
/// quotes so `session` values pass `log_path_for` validation and the
/// rendered line reads cleanly.
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
/// message plus remaining fields for the per-session line.
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
    use tracing::subscriber::with_default;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::Registry;

    fn read_log(session: &str) -> String {
        let p = crate::process::worker_registry::log_path_for(session).unwrap();
        std::fs::read_to_string(p).unwrap_or_default()
    }

    #[test]
    #[serial]
    fn tee_routes_each_event_to_its_own_session_log() {
        let _home = crate::session::test_support::isolate_app_dir();
        let sub = Registry::default().with(SessionTeeLayer::new());
        with_default(sub, || {
            tracing::info!(target: "acp.protocol", "no session here");
            // `acp.tee` is skipped so the layer cannot re-enter itself.
            tracing::warn!(target: "acp.tee", session = %"sess-z", "internal");
        });
        let dir = crate::process::worker_registry::workers_dir().unwrap();
        assert_eq!(
            std::fs::read_dir(&dir).map(|rd| rd.count()).unwrap_or(0),
            0,
            "a sessionless event and an acp.tee event must not create a log file"
        );

        let sub = Registry::default().with(SessionTeeLayer::new());
        with_default(sub, || {
            tracing::warn!(target: "acp.protocol", session = %"sess-a", "watchdog fired");
            tracing::info!(target: "acp.protocol", session = %"sess-x", "x only");
            tracing::info!(target: "acp.protocol", session = %"sess-y", "y only");
            let span = tracing::info_span!("acp_session", session = %"sess-span");
            let _g = span.enter();
            tracing::warn!(target: "acp.protocol", "inherited via span");
        });
        let a = read_log("sess-a");
        assert!(
            a.contains("watchdog fired") && a.contains("acp.protocol"),
            "{a}"
        );
        let (x, y) = (read_log("sess-x"), read_log("sess-y"));
        assert!(x.contains("x only") && !x.contains("y only"), "x log: {x}");
        assert!(y.contains("y only") && !y.contains("x only"), "y log: {y}");
        assert!(read_log("sess-span").contains("inherited via span"));
        assert!(read_log("sess-z").is_empty());
    }
}
