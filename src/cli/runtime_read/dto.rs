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
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub default_profile: Option<String>,
    /// What the daemon resolved as the default, published whether or not the
    /// snapshot carries it. `default_profile` says whether that name resolves;
    /// this says which name, so a client that has to refuse can name the
    /// profile the user has to create.
    #[serde(deserialize_with = "deserialize_required_nullable")]
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
    #[serde(deserialize_with = "deserialize_required_nullable")]
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
        #[serde(deserialize_with = "deserialize_required_nullable")]
        observed_at: Option<String>,
    },
    Observed {
        revision: u64,
        observed_at: String,
    },
    Unavailable {
        #[serde(deserialize_with = "deserialize_required_nullable")]
        revision: Option<u64>,
        #[serde(deserialize_with = "deserialize_required_nullable")]
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
    pub listed: bool,
    pub aliases: Vec<String>,
    pub health: ProfileHealth,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProfileRead {
    pub name: String,
    pub listed: bool,
    pub aliases: Vec<String>,
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
    /// Required daemon-computed equality key, never rendered or resolved on the client.
    pub merge_key: String,
    pub scope: ProjectScope,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub default_base_branch: Option<String>,
    /// Registry membership is required; synthesized rows need a scoped session reference.
    pub registered: bool,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ProjectScope {
    Global,
    Profile,
}

/// CLI projection shared by producer and consumer. Commands and alias-only
/// profiles use the daemon-wide credential gate, not a per-profile ACL.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub last_accessed_at: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub idle_entered_at: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub last_error: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub archived_at: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub trashed_at: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub active_snoozed_until: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub pinned_at: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub agent_session_id: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub parent_session_id: Option<String>,
    pub has_worktree_info: bool,
    pub has_managed_worktree: bool,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub worktree: Option<WorktreeRead>,
    pub workspace_repos: Vec<WorkspaceRepo>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
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

impl From<crate::session::Status> for WireStatus {
    fn from(status: crate::session::Status) -> Self {
        use crate::session::Status;
        match status {
            Status::Running => Self::Running,
            Status::Waiting => Self::Waiting,
            Status::Idle => Self::Idle,
            Status::Unknown => Self::Unknown,
            Status::Stopped => Self::Stopped,
            Status::Error => Self::Error,
            Status::Starting => Self::Starting,
            Status::Deleting => Self::Deleting,
            Status::Creating => Self::Creating,
        }
    }
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
    /// CLI JSON status values are lowercase; human status and wire values are PascalCase.
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum WireState {
    Live,
    Archived,
    Trashed,
}

impl From<crate::session::SessionBucket> for WireState {
    fn from(bucket: crate::session::SessionBucket) -> Self {
        use crate::session::SessionBucket;
        match bucket {
            SessionBucket::Active => Self::Live,
            SessionBucket::Archived => Self::Archived,
            SessionBucket::Trashed => Self::Trashed,
        }
    }
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorktreeRead {
    pub branch: String,
    pub main_repo_path: String,
    pub managed_by_aoe: bool,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub base_branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
                formatter.write_str("a hello envelope")
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

fn deserialize_required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
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

/// How many rows one session's ancestry may span. Far above any chain a fork
/// can produce and far below the point where the walk costs anything.
const MAX_SESSION_DEPTH: usize = 64;
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
        // The map and profile row must describe the same load.
        if *health != profile.health {
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
            .any(|profile| &profile.name == default || profile.aliases.contains(default))
        {
            return Err("schema_invalid");
        }
    }
    if let Some(resolved) = &snapshot.resolved_default_profile {
        if resolved.is_empty() {
            return Err("schema_invalid");
        }
    }

    validate_freshness(&snapshot.status_freshness)?;
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
    let listed_profiles: HashSet<&str> = snapshot
        .profiles
        .iter()
        .filter(|profile| profile.listed)
        .map(|profile| profile.name.as_str())
        .collect();
    let mut listed_ids = HashSet::new();
    let mut session_ids = HashSet::new();
    let mut parents: HashMap<(&str, &str), Option<&str>> = HashMap::new();
    for session in &snapshot.sessions {
        if !session_ids.insert((session.profile.as_str(), session.id.as_str()))
            || (listed_profiles.contains(session.profile.as_str())
                && !listed_ids.insert(session.id.as_str()))
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
            (session.profile.as_str(), session.id.as_str()),
            session.parent_session_id.as_deref(),
        );
    }
    for ((profile, id), parent) in &parents {
        if let Some(parent) = parent {
            // Hidden copies cannot create an edge in the canonical graph.
            if listed_profiles.contains(profile)
                && !parents.contains_key(&(*profile, *parent))
                && listed_ids.contains(parent)
            {
                return Err("schema_invalid");
            }
            let mut seen = HashSet::new();
            let mut cursor = Some(*id);
            let mut depth = 0usize;
            while let Some(node) = cursor {
                if !seen.insert(node) || depth >= MAX_SESSION_DEPTH {
                    return Err("schema_invalid");
                }
                depth += 1;
                cursor = parents.get(&(*profile, node)).copied().flatten();
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
    if hello.profiles.len() != snapshot.profiles.len()
        || hello
            .profiles
            .iter()
            .zip(&snapshot.profiles)
            .any(|(hello, snapshot)| {
                hello.name != snapshot.name
                    || hello.listed != snapshot.listed
                    || hello.aliases != snapshot.aliases
                    || hello.health != snapshot.health
            })
    {
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
    let mut selectable: HashSet<&str> = profiles
        .iter()
        .map(|profile| profile.name.as_str())
        .collect();
    for profile in profiles {
        if !names.insert(profile.name.as_str()) {
            return Err("schema_invalid");
        }
        if profile.name.is_empty()
            || (!profile.listed && !crate::session::valid_profile_name(&profile.name))
        {
            return Err("schema_invalid");
        }
        validate_aliases(&profile.aliases, &mut selectable)?;
        validate_profile_health(&profile.health)?;
        validate_groups(&profile.groups)?;
        validate_projects(&profile.projects, ProjectScope::Profile)?;
    }
    Ok(())
}

/// Hello names and aliases are correlated with the complete snapshot.
fn validate_profile_hellos(profiles: &[ProfileHello]) -> Result<(), &'static str> {
    let mut names = HashSet::new();
    let mut selectable: HashSet<&str> = profiles
        .iter()
        .map(|profile| profile.name.as_str())
        .collect();
    for profile in profiles {
        if !names.insert(profile.name.as_str()) {
            return Err("schema_invalid");
        }
        if profile.name.is_empty()
            || (!profile.listed && !crate::session::valid_profile_name(&profile.name))
        {
            return Err("schema_invalid");
        }
        validate_aliases(&profile.aliases, &mut selectable)?;
        validate_profile_health(&profile.health)?;
    }
    Ok(())
}

fn validate_aliases<'a>(
    aliases: &'a [String],
    names: &mut HashSet<&'a str>,
) -> Result<(), &'static str> {
    for alias in aliases {
        if !crate::session::valid_profile_name(alias) || !names.insert(alias.as_str()) {
            return Err("schema_invalid");
        }
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

/// Preserve registry order and validate group-tree references, not filesystem syntax.
fn validate_groups(groups: &[GroupRead]) -> Result<(), &'static str> {
    let mut unique: HashSet<&str> = HashSet::new();
    for group in groups {
        if !unique.insert(group.path.as_str()) {
            return Err("schema_invalid");
        }
    }
    let paths = unique;
    for group in groups {
        let expected_name = group.path.rsplit('/').next().unwrap_or_default();
        if group.name != expected_name {
            return Err("schema_invalid");
        }
        // Child lists may be incomplete, but every named child must exist.
        let mut child_names: HashSet<&str> = HashSet::new();
        for child in &group.children {
            let child_path = format!("{}/{}", group.path, child);
            if !child_names.insert(child.as_str()) || !paths.contains(child_path.as_str()) {
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
            || !(if project.registered {
                valid_stored_project_path(&project.path)
            } else {
                valid_session_project_path(&project.path)
            })
            || (project.merge_key.is_empty()
                && (project.registered || !project.path.is_empty()))
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
    // Stored display text is preserved; identifiers retain their own checks.
    validate_safe_text(&session.id)?;
    if session.profile.is_empty() {
        return Err("schema_invalid");
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
    if !valid_session_project_path(&session.project_path) {
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
            || !valid_stored_project_path(&worktree.main_repo_path)
        {
            return Err("schema_invalid");
        }
    }
    // Preserve workspace order and native basenames; identity is (name, source_path).
    let mut repo_identities: HashSet<(&str, &str)> = HashSet::new();
    for repository in &session.workspace_repos {
        if !valid_absolute_path(&repository.source_path)
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
    value.ends_with('Z') && chrono::DateTime::parse_from_rfc3339(value).is_ok()
}

/// Canonical absolute POSIX spelling; native components may contain controls, but not NUL.
pub(crate) fn valid_absolute_path(value: &str) -> bool {
    value == "/"
        || (value.starts_with('/')
            && value.split('/').skip(1).all(|component| {
                !component.is_empty()
                    && component != "."
                    && component != ".."
                    && !component.contains('\0')
            }))
}

/// Registry spellings also permit trailing separators without rewriting the stored value.
pub(crate) fn valid_stored_project_path(value: &str) -> bool {
    let trimmed = value.trim_end_matches('/');
    (trimmed.is_empty() && value.starts_with('/')) || valid_absolute_path(trimmed)
}

/// The existing session writer uses an empty string for a non-UTF-8 native path.
pub(crate) fn valid_session_project_path(value: &str) -> bool {
    value.is_empty() || valid_stored_project_path(value)
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
                listed: true,
                aliases: vec![],
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

    /// A local Hello must agree with the admitted peer UID.
    fn local_hello(uid: Option<u32>) -> HelloData {
        HelloData {
            protocol_version: crate::server::runtime_ws::PROTOCOL_VERSION,
            runtime_epoch: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
            prebind_instance_id: "bbbbbbbb-cccc-dddd-eeee-ffffffffffff".into(),
            runtime_instance_id: "cccccccc-dddd-eeee-ffff-000000000000".into(),
            namespace: "debug:agent-of-empires-dev".into(),
            owner: Owner {
                kind: OwnerKind::LocalOwner,
                uid,
            },
            local_owner: true,
            health: AggregateHealth::Healthy,
            profiles: vec![ProfileHello {
                name: "main".into(),
                listed: true,
                aliases: vec![],
                health: health(),
            }],
            status_freshness: StatusFreshness::Observed {
                revision: 1,
                observed_at: "2026-01-01T00:00:00Z".into(),
            },
        }
    }

    #[test]
    fn a_local_owner_the_admitted_uid_does_not_match_is_a_peer_identity_refusal() {
        let value = snapshot();
        assert_eq!(
            validate_cross_message(&local_hello(Some(501)), &value, Some(501)),
            Ok(()),
            "the uid the client admitted is the uid the Hello claims"
        );
        assert_eq!(
            validate_cross_message(&local_hello(Some(501)), &value, Some(0)),
            Err("peer_identity"),
            "a publisher claiming a uid other than the admitted one is not the publisher"
        );
        assert_eq!(
            validate_cross_message(&local_hello(None), &value, Some(501)),
            Err("peer_identity"),
            "a local owner that names no uid cannot be the uid that was admitted"
        );
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

    /// Profile component health and its summary must agree.
    #[test]
    fn a_profile_health_the_summary_disagrees_with_is_refused() {
        let mut value = snapshot();
        value.health.profiles.insert(
            "main".into(),
            ProfileHealth {
                metadata: ComponentHealth::Degraded {
                    code: HealthCode::Metadata,
                },
                ..health()
            },
        );
        value.profiles[0].health = health();
        assert_eq!(
            validate_snapshot(&value),
            Err("schema_invalid"),
            "a degraded component the profile row does not carry is a snapshot that describes two profiles"
        );
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
        foreign.parent_session_id = None;
        value.profiles.push(ProfileRead {
            name: "other".into(),
            listed: true,
            aliases: vec![],
            groups: vec![],
            projects: value.profiles[0].projects.clone(),
            health: health(),
        });
        value.health.profiles.insert("other".into(), health());
        value.sessions = vec![base, foreign];
        value.sessions[0].parent_session_id = None;
        assert_eq!(validate_snapshot(&value), Ok(()));
        value.sessions[0].parent_session_id = Some("b".into());
        assert_eq!(
            validate_snapshot(&value),
            Err("schema_invalid"),
            "a parent in another profile is still refused"
        );
    }

    /// One parent chain of `len` rows, built flat so that constructing the
    /// input cannot itself recurse.
    fn parent_chain_snapshot(len: usize) -> SnapshotData {
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
            id: "s0".into(),
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
            workspace_repos: vec![],
        };
        value.sessions = (0..len)
            .map(|index| {
                let mut row = base.clone();
                row.id = format!("s{index}");
                row.parent_session_id = (index > 0).then(|| format!("s{}", index - 1));
                row
            })
            .collect();
        value
    }

    #[test]
    fn hidden_copies_scope_parent_graphs_without_relaxing_listed_uuid_uniqueness() {
        let mut value = parent_chain_snapshot(2);
        let mut hidden = value.profiles[0].clone();
        hidden.name = "outside".into();
        hidden.listed = false;
        value.profiles.push(hidden);
        value.health.profiles.insert("outside".into(), health());
        let copy: Vec<_> = value
            .sessions
            .iter()
            .cloned()
            .map(|mut row| {
                row.profile = "outside".into();
                row
            })
            .collect();
        value.sessions.extend(copy);
        assert_eq!(validate_snapshot(&value), Ok(()));
        let mut duplicate = value.clone();
        duplicate.sessions.push(duplicate.sessions[2].clone());
        assert_eq!(validate_snapshot(&duplicate), Err("schema_invalid"));
        let mut cycle = value.clone();
        cycle.sessions[2].parent_session_id = Some("s1".into());
        assert_eq!(validate_snapshot(&cycle), Err("schema_invalid"));
        let mut listed_copy = value.clone();
        listed_copy.profiles[1].listed = true;
        assert_eq!(validate_snapshot(&listed_copy), Err("schema_invalid"));
        value.sessions[0].parent_session_id = Some("hidden-only".into());
        let mut hidden_only = value.sessions[2].clone();
        hidden_only.id = "hidden-only".into();
        value.sessions.push(hidden_only);
        assert_eq!(validate_snapshot(&value), Ok(()));
    }

    #[test]
    fn raw_commands_do_not_relax_session_identity() {
        let mut value = parent_chain_snapshot(1);
        value.sessions[0].command = "printf 'one\tvalue'\nprintf 'two\n'".into();
        assert_eq!(validate_snapshot(&value), Ok(()));
        let mut bad_id = value.clone();
        bad_id.sessions[0].id.push('\n');
        assert_eq!(validate_snapshot(&bad_id), Err("schema_invalid"));
    }

    #[test]
    fn accepted_stored_titles_do_not_refuse_the_snapshot() {
        let mut value = parent_chain_snapshot(1);
        for title in [
            "",
            "line1\nline2",
            "left\u{0085}right",
            "\u{202e}title",
            "\u{1b}[31mred",
        ] {
            value.sessions[0].title = title.into();
            assert_eq!(validate_snapshot(&value), Ok(()), "{title:?}");
        }
        value.sessions[0].id = "invalid\nidentifier".into();
        assert_eq!(validate_snapshot(&value), Err("schema_invalid"));
    }

    #[test]
    fn a_parent_chain_deeper_than_the_bound_is_refused() {
        assert_eq!(
            validate_snapshot(&parent_chain_snapshot(MAX_SESSION_DEPTH + 1)),
            Err("schema_invalid"),
            "a chain one row past the bound is refused"
        );
        assert_eq!(
            validate_snapshot(&parent_chain_snapshot(MAX_SESSION_DEPTH)),
            Ok(()),
            "a chain of exactly the bound is still accepted"
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
            "2026-01-01T00:00:00.25Z",         // two fractional digits
            "2026-01-01T00:00:00.2500Z",       // four, a width AutoSi never writes
            "2026-01-01T00:00:00.25000000Z",   // eight
            "2026-01-01T00:00:00.1234567890Z", // ten
            "2026-01-01T00:00:00.12a456Z",     // a non-digit in the fraction
            "2026-01-01T00:00:0\u{1b}Z",       // a control character
        ] {
            assert!(!valid_timestamp(refused), "{refused} must be refused");
        }
    }

    #[test]
    fn stored_project_paths_preserve_native_components_and_trailing_separators() {
        for admitted in [
            "/repo",
            "/repo/",
            "/repo///",
            "/",
            "/team\twork/",
            "/name\nline",
            "/repo/\u{202e}",
        ] {
            assert!(
                valid_stored_project_path(admitted),
                "{admitted} is a directory the store can hold"
            );
        }
        for refused in ["repo", "", "/repo/../etc", "/repo//one", "/repo/\0"] {
            assert!(
                !valid_stored_project_path(refused),
                "{refused:?} is not an admitted absolute spelling"
            );
        }
        assert!(valid_absolute_path("/repo"));
        assert!(!valid_absolute_path("/repo/"));
    }

    /// Missing registry membership is refused rather than defaulted.
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

    /// Preserve stored order and native basenames; refuse duplicate repository identities.
    #[test]
    fn workspace_repos_are_accepted_in_any_order_and_refused_when_repeated() {
        let repo = |name: &str, source: &str| WorkspaceRepo {
            name: name.into(),
            source_path: source.into(),
            branch: "main".into(),
        };
        assert_eq!(
            validate_session(&session_with_repos(vec![
                repo("a\t", "/srv/a"),
                repo("a\t", "/srv/b"),
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
                repo("a\t", "/srv/a"),
                repo("a\t", "/srv/a"),
            ])),
            Err("schema_invalid"),
            "a repeated repo is refused"
        );
    }

    #[test]
    fn stored_custom_tools_and_git_labels_do_not_poison_a_session() {
        let mut session = session_with_repos(vec![WorkspaceRepo {
            name: "repo".into(),
            source_path: "/repo".into(),
            branch: "feature\u{202e}review".into(),
        }]);
        session.tool = "custom\tagent".into();
        session.has_worktree_info = true;
        session.has_managed_worktree = true;
        session.worktree = Some(WorktreeRead {
            branch: "feature\u{202e}review".into(),
            main_repo_path: "/repo/".into(),
            managed_by_aoe: true,
            base_branch: Some("base\u{2066}review".into()),
        });
        assert_eq!(validate_session(&session), Ok(()));
        session.has_managed_worktree = false;
        assert_eq!(validate_session(&session), Err("schema_invalid"));
    }

    #[test]
    fn an_empty_stored_session_path_is_not_an_empty_registered_path() {
        let mut value = parent_chain_snapshot(1);
        value.sessions[0].project_path.clear();
        let project = &mut value.profiles[0].projects[0];
        project.path.clear();
        project.merge_key.clear();
        project.name.clear();
        project.registered = false;
        assert_eq!(validate_snapshot(&value), Ok(()));
        value.profiles[0].projects[0].registered = true;
        assert_eq!(validate_snapshot(&value), Err("schema_invalid"));
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
    #[test]
    fn aliases_are_unique_disjoint_and_identical_across_frames() {
        let mut hello = local_hello(Some(501));
        let mut value = snapshot();
        value.health.profiles.insert("main".into(), health());
        for aliases in [
            vec!["alias".into(), "alias".into()],
            vec!["main".into()],
            vec!["".into()],
        ] {
            value.profiles[0].aliases = aliases;
            assert_eq!(validate_snapshot(&value), Err("schema_invalid"));
        }
        value.profiles[0].aliases = vec!["alias".into()];
        value.default_profile = Some("alias".into());
        value.resolved_default_profile = Some("alias".into());
        assert_eq!(validate_snapshot(&value), Ok(()));
        assert_eq!(
            validate_cross_message(&hello, &value, Some(501)),
            Err("schema_invalid")
        );
        hello.profiles[0].aliases = vec!["alias".into()];
        assert_eq!(validate_cross_message(&hello, &value, Some(501)), Ok(()));
        hello.profiles[0].listed = false;
        assert_eq!(
            validate_cross_message(&hello, &value, Some(501)),
            Err("schema_invalid")
        );
        value.profiles.push(ProfileRead {
            name: "other".into(),
            listed: false,
            aliases: vec!["alias".into()],
            groups: vec![],
            projects: vec![],
            health: health(),
        });
        assert_eq!(validate_profiles(&value.profiles), Err("schema_invalid"));
    }
}
