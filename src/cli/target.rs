//! Shared multi-profile target selection for bulk fleet operations.
//!
//! `send`, `session restart`, and `status` all need the same question
//! answered: "given these filters, which sessions am I acting on, and in which
//! profile does each live?" This module is the single answer. It generalizes
//! the per-profile `pick_targets_for_restart_all` into an AND-combined filter
//! that can span every profile, mirroring the `scripts/fleet` join semantics
//! (`list --all --json` ⋈ `status -v` on path) so one native primitive retires
//! that stopgap.
//!
//! Safety invariants baked in here so no caller can forget them:
//! - transitional states (`Deleting`/`Creating`) are never selected;
//! - cockpit-mode sessions (lifecycle owned by `aoe serve`, not tmux) are
//!   never selected;
//! - the invoking session can be excluded (so a bulk `send`/`restart` never
//!   clobbers or kills the operator's own pane);
//! - for mutating verbs, archived/snoozed sessions are excluded unless the
//!   caller opts in with `--include-archived`.

use anyhow::Result;
use clap::Args;

use crate::session::{Instance, Status, Storage};

/// AND-combined fleet selector, flattened into the bulk verbs' arg structs.
///
/// An empty filter selects every (non-transitional, non-cockpit) session in
/// the active profile; each populated field narrows the set further. The verbs
/// layer their own safety on top — notably `send` previews unless `--yes`.
///
/// Profile selection note: aoe's global `-p/--profile` already names the
/// *ambient* profile, so the multi-profile subset selector here is spelled
/// `--profiles a,b,c` (comma list) to avoid colliding with it; `--all` spans
/// every profile.
#[derive(Args, Debug, Default, Clone)]
pub struct TargetFilter {
    /// Operate across every profile, not just the active one.
    #[arg(long)]
    pub all: bool,

    /// Restrict to these profiles (comma-separated). Implies cross-profile
    /// scanning. Ignored when `--all` is set (which already spans all).
    #[arg(long, value_delimiter = ',')]
    pub profiles: Vec<String>,

    /// Keep only sessions whose group path starts with one of these prefixes
    /// (repeatable). Matches the fleet script's prefix semantics.
    #[arg(long = "group")]
    pub groups: Vec<String>,

    /// Keep only sessions in one of these live states (repeatable):
    /// running|waiting|idle|stopped|error|starting|unknown.
    #[arg(long = "state", value_parser = parse_status_filter)]
    pub states: Vec<Status>,

    /// Shorthand for `--state error` — the highest-frequency fleet filter.
    #[arg(long)]
    pub errors: bool,

    /// Keep only these session ids (comma-separated). Exact id or unique
    /// id-prefix; non-matching ids are silently ignored (they may live in a
    /// profile outside the scanned set).
    #[arg(long, value_delimiter = ',')]
    pub ids: Vec<String>,

    /// Keep only sessions whose project path contains this substring.
    #[arg(long)]
    pub path: Option<String>,

    /// Include archived/snoozed sessions in mutating verbs. Off by default so
    /// a bulk op never wakes a parked session. No effect on `status`, which
    /// always lists them.
    #[arg(long = "include-archived")]
    pub include_archived: bool,
}

fn parse_status_filter(s: &str) -> Result<Status, String> {
    match s.to_ascii_lowercase().as_str() {
        "running" => Ok(Status::Running),
        "waiting" => Ok(Status::Waiting),
        "idle" => Ok(Status::Idle),
        "stopped" => Ok(Status::Stopped),
        "error" => Ok(Status::Error),
        "starting" => Ok(Status::Starting),
        "unknown" => Ok(Status::Unknown),
        other => Err(format!(
            "unknown state '{other}' (expected running|waiting|idle|stopped|error|starting|unknown)"
        )),
    }
}

impl TargetFilter {
    /// Whether the user supplied any selector. Callers use this to decide
    /// bulk-vs-single dispatch: `send`/`restart` treat "filter present" as
    /// "operate on the matched set" and "no filter + no identifier" as a
    /// no-op guard rather than an accidental whole-fleet blast.
    pub fn has_selector(&self) -> bool {
        self.all
            || !self.profiles.is_empty()
            || !self.groups.is_empty()
            || !self.states.is_empty()
            || self.errors
            || !self.ids.is_empty()
            || self.path.is_some()
    }

    /// Effective state set, folding the `--errors` shorthand into `--state`.
    fn effective_states(&self) -> Vec<Status> {
        let mut states = self.states.clone();
        if self.errors && !states.contains(&Status::Error) {
            states.push(Status::Error);
        }
        states
    }
}

/// A selected session plus the profile it was loaded from. Bulk verbs key on
/// `profile` to open the right `Storage` for the follow-up mutation.
pub struct ResolvedTarget {
    pub profile: String,
    pub instance: Instance,
}

/// Resolve the profile set to scan from the ambient profile + filter. `--all`
/// wins; otherwise an explicit `--profiles` subset; otherwise just the ambient
/// profile (empty string resolves to the configured default downstream in
/// `Storage::new`).
fn profiles_to_scan(ambient_profile: &str, filter: &TargetFilter) -> Result<Vec<String>> {
    if filter.all {
        return crate::session::list_profiles();
    }
    if !filter.profiles.is_empty() {
        return Ok(filter.profiles.clone());
    }
    Ok(vec![ambient_profile.to_string()])
}

/// Resolve the full target set across all in-scope profiles.
///
/// `exclude_id` drops the invoking session (pass `Some(self_id)` for mutating
/// verbs, `None` for `status` where the operator wants to see their own row).
/// `mutating` gates the archived/snoozed exclusion: `true` for `send`/`restart`
/// (respecting `--include-archived`), `false` for `status`.
///
/// Live status is refreshed before filtering so `--state`/`--errors` match the
/// real current state, not the last persisted value.
pub fn resolve_targets(
    ambient_profile: &str,
    filter: &TargetFilter,
    exclude_id: Option<&str>,
    mutating: bool,
) -> Result<Vec<ResolvedTarget>> {
    // One tmux cache refresh for the whole sweep; update_status reads from it.
    crate::tmux::refresh_session_cache();

    let states = filter.effective_states();
    let profiles = profiles_to_scan(ambient_profile, filter)?;
    let mut out = Vec::new();

    for profile in &profiles {
        let Ok(storage) = Storage::new(profile) else {
            continue;
        };
        let Ok((mut instances, _groups)) = storage.load_with_groups() else {
            continue;
        };
        // Resolve to the canonical profile name (handles ambient "").
        let resolved_profile = storage.profile().to_string();

        for inst in &mut instances {
            inst.update_status();
        }

        for inst in instances {
            if instance_passes(&inst, filter, &states, exclude_id, mutating) {
                out.push(ResolvedTarget {
                    profile: resolved_profile.clone(),
                    instance: inst,
                });
            }
        }
    }

    Ok(out)
}

/// AND-combined acceptance test for one instance against the filter, plus the
/// non-negotiable safety skips (transitional/cockpit states, self-exclusion,
/// archived/snoozed for mutating verbs). Pure and side-effect-free so it is
/// unit-testable without touching storage; `resolve_targets` is the I/O shell
/// around it. `states` is the caller's `effective_states()` (errors folded in).
fn instance_passes(
    inst: &Instance,
    filter: &TargetFilter,
    states: &[Status],
    exclude_id: Option<&str>,
    mutating: bool,
) -> bool {
    if matches!(inst.status, Status::Deleting | Status::Creating) {
        return false;
    }
    #[cfg(feature = "serve")]
    if inst.cockpit_mode {
        return false;
    }
    if let Some(skip) = exclude_id {
        if inst.id == skip {
            return false;
        }
    }
    if mutating && !filter.include_archived && (inst.is_archived() || inst.is_snoozed()) {
        return false;
    }
    if !filter.groups.is_empty()
        && !filter
            .groups
            .iter()
            .any(|g| inst.group_path.starts_with(g.as_str()))
    {
        return false;
    }
    if !states.is_empty() && !states.contains(&inst.status) {
        return false;
    }
    if !filter.ids.is_empty()
        && !filter
            .ids
            .iter()
            .any(|id| inst.id == *id || inst.id.starts_with(id.as_str()))
    {
        return false;
    }
    if let Some(sub) = &filter.path {
        if !inst.project_path.contains(sub.as_str()) {
            return false;
        }
    }
    true
}

/// The invoking session's own instance id, from the `AOE_INSTANCE_ID` env var
/// aoe exports into every pane. `None` when not running under aoe (e.g. a bare
/// CLI invocation), in which case there is no self to exclude.
pub fn self_instance_id() -> Option<String> {
    std::env::var("AOE_INSTANCE_ID")
        .ok()
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inst(id: &str, status: Status) -> Instance {
        let mut i = Instance::new(id, "/tmp/proj");
        i.id = id.to_string();
        i.status = status;
        i
    }

    fn empty() -> TargetFilter {
        TargetFilter::default()
    }

    // Mutating, empty filter: every live state passes; transitional states are
    // always skipped. Mirrors the old `pick_targets_for_restart_all` invariant.
    #[test]
    fn empty_mutating_filter_skips_only_transitional() {
        let f = empty();
        let states = f.effective_states();
        for (status, expect) in [
            (Status::Running, true),
            (Status::Waiting, true),
            (Status::Idle, true),
            (Status::Stopped, true),
            (Status::Error, true),
            (Status::Starting, true),
            (Status::Unknown, true),
            (Status::Deleting, false),
            (Status::Creating, false),
        ] {
            assert_eq!(
                instance_passes(&inst("x", status), &f, &states, None, true),
                expect,
                "status {status:?}"
            );
        }
    }

    #[test]
    fn exclude_id_drops_self() {
        let f = empty();
        let states = f.effective_states();
        let me = inst("self-1", Status::Running);
        assert!(!instance_passes(&me, &f, &states, Some("self-1"), true));
        assert!(instance_passes(&me, &f, &states, Some("other"), true));
    }

    #[test]
    fn errors_shorthand_folds_into_state_filter() {
        let mut f = empty();
        f.errors = true;
        let states = f.effective_states();
        assert!(instance_passes(
            &inst("e", Status::Error),
            &f,
            &states,
            None,
            true
        ));
        assert!(!instance_passes(
            &inst("r", Status::Running),
            &f,
            &states,
            None,
            true
        ));
    }

    #[test]
    fn state_filter_is_membership() {
        let mut f = empty();
        f.states = vec![Status::Waiting, Status::Idle];
        let states = f.effective_states();
        assert!(instance_passes(
            &inst("w", Status::Waiting),
            &f,
            &states,
            None,
            true
        ));
        assert!(instance_passes(
            &inst("i", Status::Idle),
            &f,
            &states,
            None,
            true
        ));
        assert!(!instance_passes(
            &inst("r", Status::Running),
            &f,
            &states,
            None,
            true
        ));
    }

    #[test]
    fn id_filter_accepts_exact_and_prefix() {
        let mut f = empty();
        f.ids = vec!["abc".to_string()];
        let states = f.effective_states();
        assert!(instance_passes(
            &inst("abc", Status::Running),
            &f,
            &states,
            None,
            true
        ));
        assert!(instance_passes(
            &inst("abcdef", Status::Running),
            &f,
            &states,
            None,
            true
        ));
        assert!(!instance_passes(
            &inst("xyz", Status::Running),
            &f,
            &states,
            None,
            true
        ));
    }

    #[test]
    fn group_filter_is_prefix_match() {
        let mut f = empty();
        f.groups = vec!["fleet".to_string()];
        let states = f.effective_states();
        let mut in_group = inst("a", Status::Running);
        in_group.group_path = "fleet/sub".to_string();
        let mut other = inst("b", Status::Running);
        other.group_path = "other".to_string();
        assert!(instance_passes(&in_group, &f, &states, None, true));
        assert!(!instance_passes(&other, &f, &states, None, true));
    }

    #[test]
    fn path_filter_is_substring() {
        let mut f = empty();
        f.path = Some("per-dev".to_string());
        let states = f.effective_states();
        let mut hit = inst("a", Status::Running);
        hit.project_path = "/Users/x/GitProjects/per-dev".to_string();
        let mut miss = inst("b", Status::Running);
        miss.project_path = "/Users/x/GitProjects/other".to_string();
        assert!(instance_passes(&hit, &f, &states, None, true));
        assert!(!instance_passes(&miss, &f, &states, None, true));
    }

    #[test]
    fn has_selector_reflects_populated_fields() {
        assert!(!empty().has_selector());
        let mut f = empty();
        f.all = true;
        assert!(f.has_selector());
    }
}
