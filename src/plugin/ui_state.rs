//! Host-owned store of UI state that plugin workers push over the `ui.state.*`
//! and `ui.notify` RPCs (#2366).
//!
//! The honest model: a worker pushes *typed display state* into a slot it
//! declared; the host stores it here and the web dashboard renders it. No
//! plugin code runs in the dashboard and the render path never awaits a worker,
//! so this store is read synchronously via [`UiStore::snapshot`].
//!
//! State is ephemeral, like the rest of the Tier 1 host: it lives in memory and
//! dies with the daemon. A plugin's entries are cleared when its worker exits
//! (a fresh worker repopulates them), guarded by a per-spawn *generation* so a
//! late write from an exited worker, or an instant respawn, cannot resurrect or
//! clobber stale state. Notifications are point-in-time events on a separate
//! bounded ring: they survive a worker exit (a plugin that posts a notification
//! and immediately crashes should still reach the browser) and the client
//! toasts each one once by tracking the monotonic `seq`.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

use aoe_plugin_api::UiSlot;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Most entries one plugin may hold at once across all slots. A cooperative
/// bound (the model is honest, not adversarial), enough to keep a buggy plugin
/// from growing host memory without limit.
const MAX_ENTRIES_PER_PLUGIN: usize = 256;
/// Largest normalized payload accepted for one entry, in bytes of JSON. The
/// pane slot gets a much larger budget than the small badge/column slots: a
/// pane can carry a full PR comment list, where a badge is a few words.
const MAX_PAYLOAD_BYTES: usize = 8 * 1024;
const MAX_PANE_PAYLOAD_BYTES: usize = 64 * 1024;
/// Notifications kept on the shared ring before the oldest are dropped.
const NOTIFICATION_RING: usize = 200;
/// Caps on notification text, so one notify cannot post an unbounded blob.
const MAX_TITLE_LEN: usize = 256;
const MAX_BODY_LEN: usize = 4096;

/// A display tone, mapped to a color by each rendering surface. A closed set so
/// a plugin cannot inject an arbitrary class or color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tone {
    Neutral,
    Info,
    Success,
    Warn,
    Danger,
}

/// Sort direction for a [`UiSlot::SortKey`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SortDirection {
    Asc,
    Desc,
}

/// A scalar a `RowColumn` exposes for client-side sorting. Kept to comparable
/// scalars (no objects/arrays) so the dashboard can order rows deterministically
/// without running plugin code.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SortValue {
    Number(f64),
    String(String),
}

/// One option in a [`UiSlot::FilterFacet`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FacetOption {
    pub value: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tone: Option<Tone>,
}

// Per-slot payloads. Each is the typed shape a worker must send for that slot;
// `ui.state.set` validates the incoming JSON against the slot's payload before
// storing, so a malformed push is rejected at the host boundary rather than
// crashing the dashboard. They carry no `session_id`: that is an RPC-level
// param and becomes part of the entry key, never duplicated in the body.

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TextPayload {
    text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tone: Option<Tone>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tooltip: Option<String>,
    /// Lucide icon name, e.g. "git-pull-request-arrow". The client maps it
    /// through a small allowlist; an unknown name renders no icon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    icon: Option<String>,
    /// URL to open (e.g. the PR). When set, the client renders the badge as a
    /// link instead of static text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    href: Option<String>,
}

/// One icon/text badge inside a `row-badge` `items` list.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BadgeItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    icon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tone: Option<Tone>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    href: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tooltip: Option<String>,
}

/// `row-badge` payload: the single-badge fields (back-compat with any plugin
/// pushing `{ text, tone, tooltip, icon, href }`) plus an optional `items` list
/// so one entry can carry several icon badges. `text` is optional here: an
/// items-only badge has no top-level text. Empty `items: []` is valid (clears
/// the row).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RowBadgePayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tone: Option<Tone>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tooltip: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    icon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    href: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    items: Vec<BadgeItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RowColumnPayload {
    text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tone: Option<Tone>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tooltip: Option<String>,
    /// Scalar driving client-side sorting (referenced by a `SortKey`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sort_value: Option<SortValue>,
    /// Tokens this row matches for client-side filtering (referenced by a
    /// `FilterFacet`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    filter_values: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SortKeyPayload {
    label: String,
    /// The `RowColumn` id whose `sort_value` this orders by.
    column: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    direction: Option<SortDirection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilterFacetPayload {
    label: String,
    /// The `RowColumn` id whose `filter_values` this filters over.
    column: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    options: Vec<FacetOption>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CardPayload {
    title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tone: Option<Tone>,
}

/// Which dock a [`UiSlot::Pane`] opens in by default. A closed set so a plugin
/// cannot name an arbitrary location; the user can still move the pane after.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PaneLocation {
    Right,
    Bottom,
}

/// `pane` payload (the dockable tool-window slot). Either the simple
/// `{ title, body }` form or an ordered `blocks` list, plus an optional
/// `default_location` picking the dock it first opens in (defaults to the
/// right dock host-side when omitted). The blocks are kept as opaque JSON on
/// purpose: the host validates only the envelope (an array of objects) and the
/// web renders the block kinds it knows, dropping the rest. This is the
/// forward-compat contract: a plugin can add fields to a known kind, or a whole
/// new kind, and never need a host change; only the web renderer grows.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PanePayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    blocks: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default_location: Option<PaneLocation>,
    /// Lucide icon name for the pane's activity-bar/tool-window icon. Opaque to
    /// the host (the web resolves it against its allowlist, falling back to a
    /// generic icon); kept only so `deny_unknown_fields` accepts it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    icon: Option<String>,
}

/// Why a `ui.state.set`/`ui.state.remove` was rejected. The host API maps each
/// to a JSON-RPC error code.
#[derive(Debug, PartialEq, Eq)]
pub enum UiError {
    /// The calling worker's generation is no longer active (it exited, or a
    /// newer worker replaced it). The write is dropped rather than resurrecting
    /// stale state.
    StaleWorker,
    /// The plugin already holds [`MAX_ENTRIES_PER_PLUGIN`] entries.
    QuotaExceeded,
    /// The payload did not match the slot's typed shape, or a scope rule
    /// (per-session slot needs a `session_id`; a global slot must not have one).
    BadRequest(String),
}

/// Identifies one stored entry. A per-session slot keys on `session_id`; a
/// global slot leaves it `None`. `id` is the plugin-chosen address within the
/// slot, gated against the manifest `ui` declarations.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct EntryKey {
    plugin_id: String,
    slot: UiSlot,
    id: String,
    session_id: Option<String>,
}

/// A notification as rendered: the seq lets the client toast each one once.
/// `Deserialize` so daemon-connected clients (the native TUI structured view,
/// #2402) can decode the same wire shape the web frontend consumes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub seq: u64,
    pub plugin_id: String,
    pub tone: Tone,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// One entry in the snapshot the web renders.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiEntry {
    pub plugin_id: String,
    pub slot: UiSlot,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Normalized, slot-validated payload. The `slot` tells the client its
    /// shape.
    pub payload: Value,
}

/// The full UI state the dashboard polls each tick. Bounded and small, so it is
/// sent whole rather than incrementally (verdict: no since_seq/tombstones).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiSnapshot {
    pub entries: Vec<UiEntry>,
    pub notifications: Vec<Notification>,
    /// Monotonic mutation counter per `(plugin_id, scope)`, where `scope` is a
    /// session id, or `""` for a global (session-less) slot. The dashboard reads
    /// a baseline from the action POST and holds a manual-action spinner until
    /// the matching scope's counter moves off it, so the spinner tracks the
    /// worker's re-pushed state for that pane instead of the fire-and-forget
    /// POST, and an unrelated session's push never clears it. Outer key is the
    /// plugin id, inner key the scope. `BTreeMap` for a deterministic serialized
    /// order; `serde(default)` so an older daemon's snapshot still decodes.
    #[serde(default)]
    pub revisions: BTreeMap<String, BTreeMap<String, u64>>,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<EntryKey, Value>,
    /// Per-plugin currently-active worker generation. Absent once a worker has
    /// exited and its state cleared; a respawn re-registers via
    /// [`UiStore::begin_generation`].
    active: HashMap<String, u64>,
    notifications: VecDeque<Notification>,
    notify_seq: u64,
    /// Mutation counter keyed by `(plugin_id, scope)`, bumped on every accepted
    /// entry change to that scope. `scope` is the entry's session id, or `""`
    /// for a global slot. Daemon-local: it resets when the daemon restarts, so
    /// the client treats any change off its baseline (including a reset to a
    /// lower value) as "state moved".
    revisions: HashMap<(String, String), u64>,
}

/// The revision scope an entry belongs to: its session id, or `""` for a global
/// (session-less) slot. Shared by writes and the snapshot so they key alike.
fn scope_of(session_id: Option<&str>) -> String {
    session_id.unwrap_or("").to_string()
}

impl Inner {
    fn bump_revision(&mut self, plugin_id: &str, scope: String) {
        let rev = self
            .revisions
            .entry((plugin_id.to_string(), scope))
            .or_insert(0);
        *rev = rev.saturating_add(1);
    }

    /// The distinct scopes a plugin currently has entries in. Used to bump every
    /// affected pane's counter when a bulk clear drops a plugin's entries.
    fn plugin_scopes(&self, plugin_id: &str) -> HashSet<String> {
        self.entries
            .keys()
            .filter(|k| k.plugin_id == plugin_id)
            .map(|k| scope_of(k.session_id.as_deref()))
            .collect()
    }
}

/// The shared store. A `std::sync::RwLock` (not `tokio::Mutex`): writes happen
/// in the host's `spawn_blocking` dispatch and the web read just clones a small
/// snapshot, so neither side holds the lock across an `.await`.
pub struct UiStore {
    inner: RwLock<Inner>,
    next_generation: AtomicU64,
}

impl Default for UiStore {
    fn default() -> Self {
        Self::new()
    }
}

impl UiStore {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(Inner::default()),
            next_generation: AtomicU64::new(1),
        }
    }

    /// Register a freshly spawned worker for `plugin_id` and return its
    /// generation. The supervisor threads this into the worker's RPC context;
    /// every `ui.state.*` write carries it so a stale worker's writes are
    /// rejected.
    pub fn begin_generation(&self, plugin_id: &str) -> u64 {
        let gen = self.next_generation.fetch_add(1, Ordering::Relaxed);
        let mut inner = self.write();
        // A fresh worker starts from a clean slate: drop any entries the
        // previous generation left behind. This makes eviction robust against a
        // fast respawn (begin running before the exited worker's clear_plugin),
        // where clearing by the old generation would otherwise no-op and leave
        // its entries visible until the new worker happened to overwrite them.
        let scopes = inner.plugin_scopes(plugin_id);
        inner.entries.retain(|k, _| k.plugin_id != plugin_id);
        for scope in scopes {
            inner.bump_revision(plugin_id, scope);
        }
        inner.active.insert(plugin_id.to_string(), gen);
        gen
    }

    /// The mutation counter for one `(plugin_id, session)` scope, or 0 if it has
    /// none yet. The action endpoint reads this for the clicked pane's session
    /// before forwarding, so the client waits only for that pane's re-pushed
    /// state, not any update from the same plugin in another session.
    pub fn revision(&self, plugin_id: &str, session_id: Option<&str>) -> u64 {
        self.read()
            .revisions
            .get(&(plugin_id.to_string(), scope_of(session_id)))
            .copied()
            .unwrap_or(0)
    }

    /// Validate and store one entry. Rejects a stale generation, a payload that
    /// does not match the slot, a scope mismatch, or a plugin over quota.
    pub fn set(
        &self,
        plugin_id: &str,
        generation: u64,
        slot: UiSlot,
        id: &str,
        session_id: Option<&str>,
        payload: &Value,
    ) -> Result<(), UiError> {
        check_scope(slot, session_id)?;
        let normalized = validate_payload(slot, payload).map_err(UiError::BadRequest)?;
        if normalized.to_string().len() > max_payload_bytes(slot) {
            return Err(UiError::BadRequest("payload too large".into()));
        }
        let key = EntryKey {
            plugin_id: plugin_id.to_string(),
            slot,
            id: id.to_string(),
            session_id: session_id.map(str::to_string),
        };
        let mut inner = self.write();
        if inner.active.get(plugin_id) != Some(&generation) {
            return Err(UiError::StaleWorker);
        }
        if !inner.entries.contains_key(&key)
            && inner
                .entries
                .keys()
                .filter(|k| k.plugin_id == plugin_id)
                .count()
                >= MAX_ENTRIES_PER_PLUGIN
        {
            return Err(UiError::QuotaExceeded);
        }
        inner.entries.insert(key, normalized);
        inner.bump_revision(plugin_id, scope_of(session_id));
        Ok(())
    }

    /// Remove one entry. A remove of an absent entry is a no-op success, but a
    /// scope mismatch (a per-session slot without a `session_id`, or vice versa)
    /// is rejected, same as `set`, so a bad call is an error rather than a silent
    /// no-op that leaves the real entry standing.
    pub fn remove(
        &self,
        plugin_id: &str,
        generation: u64,
        slot: UiSlot,
        id: &str,
        session_id: Option<&str>,
    ) -> Result<(), UiError> {
        check_scope(slot, session_id)?;
        let key = EntryKey {
            plugin_id: plugin_id.to_string(),
            slot,
            id: id.to_string(),
            session_id: session_id.map(str::to_string),
        };
        let mut inner = self.write();
        if inner.active.get(plugin_id) != Some(&generation) {
            return Err(UiError::StaleWorker);
        }
        if inner.entries.remove(&key).is_some() {
            inner.bump_revision(plugin_id, scope_of(session_id));
        }
        Ok(())
    }

    /// Push a notification onto the shared ring and return its seq. No
    /// generation check: notifications outlive the worker that posted them.
    pub fn notify(
        &self,
        plugin_id: &str,
        tone: Tone,
        title: String,
        body: Option<String>,
        session_id: Option<String>,
    ) -> Result<u64, UiError> {
        if title.is_empty() {
            return Err(UiError::BadRequest("notification title is required".into()));
        }
        if title.len() > MAX_TITLE_LEN {
            return Err(UiError::BadRequest("notification title too long".into()));
        }
        if body.as_ref().is_some_and(|b| b.len() > MAX_BODY_LEN) {
            return Err(UiError::BadRequest("notification body too long".into()));
        }
        let mut inner = self.write();
        inner.notify_seq += 1;
        let seq = inner.notify_seq;
        inner.notifications.push_back(Notification {
            seq,
            plugin_id: plugin_id.to_string(),
            tone,
            title,
            body,
            session_id,
        });
        while inner.notifications.len() > NOTIFICATION_RING {
            inner.notifications.pop_front();
        }
        Ok(seq)
    }

    /// Clear a plugin's entries when its worker exits, but only if `generation`
    /// is still the active one. An instant respawn (which already called
    /// [`UiStore::begin_generation`]) leaves the new generation in place, so the
    /// old worker's exit does not wipe the new worker's state. Notifications are
    /// left untouched. Returns whether anything was cleared.
    pub fn clear_plugin(&self, plugin_id: &str, generation: u64) -> bool {
        let mut inner = self.write();
        if inner.active.get(plugin_id) != Some(&generation) {
            return false;
        }
        inner.active.remove(plugin_id);
        let scopes = inner.plugin_scopes(plugin_id);
        inner.entries.retain(|k, _| k.plugin_id != plugin_id);
        let changed = !scopes.is_empty();
        for scope in scopes {
            inner.bump_revision(plugin_id, scope);
        }
        changed
    }

    /// Clone the full state for the web to render.
    pub fn snapshot(&self) -> UiSnapshot {
        let inner = self.read();
        let mut entries: Vec<UiEntry> = inner
            .entries
            .iter()
            .map(|(k, payload)| UiEntry {
                plugin_id: k.plugin_id.clone(),
                slot: k.slot,
                id: k.id.clone(),
                session_id: k.session_id.clone(),
                payload: payload.clone(),
            })
            .collect();
        // Deterministic order so the snapshot does not jitter between polls.
        // `slot` is part of the key (a plugin may reuse one id across two slots),
        // so it is part of the sort key too, or those entries would compare equal
        // and fall back to HashMap iteration order.
        entries.sort_by(|a, b| {
            (&a.plugin_id, a.slot, &a.id, &a.session_id).cmp(&(
                &b.plugin_id,
                b.slot,
                &b.id,
                &b.session_id,
            ))
        });
        let mut revisions: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
        for ((plugin_id, scope), rev) in &inner.revisions {
            revisions
                .entry(plugin_id.clone())
                .or_default()
                .insert(scope.clone(), *rev);
        }
        UiSnapshot {
            entries,
            notifications: inner.notifications.iter().cloned().collect(),
            revisions,
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Inner> {
        self.inner.read().unwrap_or_else(|p| p.into_inner())
    }
    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Inner> {
        self.inner.write().unwrap_or_else(|p| p.into_inner())
    }
}

/// Per-slot payload ceiling. The pane carries lists (a full PR comment set), so
/// it gets a larger budget than the small single-value slots.
fn max_payload_bytes(slot: UiSlot) -> usize {
    match slot {
        UiSlot::Pane => MAX_PANE_PAYLOAD_BYTES,
        _ => MAX_PAYLOAD_BYTES,
    }
}

/// A per-session slot needs a `session_id`; a global slot must not carry one.
/// `Notification` is not a `ui.state.set` target (use `ui.notify`).
fn check_scope(slot: UiSlot, session_id: Option<&str>) -> Result<(), UiError> {
    if slot == UiSlot::Notification {
        return Err(UiError::BadRequest(
            "notification is pushed via ui.notify, not ui.state.set".into(),
        ));
    }
    match (slot.is_per_session(), session_id.is_some()) {
        (true, false) => Err(UiError::BadRequest(format!(
            "slot {slot:?} requires a session_id"
        ))),
        (false, true) => Err(UiError::BadRequest(format!(
            "slot {slot:?} is global and must not carry a session_id"
        ))),
        _ => Ok(()),
    }
}

/// Validate `raw` against the slot's typed payload and return the normalized
/// JSON (re-serialized from the parsed struct, so unknown fields are rejected
/// and the stored shape is canonical).
fn validate_payload(slot: UiSlot, raw: &Value) -> Result<Value, String> {
    fn normalize<T: serde::de::DeserializeOwned + Serialize>(raw: &Value) -> Result<Value, String> {
        let parsed: T = serde_json::from_value(raw.clone()).map_err(|e| e.to_string())?;
        serde_json::to_value(parsed).map_err(|e| e.to_string())
    }
    match slot {
        UiSlot::StatusBar | UiSlot::DetailBadge => normalize::<TextPayload>(raw),
        UiSlot::RowBadge => normalize::<RowBadgePayload>(raw),
        UiSlot::RowColumn => normalize::<RowColumnPayload>(raw),
        UiSlot::SortKey => normalize::<SortKeyPayload>(raw),
        UiSlot::FilterFacet => normalize::<FilterFacetPayload>(raw),
        UiSlot::Card => normalize::<CardPayload>(raw),
        UiSlot::Pane => normalize::<PanePayload>(raw),
        UiSlot::Notification => Err("notification is pushed via ui.notify".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store() -> UiStore {
        UiStore::new()
    }

    #[test]
    fn set_get_and_remove_global_entry() {
        let s = store();
        let g = s.begin_generation("acme.kit");
        s.set(
            "acme.kit",
            g,
            UiSlot::StatusBar,
            "build",
            None,
            &json!({"text": "ok", "tone": "success"}),
        )
        .unwrap();
        let snap = s.snapshot();
        assert_eq!(snap.entries.len(), 1);
        assert_eq!(snap.entries[0].slot, UiSlot::StatusBar);
        assert_eq!(snap.entries[0].payload["text"], json!("ok"));

        s.remove("acme.kit", g, UiSlot::StatusBar, "build", None)
            .unwrap();
        assert_eq!(s.snapshot().entries.len(), 0);
    }

    #[test]
    fn scope_rules_enforced() {
        let s = store();
        let g = s.begin_generation("acme.kit");
        // Global slot must not carry a session_id.
        assert!(matches!(
            s.set(
                "acme.kit",
                g,
                UiSlot::StatusBar,
                "x",
                Some("s1"),
                &json!({"text": "hi"})
            ),
            Err(UiError::BadRequest(_))
        ));
        // Per-session slot requires a session_id.
        assert!(matches!(
            s.set(
                "acme.kit",
                g,
                UiSlot::RowBadge,
                "x",
                None,
                &json!({"text": "hi"})
            ),
            Err(UiError::BadRequest(_))
        ));
        // Notification is not a ui.state.set target.
        assert!(matches!(
            s.set(
                "acme.kit",
                g,
                UiSlot::Notification,
                "x",
                None,
                &json!({"text": "hi"})
            ),
            Err(UiError::BadRequest(_))
        ));
        // remove enforces the same scope rules, so a wrong-scope remove is a
        // rejection rather than a silent no-op that leaves the entry standing.
        assert!(matches!(
            s.remove("acme.kit", g, UiSlot::RowBadge, "x", None),
            Err(UiError::BadRequest(_))
        ));
    }

    #[test]
    fn malformed_payload_rejected() {
        let s = store();
        let g = s.begin_generation("acme.kit");
        // Missing required `text` on a text slot (status-bar still requires it).
        assert!(matches!(
            s.set(
                "acme.kit",
                g,
                UiSlot::StatusBar,
                "b",
                None,
                &json!({"tone": "info"})
            ),
            Err(UiError::BadRequest(_))
        ));
        // Unknown field rejected (deny_unknown_fields).
        assert!(matches!(
            s.set(
                "acme.kit",
                g,
                UiSlot::RowBadge,
                "b",
                Some("s1"),
                &json!({"text": "x", "bogus": 1})
            ),
            Err(UiError::BadRequest(_))
        ));
        // Bad tone value rejected.
        assert!(matches!(
            s.set(
                "acme.kit",
                g,
                UiSlot::RowBadge,
                "b",
                Some("s1"),
                &json!({"text": "x", "tone": "rainbow"})
            ),
            Err(UiError::BadRequest(_))
        ));
    }

    #[test]
    fn stale_generation_rejected_and_clear_is_generation_guarded() {
        let s = store();
        let g1 = s.begin_generation("acme.kit");
        s.set(
            "acme.kit",
            g1,
            UiSlot::Card,
            "c",
            None,
            &json!({"title": "Hi"}),
        )
        .unwrap();
        assert_eq!(s.snapshot().entries.len(), 1);
        // Worker respawns: starting the new generation evicts the old
        // generation's entries up front, so no stale state survives even when
        // begin runs before the exited worker's clear_plugin.
        let g2 = s.begin_generation("acme.kit");
        assert_eq!(s.snapshot().entries.len(), 0);
        // A late write from the old generation is rejected, not applied.
        assert_eq!(
            s.set(
                "acme.kit",
                g1,
                UiSlot::Card,
                "c2",
                None,
                &json!({"title": "stale"})
            ),
            Err(UiError::StaleWorker)
        );
        // The old worker's exit must NOT wipe the live g2 state.
        assert!(!s.clear_plugin("acme.kit", g1));
        // The current generation can write and be cleared.
        s.set(
            "acme.kit",
            g2,
            UiSlot::Card,
            "c3",
            None,
            &json!({"title": "new"}),
        )
        .unwrap();
        assert!(s.clear_plugin("acme.kit", g2));
        assert_eq!(s.snapshot().entries.len(), 0);
    }

    #[test]
    fn notifications_survive_clear_and_carry_monotonic_seq() {
        let s = store();
        let g = s.begin_generation("acme.kit");
        s.set(
            "acme.kit",
            g,
            UiSlot::StatusBar,
            "x",
            None,
            &json!({"text": "hi"}),
        )
        .unwrap();
        let seq1 = s
            .notify("acme.kit", Tone::Danger, "Build failed".into(), None, None)
            .unwrap();
        let seq2 = s
            .notify(
                "acme.kit",
                Tone::Info,
                "Done".into(),
                Some("see log".into()),
                Some("s1".into()),
            )
            .unwrap();
        assert!(seq2 > seq1);
        // Clearing entries on worker exit leaves notifications in place.
        s.clear_plugin("acme.kit", g);
        let snap = s.snapshot();
        assert_eq!(snap.entries.len(), 0);
        assert_eq!(snap.notifications.len(), 2);
        assert_eq!(snap.notifications[1].session_id.as_deref(), Some("s1"));
    }

    #[test]
    fn empty_notification_title_rejected() {
        let s = store();
        assert!(matches!(
            s.notify("acme.kit", Tone::Info, String::new(), None, None),
            Err(UiError::BadRequest(_))
        ));
    }

    #[test]
    fn row_badge_accepts_items_list() {
        let s = store();
        let g = s.begin_generation("acme.kit");
        s.set(
            "acme.kit",
            g,
            UiSlot::RowBadge,
            "repos",
            Some("s1"),
            &json!({"items": [
                {"icon": "git-pull-request-arrow", "tone": "success", "href": "https://x/pr/1", "tooltip": "PR #1"},
                {"icon": "git-pull-request-draft", "tone": "warn"}
            ]}),
        )
        .unwrap();
        let snap = s.snapshot();
        assert_eq!(
            snap.entries[0].payload["items"].as_array().unwrap().len(),
            2
        );
        // Empty items is valid (clears the row).
        s.set(
            "acme.kit",
            g,
            UiSlot::RowBadge,
            "repos",
            Some("s1"),
            &json!({"items": []}),
        )
        .unwrap();
        // A bad tone inside an item is still rejected.
        assert!(matches!(
            s.set(
                "acme.kit",
                g,
                UiSlot::RowBadge,
                "repos",
                Some("s1"),
                &json!({"items": [{"tone": "rainbow"}]})
            ),
            Err(UiError::BadRequest(_))
        ));
    }

    #[test]
    fn pane_blocks_are_forward_compatible() {
        let s = store();
        let g = s.begin_generation("acme.kit");
        // A mix of known kinds and an unknown kind: the unknown one is accepted
        // and stored verbatim, not rejected, so an old host renders what it knows.
        s.set(
            "acme.kit",
            g,
            UiSlot::Pane,
            "gh",
            Some("s1"),
            &json!({"title": "GitHub", "default_location": "bottom", "blocks": [
                {"kind": "heading", "text": "GitHub"},
                {"kind": "row", "label": "nexus", "value": "PR #12", "href": "https://x/pr/12"},
                {"kind": "divider"},
                {"kind": "some-future-kind", "whatever": {"nested": true}}
            ]}),
        )
        .unwrap();
        let snap = s.snapshot();
        let blocks = snap.entries[0].payload["blocks"].as_array().unwrap();
        assert_eq!(blocks.len(), 4);
        assert_eq!(blocks[3]["kind"], json!("some-future-kind"));
        assert_eq!(snap.entries[0].payload["default_location"], json!("bottom"));
        // The simple title/body form still works, and default_location is optional.
        s.set(
            "acme.kit",
            g,
            UiSlot::Pane,
            "gh",
            Some("s1"),
            &json!({"title": "T", "body": "B"}),
        )
        .unwrap();
    }

    #[test]
    fn pane_rejects_unknown_default_location() {
        let s = store();
        let g = s.begin_generation("acme.kit");
        assert!(matches!(
            s.set(
                "acme.kit",
                g,
                UiSlot::Pane,
                "gh",
                Some("s1"),
                &json!({"default_location": "sideways"})
            ),
            Err(UiError::BadRequest(_))
        ));
    }

    #[test]
    fn pane_payload_cap_is_larger_than_other_slots() {
        let s = store();
        let g = s.begin_generation("acme.kit");
        // A pane body that would blow the 8KB badge cap but fits the 64KB pane
        // cap: a long comment list. ~40KB of note text well over MAX_PAYLOAD_BYTES.
        let big = "x".repeat(40 * 1024);
        s.set(
            "acme.kit",
            g,
            UiSlot::Pane,
            "gh",
            Some("s1"),
            &json!({"blocks": [{"kind": "note", "text": big}]}),
        )
        .unwrap();
        // Past the pane cap is still rejected.
        let too_big = "x".repeat(64 * 1024);
        assert!(matches!(
            s.set(
                "acme.kit",
                g,
                UiSlot::Pane,
                "gh",
                Some("s1"),
                &json!({"blocks": [{"kind": "note", "text": too_big}]})
            ),
            Err(UiError::BadRequest(_))
        ));
        // A non-pane slot keeps the small 8KB cap.
        let over_badge = "x".repeat(9 * 1024);
        assert!(matches!(
            s.set(
                "acme.kit",
                g,
                UiSlot::RowBadge,
                "b",
                Some("s1"),
                &json!({"text": over_badge})
            ),
            Err(UiError::BadRequest(_))
        ));
    }

    #[test]
    fn per_plugin_quota_enforced() {
        let s = store();
        let g = s.begin_generation("acme.kit");
        for i in 0..MAX_ENTRIES_PER_PLUGIN {
            s.set(
                "acme.kit",
                g,
                UiSlot::Card,
                &format!("c{i}"),
                None,
                &json!({"title": "x"}),
            )
            .unwrap();
        }
        assert_eq!(
            s.set(
                "acme.kit",
                g,
                UiSlot::Card,
                "overflow",
                None,
                &json!({"title": "x"})
            ),
            Err(UiError::QuotaExceeded)
        );
        // Updating an existing entry is not blocked by the quota.
        s.set(
            "acme.kit",
            g,
            UiSlot::Card,
            "c0",
            None,
            &json!({"title": "y"}),
        )
        .unwrap();
    }

    #[test]
    fn revision_bumps_on_mutation_and_surfaces_in_snapshot() {
        let s = store();
        // Absent until the plugin first mutates state (global scope here).
        assert_eq!(s.revision("acme.kit", None), 0);
        let g = s.begin_generation("acme.kit");

        s.set(
            "acme.kit",
            g,
            UiSlot::Card,
            "c0",
            None,
            &json!({"title": "x"}),
        )
        .unwrap();
        assert_eq!(s.revision("acme.kit", None), 1);

        // An identical re-push still bumps: a refresh that returns unchanged
        // data must still move the counter, or a waiting spinner would hang.
        s.set(
            "acme.kit",
            g,
            UiSlot::Card,
            "c0",
            None,
            &json!({"title": "x"}),
        )
        .unwrap();
        assert_eq!(s.revision("acme.kit", None), 2);

        // Removing a present entry bumps; removing an absent one does not.
        s.remove("acme.kit", g, UiSlot::Card, "c0", None).unwrap();
        assert_eq!(s.revision("acme.kit", None), 3);
        s.remove("acme.kit", g, UiSlot::Card, "gone", None).unwrap();
        assert_eq!(s.revision("acme.kit", None), 3);

        // The counter is exposed in the polled snapshot, keyed plugin -> scope.
        let snap = s.snapshot();
        assert_eq!(
            snap.revisions.get("acme.kit").and_then(|m| m.get("")),
            Some(&3)
        );
        assert_eq!(snap.revisions.get("other.kit"), None);
    }

    #[test]
    fn revision_is_scoped_per_session() {
        let s = store();
        let g = s.begin_generation("acme.kit");
        // A pane push for session s1 bumps only s1's scope.
        s.set(
            "acme.kit",
            g,
            UiSlot::Pane,
            "p",
            Some("s1"),
            &json!({"title": "a"}),
        )
        .unwrap();
        assert_eq!(s.revision("acme.kit", Some("s1")), 1);
        assert_eq!(s.revision("acme.kit", Some("s2")), 0);

        // A push for an unrelated session must not move s1's counter, so s1's
        // refresh spinner cannot be cleared by s2's activity.
        s.set(
            "acme.kit",
            g,
            UiSlot::Pane,
            "p",
            Some("s2"),
            &json!({"title": "b"}),
        )
        .unwrap();
        assert_eq!(s.revision("acme.kit", Some("s1")), 1);
        assert_eq!(s.revision("acme.kit", Some("s2")), 1);

        // A bulk clear bumps every scope the plugin had entries in.
        s.clear_plugin("acme.kit", g);
        assert_eq!(s.revision("acme.kit", Some("s1")), 2);
        assert_eq!(s.revision("acme.kit", Some("s2")), 2);
    }
}
