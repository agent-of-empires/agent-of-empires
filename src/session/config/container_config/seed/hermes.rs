//! Native Hermes home identities and positive configuration readers.

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use anyhow::{bail, Context, Result};

use super::{canonical_expected_path, guard, NativeRule, NativeStateBoundary, StateOrigin};

mod mixed;
mod state;

pub(super) use mixed::{seed_nodes, seed_plugins, seed_projects, seed_skill_controls, seed_skills};

#[derive(Default)]
pub(super) struct Scopes {
    pub(super) default_root: Option<PathBuf>,
    pub(super) source: Option<usize>,
    homes: Vec<HomeScope>,
    rosters: Vec<Roster>,
}

struct HomeScope {
    lookup: PathBuf,
    physical: PathBuf,
    native_home: PathBuf,
    profile: bool,
    home_rules: bool,
    machine_rules: bool,
}

enum Roster {
    Directory {
        path: PathBuf,
        root: guard::SourceRoot,
    },
    Entry {
        path: PathBuf,
        identity: Option<(u64, u64, u32)>,
    },
}

impl Roster {
    fn path(&self) -> &Path {
        match self {
            Self::Directory { path, .. } | Self::Entry { path, .. } => path,
        }
    }
    fn validate(&self) -> Result<()> {
        match self {
            Self::Directory { root, .. } => root.validate(),
            Self::Entry { path, identity } => {
                let current = match fs::metadata(path) {
                    Ok(metadata) => Some((metadata.dev(), metadata.ino(), metadata.mode())),
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                        ) =>
                    {
                        None
                    }
                    Err(error) => return Err(error).context("validating native profile roster"),
                };
                if current != *identity {
                    bail!("native profile roster changed during configuration seeding");
                }
                Ok(())
            }
        }
    }
}

impl Scopes {
    pub(super) fn validate(&self) -> Result<()> {
        for scope in &self.homes {
            if canonical_expected_path(&scope.lookup)? != scope.physical {
                bail!("native home alias changed during configuration seeding");
            }
        }
        for roster in &self.rosters {
            roster.validate()?;
        }
        Ok(())
    }
}

pub(super) fn valid_profile(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
        && !matches!(
            name,
            "default" | "hermes" | "test" | "tmp" | "root" | "sudo"
        )
}

fn scope_coordinates(root: &Path) -> Result<(PathBuf, PathBuf, bool)> {
    let physical = canonical_expected_path(root)?;
    let profile = root
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(valid_profile)
        && root
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == "profiles");
    let lookup = if profile {
        let parent = root
            .parent()
            .and_then(Path::parent)
            .context("native profile has no default root")?;
        canonical_expected_path(parent)?
            .join("profiles")
            .join(root.file_name().unwrap())
    } else {
        physical.clone()
    };
    Ok((lookup, physical, profile))
}

fn ensure_scope(boundary: &mut NativeStateBoundary, root: &Path) -> Result<usize> {
    let (lookup, physical, profile) = scope_coordinates(root)?;
    if let Some(index) = boundary
        .hermes
        .homes
        .iter()
        .position(|scope| scope.lookup == lookup)
    {
        return Ok(index);
    }
    let index = boundary.hermes.homes.len();
    boundary.hermes.homes.push(HomeScope {
        native_home: lookup.clone(),
        lookup,
        physical,
        profile,
        home_rules: false,
        machine_rules: false,
    });
    Ok(index)
}

fn compile_rules(specs: &'static [state::StateSpec]) -> Vec<(&'static str, Arc<NativeRule>)> {
    specs
        .iter()
        .map(|spec| {
            (
                spec.pattern,
                Arc::new(
                    NativeRule::new(spec.pattern, spec.exact)
                        .expect("Hermes state rules are static and validated"),
                ),
            )
        })
        .collect()
}

static HOME_RULES: LazyLock<Vec<(&'static str, Arc<NativeRule>)>> =
    LazyLock::new(|| compile_rules(state::HOME_STATE));
static MACHINE_RULES: LazyLock<Vec<(&'static str, Arc<NativeRule>)>> =
    LazyLock::new(|| compile_rules(state::MACHINE_STATE));

fn native_default_root(root: &Path, conventional: Option<&Path>) -> Result<PathBuf> {
    let physical = canonical_expected_path(root)?;
    if let Some(conventional) = conventional {
        if physical.starts_with(conventional) {
            return Ok(conventional.to_path_buf());
        }
    }
    if root
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == "profiles")
    {
        return Ok(canonical_expected_path(
            root.parent()
                .and_then(Path::parent)
                .context("native profile has no default root")?,
        )?);
    }
    Ok(physical)
}

fn register_machine(boundary: &mut NativeStateBoundary, root: &Path) -> Result<()> {
    let scope = ensure_scope(boundary, root)?;
    if boundary.hermes.homes[scope].machine_rules {
        return Ok(());
    }
    boundary.hermes.homes[scope].machine_rules = true;
    for (marker, rule) in MACHINE_RULES.iter() {
        boundary.add_state_rule(
            root,
            Arc::clone(rule),
            StateOrigin::Hermes { scope, marker },
        )?;
    }
    Ok(())
}

pub(super) fn register_home(boundary: &mut NativeStateBoundary, root: &Path) -> Result<usize> {
    let scope = ensure_scope(boundary, root)?;
    if boundary.hermes.homes[scope].home_rules {
        return Ok(scope);
    }
    boundary.hermes.homes[scope].home_rules = true;
    boundary.add_storage_root(root)?;
    for (marker, rule) in HOME_RULES.iter() {
        boundary.add_state_rule(
            root,
            Arc::clone(rule),
            StateOrigin::Hermes { scope, marker },
        )?;
    }
    let machine = native_default_root(root, boundary.hermes.default_root.as_deref())?;
    register_machine(boundary, &machine)?;
    if boundary.hermes.homes[scope].lookup != machine {
        register_home(boundary, &machine)?;
    } else {
        register_roster(boundary, &machine.join("profiles"), false)?;
        register_roster(boundary, &machine.join("profiles/.deleted"), true)?;
    }
    Ok(scope)
}

fn register_roster(boundary: &mut NativeStateBoundary, path: &Path, deleted: bool) -> Result<()> {
    if boundary
        .hermes
        .rosters
        .iter()
        .any(|roster| roster.path() == path)
    {
        return Ok(());
    }
    let metadata = match fs::metadata(path) {
        Ok(metadata) => Some(metadata),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            None
        }
        Err(error) => return Err(error).context("inspecting native profile roster"),
    };
    if !metadata.as_ref().is_some_and(|metadata| metadata.is_dir()) {
        boundary.hermes.rosters.push(Roster::Entry {
            path: path.to_path_buf(),
            identity: metadata.map(|metadata| (metadata.dev(), metadata.ino(), metadata.mode())),
        });
        return Ok(());
    }
    boundary.hermes.rosters.push(Roster::Directory {
        path: path.to_path_buf(),
        root: guard::SourceRoot::new(path)?,
    });
    let profiles = if deleted {
        path.parent()
            .context("native tombstones have no profile root")?
    } else {
        path
    };
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str().filter(|name| valid_profile(name)) else {
            continue;
        };
        let metadata = match fs::metadata(entry.path()) {
            Ok(metadata) => Some(metadata),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                None
            }
            Err(error) => return Err(error).context("inspecting native profile entry"),
        };
        let directory = metadata.as_ref().is_some_and(|metadata| metadata.is_dir());
        boundary.hermes.rosters.push(Roster::Entry {
            path: entry.path(),
            identity: metadata.map(|metadata| (metadata.dev(), metadata.ino(), metadata.mode())),
        });
        if deleted || directory {
            register_home(boundary, &profiles.join(name))?;
        }
    }
    Ok(())
}

pub(super) fn register_source(boundary: &mut NativeStateBoundary, source: &Path) -> Result<()> {
    boundary.add_route(source)?;
    let physical = canonical_expected_path(source)?;
    let matches: Vec<_> = boundary
        .hermes
        .homes
        .iter()
        .enumerate()
        .filter(|(_, scope)| scope.home_rules && scope.physical == physical)
        .map(|(index, _)| index)
        .collect();
    let scope = if let Some(index) = matches
        .iter()
        .copied()
        .find(|index| boundary.hermes.homes[*index].lookup == source)
    {
        index
    } else if matches.len() == 1 {
        matches[0]
    } else if matches.is_empty() {
        register_home(boundary, source)?
    } else {
        bail!(
            "native configuration home has ambiguous profile origins; preserve it before retrying"
        );
    };
    boundary.hermes.source = Some(scope);
    register_roster(boundary, &source.join("profiles"), false)?;
    register_roster(boundary, &source.join("profiles/.deleted"), true)?;
    Ok(())
}

pub(super) fn original_scope_map(
    boundary: &mut NativeStateBoundary,
    host: &Path,
) -> Result<HashMap<usize, usize>> {
    let original_count = boundary.hermes.homes.len();
    let matching: Vec<_> = boundary
        .hermes
        .homes
        .iter()
        .enumerate()
        .filter(|(_, scope)| scope.home_rules && scope.physical == host)
        .map(|(index, _)| index)
        .collect();
    if matching.is_empty() {
        return Ok(HashMap::new());
    }
    let selected = matching
        .iter()
        .copied()
        .find(|index| boundary.hermes.homes[*index].lookup == host)
        .or_else(|| (matching.len() == 1).then(|| matching[0]));
    if boundary.hermes.source.is_some() && selected.is_none() {
        bail!("stopped native home has ambiguous profile origins");
    }
    let source = boundary.source_root.path().to_path_buf();
    let reserved = register_home(boundary, &source)?;
    let candidates: Vec<_> = boundary.hermes.homes[..original_count]
        .iter()
        .enumerate()
        .filter_map(|(index, scope)| {
            scope
                .lookup
                .strip_prefix(host)
                .or_else(|_| scope.physical.strip_prefix(host))
                .ok()
                .map(|relative| {
                    (
                        index,
                        source.join(relative),
                        scope.native_home.clone(),
                        scope.profile,
                        scope.home_rules,
                        scope.machine_rules,
                    )
                })
        })
        .collect();
    let mut result = HashMap::new();
    let mut assigned = std::collections::HashSet::new();
    for (old, lookup, native_home, profile, home_rules, machine_rules) in candidates {
        let physical = canonical_expected_path(&lookup)?;
        let mapped = if Some(old) == selected {
            reserved
        } else {
            boundary
                .hermes
                .homes
                .iter()
                .enumerate()
                .find(|(index, scope)| {
                    *index != reserved
                        && !assigned.contains(index)
                        && scope.lookup == lookup
                        && (scope.native_home == scope.lookup || scope.native_home == native_home)
                })
                .map(|(index, _)| index)
                .unwrap_or_else(|| {
                    let index = boundary.hermes.homes.len();
                    boundary.hermes.homes.push(HomeScope {
                        lookup: lookup.clone(),
                        physical,
                        native_home: native_home.clone(),
                        profile,
                        home_rules,
                        machine_rules,
                    });
                    index
                })
        };
        let scope = &mut boundary.hermes.homes[mapped];
        scope.native_home = native_home;
        // The original is a complete mounted home, not a live named-profile
        // directory merely because the configured host source had that spelling.
        scope.profile = mapped != reserved && profile;
        scope.home_rules |= home_rules;
        scope.machine_rules |= machine_rules;
        assigned.insert(mapped);
        result.insert(old, mapped);
    }
    // Profiles discovered only in the retained original still resolve their
    // authored absolute inputs through their original mounted coordinates.
    for scope in &mut boundary.hermes.homes {
        if scope.native_home == scope.lookup {
            if let Ok(relative) = scope.lookup.strip_prefix(&source) {
                scope.native_home = host.join(relative);
            }
        }
    }
    if boundary.hermes.source.is_some() {
        boundary.hermes.source = Some(reserved);
    }
    Ok(result)
}
