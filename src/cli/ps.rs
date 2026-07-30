//! `aoe ps`: a substrate-agnostic runtime view of in-flight sessions.
//!
//! One row per running session across two substrates: `tmux` (agent panes
//! tracked through the tmux session cache and pane metadata) and `acp` (the
//! structured-view workers in the on-disk worker registry). It is additive and
//! read-only: it never mutates session storage or the worker registry, and
//! every substrate probe is fail-soft, so a dead tmux server or an unreadable
//! registry degrades to fewer rows rather than a non-zero exit.
//!
//! The pure layer (`merge_rows`, `normalize_*`, `format_age`, `filter_rows`,
//! `render_*`) takes only in-memory structs and is unit-tested without any
//! tmux, disk, or network access. The impure `run` shell gathers the substrate
//! snapshots and feeds them to the pure layer.

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::session::{Instance, Status, Storage};
use crate::util::now_secs;

const COL_SESSION: usize = 30;
const COL_SUBSTRATE: usize = 9;
const COL_STATE: usize = 9;
const COL_PID: usize = 8;
const COL_AGE: usize = 6;
const COL_AGENT: usize = 14;
const TITLE_BUDGET: usize = 20;

#[derive(Args)]
pub struct PsArgs {
    /// Output as JSON
    #[arg(long)]
    json: bool,

    /// Show only tmux-backed sessions
    #[arg(long)]
    tmux: bool,

    /// Show only ACP (structured-view) workers
    #[arg(long, conflicts_with = "tmux")]
    acp: bool,

    /// Include dead sessions and orphaned substrate entries (hidden by default)
    #[arg(long)]
    dead: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Substrate {
    Tmux,
    Acp,
}

impl Substrate {
    fn as_str(self) -> &'static str {
        match self {
            Substrate::Tmux => "tmux",
            Substrate::Acp => "acp",
        }
    }

    fn order(self) -> u8 {
        match self {
            Substrate::Tmux => 0,
            Substrate::Acp => 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubstrateFilter {
    All,
    Tmux,
    Acp,
}

/// Canonical session identity from storage: the join key for both substrates.
/// `created_at_epoch` (Unix seconds) is the single uniform AGE source, so a
/// matched row's AGE means the same thing (session age) regardless of substrate.
struct InstanceRow {
    id: String,
    title: String,
    created_at_epoch: u64,
}

/// A tmux substrate probe. `session_name` is the full tmux name, which embeds
/// only the 8-char truncated id suffix (`{PREFIX}{title}_{id8}`), so the merge
/// joins it back to an `InstanceRow` by that suffix, not the full id.
/// `activity_epoch` is only the AGE fallback for orphans (a matched row's AGE
/// comes from the instance's `created_at`).
struct TmuxState {
    session_name: String,
    status: Status,
    pid: Option<u32>,
    activity_epoch: Option<i64>,
    agent: String,
}

/// An acp substrate probe. `state` is pre-normalized by `normalize_acp_state`
/// (serve-gated) so this struct carries no serve-only types and the merge stays
/// feature-independent. `session_id` is the full id (`== Instance.id`).
/// `started_at` is only the AGE fallback for orphans (see `TmuxState`).
struct AcpState {
    session_id: String,
    pid: u32,
    agent: String,
    state: &'static str,
    started_at: u64,
}

/// One output row. `id` is the full session id for a matched row; for an orphan
/// it is the best identity available (the tmux 8-char id suffix, or the
/// registry `session_id`), since no matching instance exists.
struct Row {
    id: String,
    title: String,
    substrate: Substrate,
    state: &'static str,
    pid: Option<u32>,
    age_secs: Option<u64>,
    agent: String,
    is_orphan: bool,
}

/// Map a tmux-derived [`Status`] to the substrate-agnostic output vocabulary.
fn normalize_tmux_state(status: Status) -> &'static str {
    match status {
        Status::Running => "running",
        Status::Waiting => "waiting",
        Status::Idle | Status::Unknown | Status::Starting | Status::Creating => "idle",
        Status::Stopped | Status::Error | Status::Deleting => "dead",
    }
}

/// The acp state ladder: dead when the runner is not live; detached when it has
/// detached and has not re-attached since; attached otherwise. This mirrors the
/// inline ladder in `aoe acp ps` (src/cli/acp.rs). The two are intentionally not
/// yet factored into one helper: its natural home is `worker_registry`, which is
/// off-limits here (under active churn), so dedup is deferred to the `aoe acp ps`
/// unification follow-up that will touch acp.rs anyway. Keep them in sync.
#[cfg(feature = "serve")]
fn normalize_acp_state(
    rec: &crate::process::worker_registry::WorkerRecord,
    live: bool,
) -> &'static str {
    if !live {
        "dead"
    } else if rec.detached_at.is_some()
        && rec.last_attached_at.unwrap_or(0) <= rec.detached_at.unwrap_or(0)
    {
        "detached"
    } else {
        "attached"
    }
}

fn format_age(age_secs: Option<u64>) -> String {
    match age_secs {
        None => "-".to_string(),
        Some(s) if s < 60 => format!("{s}s"),
        Some(s) if s < 3600 => format!("{}m", s / 60),
        Some(s) if s < 86400 => format!("{}h", s / 3600),
        Some(s) => format!("{}d", s / 86400),
    }
}

/// The 8-char truncated id a tmux session name ends with, i.e. the segment
/// after the final `_` in `{PREFIX}{sanitized_title}_{truncate_id(id, 8)}`.
fn tmux_id_suffix(session_name: &str) -> Option<&str> {
    session_name.rsplit_once('_').map(|(_, suffix)| suffix)
}

/// Join both substrate snapshots against the canonical instances and produce
/// the filtered, sorted rows. tmux matches by 8-char id suffix (the name only
/// carries the truncated id); acp matches by full session id. A substrate entry
/// with no matching instance is an orphan, shown only when `include_dead`.
fn merge_rows(
    instances: &[InstanceRow],
    tmux_states: &[TmuxState],
    acp_states: &[AcpState],
    now: u64,
    filter: SubstrateFilter,
    include_dead: bool,
) -> Vec<Row> {
    let mut rows = Vec::with_capacity(tmux_states.len() + acp_states.len());

    for st in tmux_states {
        let suffix = tmux_id_suffix(&st.session_name);
        // On an 8-char id collision `find` takes the first match; the tmux name
        // only carries id8, so nothing more precise is available anyway.
        let matched =
            suffix.and_then(|s| instances.iter().find(|i| super::truncate_id(&i.id, 8) == s));
        let (id, title, is_orphan, age_secs) = match matched {
            Some(i) => (
                i.id.clone(),
                i.title.clone(),
                false,
                Some(now.saturating_sub(i.created_at_epoch)),
            ),
            None => (
                suffix.unwrap_or(&st.session_name).to_string(),
                String::new(),
                true,
                st.activity_epoch
                    .map(|epoch| now.saturating_sub(epoch.max(0) as u64)),
            ),
        };
        rows.push(Row {
            id,
            title,
            substrate: Substrate::Tmux,
            state: normalize_tmux_state(st.status),
            pid: st.pid,
            age_secs,
            agent: st.agent.clone(),
            is_orphan,
        });
    }

    for st in acp_states {
        let matched = instances.iter().find(|i| i.id == st.session_id);
        let (id, title, is_orphan, age_secs) = match matched {
            Some(i) => (
                i.id.clone(),
                i.title.clone(),
                false,
                now.saturating_sub(i.created_at_epoch),
            ),
            None => (
                st.session_id.clone(),
                String::new(),
                true,
                now.saturating_sub(st.started_at),
            ),
        };
        rows.push(Row {
            id,
            title,
            substrate: Substrate::Acp,
            state: st.state,
            pid: Some(st.pid),
            age_secs: Some(age_secs),
            agent: st.agent.clone(),
            is_orphan,
        });
    }

    filter_rows(rows, filter, include_dead)
}

/// Apply the substrate filter and the dead/orphan gate, then sort for a stable
/// output (tmux before acp, then title, then id).
fn filter_rows(rows: Vec<Row>, filter: SubstrateFilter, include_dead: bool) -> Vec<Row> {
    let mut out: Vec<Row> = rows
        .into_iter()
        .filter(|r| match filter {
            SubstrateFilter::All => true,
            SubstrateFilter::Tmux => r.substrate == Substrate::Tmux,
            SubstrateFilter::Acp => r.substrate == Substrate::Acp,
        })
        .filter(|r| include_dead || (!r.is_orphan && r.state != "dead"))
        .collect();
    out.sort_by(|a, b| {
        a.substrate
            .order()
            .cmp(&b.substrate.order())
            .then_with(|| a.title.cmp(&b.title))
            .then_with(|| a.id.cmp(&b.id))
    });
    out
}

/// Stable JSON schema for one row. `session` is the full id for a matched row,
/// else the orphan's best-available identity (see [`Row`]).
#[derive(Serialize)]
struct RowJson {
    session: String,
    substrate: &'static str,
    state: &'static str,
    pid: Option<u32>,
    age_secs: Option<u64>,
    agent: String,
}

/// Project rows into the serializable schema. Kept separate from the print so
/// the schema is unit-testable and `run` can route through the shared
/// `output::print_json` helper.
fn rows_json(rows: &[Row]) -> Vec<RowJson> {
    rows.iter()
        .map(|r| RowJson {
            session: r.id.clone(),
            substrate: r.substrate.as_str(),
            state: r.state,
            pid: r.pid,
            age_secs: r.age_secs,
            agent: r.agent.clone(),
        })
        .collect()
}

/// The SESSION cell: short id plus a truncated title (id only for orphans).
fn session_cell(row: &Row) -> String {
    let short = super::truncate_id(&row.id, 8);
    if row.title.is_empty() {
        short.to_string()
    } else {
        format!("{} {}", short, super::truncate(&row.title, TITLE_BUDGET))
    }
}

fn render_table(rows: &[Row]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<cs$} {:<csub$} {:<cst$} {:<cp$} {:<ca$} AGENT",
        "SESSION",
        "SUBSTRATE",
        "STATE",
        "PID",
        "AGE",
        cs = COL_SESSION,
        csub = COL_SUBSTRATE,
        cst = COL_STATE,
        cp = COL_PID,
        ca = COL_AGE,
    );
    let _ = writeln!(
        out,
        "{}",
        "-".repeat(COL_SESSION + COL_SUBSTRATE + COL_STATE + COL_PID + COL_AGE + COL_AGENT + 5)
    );
    for r in rows {
        let pid = r
            .pid
            .map(|p| p.to_string())
            .unwrap_or_else(|| "-".to_string());
        let _ = writeln!(
            out,
            "{:<cs$} {:<csub$} {:<cst$} {:<cp$} {:<ca$} {}",
            super::truncate(&session_cell(r), COL_SESSION),
            r.substrate.as_str(),
            r.state,
            pid,
            format_age(r.age_secs),
            r.agent,
            cs = COL_SESSION,
            csub = COL_SUBSTRATE,
            cst = COL_STATE,
            cp = COL_PID,
            ca = COL_AGE,
        );
    }
    out
}

fn load_instances(profile: &str, profile_explicit: bool) -> Vec<Instance> {
    let mut out = Vec::new();
    let profiles = if profile_explicit {
        vec![profile.to_string()]
    } else {
        crate::session::list_profiles().unwrap_or_default()
    };
    for name in &profiles {
        if let Ok(storage) = Storage::open_unwatched(name) {
            if let Ok((instances, _)) = storage.load_with_groups() {
                out.extend(instances);
            }
        }
    }
    out
}

/// Restrict orphan detection to agent sessions. Terminal, tool, and container
/// terminal sessions share the `aoe_` root prefix but are auxiliary panes, not
/// agent sessions, so they must not surface as `aoe ps` rows.
fn is_agent_session_name(name: &str) -> bool {
    name.starts_with(crate::tmux::SESSION_PREFIX)
        && !name.starts_with(crate::tmux::TERMINAL_PREFIX)
        && !name.starts_with(crate::tmux::CONTAINER_TERMINAL_PREFIX)
        && !name.starts_with(crate::tmux::TOOL_PREFIX)
}

fn collect_tmux_states(instances: &mut [Instance]) -> Vec<TmuxState> {
    use std::collections::HashSet;

    crate::tmux::refresh_session_cache();
    let meta = crate::tmux::batch_pane_metadata().unwrap_or_default();

    let mut states = Vec::new();
    let mut known: HashSet<String> = HashSet::new();

    for inst in instances.iter_mut() {
        if inst.is_structured() {
            continue;
        }
        let name = crate::tmux::resolve_agent_session_name_in(
            &meta,
            &inst.id,
            &crate::tmux::Session::generate_name(&inst.id, &inst.title),
        );
        inst.update_status_with_metadata(meta.get(&name), Some(&name));
        let agent = if inst.tool.is_empty() {
            meta.get(&name)
                .and_then(|m| m.pane_current_command.clone())
                .unwrap_or_default()
        } else {
            inst.tool.clone()
        };
        states.push(TmuxState {
            session_name: name.clone(),
            status: inst.status,
            pid: crate::process::get_pane_pid(&name),
            activity_epoch: crate::tmux::session_activity(&name),
            agent,
        });
        known.insert(name);
    }

    for (name, m) in &meta {
        if known.contains(name) || !is_agent_session_name(name) {
            continue;
        }
        states.push(TmuxState {
            session_name: name.clone(),
            status: if m.pane_dead {
                Status::Stopped
            } else {
                Status::Idle
            },
            pid: crate::process::get_pane_pid(name),
            activity_epoch: crate::tmux::session_activity(name),
            agent: m.pane_current_command.clone().unwrap_or_default(),
        });
    }

    states
}

#[cfg(feature = "serve")]
fn collect_acp_states() -> Vec<AcpState> {
    use crate::process::worker_registry;
    worker_registry::list()
        .unwrap_or_default()
        .into_iter()
        .map(|rec| {
            let live = worker_registry::is_record_live(&rec);
            AcpState {
                state: normalize_acp_state(&rec, live),
                session_id: rec.session_id,
                pid: rec.pid,
                agent: rec.agent_name,
                started_at: rec.started_at,
            }
        })
        .collect()
}

#[cfg(not(feature = "serve"))]
fn collect_acp_states() -> Vec<AcpState> {
    Vec::new()
}

#[tracing::instrument(target = "cli.ps", skip_all, fields(profile = %profile))]
pub async fn run(profile: &str, profile_explicit: bool, args: PsArgs) -> Result<()> {
    #[cfg(not(feature = "serve"))]
    if args.acp {
        anyhow::bail!("--acp requires a build with the serve feature");
    }

    let filter = if args.tmux {
        SubstrateFilter::Tmux
    } else if args.acp {
        SubstrateFilter::Acp
    } else {
        SubstrateFilter::All
    };

    // `--profile` scopes only the instance load: an explicit profile lists that
    // one, otherwise every profile. The worker registry is global (no profile
    // field), so acp workers whose session belongs to an unlisted profile fail
    // the id-join and surface as orphans, hidden unless `--dead`.
    let mut instances = load_instances(profile, profile_explicit);
    let now = now_secs();

    let tmux_states = collect_tmux_states(&mut instances);
    let acp_states = collect_acp_states();

    let instance_rows: Vec<InstanceRow> = instances
        .iter()
        .map(|i| InstanceRow {
            id: i.id.clone(),
            title: i.title.clone(),
            created_at_epoch: i.created_at.timestamp().max(0) as u64,
        })
        .collect();

    let rows = merge_rows(
        &instance_rows,
        &tmux_states,
        &acp_states,
        now,
        filter,
        args.dead,
    );

    if args.json {
        super::output::print_json(&rows_json(&rows))?;
    } else if rows.is_empty() {
        println!("No running sessions.");
    } else {
        print!("{}", render_table(&rows));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // A fixed session creation epoch so AGE assertions are deterministic; kept
    // distinct from the tmux activity epoch below to prove matched rows take
    // their AGE from `created_at`, not the substrate-native fallback.
    const CREATED_AT: u64 = 1000;
    const ACTIVITY: i64 = 1500;

    fn inst(id: &str, title: &str) -> InstanceRow {
        InstanceRow {
            id: id.to_string(),
            title: title.to_string(),
            created_at_epoch: CREATED_AT,
        }
    }

    fn tmux_state(name: &str, status: Status) -> TmuxState {
        TmuxState {
            session_name: name.to_string(),
            status,
            pid: Some(42),
            activity_epoch: Some(ACTIVITY),
            agent: "claude".to_string(),
        }
    }

    fn acp_state(session_id: &str, state: &'static str, started_at: u64) -> AcpState {
        AcpState {
            session_id: session_id.to_string(),
            pid: 7,
            agent: "claude-agent-acp".to_string(),
            state,
            started_at,
        }
    }

    #[test]
    fn normalize_tmux_state_maps_every_status() {
        assert_eq!(normalize_tmux_state(Status::Running), "running");
        assert_eq!(normalize_tmux_state(Status::Waiting), "waiting");
        assert_eq!(normalize_tmux_state(Status::Idle), "idle");
        assert_eq!(normalize_tmux_state(Status::Unknown), "idle");
        assert_eq!(normalize_tmux_state(Status::Starting), "idle");
        assert_eq!(normalize_tmux_state(Status::Creating), "idle");
        assert_eq!(normalize_tmux_state(Status::Stopped), "dead");
        assert_eq!(normalize_tmux_state(Status::Error), "dead");
        assert_eq!(normalize_tmux_state(Status::Deleting), "dead");
    }

    #[test]
    fn format_age_scales_units() {
        assert_eq!(format_age(None), "-");
        assert_eq!(format_age(Some(5)), "5s");
        assert_eq!(format_age(Some(59)), "59s");
        assert_eq!(format_age(Some(60)), "1m");
        assert_eq!(format_age(Some(3599)), "59m");
        assert_eq!(format_age(Some(3600)), "1h");
        assert_eq!(format_age(Some(86399)), "23h");
        assert_eq!(format_age(Some(86400)), "1d");
    }

    #[test]
    fn tmux_id_suffix_extracts_trailing_id() {
        assert_eq!(tmux_id_suffix("aoe_My_Session_abcd1234"), Some("abcd1234"));
        assert_eq!(tmux_id_suffix("aoe__abcd1234"), Some("abcd1234"));
        assert_eq!(tmux_id_suffix("nounderscore"), None);
    }

    #[test]
    fn merge_matches_tmux_by_truncated_id_suffix() {
        let instances = vec![inst("abcd1234ef567890", "My Session")];
        let tmux = vec![tmux_state("aoe_My_Session_abcd1234", Status::Running)];
        let rows = merge_rows(&instances, &tmux, &[], 2000, SubstrateFilter::All, false);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "abcd1234ef567890");
        assert_eq!(rows[0].title, "My Session");
        assert!(!rows[0].is_orphan);
        assert_eq!(rows[0].state, "running");
        // AGE is now - created_at (1000), NOT now - activity (1500).
        assert_eq!(rows[0].age_secs, Some(1000));
    }

    #[test]
    fn merge_flags_tmux_session_without_instance_as_orphan() {
        let tmux = vec![tmux_state("aoe_Ghost_99999999", Status::Running)];
        let hidden = merge_rows(&[], &tmux, &[], 2000, SubstrateFilter::All, false);
        assert!(hidden.is_empty(), "orphan is hidden without --dead");
        let shown = merge_rows(&[], &tmux, &[], 2000, SubstrateFilter::All, true);
        assert_eq!(shown.len(), 1);
        assert!(shown[0].is_orphan);
        assert_eq!(shown[0].id, "99999999");
        // Orphans have no instance, so AGE falls back to tmux activity (1500).
        assert_eq!(shown[0].age_secs, Some(500));
    }

    #[test]
    fn merge_matches_acp_by_full_session_id() {
        let instances = vec![inst("full-session-id-1234", "Structured")];
        let acp = vec![acp_state("full-session-id-1234", "attached", 500)];
        let rows = merge_rows(&instances, &[], &acp, 2000, SubstrateFilter::All, false);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].substrate, Substrate::Acp);
        assert_eq!(rows[0].title, "Structured");
        assert!(!rows[0].is_orphan);
        assert_eq!(rows[0].pid, Some(7));
        // AGE is now - created_at (1000), NOT now - started_at (1500).
        assert_eq!(rows[0].age_secs, Some(1000));
    }

    #[test]
    fn merge_flags_acp_record_without_instance_as_orphan() {
        let acp = vec![acp_state("gone", "attached", 500)];
        assert!(merge_rows(&[], &[], &acp, 1, SubstrateFilter::All, false).is_empty());
        let shown = merge_rows(&[], &[], &acp, 2000, SubstrateFilter::All, true);
        assert_eq!(shown.len(), 1);
        assert!(shown[0].is_orphan);
        // Orphans fall back to the worker's started_at (500).
        assert_eq!(shown[0].age_secs, Some(1500));
    }

    #[test]
    fn filter_hides_dead_by_default_and_reveals_with_flag() {
        let instances = vec![inst("abcd1234ef567890", "Dead One")];
        let tmux = vec![tmux_state("aoe_Dead_One_abcd1234", Status::Error)];
        let hidden = merge_rows(&instances, &tmux, &[], 0, SubstrateFilter::All, false);
        assert!(hidden.is_empty(), "dead is hidden by default");
        let shown = merge_rows(&instances, &tmux, &[], 0, SubstrateFilter::All, true);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].state, "dead");
        assert!(!shown[0].is_orphan);
    }

    #[test]
    fn filter_by_substrate_selects_one_side() {
        let instances = vec![inst("abcd1234ef567890", "T"), inst("acp-id-1", "A")];
        let tmux = vec![tmux_state("aoe_T_abcd1234", Status::Running)];
        let acp = vec![acp_state("acp-id-1", "attached", 0)];
        let only_tmux = merge_rows(&instances, &tmux, &acp, 0, SubstrateFilter::Tmux, false);
        assert_eq!(only_tmux.len(), 1);
        assert_eq!(only_tmux[0].substrate, Substrate::Tmux);
        let only_acp = merge_rows(&instances, &tmux, &acp, 0, SubstrateFilter::Acp, false);
        assert_eq!(only_acp.len(), 1);
        assert_eq!(only_acp[0].substrate, Substrate::Acp);
    }

    #[test]
    fn acp_filter_with_dead_reveals_dead_acp_orphan() {
        let acp = vec![acp_state("gone", "dead", 0)];
        assert!(
            merge_rows(&[], &[], &acp, 0, SubstrateFilter::Acp, false).is_empty(),
            "a dead acp orphan is hidden under --acp without --dead"
        );
        let shown = merge_rows(&[], &[], &acp, 0, SubstrateFilter::Acp, true);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].substrate, Substrate::Acp);
        assert_eq!(shown[0].state, "dead");
    }

    #[test]
    fn merge_sorts_tmux_before_acp() {
        let instances = vec![inst("abcd1234ef567890", "Zeta"), inst("acp-id-1", "Alpha")];
        let tmux = vec![tmux_state("aoe_Zeta_abcd1234", Status::Running)];
        let acp = vec![acp_state("acp-id-1", "attached", 0)];
        let rows = merge_rows(&instances, &tmux, &acp, 0, SubstrateFilter::All, false);
        assert_eq!(rows.len(), 2);
        // tmux sorts ahead of acp even though its title ("Zeta") sorts after
        // the acp row's title ("Alpha"): substrate is the primary sort key.
        assert_eq!(rows[0].substrate, Substrate::Tmux);
        assert_eq!(rows[1].substrate, Substrate::Acp);
    }

    #[test]
    fn merge_sort_tiebreaks_by_title_then_id() {
        // Same substrate: title is the secondary key, id the tertiary. Two rows
        // share the title "Same" to force the id tiebreak.
        let instances = vec![
            inst("2222aaaabbbbcccc", "Same"),
            inst("1111aaaabbbbcccc", "Same"),
            inst("3333ffff00001111", "Alpha"),
        ];
        let tmux = vec![
            tmux_state("aoe_Same_2222aaaa", Status::Running),
            tmux_state("aoe_Same_1111aaaa", Status::Running),
            tmux_state("aoe_Alpha_3333ffff", Status::Running),
        ];
        let rows = merge_rows(&instances, &tmux, &[], 0, SubstrateFilter::All, false);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].title, "Alpha");
        assert_eq!(rows[1].id, "1111aaaabbbbcccc");
        assert_eq!(rows[2].id, "2222aaaabbbbcccc");
    }

    #[test]
    fn render_json_projects_stable_schema() {
        let instances = vec![inst("abcd1234ef567890", "My Session")];
        let tmux = vec![tmux_state("aoe_My_Session_abcd1234", Status::Running)];
        let rows = merge_rows(&instances, &tmux, &[], 2000, SubstrateFilter::All, false);
        let v = serde_json::to_value(rows_json(&rows)).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        let row = &arr[0];
        assert_eq!(row["session"], "abcd1234ef567890");
        assert_eq!(row["substrate"], "tmux");
        assert_eq!(row["state"], "running");
        assert_eq!(row["pid"], 42);
        assert_eq!(row["age_secs"], 1000);
        assert_eq!(row["agent"], "claude");
        // Exactly the six documented keys, no more.
        assert_eq!(row.as_object().unwrap().len(), 6);
    }

    #[test]
    fn render_json_empty_is_array() {
        assert!(rows_json(&[]).is_empty());
        assert_eq!(serde_json::to_string(&rows_json(&[])).unwrap(), "[]");
    }

    #[test]
    fn render_table_has_header_underline_and_row() {
        let instances = vec![inst("abcd1234ef567890", "My Session")];
        let tmux = vec![tmux_state("aoe_My_Session_abcd1234", Status::Running)];
        let rows = merge_rows(&instances, &tmux, &[], 2000, SubstrateFilter::All, false);
        let table = render_table(&rows);
        assert!(table.contains("SESSION"));
        assert!(table.contains("SUBSTRATE"));
        assert!(table.contains("----"), "header underline present");
        assert!(table.contains("abcd1234"));
        assert!(table.contains("tmux"));
        assert!(table.contains("running"));
        assert!(table.contains("claude"));
    }

    #[test]
    fn session_cell_truncates_long_title() {
        let row = Row {
            id: "abcd1234ef567890".to_string(),
            title: "A very long session title that exceeds the budget".to_string(),
            substrate: Substrate::Tmux,
            state: "running",
            pid: None,
            age_secs: None,
            agent: String::new(),
            is_orphan: false,
        };
        let cell = session_cell(&row);
        assert!(cell.starts_with("abcd1234 "));
        assert!(
            cell.contains("..."),
            "long title is truncated with an ellipsis"
        );
        assert!(
            !cell.contains("exceeds the budget"),
            "the tail of an over-budget title is dropped"
        );
    }

    #[test]
    fn session_cell_omits_title_for_orphan() {
        let row = Row {
            id: "99999999".to_string(),
            title: String::new(),
            substrate: Substrate::Tmux,
            state: "running",
            pid: None,
            age_secs: None,
            agent: String::new(),
            is_orphan: true,
        };
        assert_eq!(session_cell(&row), "99999999");
    }

    #[cfg(feature = "serve")]
    #[test]
    fn normalize_acp_state_ladder() {
        use crate::process::worker_registry::WorkerRecord;
        use std::path::PathBuf;

        let mut rec = WorkerRecord::new(
            "s".into(),
            1,
            PathBuf::from("/tmp/s.sock"),
            "claude-agent-acp".into(),
            "claude".into(),
            PathBuf::from("/repo"),
            None,
            vec![],
            vec![],
            None,
            None,
        );
        assert_eq!(normalize_acp_state(&rec, false), "dead");
        assert_eq!(normalize_acp_state(&rec, true), "attached");
        rec.detached_at = Some(100);
        rec.last_attached_at = Some(50);
        assert_eq!(normalize_acp_state(&rec, true), "detached");
        rec.last_attached_at = Some(150);
        assert_eq!(normalize_acp_state(&rec, true), "attached");
    }
}
