//! Host-owned store for state pushed through plugin UI RPCs.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

use aoe_plugin_api::UiSlot;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_ENTRIES_PER_SCOPE: usize = 32;
const MAX_ENTRIES_PER_PLUGIN: usize = 1024;
const MAX_PAYLOAD_BYTES: usize = 8 * 1024;
const MAX_PANE_PAYLOAD_BYTES: usize = 64 * 1024;
const MAX_COMPOSER_DRAFT_TEXT_BYTES: usize = 16 * 1024;
const MAX_COMPOSER_ACTION_PAYLOAD_BYTES: usize = 20 * 1024;
const NOTIFICATION_RING: usize = 200;
const MAX_TITLE_LEN: usize = 256;
const MAX_BODY_LEN: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tone {
    Neutral,
    Info,
    Success,
    Warn,
    Danger,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SortDirection {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SortValue {
    Number(f64),
    String(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FacetOption {
    pub value: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tone: Option<Tone>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TextPayload {
    text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tone: Option<Tone>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tooltip: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    icon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    href: Option<String>,
}

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sort_value: Option<SortValue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    filter_values: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SortKeyPayload {
    label: String,
    column: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    direction: Option<SortDirection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilterFacetPayload {
    label: String,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PaneLocation {
    Right,
    Bottom,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PaneFooter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    icon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tone: Option<Tone>,
}

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    icon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    footer: Option<PaneFooter>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposerActionPayload {
    label: String,
    method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    icon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tone: Option<Tone>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tooltip: Option<String>,
    #[serde(default)]
    disabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    draft_operation: Option<ComposerDraftOperation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum ComposerDraftOperation {
    InsertText { id: String, text: String },
    ReplaceSelection { id: String, text: String },
    SetText { id: String, text: String },
}

impl ComposerDraftOperation {
    fn valid(&self) -> bool {
        match self {
            ComposerDraftOperation::InsertText { id, text }
            | ComposerDraftOperation::ReplaceSelection { id, text }
            | ComposerDraftOperation::SetText { id, text } => {
                !id.is_empty() && id.len() <= 128 && text.len() <= MAX_COMPOSER_DRAFT_TEXT_BYTES
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum UiError {
    StaleWorker,
    QuotaExceeded,
    BadRequest(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct EntryKey {
    plugin_id: String,
    slot: UiSlot,
    id: String,
    session_id: Option<String>,
}

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub href: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiEntry {
    pub plugin_id: String,
    pub slot: UiSlot,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub payload: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UiSnapshot {
    pub entries: Vec<UiEntry>,
    pub notifications: Vec<Notification>,
    #[serde(default)]
    pub revisions: BTreeMap<String, BTreeMap<String, u64>>,
}

fn push_link(
    out: &mut Vec<(String, String)>,
    seen: &mut HashSet<String>,
    href: Option<&Value>,
    label: Option<&Value>,
) {
    let Some(href) = href.and_then(Value::as_str) else {
        return;
    };
    if !crate::util::is_http_url(href) || !seen.insert(href.to_string()) {
        return;
    }
    let label = label
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(href)
        .to_string();
    out.push((href.to_string(), label));
}

impl UiSnapshot {
    pub fn links_for(
        &self,
        plugin_id: &str,
        slot: UiSlot,
        id: &str,
        session_id: &str,
    ) -> Vec<(String, String)> {
        let Some(entry) = self.entries.iter().find(|e| {
            e.plugin_id == plugin_id
                && e.slot == slot
                && e.id == id
                && e.session_id.as_deref() == Some(session_id)
        }) else {
            return Vec::new();
        };
        let mut out: Vec<(String, String)> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        if let Some(items) = entry.payload.get("items").and_then(Value::as_array) {
            for raw in items {
                push_link(
                    &mut out,
                    &mut seen,
                    raw.get("href"),
                    raw.get("tooltip").or_else(|| raw.get("text")),
                );
            }
        }
        if out.is_empty() {
            push_link(
                &mut out,
                &mut seen,
                entry.payload.get("href"),
                entry
                    .payload
                    .get("tooltip")
                    .or_else(|| entry.payload.get("text")),
            );
        }
        out
    }
}

#[derive(Default)]
struct Inner {
    entries: HashMap<EntryKey, Value>,
    active: HashMap<String, u64>,
    notifications: VecDeque<Notification>,
    notify_seq: u64,
    revisions: HashMap<(String, String), u64>,
}

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

    fn plugin_scopes(&self, plugin_id: &str) -> HashSet<String> {
        self.entries
            .keys()
            .filter(|k| k.plugin_id == plugin_id)
            .map(|k| scope_of(k.session_id.as_deref()))
            .collect()
    }
}

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

    pub fn begin_generation(&self, plugin_id: &str) -> u64 {
        let gen = self.next_generation.fetch_add(1, Ordering::Relaxed);
        let mut inner = self.write();
        let scopes = inner.plugin_scopes(plugin_id);
        inner.entries.retain(|k, _| k.plugin_id != plugin_id);
        for scope in scopes {
            inner.bump_revision(plugin_id, scope);
        }
        inner.active.insert(plugin_id.to_string(), gen);
        gen
    }

    pub fn revision(&self, plugin_id: &str, session_id: Option<&str>) -> u64 {
        self.read()
            .revisions
            .get(&(plugin_id.to_string(), scope_of(session_id)))
            .copied()
            .unwrap_or(0)
    }

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
        if !inner.entries.contains_key(&key) {
            let scope = scope_of(session_id);
            let mut plugin_entries = 0usize;
            let mut scope_entries = 0usize;
            for existing in inner.entries.keys().filter(|k| k.plugin_id == plugin_id) {
                plugin_entries += 1;
                if scope_of(existing.session_id.as_deref()) == scope {
                    scope_entries += 1;
                }
            }
            if scope_entries >= MAX_ENTRIES_PER_SCOPE || plugin_entries >= MAX_ENTRIES_PER_PLUGIN {
                return Err(UiError::QuotaExceeded);
            }
        }
        inner.entries.insert(key, normalized);
        inner.bump_revision(plugin_id, scope_of(session_id));
        Ok(())
    }

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

    pub fn notify(
        &self,
        plugin_id: &str,
        tone: Tone,
        title: String,
        body: Option<String>,
        session_id: Option<String>,
        href: Option<String>,
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
        if let Some(href) = &href {
            if !crate::util::is_http_url(href) {
                return Err(UiError::BadRequest(
                    "notification href must be http/https".into(),
                ));
            }
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
            href,
        });
        while inner.notifications.len() > NOTIFICATION_RING {
            inner.notifications.pop_front();
        }
        Ok(seq)
    }

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

fn max_payload_bytes(slot: UiSlot) -> usize {
    match slot {
        UiSlot::Pane | UiSlot::HomePane => MAX_PANE_PAYLOAD_BYTES,
        UiSlot::ComposerAction => MAX_COMPOSER_ACTION_PAYLOAD_BYTES,
        _ => MAX_PAYLOAD_BYTES,
    }
}

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

const MAX_BLOCK_DEPTH: usize = 16;

fn check_block_depth(blocks: Option<&[Value]>) -> Result<(), String> {
    fn depth_ok(blocks: &[Value], remaining: usize) -> bool {
        if remaining == 0 {
            return blocks.is_empty();
        }
        blocks
            .iter()
            .all(|b| match b.get("children").and_then(Value::as_array) {
                Some(children) => depth_ok(children, remaining - 1),
                None => true,
            })
    }
    match blocks {
        Some(blocks) if !depth_ok(blocks, MAX_BLOCK_DEPTH) => {
            Err(format!("pane blocks nest deeper than {MAX_BLOCK_DEPTH}"))
        }
        _ => Ok(()),
    }
}

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
        UiSlot::Pane | UiSlot::HomePane => {
            let parsed: PanePayload =
                serde_json::from_value(raw.clone()).map_err(|e| e.to_string())?;
            check_block_depth(parsed.blocks.as_deref())?;
            serde_json::to_value(parsed).map_err(|e| e.to_string())
        }
        UiSlot::ComposerAction => {
            let parsed: ComposerActionPayload =
                serde_json::from_value(raw.clone()).map_err(|e| e.to_string())?;
            if parsed.label.is_empty() {
                return Err("composer action label is required".into());
            }
            if parsed.method.is_empty() {
                return Err("composer action method is required".into());
            }
            if parsed
                .draft_operation
                .as_ref()
                .is_some_and(|op| !op.valid())
            {
                return Err("composer draft operation requires a bounded id and text".into());
            }
            serde_json::to_value(parsed).map_err(|e| e.to_string())
        }
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

    fn set(
        store: &UiStore,
        generation: u64,
        slot: UiSlot,
        id: &str,
        session_id: Option<&str>,
        payload: Value,
    ) -> Result<(), UiError> {
        store.set("acme.kit", generation, slot, id, session_id, &payload)
    }

    fn entry(session_id: &str, payload: Value) -> UiEntry {
        UiEntry {
            plugin_id: "acme.gh".into(),
            slot: UiSlot::DetailBadge,
            id: "pr".into(),
            session_id: Some(session_id.into()),
            payload,
        }
    }

    #[test]
    fn links_for_reads_items_then_falls_back_to_top_level_href() {
        let snap = UiSnapshot {
            entries: vec![entry(
                "s1",
                json!({
                    "items": [
                        {"href": "https://example.com/pr/1", "tooltip": "PR 1"},
                        {"href": "https://example.com/pr/1", "text": "dup"},
                        {"href": "javascript:alert(1)", "text": "evil"},
                        {"href": "https://example.com/pr/2", "text": "PR 2"},
                    ]
                }),
            )],
            notifications: vec![],
            revisions: BTreeMap::new(),
        };
        assert_eq!(
            snap.links_for("acme.gh", UiSlot::DetailBadge, "pr", "s1"),
            vec![
                ("https://example.com/pr/1".to_string(), "PR 1".to_string()),
                ("https://example.com/pr/2".to_string(), "PR 2".to_string()),
            ]
        );

        let snap = UiSnapshot {
            entries: vec![entry("s1", json!({"href": "https://example.com/pr/9"}))],
            notifications: vec![],
            revisions: BTreeMap::new(),
        };
        assert_eq!(
            snap.links_for("acme.gh", UiSlot::DetailBadge, "pr", "s1"),
            vec![(
                "https://example.com/pr/9".to_string(),
                "https://example.com/pr/9".to_string()
            )]
        );

        assert!(snap
            .links_for("acme.gh", UiSlot::DetailBadge, "pr", "other")
            .is_empty());
        let snap = UiSnapshot {
            entries: vec![entry("s1", json!({"href": "file:///etc/passwd"}))],
            notifications: vec![],
            revisions: BTreeMap::new(),
        };
        assert!(snap
            .links_for("acme.gh", UiSlot::DetailBadge, "pr", "s1")
            .is_empty());
    }

    #[test]
    fn set_get_and_remove_global_entry() {
        let s = store();
        let g = s.begin_generation("acme.kit");
        set(
            &s,
            g,
            UiSlot::StatusBar,
            "build",
            None,
            json!({"text": "ok", "tone": "success"}),
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
    fn set_enforces_scope_rules_and_payload_shape() {
        let s = store();
        let g = s.begin_generation("acme.kit");
        let composer = |draft: Value| json!({"label": "Voice", "method": "voice.start", "draft_operation": draft});
        let cases: Vec<(&str, UiSlot, Option<&str>, Value, bool)> = vec![
            (
                "status bar is global only",
                UiSlot::StatusBar,
                Some("s1"),
                json!({"text": "hi"}),
                false,
            ),
            (
                "row badge is session scoped",
                UiSlot::RowBadge,
                None,
                json!({"text": "hi"}),
                false,
            ),
            (
                "composer action is session scoped",
                UiSlot::ComposerAction,
                None,
                json!({"label": "Voice", "method": "voice.start"}),
                false,
            ),
            (
                "home pane is global only",
                UiSlot::HomePane,
                Some("s1"),
                json!({"title": "memory"}),
                false,
            ),
            (
                "home pane accepts blocks",
                UiSlot::HomePane,
                None,
                json!({"title": "memory", "blocks": [{"kind": "sparkline", "values": [1, 2]}]}),
                true,
            ),
            (
                "notification is not a stored slot",
                UiSlot::Notification,
                None,
                json!({"text": "hi"}),
                false,
            ),
            (
                "status bar requires text",
                UiSlot::StatusBar,
                None,
                json!({"tone": "info"}),
                false,
            ),
            (
                "unknown field rejected",
                UiSlot::RowBadge,
                Some("s1"),
                json!({"text": "x", "bogus": 1}),
                false,
            ),
            (
                "unknown tone rejected",
                UiSlot::RowBadge,
                Some("s1"),
                json!({"text": "x", "tone": "rainbow"}),
                false,
            ),
            (
                "draft operation needs an id",
                UiSlot::ComposerAction,
                Some("s1"),
                composer(json!({"kind": "insert-text", "id": "", "text": "hello"})),
                false,
            ),
            (
                "insert-text draft accepted",
                UiSlot::ComposerAction,
                Some("s1"),
                json!({
                    "label": "Voice",
                    "method": "voice.start",
                    "icon": "mic",
                    "draft_operation": {"kind": "insert-text", "id": "op-1", "text": "hello"}
                }),
                true,
            ),
            (
                "set-text draft may be empty",
                UiSlot::ComposerAction,
                Some("s1"),
                composer(json!({"kind": "set-text", "id": "op-2", "text": ""})),
                true,
            ),
            (
                "draft text may exceed the slot payload cap",
                UiSlot::ComposerAction,
                Some("s1"),
                composer(json!({
                    "kind": "insert-text",
                    "id": "op-3",
                    "text": "x".repeat(MAX_PAYLOAD_BYTES + 512)
                })),
                true,
            ),
            (
                "draft text beyond its own cap rejected",
                UiSlot::ComposerAction,
                Some("s1"),
                composer(json!({
                    "kind": "insert-text",
                    "id": "op-4",
                    "text": "x".repeat(MAX_COMPOSER_DRAFT_TEXT_BYTES + 1)
                })),
                false,
            ),
        ];
        for (name, slot, session, payload, ok) in cases {
            let result = set(&s, g, slot, "x", session, payload);
            assert_eq!(result.is_ok(), ok, "{name}: {result:?}");
        }
        assert!(matches!(
            s.remove("acme.kit", g, UiSlot::RowBadge, "x", None),
            Err(UiError::BadRequest(_))
        ));
    }

    #[test]
    fn stale_generation_rejected_and_clear_is_generation_guarded() {
        let s = store();
        let g1 = s.begin_generation("acme.kit");
        set(&s, g1, UiSlot::Card, "c", None, json!({"title": "Hi"})).unwrap();
        assert_eq!(s.snapshot().entries.len(), 1);
        let g2 = s.begin_generation("acme.kit");
        assert_eq!(s.snapshot().entries.len(), 0);
        assert_eq!(
            set(&s, g1, UiSlot::Card, "c2", None, json!({"title": "stale"})),
            Err(UiError::StaleWorker)
        );
        assert!(!s.clear_plugin("acme.kit", g1));
        set(&s, g2, UiSlot::Card, "c3", None, json!({"title": "new"})).unwrap();
        assert!(s.clear_plugin("acme.kit", g2));
        assert_eq!(s.snapshot().entries.len(), 0);
    }

    #[test]
    fn notifications_survive_clear_and_carry_monotonic_seq() {
        let s = store();
        let g = s.begin_generation("acme.kit");
        set(&s, g, UiSlot::StatusBar, "x", None, json!({"text": "hi"})).unwrap();
        let seq1 = s
            .notify(
                "acme.kit",
                Tone::Danger,
                "Build failed".into(),
                None,
                None,
                None,
            )
            .unwrap();
        let seq2 = s
            .notify(
                "acme.kit",
                Tone::Info,
                "Done".into(),
                Some("see log".into()),
                Some("s1".into()),
                None,
            )
            .unwrap();
        assert!(seq2 > seq1);
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
            s.notify("acme.kit", Tone::Info, String::new(), None, None, None),
            Err(UiError::BadRequest(_))
        ));
    }

    #[test]
    fn notify_rejects_non_http_href() {
        let s = store();
        assert!(matches!(
            s.notify(
                "acme.kit",
                Tone::Info,
                "Open".into(),
                None,
                None,
                Some("javascript:alert(1)".into()),
            ),
            Err(UiError::BadRequest(_))
        ));
        let seq = s
            .notify(
                "acme.kit",
                Tone::Info,
                "Open".into(),
                None,
                None,
                Some("https://example.com".into()),
            )
            .unwrap();
        assert_eq!(
            s.snapshot().notifications[0].href.as_deref(),
            Some("https://example.com")
        );
        assert_eq!(seq, 1);
    }

    #[test]
    fn row_badge_accepts_items_list() {
        let s = store();
        let g = s.begin_generation("acme.kit");
        set(&s, g, UiSlot::RowBadge, "repos", Some("s1"), json!({"items": [
                {"icon": "git-pull-request-arrow", "tone": "success", "href": "https://x/pr/1", "tooltip": "PR #1"},
                {"icon": "git-pull-request-draft", "tone": "warn"}
            ]}))
        .unwrap();
        let snap = s.snapshot();
        assert_eq!(
            snap.entries[0].payload["items"].as_array().unwrap().len(),
            2
        );
        set(
            &s,
            g,
            UiSlot::RowBadge,
            "repos",
            Some("s1"),
            json!({"items": []}),
        )
        .unwrap();
        assert!(matches!(
            set(
                &s,
                g,
                UiSlot::RowBadge,
                "repos",
                Some("s1"),
                json!({"items": [{"tone": "rainbow"}]})
            ),
            Err(UiError::BadRequest(_))
        ));
    }

    #[test]
    fn pane_blocks_are_forward_compatible() {
        let s = store();
        let g = s.begin_generation("acme.kit");
        set(
            &s,
            g,
            UiSlot::Pane,
            "gh",
            Some("s1"),
            json!({"title": "GitHub", "default_location": "bottom", "blocks": [
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
        set(
            &s,
            g,
            UiSlot::Pane,
            "gh",
            Some("s1"),
            json!({"title": "T", "body": "B"}),
        )
        .unwrap();
    }

    #[test]
    fn pane_rejects_blocks_nested_past_the_depth_cap() {
        let s = store();
        let g = s.begin_generation("acme.kit");
        let chain = |depth: usize| {
            let mut block = json!({"kind": "row", "label": "leaf"});
            for _ in 0..depth {
                block = json!({"kind": "section", "children": [block]});
            }
            json!({"blocks": [block]})
        };
        set(
            &s,
            g,
            UiSlot::Pane,
            "gh",
            Some("s1"),
            chain(MAX_BLOCK_DEPTH - 1),
        )
        .unwrap();
        assert!(matches!(
            set(
                &s,
                g,
                UiSlot::Pane,
                "gh",
                Some("s1"),
                chain(MAX_BLOCK_DEPTH + 1)
            ),
            Err(UiError::BadRequest(_))
        ));
        let mut inert = json!("leaf");
        for _ in 0..64 {
            inert = json!({"nested": inert});
        }
        set(
            &s,
            g,
            UiSlot::Pane,
            "gh",
            Some("s1"),
            json!({"blocks": [{"kind": "some-future-kind", "payload": inert}]}),
        )
        .unwrap();
    }

    #[test]
    fn pane_footer_round_trips_and_rejects_unknown_fields() {
        let s = store();
        let g = s.begin_generation("acme.kit");
        set(&s, g, UiSlot::Pane, "gh", Some("s1"), json!({"blocks": [{"kind": "heading", "text": "GitHub"}],
                    "footer": {"text": "refreshed 12:07", "value": "blocked", "tone": "danger", "icon": "refresh-cw"}}))
        .unwrap();
        let snap = s.snapshot();
        assert_eq!(snap.entries[0].payload["footer"]["value"], json!("blocked"));
        assert_eq!(snap.entries[0].payload["footer"]["tone"], json!("danger"));
        assert!(matches!(
            set(
                &s,
                g,
                UiSlot::Pane,
                "gh",
                Some("s1"),
                json!({"footer": {"txt": "oops"}})
            ),
            Err(UiError::BadRequest(_))
        ));
    }

    #[test]
    fn pane_rejects_unknown_default_location() {
        let s = store();
        let g = s.begin_generation("acme.kit");
        assert!(matches!(
            set(
                &s,
                g,
                UiSlot::Pane,
                "gh",
                Some("s1"),
                json!({"default_location": "sideways"})
            ),
            Err(UiError::BadRequest(_))
        ));
    }

    #[test]
    fn pane_payload_cap_is_larger_than_other_slots() {
        let s = store();
        let g = s.begin_generation("acme.kit");
        let pane = |text: String| {
            set(
                &s,
                g,
                UiSlot::Pane,
                "gh",
                Some("s1"),
                json!({"blocks": [{"kind": "note", "text": text}]}),
            )
        };

        pane("x".repeat(40 * 1024)).expect("40 KiB fits in a pane");
        assert!(matches!(
            pane("x".repeat(64 * 1024)),
            Err(UiError::BadRequest(_))
        ));
        assert!(matches!(
            set(
                &s,
                g,
                UiSlot::RowBadge,
                "b",
                Some("s1"),
                json!({"text": "x".repeat(9 * 1024)})
            ),
            Err(UiError::BadRequest(_))
        ));
    }

    #[test]
    fn per_scope_quota_blocks_only_that_scope_and_frees_on_remove() {
        let s = store();
        let g = s.begin_generation("acme.kit");
        let badge = |id: &str, session: &str| {
            set(
                &s,
                g,
                UiSlot::RowBadge,
                id,
                Some(session),
                json!({"text": "x"}),
            )
        };

        for i in 0..MAX_ENTRIES_PER_SCOPE {
            badge(&format!("b{i}"), "s1").unwrap();
        }
        assert_eq!(badge("overflow", "s1"), Err(UiError::QuotaExceeded));
        badge("b0", "s2").expect("another scope has its own budget");
        badge("b0", "s1").expect("rewriting an existing key is not a new entry");

        s.remove("acme.kit", g, UiSlot::RowBadge, "b0", Some("s1"))
            .unwrap();
        badge("replacement", "s1").expect("removing an entry frees its scope slot");
    }

    #[test]
    fn per_plugin_backstop_bounds_fabricated_scopes() {
        let s = store();
        let g = s.begin_generation("acme.kit");
        let badge = |session: &str| {
            set(
                &s,
                g,
                UiSlot::RowBadge,
                "b",
                Some(session),
                json!({"text": "x"}),
            )
        };

        for i in 0..MAX_ENTRIES_PER_PLUGIN {
            badge(&format!("s{i}")).unwrap();
        }
        assert_eq!(badge("overflow"), Err(UiError::QuotaExceeded));
    }

    #[test]
    fn revision_bumps_on_mutation_and_surfaces_in_snapshot() {
        let s = store();
        assert_eq!(s.revision("acme.kit", None), 0);
        let g = s.begin_generation("acme.kit");

        set(&s, g, UiSlot::Card, "c0", None, json!({"title": "x"})).unwrap();
        assert_eq!(s.revision("acme.kit", None), 1);

        set(&s, g, UiSlot::Card, "c0", None, json!({"title": "x"})).unwrap();
        assert_eq!(s.revision("acme.kit", None), 2);

        s.remove("acme.kit", g, UiSlot::Card, "c0", None).unwrap();
        assert_eq!(s.revision("acme.kit", None), 3);
        s.remove("acme.kit", g, UiSlot::Card, "gone", None).unwrap();
        assert_eq!(s.revision("acme.kit", None), 3);

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
        set(&s, g, UiSlot::Pane, "p", Some("s1"), json!({"title": "a"})).unwrap();
        assert_eq!(s.revision("acme.kit", Some("s1")), 1);
        assert_eq!(s.revision("acme.kit", Some("s2")), 0);

        set(&s, g, UiSlot::Pane, "p", Some("s2"), json!({"title": "b"})).unwrap();
        assert_eq!(s.revision("acme.kit", Some("s1")), 1);
        assert_eq!(s.revision("acme.kit", Some("s2")), 1);

        s.clear_plugin("acme.kit", g);
        assert_eq!(s.revision("acme.kit", Some("s1")), 2);
        assert_eq!(s.revision("acme.kit", Some("s2")), 2);
    }
}
