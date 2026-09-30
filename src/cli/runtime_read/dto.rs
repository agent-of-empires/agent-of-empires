use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::server::runtime_ws::PROTOCOL_VERSION;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HelloData {
    pub protocol_version: u16,
    pub runtime_epoch: String,
    pub prebind_instance_id: String,
    pub runtime_instance_id: String,
    pub namespace: String,
    pub owner: Owner,
    pub local_owner: bool,
    pub health: AggregateHealth,
    pub profiles: Vec<ProfileHello>,
    pub status_freshness: StatusFreshness,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SnapshotData {
    pub namespace: String,
    pub cursor: Cursor,
    pub health: SnapshotHealth,
    pub default_profile: Option<String>,
    /// What the daemon resolved as the default, published whether or not the
    /// snapshot carries it. `default_profile` says whether that name resolves;
    /// this says which name, so a client that has to refuse can name the
    /// profile the user has to create.
    pub resolved_default_profile: Option<String>,
    pub profiles: Vec<ProfileRead>,
    pub sessions: Vec<SessionRead>,
    pub global_projects: Vec<ProjectRead>,
    pub status_freshness: StatusFreshness,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Cursor {
    pub epoch: String,
    pub revision: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Owner {
    pub kind: OwnerKind,
    pub uid: Option<u32>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OwnerKind {
    LocalOwner,
    Remote,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum AggregateHealth {
    Healthy,
    Degraded { code: HealthCode },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SnapshotHealth {
    pub global_enumeration: ComponentHealth,
    pub global_metadata: ComponentHealth,
    #[serde(deserialize_with = "deserialize_profile_health_map")]
    pub profiles: BTreeMap<String, ProfileHealth>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ComponentHealth {
    Healthy,
    Degraded { code: HealthCode },
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HealthCode {
    Enumeration,
    ProfileEnumeration,
    Metadata,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProfileHealth {
    pub profile_enumeration: ComponentHealth,
    pub metadata: ComponentHealth,
    pub profile_data: ComponentHealth,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum StatusFreshness {
    Unobserved {
        revision: u64,
        observed_at: Option<String>,
    },
    Observed {
        revision: u64,
        observed_at: String,
    },
    Unavailable {
        revision: Option<u64>,
        observed_at: Option<String>,
    },
}

/// What the Hello says about one profile: its name, and whether the daemon
/// could read it at all. The inventory is not repeated, because the Snapshot
/// that follows carries every group and project of every profile and each of
/// them is validated there.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProfileHello {
    pub name: String,
    pub health: ProfileHealth,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProfileRead {
    pub name: String,
    pub groups: Vec<GroupRead>,
    pub projects: Vec<ProjectRead>,
    pub health: ProfileHealth,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GroupRead {
    pub name: String,
    pub path: String,
    pub children: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectRead {
    pub name: String,
    pub path: String,
    /// Opaque equality metadata the producer computed on its own filesystem.
    /// Two rows naming one directory share it, which is what the merge needs;
    /// it is not a path, is not rendered, and is never resolved here, because
    /// resolving it would resolve it against the reader's machine.
    ///
    /// Required, and validated for type and non-emptiness only: a stored alias
    /// may resolve to a target whose characters a rendered path may not carry,
    /// and a key is not rendered. A frame without one is refused rather than
    /// merged on a path, because that fallback is the defect it replaces.
    pub merge_key: String,
    pub scope: ProjectScope,
    pub default_base_branch: Option<String>,
    /// Whether the row came from a project registry rather than being
    /// synthesized so a session's project path resolves. Only a synthesized
    /// row may be absent from both registries, and a session row of the same
    /// profile must justify it.
    ///
    /// Required, and not defaulted, because both published schemas already
    /// list it. A missing flag is a missing answer, and the only direction a
    /// default here could be safe in would be one that hides rows.
    pub registered: bool,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ProjectScope {
    Global,
    Profile,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionRead {
    pub id: String,
    pub title: String,
    pub project_path: String,
    pub group_path: String,
    pub tool: String,
    pub command: String,
    pub profile: String,
    pub status: WireStatus,
    pub state: WireState,
    pub created_at: String,
    pub last_accessed_at: Option<String>,
    pub idle_entered_at: Option<String>,
    pub last_error: Option<String>,
    pub archived_at: Option<String>,
    pub trashed_at: Option<String>,
    pub active_snoozed_until: Option<String>,
    pub pinned_at: Option<String>,
    pub agent_session_id: Option<String>,
    pub parent_session_id: Option<String>,
    pub has_worktree_info: bool,
    pub has_managed_worktree: bool,
    pub worktree: Option<WorktreeRead>,
    pub workspace_repos: Vec<WorkspaceRepo>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub(crate) enum WireStatus {
    Running,
    Waiting,
    Idle,
    Unknown,
    Stopped,
    Error,
    Starting,
    Deleting,
    Creating,
}

impl WireStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Running => "Running",
            Self::Waiting => "Waiting",
            Self::Idle => "Idle",
            Self::Unknown => "Unknown",
            Self::Stopped => "Stopped",
            Self::Error => "Error",
            Self::Starting => "Starting",
            Self::Deleting => "Deleting",
            Self::Creating => "Creating",
        }
    }

    /// The machine-readable spelling every JSON projection uses. The human
    /// `Status:` line keeps the PascalCase wire form; a consumer parsing this
    /// output has always seen lowercase, as `aoe session show --json` did
    /// before the read existed.
    pub(crate) fn json_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Idle => "idle",
            Self::Unknown => "unknown",
            Self::Stopped => "stopped",
            Self::Error => "error",
            Self::Starting => "starting",
            Self::Deleting => "deleting",
            Self::Creating => "creating",
        }
    }

    /// The local status this wire value names, so the renderer's tallies are
    /// the local command's own rather than a second set of rules.
    pub(crate) fn session_status(self) -> crate::session::Status {
        use crate::session::Status;
        match self {
            Self::Running => Status::Running,
            Self::Waiting => Status::Waiting,
            Self::Idle => Status::Idle,
            Self::Unknown => Status::Unknown,
            Self::Stopped => Status::Stopped,
            Self::Error => Status::Error,
            Self::Starting => Status::Starting,
            Self::Deleting => Status::Deleting,
            Self::Creating => Status::Creating,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum WireState {
    Live,
    Archived,
    Trashed,
}

impl WireState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Archived => "archived",
            Self::Trashed => "trashed",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorktreeRead {
    pub branch: String,
    pub main_repo_path: String,
    pub managed_by_aoe: bool,
    pub base_branch: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkspaceRepo {
    pub name: String,
    pub source_path: String,
    pub branch: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(
    tag = "kind",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum ApplicationFrame {
    Hello(HelloData),
    Snapshot(SnapshotData),
}

pub(crate) fn parse_hello(bytes: &[u8]) -> Result<HelloData, HelloParseError> {
    if let Ok(probe) = serde_json::from_slice::<HelloVersionProbe>(bytes) {
        if probe.protocol_version != PROTOCOL_VERSION {
            return Err(HelloParseError::ProtocolVersion);
        }
    }
    match serde_json::from_slice::<ApplicationFrame>(bytes) {
        Ok(ApplicationFrame::Hello(data)) => Ok(data),
        Ok(ApplicationFrame::Snapshot(_)) => Err(HelloParseError::Schema),
        Err(_) => Err(HelloParseError::Schema),
    }
}

pub(crate) fn parse_snapshot(bytes: &[u8]) -> Result<SnapshotData, ()> {
    match serde_json::from_slice::<ApplicationFrame>(bytes) {
        Ok(ApplicationFrame::Snapshot(data)) => Ok(data),
        _ => Err(()),
    }
}

#[derive(Debug)]
pub(crate) enum HelloParseError {
    ProtocolVersion,
    Schema,
}

struct HelloVersionProbe {
    protocol_version: u16,
}

impl<'de> Deserialize<'de> for HelloVersionProbe {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ProbeVisitor;

        impl<'de> Visitor<'de> for ProbeVisitor {
            type Value = HelloVersionProbe;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a protocol 2 hello envelope")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut kind: Option<String> = None;
                let mut data: Option<serde_json::Value> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "kind" if kind.replace(map.next_value()?).is_none() => {}
                        "data" if data.replace(map.next_value()?).is_none() => {}
                        _ => return Err(de::Error::duplicate_field("field")),
                    }
                }
                if kind.as_deref() != Some("hello") {
                    return Err(de::Error::custom("not a hello frame"));
                }
                let data = data.ok_or_else(|| de::Error::missing_field("data"))?;
                let mut object = match data {
                    serde_json::Value::Object(object) => object,
                    _ => return Err(de::Error::custom("hello data is not an object")),
                };
                let version = object
                    .remove("protocol_version")
                    .ok_or_else(|| de::Error::missing_field("protocol_version"))?;
                Ok(HelloVersionProbe {
                    protocol_version: serde_json::from_value(version)
                        .map_err(|_| de::Error::custom("invalid protocol version"))?,
                })
            }
        }

        deserializer.deserialize_map(ProbeVisitor)
    }
}

fn deserialize_profile_health_map<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<String, ProfileHealth>, D::Error>
where
    D: Deserializer<'de>,
{
    struct MapVisitor;

    impl<'de> Visitor<'de> for MapVisitor {
        type Value = BTreeMap<String, ProfileHealth>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a map of profile health values")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut output = BTreeMap::new();
            while let Some(key) = map.next_key::<String>()? {
                let value = map.next_value()?;
                if output.insert(key.clone(), value).is_some() {
                    return Err(de::Error::custom(format!(
                        "duplicate profile health key {key:?}"
                    )));
                }
            }
            Ok(output)
        }
    }

    deserializer.deserialize_map(MapVisitor)
}

pub(crate) fn valid_namespace(value: &str) -> bool {
    let Some((kind, suffix)) = value.split_once(':') else {
        return false;
    };
    matches!(kind, "debug" | "release")
        && (1..=200).contains(&suffix.len())
        && suffix.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
}

pub(crate) fn valid_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
        })
}

pub(crate) fn validate_hello(hello: &HelloData) -> Result<(), &'static str> {
    if hello.protocol_version != PROTOCOL_VERSION {
        return Err("protocol_mismatch");
    }
    if !valid_namespace(&hello.namespace)
        || !valid_uuid(&hello.runtime_epoch)
        || !valid_uuid(&hello.prebind_instance_id)
        || !valid_uuid(&hello.runtime_instance_id)
    {
        return Err("schema_invalid");
    }
    match (hello.local_owner, hello.owner.kind, hello.owner.uid) {
        (true, OwnerKind::LocalOwner, Some(_)) => {}
        (false, OwnerKind::Remote, None) => {}
        _ => return Err("schema_invalid"),
    }
    validate_profile_hellos(&hello.profiles)?;
    validate_freshness(&hello.status_freshness)?;
    if let AggregateHealth::Degraded { code } = hello.health {
        validate_health_code(code, &hello_health_codes())?;
    }
    Ok(())
}

pub(crate) fn validate_snapshot(snapshot: &SnapshotData) -> Result<(), &'static str> {
    if !valid_namespace(&snapshot.namespace)
        || !valid_uuid(&snapshot.cursor.epoch)
        || snapshot.cursor.revision == 0
    {
        return Err("schema_invalid");
    }
    validate_global_health(snapshot.health.global_enumeration)?;
    validate_global_health(snapshot.health.global_metadata)?;
    validate_profiles(&snapshot.profiles)?;
    if snapshot.health.profiles.len() != snapshot.profiles.len() {
        return Err("schema_invalid");
    }
    for profile in &snapshot.profiles {
        let health = snapshot
            .health
            .profiles
            .get(&profile.name)
            .ok_or("schema_invalid")?;
        if serde_json::to_vec(health).ok() != serde_json::to_vec(&profile.health).ok() {
            return Err("schema_invalid");
        }
        validate_profile_health(&profile.health)?;
    }
    // `default_profile` names a row this snapshot carries or names nothing.
    // The resolved name beside it may name no row, and that is not a schema
    // fault: it is the state a client reports by name.
    if let Some(default) = &snapshot.default_profile {
        if !snapshot
            .profiles
            .iter()
            .any(|profile| &profile.name == default)
        {
            return Err("schema_invalid");
        }
    }
    if let Some(resolved) = &snapshot.resolved_default_profile {
        validate_safe_text(resolved)?;
    }

    validate_freshness(&snapshot.status_freshness)?;
    // Session rows keep the store's own order, because that is the order a
    // local `aoe list` prints them in. Distinct ids are the identity rule.
    let mut unique_ids: HashSet<&str> = HashSet::new();
    if snapshot
        .sessions
        .iter()
        .any(|session| !unique_ids.insert(session.id.as_str()))
    {
        return Err("schema_invalid");
    }
    validate_projects(&snapshot.global_projects, ProjectScope::Global)?;
    let profile_names: HashSet<&str> = snapshot
        .profiles
        .iter()
        .map(|profile| profile.name.as_str())
        .collect();
    let mut profile_groups: HashMap<&str, HashSet<&str>> = HashMap::new();
    let mut profile_projects: HashMap<&str, HashSet<&str>> = HashMap::new();
    // Every project path a session of that profile uses, so an unregistered
    // project row can be checked against the row that justifies it.
    let mut profile_session_paths: HashMap<&str, HashSet<&str>> = HashMap::new();
    for session in &snapshot.sessions {
        profile_session_paths
            .entry(session.profile.as_str())
            .or_default()
            .insert(session.project_path.as_str());
    }
    for profile in &snapshot.profiles {
        profile_groups.insert(
            profile.name.as_str(),
            profile
                .groups
                .iter()
                .map(|group| group.path.as_str())
                .collect(),
        );
        profile_projects.insert(
            profile.name.as_str(),
            profile
                .projects
                .iter()
                .map(|project| project.path.as_str())
                .collect(),
        );
        // An unregistered row exists only to make a session's project path
        // resolve, so a session row of this profile must name it. Without
        // that, a server could withhold a registry project by relabelling it.
        let used = profile_session_paths
            .get(profile.name.as_str())
            .cloned()
            .unwrap_or_default();
        if profile
            .projects
            .iter()
            .any(|project| !project.registered && !used.contains(project.path.as_str()))
        {
            return Err("schema_invalid");
        }
    }
    let global_projects: HashSet<&str> = snapshot
        .global_projects
        .iter()
        .map(|project| project.path.as_str())
        .collect();
    let mut session_ids = HashSet::new();
    let mut parents: HashMap<&str, (&str, Option<&str>)> = HashMap::new();
    for session in &snapshot.sessions {
        if !session_ids.insert(session.id.as_str())
            || !profile_names.contains(session.profile.as_str())
        {
            return Err("schema_invalid");
        }
        validate_session(session)?;
        let groups = profile_groups
            .get(session.profile.as_str())
            .ok_or("schema_invalid")?;
        let projects = profile_projects
            .get(session.profile.as_str())
            .ok_or("schema_invalid")?;
        if (!session.group_path.is_empty() && !groups.contains(session.group_path.as_str()))
            || (!projects.contains(session.project_path.as_str())
                && !global_projects.contains(session.project_path.as_str()))
        {
            return Err("schema_invalid");
        }
        parents.insert(
            session.id.as_str(),
            (
                session.profile.as_str(),
                session.parent_session_id.as_deref(),
            ),
        );
    }
    for (id, (profile, parent)) in &parents {
        if let Some(parent) = parent {
            // A parent that names no row is persisted state, not corruption: the
            // local path keeps and prints the stored id, and `rm --purge` of a
            // parent leaves the child pointing at nothing. So the graph rules
            // that can still be stated apply to the part that is there: the
            // parent, when it is a row at all, sits in the same profile, and the
            // edges that do land on a row form no cycle.
            if let Some((parent_profile, _)) = parents.get(parent) {
                if parent_profile != profile {
                    return Err("schema_invalid");
                }
            }
            let mut seen = HashSet::new();
            let mut cursor = Some(*id);
            while let Some(node) = cursor {
                if !seen.insert(node) {
                    return Err("schema_invalid");
                }
                cursor = parents.get(node).and_then(|(_, parent)| *parent);
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_cross_message(
    hello: &HelloData,
    snapshot: &SnapshotData,
    local_owner: Option<u32>,
) -> Result<(), &'static str> {
    if hello.runtime_epoch != snapshot.cursor.epoch {
        return Err("schema_invalid");
    }
    let hello_names: Vec<&str> = hello
        .profiles
        .iter()
        .map(|profile| profile.name.as_str())
        .collect();
    let snapshot_names: Vec<&str> = snapshot
        .profiles
        .iter()
        .map(|profile| profile.name.as_str())
        .collect();
    if hello_names != snapshot_names {
        return Err("schema_invalid");
    }
    match local_owner {
        Some(uid) => {
            if hello.namespace != snapshot.namespace
                || !hello.local_owner
                || hello.owner.kind != OwnerKind::LocalOwner
                || hello.owner.uid != Some(uid)
            {
                return Err("peer_identity");
            }
        }
        None => {
            if hello.namespace != snapshot.namespace
                || hello.local_owner
                || hello.owner.kind != OwnerKind::Remote
                || hello.owner.uid.is_some()
            {
                return Err("schema_invalid");
            }
        }
    }
    Ok(())
}

/// Profiles in the producer's own order, which is the order a local
/// `aoe profile` prints them in. The wire therefore carries presentation, and
/// the identity rule is uniqueness rather than a canonical sort.
fn validate_profiles(profiles: &[ProfileRead]) -> Result<(), &'static str> {
    let mut names = HashSet::new();
    for profile in profiles {
        if !names.insert(profile.name.as_str()) {
            return Err("schema_invalid");
        }
        validate_safe_text(&profile.name)?;
        validate_profile_health(&profile.health)?;
        validate_groups(&profile.groups)?;
        validate_projects(&profile.projects, ProjectScope::Profile)?;
    }
    Ok(())
}

/// The Hello's half of the same rule. Everything the handshake needs in order
/// to fail closed before the much larger Snapshot is accepted is here: unique
/// names, a name that is safe to print, and per-profile health. The inventory
/// itself belongs to the Snapshot, which carries it and validates every group
/// and project in it.
fn validate_profile_hellos(profiles: &[ProfileHello]) -> Result<(), &'static str> {
    let mut names = HashSet::new();
    for profile in profiles {
        if !names.insert(profile.name.as_str()) {
            return Err("schema_invalid");
        }
        validate_safe_text(&profile.name)?;
        validate_profile_health(&profile.health)?;
    }
    Ok(())
}

fn validate_profile_health(health: &ProfileHealth) -> Result<(), &'static str> {
    validate_profile_component(health.profile_enumeration)?;
    validate_profile_component(health.metadata)?;
    validate_profile_component(health.profile_data)
}

/// One health code against the components that may actually carry it. The
/// client's vocabulary is the producer's, and each component is degraded by
/// its own failure, so a code outside the component's own set is a schema
/// violation rather than a fact about the daemon.
fn validate_health_code(code: HealthCode, allowed: &[HealthCode]) -> Result<(), &'static str> {
    if allowed.contains(&code) {
        Ok(())
    } else {
        Err("schema_invalid")
    }
}

/// The codes a global component is degraded by.
const GLOBAL_HEALTH_CODES: &[HealthCode] = &[HealthCode::Enumeration, HealthCode::Metadata];

/// The codes a profile-scoped component is degraded by.
const PROFILE_COMPONENT_CODES: &[HealthCode] =
    &[HealthCode::ProfileEnumeration, HealthCode::Metadata];

/// The aggregate a Hello may carry: exactly the union of the codes the
/// components folded into it can each carry, so the roll-up cannot name a code
/// the components it rolls up are incapable of. Derived rather than written
/// out, because a hand-written subset of the union is a way to refuse a Hello
/// the producer is entitled to send.
fn hello_health_codes() -> Vec<HealthCode> {
    let mut codes: Vec<HealthCode> = Vec::new();
    for component in [GLOBAL_HEALTH_CODES, PROFILE_COMPONENT_CODES] {
        for code in component {
            if !codes.contains(code) {
                codes.push(*code);
            }
        }
    }
    codes
}

fn validate_global_health(health: ComponentHealth) -> Result<(), &'static str> {
    if let ComponentHealth::Degraded { code } = health {
        validate_health_code(code, GLOBAL_HEALTH_CODES)?;
    }
    Ok(())
}

fn validate_profile_component(health: ComponentHealth) -> Result<(), &'static str> {
    if let ComponentHealth::Degraded { code } = health {
        validate_health_code(code, PROFILE_COMPONENT_CODES)?;
    }
    Ok(())
}

/// Groups in the producer's own order, which is the order a local
/// `aoe group list` prints them in: the registry's insertion order, then the
/// groups the sessions imply. Every structural rule still holds; only the
/// canonical sort is gone, and distinct paths replace it.
fn validate_groups(groups: &[GroupRead]) -> Result<(), &'static str> {
    let mut unique: HashSet<&str> = HashSet::new();
    for group in groups {
        if !unique.insert(group.path.as_str()) {
            return Err("schema_invalid");
        }
    }
    let paths = unique;
    for group in groups {
        if !valid_group_path(&group.path) || !valid_text(&group.name) {
            return Err("schema_invalid");
        }
        let expected_name = group.path.rsplit('/').next().unwrap_or_default();
        if group.name != expected_name {
            return Err("schema_invalid");
        }
        // A child name must name a group that really sits directly under this
        // one, and no name twice. The producer does not have to list every
        // child: the local group tree hands out its rows without the child
        // lists filled in, so demanding completeness would refuse the very
        // rows the local `aoe group list --json` prints.
        let mut child_names: HashSet<&str> = HashSet::new();
        for child in &group.children {
            let child_path = format!("{}/{}", group.path, child);
            if !child_names.insert(child.as_str()) || !paths.contains(child_path.as_str()) {
                return Err("schema_invalid");
            }
        }
        if let Some((parent, _)) = group.path.rsplit_once('/') {
            if !paths.contains(parent) {
                return Err("schema_invalid");
            }
        }
    }
    Ok(())
}

/// Projects in the producer's own registry order, and unique by identity:
/// name, path and the registered flag together, so a synthesized row and the
/// registry row it stands in for are two identities rather than one repeated.
fn validate_projects(projects: &[ProjectRead], scope: ProjectScope) -> Result<(), &'static str> {
    let mut identities = HashSet::new();
    for project in projects {
        if project.scope as u8 != scope as u8
            // The global list is the global registry, so every row in it is
            // registered by definition; only a profile list may hold a
            // synthesized row.
            || (matches!(scope, ProjectScope::Global) && !project.registered)
            || !valid_text(&project.name)
            || !valid_stored_project_path(&project.path)
            || project.merge_key.is_empty()
            || project
                .default_base_branch
                .as_deref()
                .is_some_and(|value| !valid_text(value))
            || !identities.insert((
                project.name.as_str(),
                project.path.as_str(),
                project.registered,
            ))
        {
            return Err("schema_invalid");
        }
    }
    Ok(())
}

fn validate_session(session: &SessionRead) -> Result<(), &'static str> {
    for value in [&session.id, &session.title, &session.profile] {
        validate_safe_text(value)?;
    }
    for value in [&session.tool, &session.command] {
        if !valid_text(value) {
            return Err("schema_invalid");
        }
    }
    if session
        .agent_session_id
        .as_deref()
        .is_some_and(|value| !valid_text(value))
        || session
            .parent_session_id
            .as_deref()
            .is_some_and(|value| !valid_text(value))
    {
        return Err("schema_invalid");
    }
    if session
        .last_error
        .as_deref()
        .is_some_and(|value| !valid_text(value))
    {
        return Err("schema_invalid");
    }
    if !valid_stored_project_path(&session.project_path)
        || (!session.group_path.is_empty() && !valid_group_path(&session.group_path))
    {
        return Err("schema_invalid");
    }
    for value in [
        session.last_accessed_at.as_deref(),
        session.idle_entered_at.as_deref(),
        session.archived_at.as_deref(),
        session.trashed_at.as_deref(),
        session.active_snoozed_until.as_deref(),
        session.pinned_at.as_deref(),
    ] {
        if value.is_some_and(|value| !valid_timestamp(value)) {
            return Err("schema_invalid");
        }
    }
    if !valid_timestamp(&session.created_at) {
        return Err("schema_invalid");
    }
    // Compared as instants, not as bytes: a fractional part sorts below the
    // `Z` of a whole second, so a byte comparison would read
    // `…:00.5Z < …:00Z` and reject a session archived after it was created.
    let created_at = parse_timestamp(&session.created_at).ok_or("schema_invalid")?;
    for later in [
        session.archived_at.as_deref(),
        session.trashed_at.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        let later = parse_timestamp(later).ok_or("schema_invalid")?;
        if later < created_at {
            return Err("schema_invalid");
        }
    }
    match session.state {
        WireState::Live if session.archived_at.is_some() || session.trashed_at.is_some() => {
            return Err("schema_invalid")
        }
        WireState::Archived if session.archived_at.is_none() || session.trashed_at.is_some() => {
            return Err("schema_invalid")
        }
        WireState::Trashed if session.trashed_at.is_none() => return Err("schema_invalid"),
        _ => {}
    }
    if session.has_worktree_info != session.worktree.is_some() {
        return Err("schema_invalid");
    }
    if session.has_managed_worktree && session.worktree.is_none() {
        return Err("schema_invalid");
    }
    if let Some(worktree) = &session.worktree {
        if session.has_managed_worktree != worktree.managed_by_aoe
            || !valid_text(&worktree.branch)
            || !valid_absolute_path(&worktree.main_repo_path)
            || worktree
                .base_branch
                .as_deref()
                .is_some_and(|value| !valid_text(value))
        {
            return Err("schema_invalid");
        }
    }
    // Repos in the store's own order, like every other collection on the wire:
    // `aoe list --json` prints them the way the workspace stored them, so a
    // producer-side sort would make the array's order depend on the transport.
    // Identity is the rule a set-like collection can still refuse, and a
    // repeated (name, source_path) pair is not a thing a workspace holds.
    let mut repo_identities: HashSet<(&str, &str)> = HashSet::new();
    for repository in &session.workspace_repos {
        if !valid_text(&repository.name)
            || !valid_text(&repository.branch)
            || !valid_absolute_path(&repository.source_path)
            || !repo_identities.insert((&repository.name, &repository.source_path))
        {
            return Err("schema_invalid");
        }
    }
    Ok(())
}

fn validate_freshness(freshness: &StatusFreshness) -> Result<(), &'static str> {
    match freshness {
        StatusFreshness::Unobserved {
            revision,
            observed_at,
        } => (*revision == 0 && observed_at.is_none())
            .then_some(())
            .ok_or("schema_invalid"),
        StatusFreshness::Observed {
            revision,
            observed_at,
        } => (*revision != 0 && valid_timestamp(observed_at))
            .then_some(())
            .ok_or("schema_invalid"),
        StatusFreshness::Unavailable {
            revision,
            observed_at,
        } => (revision.is_none() && observed_at.is_none())
            .then_some(())
            .ok_or("schema_invalid"),
    }
}

pub(crate) fn freshness_observed(freshness: &StatusFreshness) -> bool {
    matches!(freshness, StatusFreshness::Observed { .. })
}

pub(crate) fn component_healthy(health: &ComponentHealth) -> bool {
    matches!(health, ComponentHealth::Healthy)
}

pub(crate) fn profile_component_healthy(health: &ComponentHealth) -> bool {
    component_healthy(health)
}

fn valid_text(value: &str) -> bool {
    !value.chars().any(is_c1_c0)
}

fn validate_safe_text(value: &str) -> Result<(), &'static str> {
    (!value.is_empty() && valid_text(value))
        .then_some(())
        .ok_or("schema_invalid")
}

fn is_c1_c0(value: char) -> bool {
    matches!(
        value as u32,
        0x00..=0x1f | 0x7f..=0x9f | 0x2028..=0x2029 | 0x202a..=0x202e | 0x2066..=0x2069
    )
}

/// `YYYY-MM-DDTHH:MM:SS[.f{3,6,9}]Z`: UTC, `Z` zoned, and either a whole
/// second or the three fractional widths chrono's `AutoSi` writes. The fixed
/// separators are checked by position and the value must parse, so a
/// permissive length is the only thing that is relaxed: a control character, a
/// numeric offset or a lowercase `t`/`z` is still refused.
///
/// Those widths are the producer's, not RFC 3339's: every instant on this
/// wire is spelled with `SecondsFormat::AutoSi`, so any other count is a frame
/// this half would echo as a spelling the local command never prints.
fn valid_timestamp(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return false;
    }
    // The head is 19 bytes: a whole second puts its `Z` at index 19, and a
    // fractional part puts a `.` there before the final `Z`.
    let fraction: &[u8] = match bytes.len() {
        20 => &[],
        length if length > 21 => match bytes[19] {
            b'.' => &bytes[20..length - 1],
            _ => return false,
        },
        _ => return false,
    };
    if !matches!(fraction.len(), 0 | 3 | 6 | 9) || !fraction.iter().all(u8::is_ascii_digit) {
        return false;
    }
    value.ends_with('Z') && parse_timestamp(value).is_some()
}

/// The instant a validated timestamp names, for the ordering rules. Byte
/// comparison is not an ordering: `Z` sorts above `.`.
fn parse_timestamp(value: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|parsed| parsed.with_timezone(&chrono::Utc))
}

/// The grammar a group path must satisfy, shared with the producer for the
/// reason [`valid_absolute_path`] names: one stored row the client refuses
/// fails the whole snapshot.
pub(crate) fn valid_group_path(value: &str) -> bool {
    !value.is_empty()
        && value.split('/').all(|component| {
            !component.is_empty() && component != "." && component != ".." && valid_text(component)
        })
}

/// The grammar a wire path must satisfy, shared with the producer: it drops a
/// stored row the client would otherwise refuse, because one such row fails the
/// whole snapshot and therefore every read command on every profile.
pub(crate) fn valid_absolute_path(value: &str) -> bool {
    value == "/"
        || (value.starts_with('/')
            && value.split('/').skip(1).all(|component| {
                !component.is_empty()
                    && component != "."
                    && component != ".."
                    && valid_text(component)
            }))
}

/// The grammar of a path a registry stored, which the store itself compares
/// with trailing separators aside: `/repo` and `/repo/` are one project there,
/// so both are admissible here. The value is validated, never rewritten, so
/// what a session row was stored with stays what the wire carries and stays
/// usable as the identifier a `session show` is given.
///
/// The worktree and workspace paths keep [`valid_absolute_path`]: they name
/// things the client does not own, so their grammar stays strict.
pub(crate) fn valid_stored_project_path(value: &str) -> bool {
    let trimmed = value.trim_end_matches('/');
    // A root spelled with nothing but separators trims away entirely, and it
    // is still the root.
    (trimmed.is_empty() && value.starts_with('/')) || valid_absolute_path(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn health() -> ProfileHealth {
        ProfileHealth {
            profile_enumeration: ComponentHealth::Healthy,
            metadata: ComponentHealth::Healthy,
            profile_data: ComponentHealth::Healthy,
        }
    }

    fn snapshot() -> SnapshotData {
        SnapshotData {
            namespace: "debug:agent-of-empires-dev".into(),
            cursor: Cursor {
                epoch: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
                revision: 1,
            },
            health: SnapshotHealth {
                global_enumeration: ComponentHealth::Healthy,
                global_metadata: ComponentHealth::Healthy,
                profiles: BTreeMap::new(),
            },
            default_profile: Some("main".into()),
            resolved_default_profile: Some("main".into()),
            profiles: vec![ProfileRead {
                name: "main".into(),
                groups: vec![],
                projects: vec![],
                health: health(),
            }],
            sessions: vec![],
            global_projects: vec![],
            status_freshness: StatusFreshness::Observed {
                revision: 1,
                observed_at: "2026-01-01T00:00:00Z".into(),
            },
        }
    }

    #[test]
    fn strict_uuid_and_namespace_grammar() {
        assert!(valid_uuid("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"));
        assert!(!valid_uuid("AAAAAAAA-bbbb-cccc-dddd-eeeeeeeeeeee"));
        assert!(valid_namespace("release:agent-of-empires"));
        assert!(!valid_namespace("release:"));
        assert!(!valid_namespace("test:agent"));
        assert!(!valid_namespace("release:../escape"));
    }

    #[test]
    fn duplicate_and_unknown_profile_health_keys_reject() {
        let duplicate = r#"{
            "global_enumeration":{"kind":"healthy"},
            "global_metadata":{"kind":"healthy"},
            "profiles":{
                "main":{"profile_enumeration":{"kind":"healthy"},"metadata":{"kind":"healthy"},"profile_data":{"kind":"healthy"}},
                "main":{"profile_enumeration":{"kind":"healthy"},"metadata":{"kind":"healthy"},"profile_data":{"kind":"healthy"}}
            }
        }"#;
        assert!(serde_json::from_str::<SnapshotHealth>(duplicate).is_err());
    }

    /// A parent that names no row is persisted state: `rm --purge` of a parent
    /// leaves the child pointing at nothing, and the local path keeps and
    /// prints it, so the snapshot is accepted. The rules that can still be
    /// stated are the ones about parents that are rows: same profile, and no
    /// cycle.
    #[test]
    fn an_orphan_parent_is_accepted_and_the_graph_rules_still_hold() {
        let mut value = snapshot();
        value.health.profiles.insert("main".into(), health());
        value.profiles[0].projects = vec![ProjectRead {
            name: "repo".into(),
            path: "/repo".into(),
            merge_key: "/repo".into(),
            scope: ProjectScope::Profile,
            default_base_branch: None,
            registered: true,
        }];
        let base = SessionRead {
            id: "a".into(),
            title: "A".into(),
            project_path: "/repo".into(),
            group_path: String::new(),
            tool: "tool".into(),
            command: String::new(),
            profile: "main".into(),
            status: WireStatus::Idle,
            state: WireState::Live,
            created_at: "2026-01-01T00:00:00Z".into(),
            last_accessed_at: None,
            idle_entered_at: None,
            last_error: None,
            archived_at: None,
            trashed_at: None,
            active_snoozed_until: None,
            pinned_at: None,
            agent_session_id: None,
            parent_session_id: Some("b".into()),
            has_worktree_info: false,
            has_managed_worktree: false,
            worktree: None,
            workspace_repos: vec![],
        };
        value.sessions = vec![base.clone()];
        assert_eq!(
            validate_snapshot(&value),
            Ok(()),
            "a parent that names no row is not a schema violation"
        );

        let mut child = base.clone();
        child.id = "b".into();
        child.parent_session_id = Some("a".into());
        value.sessions = vec![base.clone(), child.clone()];
        assert_eq!(
            validate_snapshot(&value),
            Err("schema_invalid"),
            "two rows pointing at each other are a cycle"
        );

        let mut foreign = child.clone();
        foreign.profile = "other".into();
        value.profiles.push(ProfileRead {
            name: "other".into(),
            groups: vec![],
            projects: vec![],
            health: health(),
        });
        value.health.profiles.insert("other".into(), health());
        value.sessions = vec![base, foreign];
        assert_eq!(
            validate_snapshot(&value),
            Err("schema_invalid"),
            "a parent in another profile is still refused"
        );
    }

    /// The Hello aggregate is a roll-up of the snapshot's components, so every
    /// code a component can be degraded by is a code the roll-up may name. A
    /// profile whose registry fails to load is a real, ordinary state, and it
    /// must not turn into a protocol rejection for every other command.
    #[test]
    fn the_hello_aggregate_accepts_every_code_its_components_carry() {
        let codes = hello_health_codes();
        for expected in [
            HealthCode::Enumeration,
            HealthCode::ProfileEnumeration,
            HealthCode::Metadata,
        ] {
            assert!(
                codes.contains(&expected),
                "{expected:?} is carried by a component the aggregate folds in"
            );
        }
        for component in [GLOBAL_HEALTH_CODES, PROFILE_COMPONENT_CODES] {
            for code in component {
                assert!(
                    codes.contains(code),
                    "the union omits a code one of its own components carries"
                );
            }
        }
    }

    #[test]
    fn hello_protocol_version_is_checked_before_dto_use() {
        let frame = json!({
            "kind": "hello",
            "data": {
                "protocol_version": 1,
                "runtime_epoch": "not-a-uuid"
            }
        });
        assert!(matches!(
            parse_hello(frame.to_string().as_bytes()),
            Err(HelloParseError::ProtocolVersion)
        ));
    }

    /// The wire keeps only the fractional part the local command serializes, so
    /// the grammar is `AutoSi`'s and still refuses every other spelling. A
    /// `.25` is a legal RFC 3339 instant this producer has never emitted, and
    /// admitting it would let a frame through that the local command could not
    /// have printed.
    #[test]
    fn a_timestamp_is_utc_z_at_the_producer_s_fraction_widths() {
        assert!(valid_timestamp("2026-01-01T00:00:00Z"));
        for fraction in ["250", "250000", "250000000"] {
            assert!(
                valid_timestamp(&format!("2026-01-01T00:00:00.{fraction}Z")),
                "{fraction}"
            );
        }
        for refused in [
            "2026-01-01T00:00:00",             // no zone
            "2026-01-01T00:00:00+00:00",       // a numeric offset
            "2026-01-01t00:00:00Z",            // a lowercase date/time separator
            "2026-01-01T00:00:00z",            // a lowercase zone
            "2026-01-01T00:00:00.Z",           // an empty fraction
            "2026-01-01T00:00:00.25Z",         // one fractional digit
            "2026-01-01T00:00:00.2500Z",       // four, a width AutoSi never writes
            "2026-01-01T00:00:00.25000000Z",   // eight
            "2026-01-01T00:00:00.1234567890Z", // ten
            "2026-01-01T00:00:00.12a456Z",     // a non-digit in the fraction
            "2026-01-01T00:00:0\u{1b}Z",       // a control character
        ] {
            assert!(!valid_timestamp(refused), "{refused} must be refused");
        }
    }

    /// A session archived a fraction of a second after it was created is not
    /// archived before it: the ordering rule compares instants, and `Z` sorts
    /// above `.` as a byte.
    #[test]
    fn the_later_timestamp_rule_compares_instants_not_bytes() {
        let mut value = snapshot();
        value.health.profiles.insert("main".into(), health());
        value.profiles[0].projects = vec![ProjectRead {
            name: "repo".into(),
            path: "/repo".into(),
            merge_key: "/repo".into(),
            scope: ProjectScope::Profile,
            default_base_branch: None,
            registered: true,
        }];
        let row = SessionRead {
            id: "a".into(),
            title: "A".into(),
            project_path: "/repo".into(),
            group_path: String::new(),
            tool: "tool".into(),
            command: String::new(),
            profile: "main".into(),
            status: WireStatus::Idle,
            state: WireState::Archived,
            created_at: "2026-01-01T00:00:00.500Z".into(),
            last_accessed_at: None,
            idle_entered_at: None,
            last_error: None,
            archived_at: Some("2026-01-01T00:00:01Z".into()),
            trashed_at: None,
            active_snoozed_until: None,
            pinned_at: None,
            agent_session_id: None,
            parent_session_id: None,
            has_worktree_info: false,
            has_managed_worktree: false,
            worktree: None,
            workspace_repos: vec![],
        };
        value.sessions.push(row.clone());
        assert_eq!(validate_snapshot(&value), Ok(()));

        let mut earlier = row;
        earlier.archived_at = Some("2026-01-01T00:00:00.100Z".into());
        value.sessions = vec![earlier];
        assert_eq!(validate_snapshot(&value), Err("schema_invalid"));
    }

    /// A stored project path is the spelling the registry holds, so a trailing
    /// separator is admissible and left alone: it is the identifier a
    /// `session show` is given. Everything the strict path grammar refuses is
    /// still refused here, because a registry holding one is holding a path
    /// that names nothing.
    #[test]
    fn a_stored_project_path_tolerates_a_trailing_separator_and_nothing_else() {
        for admitted in ["/repo", "/repo/", "/repo///", "/"] {
            assert!(
                valid_stored_project_path(admitted),
                "{admitted} is a directory the store can hold"
            );
        }
        for refused in ["repo", "", "/repo/../etc", "/repo//one", "/repo/\u{202e}"] {
            assert!(
                !valid_stored_project_path(refused),
                "{refused} names nothing the client can match on"
            );
        }
        // The worktree and workspace paths keep the strict grammar: they name
        // things the client does not own, so a trailing separator there is a
        // spelling no read can rely on.
        assert!(valid_absolute_path("/repo"));
        assert!(!valid_absolute_path("/repo/"));
    }

    #[test]
    fn status_wire_values_are_pascal_case() {
        assert_eq!(WireStatus::Waiting.as_str(), "Waiting");
        assert_eq!(WireStatus::Creating.as_str(), "Creating");
    }

    /// `registered` was the one field the decoder defaulted, and both published
    /// schemas already list it as required, so a snapshot that omitted it
    /// decoded to a guess while the schema would have refused it. No shipped
    /// producer can omit the field; the gap was that the client was more
    /// permissive than the contract it publishes.
    #[test]
    fn an_absent_registered_flag_is_refused_rather_than_guessed() {
        let named = json!({
            "name": "alpha",
            "path": "/srv/alpha",
            "merge_key": "/srv/alpha",
            "scope": {"kind": "profile"},
            "default_base_branch": null,
            "registered": true,
        });
        let mut absent = named.clone();
        absent
            .as_object_mut()
            .expect("an object")
            .remove("registered");
        assert!(
            serde_json::from_value::<ProjectRead>(absent.clone()).is_err(),
            "the decoder must not supply a value the schema requires"
        );

        // A registry row and a synthesized row are the two answers the flag
        // distinguishes, and both still decode when it is present.
        for registered in [true, false] {
            let mut value = absent.clone();
            value["registered"] = json!(registered);
            let project: ProjectRead =
                serde_json::from_value(value).expect("a row that names the flag deserializes");
            assert_eq!(project.registered, registered);
        }
    }

    /// Repos are accepted in any order, like every other collection here: the
    /// producer emits the store's own order, and an ordering rule on top of it
    /// would refuse a snapshot the local command renders from the same rows.
    /// What identity still buys is the refusal of a repeated
    /// (name, source_path) pair, which a workspace cannot hold.
    #[test]
    fn workspace_repos_are_accepted_in_any_order_and_refused_when_repeated() {
        let repo = |name: &str, source: &str| WorkspaceRepo {
            name: name.into(),
            source_path: source.into(),
            branch: "main".into(),
        };
        assert_eq!(
            validate_session(&session_with_repos(vec![
                repo("a", "/srv/a"),
                repo("a", "/srv/b"),
                repo("b", "/srv/a"),
            ])),
            Ok(())
        );
        assert_eq!(
            validate_session(&session_with_repos(vec![
                repo("b", "/srv/a"),
                repo("a", "/srv/b"),
            ])),
            Ok(()),
            "a descending stored order reaches the wire and must be kept"
        );
        assert_eq!(
            validate_session(&session_with_repos(vec![
                repo("a", "/srv/a"),
                repo("a", "/srv/a"),
            ])),
            Err("schema_invalid"),
            "a repeated repo is refused"
        );
    }

    fn session_with_repos(repos: Vec<WorkspaceRepo>) -> SessionRead {
        SessionRead {
            id: "a".into(),
            title: "A".into(),
            project_path: "/repo".into(),
            group_path: String::new(),
            tool: "tool".into(),
            command: String::new(),
            profile: "main".into(),
            status: WireStatus::Idle,
            state: WireState::Live,
            created_at: "2026-01-01T00:00:00Z".into(),
            last_accessed_at: None,
            idle_entered_at: None,
            last_error: None,
            archived_at: None,
            trashed_at: None,
            active_snoozed_until: None,
            pinned_at: None,
            agent_session_id: None,
            parent_session_id: None,
            has_worktree_info: false,
            has_managed_worktree: false,
            worktree: None,
            workspace_repos: repos,
        }
    }
}
