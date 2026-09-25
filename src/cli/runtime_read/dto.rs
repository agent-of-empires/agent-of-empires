use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

pub(crate) const PROTOCOL_VERSION: u16 = 2;

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
    pub profiles: Vec<ProfileRead>,
    pub status_freshness: StatusFreshness,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SnapshotData {
    pub namespace: String,
    pub cursor: Cursor,
    pub health: SnapshotHealth,
    pub default_profile: Option<String>,
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
    ProfileData,
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
    pub scope: ProjectScope,
    pub default_base_branch: Option<String>,
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
    pub has_terminal: bool,
    pub has_worktree_info: bool,
    pub has_managed_worktree: bool,
    pub has_cleanable_worktree: bool,
    pub worktree: Option<WorktreeRead>,
    pub workspace_repos: Vec<WorkspaceRepo>,
    pub cleanup_defaults: CleanupDefaults,
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

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CleanupDefaults {
    pub delete_worktree: bool,
    pub delete_branch: bool,
    pub delete_sandbox: bool,
    pub delete_to_trash: bool,
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
    validate_profiles(&hello.profiles)?;
    validate_freshness(&hello.status_freshness)?;
    if let AggregateHealth::Degraded { code } = hello.health {
        validate_health_code(code)?;
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
    if let Some(default) = &snapshot.default_profile {
        if !snapshot
            .profiles
            .iter()
            .any(|profile| &profile.name == default)
        {
            return Err("schema_invalid");
        }
    }

    validate_freshness(&snapshot.status_freshness)?;
    if snapshot
        .sessions
        .windows(2)
        .any(|pair| pair[0].id >= pair[1].id)
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
            let Some((parent_profile, _)) = parents.get(parent) else {
                return Err("schema_invalid");
            };
            if parent_profile != profile {
                return Err("schema_invalid");
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

fn validate_profiles(profiles: &[ProfileRead]) -> Result<(), &'static str> {
    if profiles.windows(2).any(|pair| pair[0].name >= pair[1].name) {
        return Err("schema_invalid");
    }
    for profile in profiles {
        validate_safe_text(&profile.name)?;
        validate_profile_health(&profile.health)?;
        validate_groups(&profile.groups)?;
        validate_projects(&profile.projects, ProjectScope::Profile)?;
    }
    Ok(())
}

fn validate_profile_health(health: &ProfileHealth) -> Result<(), &'static str> {
    validate_profile_component(health.profile_enumeration)?;
    validate_profile_component(health.metadata)?;
    validate_profile_component(health.profile_data)
}

fn validate_health_code(code: HealthCode) -> Result<(), &'static str> {
    let _ = code;
    Ok(())
}

fn validate_global_health(health: ComponentHealth) -> Result<(), &'static str> {
    if let ComponentHealth::Degraded { code } = health {
        if !matches!(code, HealthCode::Enumeration | HealthCode::Metadata) {
            return Err("schema_invalid");
        }
    }
    Ok(())
}

fn validate_profile_component(health: ComponentHealth) -> Result<(), &'static str> {
    if let ComponentHealth::Degraded { code } = health {
        if !matches!(
            code,
            HealthCode::ProfileEnumeration | HealthCode::Metadata | HealthCode::ProfileData
        ) {
            return Err("schema_invalid");
        }
    }
    Ok(())
}

fn validate_groups(groups: &[GroupRead]) -> Result<(), &'static str> {
    if groups.windows(2).any(|pair| pair[0].path >= pair[1].path) {
        return Err("schema_invalid");
    }
    let paths: HashSet<&str> = groups.iter().map(|group| group.path.as_str()).collect();
    for group in groups {
        if !valid_group_path(&group.path) || !valid_text(&group.name) {
            return Err("schema_invalid");
        }
        let expected_name = group.path.rsplit('/').next().unwrap_or_default();
        if group.name != expected_name || group.children.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err("schema_invalid");
        }
        let expected: BTreeMap<&str, usize> = groups
            .iter()
            .filter_map(|candidate| {
                candidate
                    .path
                    .strip_prefix(&format!("{}/", group.path))
                    .and_then(|tail| tail.split('/').next())
                    .map(|name| (name, 0))
            })
            .fold(BTreeMap::new(), |mut map, (name, value)| {
                *map.entry(name).or_insert(value) += 1;
                map
            });
        let actual: BTreeMap<&str, usize> = group
            .children
            .iter()
            .map(|name| (name.as_str(), 0))
            .fold(BTreeMap::new(), |mut map, (name, value)| {
                *map.entry(name).or_insert(value) += 1;
                map
            });
        if expected != actual {
            return Err("schema_invalid");
        }
        if let Some((parent, _)) = group.path.rsplit_once('/') {
            if !paths.contains(parent) {
                return Err("schema_invalid");
            }
        }
    }
    Ok(())
}

fn validate_projects(projects: &[ProjectRead], scope: ProjectScope) -> Result<(), &'static str> {
    if projects
        .windows(2)
        .any(|pair| (&pair[0].name, &pair[0].path) >= (&pair[1].name, &pair[1].path))
    {
        return Err("schema_invalid");
    }
    let mut identities = HashSet::new();
    for project in projects {
        if project.scope as u8 != scope as u8
            || !valid_text(&project.name)
            || !valid_absolute_path(&project.path)
            || project
                .default_base_branch
                .as_deref()
                .is_some_and(|value| !valid_text(value))
            || !identities.insert((project.name.as_str(), project.path.as_str()))
        {
            return Err("schema_invalid");
        }
    }
    Ok(())
}

fn validate_session(session: &SessionRead) -> Result<(), &'static str> {
    for value in [
        &session.id,
        &session.title,
        &session.tool,
        &session.command,
        &session.profile,
    ] {
        validate_safe_text(value)?;
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
    let _ = (
        session.has_terminal,
        session.has_cleanable_worktree,
        session.cleanup_defaults.delete_worktree,
        session.cleanup_defaults.delete_branch,
        session.cleanup_defaults.delete_sandbox,
        session.cleanup_defaults.delete_to_trash,
    );
    if session
        .last_error
        .as_deref()
        .is_some_and(|value| !valid_text(value))
    {
        return Err("schema_invalid");
    }
    if !valid_absolute_path(&session.project_path)
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
    if session
        .archived_at
        .as_deref()
        .is_some_and(|value| value < session.created_at.as_str())
        || session
            .trashed_at
            .as_deref()
            .is_some_and(|value| value < session.created_at.as_str())
    {
        return Err("schema_invalid");
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
    if session
        .workspace_repos
        .windows(2)
        .any(|pair| (&pair[0].name, &pair[0].source_path) >= (&pair[1].name, &pair[1].source_path))
    {
        return Err("schema_invalid");
    }
    for repository in &session.workspace_repos {
        if !valid_text(&repository.name)
            || !valid_text(&repository.branch)
            || !valid_absolute_path(&repository.source_path)
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

fn valid_timestamp(value: &str) -> bool {
    value.len() == 20
        && value.as_bytes()[4] == b'-'
        && value.as_bytes()[7] == b'-'
        && value.as_bytes()[10] == b'T'
        && value.as_bytes()[13] == b':'
        && value.as_bytes()[16] == b':'
        && value.ends_with('Z')
        && chrono::DateTime::parse_from_rfc3339(value).is_ok()
}

fn valid_group_path(value: &str) -> bool {
    !value.is_empty()
        && value.split('/').all(|component| {
            !component.is_empty() && component != "." && component != ".." && valid_text(component)
        })
}

fn valid_absolute_path(value: &str) -> bool {
    value == "/"
        || (value.starts_with('/')
            && value.split('/').skip(1).all(|component| {
                !component.is_empty()
                    && component != "."
                    && component != ".."
                    && valid_text(component)
            }))
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

    #[test]
    fn parent_must_exist_in_same_profile_and_graph_is_acyclic() {
        let mut value = snapshot();
        value.health.profiles.insert("main".into(), health());
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
            has_terminal: false,
            has_worktree_info: false,
            has_managed_worktree: false,
            has_cleanable_worktree: false,
            worktree: None,
            workspace_repos: vec![],
            cleanup_defaults: CleanupDefaults {
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: false,
                delete_to_trash: false,
            },
        };
        value.sessions.push(base.clone());
        assert_eq!(validate_snapshot(&value), Err("schema_invalid"));
        let mut child = base.clone();
        child.id = "b".into();
        child.parent_session_id = Some("a".into());
        value.sessions = vec![base.clone(), child];
        assert_eq!(validate_snapshot(&value), Err("schema_invalid"));
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

    #[test]
    fn status_wire_values_are_pascal_case() {
        assert_eq!(WireStatus::Waiting.as_str(), "Waiting");
        assert_eq!(WireStatus::Creating.as_str(), "Creating");
    }
}
