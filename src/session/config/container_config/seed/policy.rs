//! Source provenance and narrowly scoped exceptions for native configuration.

use std::path::{Path, PathBuf};

use anyhow::Result;

use super::guard;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum StateOrigin {
    Storage,
    Native,
    Hermes { scope: usize, marker: &'static str },
}

pub(super) struct NativeRule {
    pub(super) pattern: glob::Pattern,
    exact: Option<regex::Regex>,
}

impl NativeRule {
    pub(super) fn new(pattern: &str, exact: Option<&str>) -> Result<Self> {
        Ok(Self {
            pattern: glob::Pattern::new(pattern)?,
            exact: exact.map(regex::Regex::new).transpose()?,
        })
    }
    pub(super) fn matches(&self, relative: &Path) -> bool {
        self.pattern.matches_path_with(
            relative,
            glob::MatchOptions {
                require_literal_separator: true,
                ..glob::MatchOptions::new()
            },
        ) && self
            .exact
            .as_ref()
            .is_none_or(|exact| relative.to_str().is_some_and(|path| exact.is_match(path)))
    }
}

#[derive(Clone, Copy)]
pub(super) enum CollectionKind {
    Skills,
    Plugins,
}

#[derive(Clone, Copy, Default)]
pub(super) enum Exception<'a> {
    #[default]
    None,
    Mixed {
        origin: StateOrigin,
        leaves: &'a [PathBuf],
    },
    Collection {
        scope: usize,
        root: &'a Path,
        kind: CollectionKind,
    },
}

#[derive(Clone, Copy, Default)]
pub(super) struct ReadAccess<'a> {
    pub(super) root: Option<&'a guard::SourceRoot>,
    pub(super) exception: Exception<'a>,
}

impl ReadAccess<'_> {
    pub(super) fn allows_file(&self, candidate: &Path, origin: StateOrigin) -> bool {
        matches!(self.exception, Exception::Mixed { origin: allowed, leaves } if origin == allowed && leaves.iter().any(|leaf| leaf == candidate))
    }

    pub(super) fn allows(
        &self,
        candidate: &Path,
        state: &Path,
        directory: bool,
        origin: StateOrigin,
    ) -> bool {
        if !directory && self.allows_file(candidate, origin) {
            return true;
        }
        // Collection traversal never admits a state node or its descendants.
        if !directory || candidate.starts_with(state) || !state.starts_with(candidate) {
            return false;
        }
        let Exception::Collection { scope, root, kind } = self.exception else {
            return false;
        };
        let StateOrigin::Hermes {
            scope: actual,
            marker,
        } = origin
        else {
            return false;
        };
        if actual != scope || !candidate.starts_with(root) {
            return false;
        }
        // Marker strings come only from the closed native catalogue. This groups
        // known metadata producers; it is not a filename-prefix permission.
        match kind {
            CollectionKind::Skills => marker.starts_with("skills/"),
            CollectionKind::Plugins => marker.starts_with("plugins/hermes-achievements/"),
        }
    }

    pub(super) fn validate(&self) -> Result<()> {
        if let Some(root) = self.root {
            root.validate()?;
        }
        Ok(())
    }
}
