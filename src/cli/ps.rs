//! `aoe ps`: a substrate-agnostic runtime view of in-flight sessions.

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

const COL_BUILD: usize = 28;
const COL_MODEL: usize = 20;
const COL_CWD: usize = 30;

#[derive(Args)]
pub struct PsArgs {
    /// Output as JSON
    #[arg(long)]
    json: bool,

    /// Show only tmux-backed sessions
    #[arg(long)]
    tmux: bool,

    /// Show only ACP (structured-view) workers, with their ACP-specific
    /// columns (BUILD, MODEL, CWD, SOCKET); `--json` adds `substrate`,
    /// `state`, `age_secs`, and `model` to the keys the removed `aoe acp ps`
    /// emitted, but sorts by substrate, then title, then id rather than by
    /// `started_at`. Dead and orphaned workers are hidden unless `--dead` is
    /// also passed; the worker registry is global, so with an explicit `-p`
    /// the workers of other profiles surface as orphans (also hidden
    /// without `--dead`)
    #[arg(long, conflicts_with = "tmux")]
    acp: bool,

    /// Include dead sessions and orphaned substrate entries (hidden by default)
    #[arg(long)]
    dead: bool,

    /// Redraw continuously until Ctrl-C; with `--json`, print one compact
    /// JSON array per line instead of clearing the screen
    #[arg(long, conflicts_with = "session")]
    watch: bool,

    /// Seconds between `--watch` redraws
    #[arg(
        long,
        requires = "watch",
        default_value_t = 2,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    interval: u64,

    /// Show one session in detail, including dead runtimes (id, id prefix,
    /// title, or path)
    #[arg(conflicts_with_all = ["tmux", "acp"])]
    session: Option<String>,
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

struct InstanceRow {
    id: String,
    title: String,
    created_at_epoch: u64,
}

struct TmuxState {
    session_name: String,
    status: Status,
    pid: Option<u32>,
    activity_epoch: Option<i64>,
    agent: String,
}

struct AcpExtra {
    build_version: String,
    build_stale: bool,
    socket: std::path::PathBuf,
    cwd: std::path::PathBuf,
    model: Option<String>,
    alive: bool,
    last_attached_at: Option<u64>,
    detached_at: Option<u64>,
}

struct AcpState {
    session_id: String,
    pid: u32,
    agent: String,
    state: &'static str,
    started_at: u64,
    acp_extra: Option<AcpExtra>,
}

struct Row {
    id: String,
    title: String,
    substrate: Substrate,
    state: &'static str,
    pid: Option<u32>,
    age_secs: Option<u64>,
    agent: String,
    is_orphan: bool,
    acp_extra: Option<AcpExtra>,
    started_at: u64,
}

fn normalize_tmux_state(status: Status) -> &'static str {
    match status {
        Status::Running => "running",
        Status::Waiting => "waiting",
        Status::Idle | Status::Unknown | Status::Starting | Status::Creating => "idle",
        Status::Stopped | Status::Error | Status::Deleting => "dead",
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

fn tmux_id_suffix(session_name: &str) -> Option<&str> {
    session_name.rsplit_once('_').map(|(_, suffix)| suffix)
}

fn merge_rows(
    instances: &[InstanceRow],
    tmux_states: &[TmuxState],
    acp_states: Vec<AcpState>,
    now: u64,
    filter: SubstrateFilter,
    include_dead: bool,
) -> Vec<Row> {
    let mut rows = Vec::with_capacity(tmux_states.len() + acp_states.len());

    for st in tmux_states {
        let suffix = tmux_id_suffix(&st.session_name);
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
            acp_extra: None,
            started_at: 0,
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
            agent: st.agent,
            is_orphan,
            acp_extra: st.acp_extra,
            started_at: st.started_at,
        });
    }

    filter_rows(rows, filter, include_dead)
}

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

#[derive(Serialize)]
struct RowJson {
    session: String,
    substrate: &'static str,
    state: &'static str,
    pid: Option<u32>,
    age_secs: Option<u64>,
    agent: String,
}

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

fn render_build_cell(build_version: &str, stale: bool) -> String {
    let base = if build_version.is_empty() {
        "<legacy>"
    } else {
        build_version
    };
    if stale {
        format!("{base} (stale)")
    } else {
        base.to_string()
    }
}

fn render_table_acp(rows: &[Row]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<cs$} {:<csub$} {:<cst$} {:<cp$} {:<ca$} {:<cag$} {:<cb$} {:<cm$} {:<ccwd$} SOCKET",
        "SESSION",
        "SUBSTRATE",
        "STATE",
        "PID",
        "AGE",
        "AGENT",
        "BUILD",
        "MODEL",
        "CWD",
        cs = COL_SESSION,
        csub = COL_SUBSTRATE,
        cst = COL_STATE,
        cp = COL_PID,
        ca = COL_AGE,
        cag = COL_AGENT,
        cb = COL_BUILD,
        cm = COL_MODEL,
        ccwd = COL_CWD,
    );
    let _ = writeln!(
        out,
        "{}",
        "-".repeat(
            COL_SESSION
                + COL_SUBSTRATE
                + COL_STATE
                + COL_PID
                + COL_AGE
                + COL_AGENT
                + COL_BUILD
                + COL_MODEL
                + COL_CWD
                + "SOCKET".len()
                + 9
        )
    );
    for (r, e) in rows.iter().filter_map(|r| Some((r, r.acp_extra.as_ref()?))) {
        let pid = r
            .pid
            .map(|p| p.to_string())
            .unwrap_or_else(|| "-".to_string());
        let build = render_build_cell(&e.build_version, e.build_stale);
        let model = e.model.clone().unwrap_or_else(|| "-".to_string());
        let cwd = e.cwd.display().to_string();
        let socket = e.socket.display().to_string();
        let _ = writeln!(
            out,
            "{:<cs$} {:<csub$} {:<cst$} {:<cp$} {:<ca$} {:<cag$} {:<cb$} {:<cm$} {:<ccwd$} {}",
            super::truncate(&session_cell(r), COL_SESSION),
            r.substrate.as_str(),
            r.state,
            pid,
            format_age(r.age_secs),
            super::truncate(&r.agent, COL_AGENT),
            super::truncate(&build, COL_BUILD),
            super::truncate(&model, COL_MODEL),
            super::truncate(&cwd, COL_CWD),
            socket,
            cs = COL_SESSION,
            csub = COL_SUBSTRATE,
            cst = COL_STATE,
            cp = COL_PID,
            ca = COL_AGE,
            cag = COL_AGENT,
            cb = COL_BUILD,
            cm = COL_MODEL,
            ccwd = COL_CWD,
        );
    }
    out
}

#[derive(Serialize)]
struct AcpRowJson {
    session_id: String,
    pid: Option<u32>,
    alive: bool,
    agent: String,
    build_version: String,
    build_stale: bool,
    socket: std::path::PathBuf,
    cwd: std::path::PathBuf,
    started_at: u64,
    last_attached_at: Option<u64>,
    detached_at: Option<u64>,
    substrate: &'static str,
    state: &'static str,
    age_secs: Option<u64>,
    model: Option<String>,
}

fn acp_rows_json(rows: &[Row]) -> Vec<AcpRowJson> {
    rows.iter()
        .filter_map(|r| {
            let e = r.acp_extra.as_ref()?;
            Some(AcpRowJson {
                session_id: r.id.clone(),
                pid: r.pid,
                alive: e.alive,
                agent: r.agent.clone(),
                build_version: e.build_version.clone(),
                build_stale: e.build_stale,
                socket: e.socket.clone(),
                cwd: e.cwd.clone(),
                started_at: r.started_at,
                last_attached_at: e.last_attached_at,
                detached_at: e.detached_at,
                substrate: r.substrate.as_str(),
                state: r.state,
                age_secs: r.age_secs,
                model: e.model.clone(),
            })
        })
        .collect()
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
            if let Ok((mut instances, _)) = storage.load_with_groups() {
                for inst in &mut instances {
                    inst.source_profile = name.clone();
                }
                out.extend(instances);
            }
        }
    }
    out
}

/// The `@aoe_kind` marker disambiguates an agent titled `term Foo` from the
/// terminal of `Foo`; unmarked legacy sessions fall back to the name shape.
fn is_agent_session_name(name: &str, marker: Option<&str>) -> bool {
    name.starts_with(crate::tmux::SESSION_PREFIX)
        && crate::tmux::SessionKind::of(name, marker) == Some(crate::tmux::SessionKind::Agent)
}

fn collect_tmux_states(instances: &mut [Instance]) -> Vec<TmuxState> {
    use std::collections::HashSet;

    {
        let mut resolved: HashSet<&str> = HashSet::new();
        for inst in instances.iter() {
            if resolved.insert(inst.source_profile.as_str()) {
                crate::session::config::profile_config::resolve_config_or_warn(
                    &inst.source_profile,
                );
            }
        }
    }

    crate::tmux::refresh_session_cache();
    let meta = match crate::tmux::batch_pane_metadata() {
        Ok(meta) => meta,
        Err(err) => {
            tracing::warn!(error = %err, "failed to collect tmux pane metadata");
            return Vec::new();
        }
    };

    let live = crate::tmux::probe_live_sessions();
    let markers: std::collections::HashMap<&str, Option<&str>> = live
        .as_ref()
        .map(|live| crate::tmux::marked_names(live).collect())
        .unwrap_or_default();

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
        inst.update_status_once(meta.get(&name), Some(&name));
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
        let marker = markers.get(name.as_str()).copied().flatten();
        if known.contains(name) || !is_agent_session_name(name, marker) {
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

fn acp_state_from_record(rec: crate::process::worker_registry::WorkerRecord) -> AcpState {
    use crate::process::worker_registry;
    let live = worker_registry::is_record_live(&rec);
    let state = worker_registry::worker_state_label(&rec, live);
    let build_stale = !worker_registry::is_build_current(&rec);
    AcpState {
        state,
        session_id: rec.session_id,
        pid: rec.pid,
        agent: rec.agent_name,
        started_at: rec.started_at,
        acp_extra: Some(AcpExtra {
            build_version: rec.build_version,
            build_stale,
            socket: rec.socket_path,
            cwd: rec.cwd,
            model: rec.model,
            alive: live,
            last_attached_at: rec.last_attached_at,
            detached_at: rec.detached_at,
        }),
    }
}

fn collect_acp_states() -> Vec<AcpState> {
    crate::process::worker_registry::list()
        .unwrap_or_default()
        .into_iter()
        .map(acp_state_from_record)
        .collect()
}

/// Collects tmux and ACP runtime state for `instances` and merges it into rows.
fn collect_rows(
    instances: &mut [Instance],
    filter: SubstrateFilter,
    include_dead: bool,
) -> Vec<Row> {
    let now = now_secs();
    let tmux_states = if matches!(filter, SubstrateFilter::Acp) {
        Vec::new()
    } else {
        collect_tmux_states(instances)
    };
    let acp_states = if matches!(filter, SubstrateFilter::Tmux) {
        Vec::new()
    } else {
        collect_acp_states()
    };

    let instance_rows: Vec<InstanceRow> = instances
        .iter()
        .map(|i| InstanceRow {
            id: i.id.clone(),
            title: i.title.clone(),
            created_at_epoch: i.created_at.timestamp().max(0) as u64,
        })
        .collect();

    merge_rows(
        &instance_rows,
        &tmux_states,
        acp_states,
        now,
        filter,
        include_dead,
    )
}

/// Renders the list view as a table, or as JSON when `json` is set.
fn render_rows(rows: &[Row], acp: bool, json: bool, compact: bool) -> Result<String> {
    if json {
        let mut out = if acp {
            to_json(&acp_rows_json(rows), compact)?
        } else {
            to_json(&rows_json(rows), compact)?
        };
        out.push('\n');
        return Ok(out);
    }
    Ok(if rows.is_empty() {
        "No running sessions.\n".to_string()
    } else if acp {
        render_table_acp(rows)
    } else {
        render_table(rows)
    })
}

/// Serializes `value` as compact or pretty JSON.
fn to_json<T: Serialize>(value: &T, compact: bool) -> Result<String> {
    Ok(if compact {
        serde_json::to_string(value)?
    } else {
        serde_json::to_string_pretty(value)?
    })
}

/// Prefixes a clear-screen sequence to table frames on a TTY.
fn watch_frame(body: String, json: bool, tty: bool) -> String {
    if tty && !json {
        format!("\x1b[2J\x1b[H{body}")
    } else {
        body
    }
}

#[derive(Serialize)]
struct AcpDetailJson {
    build_version: String,
    build_stale: bool,
    socket: std::path::PathBuf,
    cwd: std::path::PathBuf,
    model: Option<String>,
    alive: bool,
    started_at: u64,
    last_attached_at: Option<u64>,
    detached_at: Option<u64>,
}

#[derive(Serialize)]
struct DetailJson {
    session: String,
    title: String,
    substrate: &'static str,
    state: &'static str,
    pid: Option<u32>,
    age_secs: Option<u64>,
    agent: String,
    #[serde(flatten)]
    acp: Option<AcpDetailJson>,
}

/// Projects a row into the single-session JSON shape.
fn detail_json(row: &Row) -> DetailJson {
    DetailJson {
        session: row.id.clone(),
        title: row.title.clone(),
        substrate: row.substrate.as_str(),
        state: row.state,
        pid: row.pid,
        age_secs: row.age_secs,
        agent: row.agent.clone(),
        acp: row.acp_extra.as_ref().map(|e| AcpDetailJson {
            build_version: e.build_version.clone(),
            build_stale: e.build_stale,
            socket: e.socket.clone(),
            cwd: e.cwd.clone(),
            model: e.model.clone(),
            alive: e.alive,
            started_at: row.started_at,
            last_attached_at: e.last_attached_at,
            detached_at: e.detached_at,
        }),
    }
}

/// Renders a row as a key/value block.
fn render_detail(row: &Row) -> String {
    use std::fmt::Write;
    let dash = |v: Option<String>| v.unwrap_or_else(|| "-".to_string());
    let mut out = String::new();
    let mut line = |key: &str, value: String| {
        let _ = writeln!(out, "{key:<14}{value}");
    };
    line("Session:", row.id.clone());
    line("Title:", row.title.clone());
    line("Substrate:", row.substrate.as_str().to_string());
    line("State:", row.state.to_string());
    line("PID:", dash(row.pid.map(|p| p.to_string())));
    line("Age:", format_age(row.age_secs));
    line(
        "Agent:",
        dash(Some(row.agent.clone()).filter(|a| !a.is_empty())),
    );
    if let Some(e) = &row.acp_extra {
        line("Build:", render_build_cell(&e.build_version, e.build_stale));
        line("Model:", dash(e.model.clone()));
        line("CWD:", e.cwd.display().to_string());
        line("Socket:", e.socket.display().to_string());
        line("Alive:", e.alive.to_string());
        line(
            "Last attached:",
            dash(e.last_attached_at.map(|t| t.to_string())),
        );
        line("Detached:", dash(e.detached_at.map(|t| t.to_string())));
    }
    out
}

/// A leftover tmux session can share a structured session's id, so match the
/// instance's own substrate.
fn pick_row(rows: Vec<Row>, id: &str, substrate: Substrate) -> Option<Row> {
    rows.into_iter()
        .find(|r| r.id == id && r.substrate == substrate)
}

/// The row for `identifier`, or a dead placeholder when the instance resolves
/// but no live runtime does.
fn detail_row(profile: &str, profile_explicit: bool, identifier: &str) -> Result<Row> {
    let mut instances = load_instances(profile, profile_explicit);
    let inst = super::resolve_session(identifier, &instances)?;
    let substrate = if inst.is_structured() {
        Substrate::Acp
    } else {
        Substrate::Tmux
    };
    let (id, title, tool) = (inst.id.clone(), inst.title.clone(), inst.tool.clone());
    let created = inst.created_at.timestamp().max(0) as u64;

    let rows = collect_rows(&mut instances, SubstrateFilter::All, true);
    Ok(pick_row(rows, &id, substrate).unwrap_or(Row {
        id,
        title,
        substrate,
        state: "dead",
        pid: None,
        age_secs: Some(now_secs().saturating_sub(created)),
        agent: tool,
        is_orphan: false,
        acp_extra: None,
        started_at: 0,
    }))
}

/// Runs `frame` until it returns `false` or `shutdown` completes. `shutdown`
/// is held across frames so a signal arriving mid-frame is still delivered.
async fn watch_loop(
    interval: std::time::Duration,
    shutdown: impl std::future::Future<Output = ()>,
    mut frame: impl FnMut() -> Result<bool>,
) -> Result<()> {
    tokio::pin!(shutdown);
    while frame()? {
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = &mut shutdown => break,
        }
    }
    Ok(())
}

#[tracing::instrument(target = "cli.ps", skip_all, fields(profile = %profile))]
pub async fn run(profile: &str, profile_explicit: bool, args: PsArgs) -> Result<()> {
    if let Some(identifier) = &args.session {
        let row = detail_row(profile, profile_explicit, identifier)?;
        if args.json {
            super::output::print_json(&detail_json(&row))?;
        } else {
            print!("{}", render_detail(&row));
        }
        return Ok(());
    }

    let filter = if args.tmux {
        SubstrateFilter::Tmux
    } else if args.acp {
        SubstrateFilter::Acp
    } else {
        SubstrateFilter::All
    };

    let tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    watch_loop(
        std::time::Duration::from_secs(args.interval),
        shutdown,
        || {
            let mut instances = load_instances(profile, profile_explicit);
            let rows = collect_rows(&mut instances, filter, args.dead);
            let body = render_rows(&rows, args.acp, args.json, args.watch)?;
            if args.watch {
                print!("{}", watch_frame(body, args.json, tty));
            } else {
                print!("{body}");
            }
            Ok(args.watch)
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

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
            acp_extra: Some(AcpExtra {
                build_version: "1.9.5+gabc123".to_string(),
                build_stale: false,
                socket: std::path::PathBuf::from("/tmp/w.sock"),
                cwd: std::path::PathBuf::from("/repo"),
                model: Some("claude-opus-4-7".to_string()),
                alive: state != "dead",
                last_attached_at: None,
                detached_at: None,
            }),
        }
    }

    fn row(id: &str, title: &str, is_orphan: bool) -> Row {
        Row {
            id: id.to_string(),
            title: title.to_string(),
            substrate: Substrate::Tmux,
            state: "running",
            pid: None,
            age_secs: None,
            agent: String::new(),
            is_orphan,
            acp_extra: None,
            started_at: 0,
        }
    }

    #[test]
    fn merge_joins_each_substrate_to_its_instance() {
        let instances = vec![inst("abcd1234ef567890", "My Session")];
        let tmux = vec![tmux_state("aoe_My_Session_abcd1234", Status::Running)];
        let rows = merge_rows(&instances, &tmux, vec![], 2000, SubstrateFilter::All, false);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].id, "abcd1234ef567890",
            "tmux joins on the id suffix"
        );
        assert_eq!(rows[0].title, "My Session");
        assert_eq!(rows[0].state, "running");
        assert_eq!(rows[0].age_secs, Some(1000));
        assert!(!rows[0].is_orphan);

        let instances = vec![inst("full-session-id-1234", "Structured")];
        let acp = vec![acp_state("full-session-id-1234", "attached", 500)];
        let rows = merge_rows(&instances, &[], acp, 2000, SubstrateFilter::All, false);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].substrate,
            Substrate::Acp,
            "acp joins on the full id"
        );
        assert_eq!(rows[0].title, "Structured");
        assert_eq!(rows[0].pid, Some(7));
        assert_eq!(rows[0].age_secs, Some(1000));
        assert!(!rows[0].is_orphan);
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
    fn records_without_an_instance_are_orphans_shown_only_with_dead() {
        let tmux = vec![tmux_state("aoe_Ghost_99999999", Status::Running)];
        assert!(
            merge_rows(&[], &tmux, vec![], 2000, SubstrateFilter::All, false).is_empty(),
            "orphan is hidden without --dead"
        );
        let shown = merge_rows(&[], &tmux, vec![], 2000, SubstrateFilter::All, true);
        assert_eq!(shown.len(), 1);
        assert!(shown[0].is_orphan);
        assert_eq!(shown[0].id, "99999999");
        assert_eq!(shown[0].age_secs, Some(500));

        let acp = || vec![acp_state("gone", "attached", 500)];
        assert!(merge_rows(&[], &[], acp(), 1, SubstrateFilter::All, false).is_empty());
        let shown = merge_rows(&[], &[], acp(), 2000, SubstrateFilter::All, true);
        assert_eq!(shown.len(), 1);
        assert!(shown[0].is_orphan);
        assert_eq!(shown[0].age_secs, Some(1500));
    }

    #[test]
    fn filters_hide_dead_rows_and_select_one_substrate() {
        let instances = vec![inst("abcd1234ef567890", "Dead One")];
        let tmux = vec![tmux_state("aoe_Dead_One_abcd1234", Status::Error)];
        assert!(
            merge_rows(&instances, &tmux, vec![], 0, SubstrateFilter::All, false).is_empty(),
            "dead is hidden by default"
        );
        let shown = merge_rows(&instances, &tmux, vec![], 0, SubstrateFilter::All, true);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].state, "dead");
        assert!(!shown[0].is_orphan);

        let instances = vec![inst("abcd1234ef567890", "T"), inst("acp-id-1", "A")];
        let tmux = vec![tmux_state("aoe_T_abcd1234", Status::Running)];
        let acp = || vec![acp_state("acp-id-1", "attached", 0)];
        for (filter, expected) in [
            (SubstrateFilter::Tmux, Substrate::Tmux),
            (SubstrateFilter::Acp, Substrate::Acp),
        ] {
            let rows = merge_rows(&instances, &tmux, acp(), 0, filter, false);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].substrate, expected);
        }

        let dead_acp = || vec![acp_state("gone", "dead", 0)];
        assert!(
            merge_rows(&[], &[], dead_acp(), 0, SubstrateFilter::Acp, false).is_empty(),
            "a dead acp orphan is hidden under --acp without --dead"
        );
        let shown = merge_rows(&[], &[], dead_acp(), 0, SubstrateFilter::Acp, true);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].substrate, Substrate::Acp);
        assert_eq!(shown[0].state, "dead");
    }

    #[test]
    fn merge_sorts_tmux_first_then_by_title_and_id() {
        let instances = vec![
            inst("2222aaaabbbbcccc", "Same"),
            inst("1111aaaabbbbcccc", "Same"),
            inst("3333ffff00001111", "Alpha"),
            inst("acp-id-1", "Alpha"),
        ];
        let tmux = vec![
            tmux_state("aoe_Same_2222aaaa", Status::Running),
            tmux_state("aoe_Same_1111aaaa", Status::Running),
            tmux_state("aoe_Alpha_3333ffff", Status::Running),
        ];
        let acp = vec![acp_state("acp-id-1", "attached", 0)];
        let rows = merge_rows(&instances, &tmux, acp, 0, SubstrateFilter::All, false);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].title, "Alpha");
        assert_eq!(rows[1].id, "1111aaaabbbbcccc");
        assert_eq!(rows[2].id, "2222aaaabbbbcccc");
        assert_eq!(rows[3].substrate, Substrate::Acp, "acp sorts after tmux");
    }

    #[test]
    fn render_json_projects_stable_schema() {
        let instances = vec![inst("abcd1234ef567890", "My Session")];
        let tmux = vec![tmux_state("aoe_My_Session_abcd1234", Status::Running)];
        let rows = merge_rows(&instances, &tmux, vec![], 2000, SubstrateFilter::All, false);
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
        assert_eq!(row.as_object().unwrap().len(), 6);

        assert!(rows_json(&[]).is_empty());
        assert_eq!(serde_json::to_string(&rows_json(&[])).unwrap(), "[]");
    }

    #[test]
    fn render_table_has_header_underline_and_row() {
        let instances = vec![inst("abcd1234ef567890", "My Session")];
        let tmux = vec![tmux_state("aoe_My_Session_abcd1234", Status::Running)];
        let rows = merge_rows(&instances, &tmux, vec![], 2000, SubstrateFilter::All, false);
        let table = render_table(&rows);
        for cell in [
            "SESSION",
            "SUBSTRATE",
            "----",
            "abcd1234",
            "tmux",
            "running",
            "claude",
        ] {
            assert!(table.contains(cell), "table missing {cell}: {table}");
        }

        let cell = session_cell(&row(
            "abcd1234ef567890",
            "A very long session title that exceeds the budget",
            false,
        ));
        assert!(cell.starts_with("abcd1234 "));
        assert!(cell.contains("..."), "long title is truncated: {cell}");
        assert!(
            !cell.contains("exceeds the budget"),
            "the tail of an over-budget title is dropped: {cell}"
        );
        assert_eq!(session_cell(&row("99999999", "", true)), "99999999");
    }

    #[test]
    fn acp_table_appends_acp_columns_and_core_table_omits_them() {
        let instances = vec![inst("acp-id-1", "Structured")];
        let acp = vec![acp_state("acp-id-1", "attached", 0)];
        let rows = merge_rows(&instances, &[], acp, 2000, SubstrateFilter::Acp, false);
        let acp_table = render_table_acp(&rows);
        for cell in [
            "SESSION",
            "SUBSTRATE",
            "STATE",
            "AGENT",
            "BUILD",
            "MODEL",
            "CWD",
            "SOCKET",
            "1.9.5+gabc123",
            "/tmp/w.sock",
            "/repo",
            "claude-opus-4-7",
        ] {
            assert!(acp_table.contains(cell), "acp table missing {cell}");
        }

        let core = render_table(&rows);
        for hidden in ["BUILD", "SOCKET"] {
            assert!(
                !core.contains(hidden),
                "core view must not unlock ACP columns"
            );
        }
    }

    #[test]
    fn acp_table_renders_orphan_row_with_absent_model_as_dash() {
        let mut acp = acp_state("gone", "attached", 500);
        if let Some(extra) = acp.acp_extra.as_mut() {
            extra.model = None;
            extra.build_version = String::new();
            extra.build_stale = true;
        }
        let rows = merge_rows(&[], &[], vec![acp], 2000, SubstrateFilter::Acp, true);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].is_orphan, "no instance join, so this is an orphan");
        let table = render_table_acp(&rows);
        assert!(
            table.contains("<legacy> (stale)"),
            "empty build shows <legacy>"
        );
        let model_cell = format!("{:<width$}", "-", width = COL_MODEL);
        assert!(
            table.contains(&model_cell),
            "absent model renders as a dash cell of width {COL_MODEL}: {table}"
        );

        let cases = [
            ("1.9.5+gabc123", false, "1.9.5+gabc123"),
            ("1.9.4+gdeadbe", true, "1.9.4+gdeadbe (stale)"),
            ("", true, "<legacy> (stale)"),
        ];
        for (version, stale, expected) in cases {
            assert_eq!(render_build_cell(version, stale), expected, "{version:?}");
        }
    }

    #[test]
    fn acp_json_schema_carries_every_old_key_plus_the_additions() {
        use crate::process::worker_registry::WorkerRecord;
        use std::path::PathBuf;

        let rec = WorkerRecord::new(
            "acp-id-1".into(),
            7,
            PathBuf::from("/tmp/w.sock"),
            "claude-agent-acp".into(),
            "claude".into(),
            PathBuf::from("/repo"),
            Some("claude-opus-4-7".into()),
            vec![],
            vec![],
            None,
            None,
        );
        let instances = vec![inst("acp-id-1", "Structured")];
        let rows = merge_rows(
            &instances,
            &[],
            vec![acp_state_from_record(rec)],
            2000,
            SubstrateFilter::Acp,
            true,
        );
        let v = serde_json::to_value(acp_rows_json(&rows)).unwrap();
        let obj = v.as_array().unwrap()[0].as_object().unwrap();

        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "age_secs",
                "agent",
                "alive",
                "build_stale",
                "build_version",
                "cwd",
                "detached_at",
                "last_attached_at",
                "model",
                "pid",
                "session_id",
                "socket",
                "started_at",
                "state",
                "substrate",
            ],
            "the 11 keys `aoe acp ps --json` emitted, plus substrate/state/age_secs/model"
        );
        assert_eq!(obj["session_id"], "acp-id-1");
        assert_eq!(obj["pid"], 7);
        assert_eq!(obj["agent"], "claude-agent-acp");
        assert_eq!(obj["model"], "claude-opus-4-7");
        assert_eq!(obj["socket"], "/tmp/w.sock");
        assert_eq!(obj["cwd"], "/repo");
        assert_eq!(obj["substrate"], "acp");
        assert!(obj["last_attached_at"].is_null());
        assert!(obj["detached_at"].is_null());
    }

    #[test]
    fn agent_session_name_uses_marker_then_name_shape() {
        use crate::tmux::{SESSION_PREFIX, TERMINAL_PREFIX, TOOL_PREFIX};
        let cases = [
            (format!("{SESSION_PREFIX}Foo_abcd1234"), None, true),
            (
                format!("{TERMINAL_PREFIX}Foo_abcd1234"),
                Some("agent"),
                true,
            ),
            (
                format!("{TERMINAL_PREFIX}Foo_abcd1234"),
                Some("term"),
                false,
            ),
            (format!("{TERMINAL_PREFIX}Foo_abcd1234"), None, false),
            (format!("{TOOL_PREFIX}lazygit_Foo_abcd1234"), None, false),
            (format!("{TOOL_PREFIX}Foo_abcd1234"), Some("agent"), true),
            ("other_Foo_abcd1234".to_string(), Some("agent"), false),
        ];
        for (name, marker, expected) in cases {
            assert_eq!(
                is_agent_session_name(&name, marker),
                expected,
                "{name} {marker:?}"
            );
        }
    }

    #[test]
    fn detail_renders_text_and_json_with_acp_extras_only_for_acp() {
        let instances = vec![inst("abcd1234ef567890", "T"), inst("acp-id-1", "A")];
        let tmux = vec![tmux_state("aoe_T_abcd1234", Status::Running)];
        let acp = vec![acp_state("acp-id-1", "attached", 0)];
        let rows = merge_rows(&instances, &tmux, acp, 2000, SubstrateFilter::All, true);

        let text = render_detail(&rows[0]);
        assert!(text.contains("Session:") && text.contains("abcd1234ef567890"));
        assert!(!text.contains("Socket:"), "tmux detail has no ACP fields");
        let v = serde_json::to_value(detail_json(&rows[0])).unwrap();
        assert!(v.get("socket").is_none());
        assert_eq!(v["session"], "abcd1234ef567890");

        let text = render_detail(&rows[1]);
        for cell in ["Socket:", "/tmp/w.sock", "claude-opus-4-7", "1.9.5+gabc123"] {
            assert!(text.contains(cell), "missing {cell}: {text}");
        }
        let v = serde_json::to_value(detail_json(&rows[1])).unwrap();
        assert_eq!(v["socket"], "/tmp/w.sock");
        assert_eq!(v["substrate"], "acp");
    }

    #[test]
    fn pick_row_matches_the_instance_substrate_when_ids_collide() {
        let mut tmux = row("acp-id-1", "A", false);
        tmux.pid = Some(1);
        let mut acp = row("acp-id-1", "A", false);
        acp.substrate = Substrate::Acp;
        acp.pid = Some(2);
        let picked = pick_row(vec![tmux, acp], "acp-id-1", Substrate::Acp).unwrap();
        assert_eq!(picked.pid, Some(2));
        assert!(pick_row(vec![row("other", "", false)], "acp-id-1", Substrate::Acp).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn watch_loop_sees_a_shutdown_delivered_during_a_frame() {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let mut tx = Some(tx);
        let mut frames = 0;
        let interval = std::time::Duration::from_secs(3600);
        watch_loop(
            interval,
            async {
                let _ = rx.await;
            },
            || {
                frames += 1;
                assert!(frames <= 2, "shutdown was lost");
                if frames == 2 {
                    let _ = tx.take().unwrap().send(());
                }
                Ok(true)
            },
        )
        .await
        .unwrap();
        assert_eq!(frames, 2);

        let mut frames = 0;
        let never = std::future::pending::<()>();
        watch_loop(interval, never, || {
            frames += 1;
            Ok(false)
        })
        .await
        .unwrap();
        assert_eq!(frames, 1, "a false frame ends the loop without waiting");
    }

    #[test]
    fn watch_frame_clears_only_table_output_on_a_tty() {
        assert!(watch_frame("x".into(), false, true).starts_with("\x1b[2J\x1b[H"));
        assert_eq!(watch_frame("x".into(), false, false), "x");
        assert_eq!(watch_frame("[]\n".into(), true, true), "[]\n");
    }

    #[test]
    fn render_rows_json_is_compact_only_when_asked() {
        let rows = vec![row("abcd1234ef567890", "T", false)];
        let compact = render_rows(&rows, false, true, true).unwrap();
        assert_eq!(compact.trim_end().lines().count(), 1);
        let pretty = render_rows(&rows, false, true, false).unwrap();
        assert!(pretty.lines().count() > 1);
        assert_eq!(
            render_rows(&[], false, false, false).unwrap(),
            "No running sessions.\n"
        );
    }

    #[test]
    fn clap_validates_watch_interval_and_detail_conflicts() {
        use clap::Parser;
        #[derive(Parser)]
        struct W {
            #[command(flatten)]
            args: PsArgs,
        }
        let ok =
            |a: &[&str]| W::try_parse_from(std::iter::once("ps").chain(a.iter().copied())).is_ok();
        assert!(ok(&[]));
        assert!(ok(&["--watch", "--interval", "5", "--json"]));
        assert!(ok(&["abcd"]));
        assert!(!ok(&["--interval", "5"]), "interval needs --watch");
        assert!(!ok(&["--watch", "--interval", "0"]));
        assert!(!ok(&["--watch", "abcd"]));
        assert!(!ok(&["--tmux", "abcd"]));
        assert!(!ok(&["--acp", "abcd"]));
    }

    #[test]
    #[serial_test::serial]
    fn load_instances_stamps_source_profile() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = crate::session::test_support::isolate_app_dir_at(temp.path());

        let storage = Storage::new_unwatched("pstest").unwrap();
        storage
            .update(|i, _| {
                *i = vec![Instance::new("sess", "/tmp/sess")];
                Ok(())
            })
            .unwrap();

        let loaded = load_instances("pstest", true);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].source_profile, "pstest");
    }
}
