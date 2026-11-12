//! Resolves a suggestion to a session prefill: template expansion and project
//! resolution.

use super::*;

/// Why `prepare` refused an item.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PrepareRefusal {
    PromptTooLong,
    /// The source closed or changed, so the item cannot start.
    Stale,
}

/// An expanded prompt would exceed [`MAX_INITIAL_PROMPT_BYTES`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PromptTooLong;

/// Replaces `{name}` with the item's variable of that name, scanning left to
/// right: `{a{pr_number}}` becomes `{a42}`. Anything else, including unknown
/// names and stray braces, stays as typed, and substituted values are never
/// scanned again: they are untrusted text. Stops once the output would pass
/// [`MAX_INITIAL_PROMPT_BYTES`].
fn expand(template: &str, variables: &BTreeMap<String, String>) -> Result<String, PromptTooLong> {
    fn push(out: &mut String, text: &str) -> Result<(), PromptTooLong> {
        if out.len() + text.len() > MAX_INITIAL_PROMPT_BYTES {
            return Err(PromptTooLong);
        }
        out.push_str(text);
        Ok(())
    }
    let mut out = String::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        push(&mut out, &rest[..open])?;
        let after = &rest[open + 1..];
        // Names hold no braces, so the first brace after `{` decides.
        let known = after
            .find(['{', '}'])
            .filter(|&i| after.as_bytes()[i] == b'}')
            .and_then(|close| variables.get(&after[..close]).map(|value| (value, close)));
        match known {
            Some((value, close)) => {
                push(&mut out, value)?;
                rest = &after[close + 1..];
            }
            None => {
                push(&mut out, "{")?;
                rest = after;
            }
        }
    }
    push(&mut out, rest)?;
    Ok(out)
}

/// The registered project `wanted` names, by exact name or path. Zero or
/// several matches resolve to nothing: a project is never guessed.
fn resolve_hint(projects: &[Project], wanted: &str) -> Option<Project> {
    let mut hits: Vec<&Project> = projects.iter().filter(|p| p.name == wanted).collect();
    if hits.is_empty() {
        hits = by_path(projects, wanted);
    }
    match hits[..] {
        [only] => Some(only.clone()),
        _ => None,
    }
}

fn by_path<'a>(projects: &'a [Project], path: &str) -> Vec<&'a Project> {
    let key = projects::canonical_key(path);
    projects
        .iter()
        .filter(|p| projects::canonical_key(&p.path) == key)
        .collect()
}

/// The registered project at exactly `path`, never matched by name.
pub(super) fn resolve_path(projects: &[Project], path: &str) -> Option<Project> {
    match by_path(projects, path)[..] {
        [only] => Some(only.clone()),
        _ => None,
    }
}

/// The primary project and extras an item resolves to. A mapping whose first
/// path resolves supplies both; otherwise the provider's `hint` applies and an
/// extra is never promoted to primary.
fn resolve_projects(
    registered: &[Project],
    hint: Option<&str>,
    mapped: Option<&[String]>,
) -> (Option<Project>, Vec<Project>) {
    let from_mapping = mapped
        .and_then(|paths| paths.split_first())
        .and_then(|(first, rest)| {
            let primary = resolve_path(registered, first)?;
            let extras = rest
                .iter()
                .filter_map(|path| resolve_path(registered, path))
                .collect();
            Some((primary, extras))
        });
    match from_mapping {
        Some((primary, extras)) => (Some(primary), extras),
        None => (
            hint.and_then(|wanted| resolve_hint(registered, wanted)),
            Vec::new(),
        ),
    }
}

/// The profile's project mappings, keyed by provider and mapping key.
pub(crate) type Mappings = HashMap<(String, String), Vec<String>>;

pub(crate) fn load_mappings(conn: &Connection, profile: &str) -> anyhow::Result<Mappings> {
    let rows: Vec<(String, String, String)> = conn
        .prepare(
            "SELECT provider_id, mapping_key, projects_json FROM suggestion_mappings
             WHERE profile = ?1",
        )?
        .query_map([profile], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<Result<_, _>>()?;
    Ok(rows
        .into_iter()
        .filter_map(|(provider_id, key, paths)| {
            Some(((provider_id, key), serde_json::from_str(&paths).ok()?))
        })
        .collect())
}

/// Sets `resolved_project` (`{ name, path }` or `null`) on every snapshot item,
/// through the same rules `prepare` applies. Touches the filesystem, so call it
/// without the store lock.
pub(crate) fn attach_projects(mappings: &Mappings, snapshot: &mut Value, registered: &[Project]) {
    let Some(items) = snapshot["items"].as_array_mut() else {
        return;
    };
    for item in items {
        let provider_id = item["provider_id"].as_str().unwrap_or_default().to_string();
        let mapped = item["mapping_key"]
            .as_str()
            .and_then(|key| mappings.get(&(provider_id, key.to_string())));
        let (project, _) = resolve_projects(
            registered,
            item["project"].as_str(),
            mapped.map(Vec::as_slice),
        );
        item["resolved_project"] = match project {
            Some(p) => json!({ "name": p.name, "path": p.path }),
            None => Value::Null,
        };
    }
}

/// The live item plus the mapping its `mapping_key` names, if any.
pub(crate) fn find_item_with_mapping(
    conn: &Connection,
    profile: &str,
    provider: &Provider,
    id: &str,
) -> anyhow::Result<Option<(Suggestion, Option<Vec<String>>)>> {
    let Some(item) = find_item(conn, profile, provider, id)? else {
        return Ok(None);
    };
    let mapped = match &item.mapping_key {
        Some(key) => get_mapping(conn, profile, &provider.id, key)?,
        None => None,
    };
    Ok(Some((item, mapped)))
}

/// Wizard prefill for `item`, resolved through the normal config cascade of its
/// project, or the saved profile settings when it names none. A `template`
/// replaces the provider's prompt, expanded from the item's variables.
///
/// A mapping whose first path resolves supplies the project and the rest of
/// its list as extras. Otherwise the provider's `project` hint applies; an
/// extra is never promoted to primary.
pub(crate) fn prepare(
    profile: &str,
    item: &Suggestion,
    template: Option<&str>,
    mapped: Option<&[String]>,
) -> Result<Value, PrepareRefusal> {
    if item.stale {
        return Err(PrepareRefusal::Stale);
    }
    let prompt = match template {
        Some(t) => {
            expand(t, &item.variables).map_err(|PromptTooLong| PrepareRefusal::PromptTooLong)?
        }
        None => item.draft.prompt.clone(),
    };
    let registered = projects::load_merged(profile).unwrap_or_default();
    let (project, extras) = resolve_projects(&registered, item.project.as_deref(), mapped);
    let mut config = match &project {
        Some(p) => resolve_config_with_repo_or_warn(profile, Path::new(&p.path)),
        None => resolve_config_or_warn(profile),
    };
    if let Some(p) = &project {
        if let Some(enabled) = p.overrides.worktree_enabled {
            config.worktree.enabled = enabled;
        }
        if let Some(enabled) = p.overrides.sandbox_enabled {
            config.sandbox.enabled_by_default = enabled;
        }
    }
    let describe = |p: &Project| json!({ "name": p.name, "path": p.path });
    let tool = config
        .session
        .default_tool
        .clone()
        .unwrap_or_else(|| "claude".to_string());
    // Same rule as the web wizard; the spawn fills model and effort from `acp_defaults`.
    let structured = matches!(
        config.acp.structured_view,
        StructuredView::Auto | StructuredView::Structured
    ) && crate::session::builder::structured::tool_acp_capable(&tool, &config);
    Ok(json!({
        "title": item.draft.title,
        "initial_prompt": prompt,
        "template_criterion": template.map(|_| &item.criterion),
        "templated": template.is_some(),
        "source_ref": item.source_ref,
        "project": project.as_ref().map(describe),
        "extra_projects": extras.iter().map(describe).collect::<Vec<_>>(),
        "tool": tool,
        "view": if structured { "structured" } else { "terminal" },
        "yolo": config.session.yolo_mode_default,
        "sandbox": config.sandbox.enabled_by_default,
        "worktree": config.worktree.enabled,
    }))
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    #[serial_test::serial]
    fn prepare_resolves_only_an_unambiguous_registered_project() {
        let _home = crate::session::test_support::isolate_app_dir();
        let add = |scope, name: &str, path: &str| {
            projects::add("default", scope, Project::new(name, path, scope), false).unwrap();
        };
        let (global, profile) = (
            projects::ProjectScope::Global,
            projects::ProjectScope::Profile,
        );
        add(global, "widgets", "/tmp/aoe-widgets");
        add(global, "twin", "/tmp/aoe-twin-a");
        add(profile, "twin", "/tmp/aoe-twin-b");
        let prepared = |project: Option<&str>| {
            let mut i = item("a", "assigned");
            i.project = project.map(String::from);
            prepare("default", &i, None, None).unwrap()
        };
        for hit in ["widgets", "/tmp/aoe-widgets"] {
            assert_eq!(prepared(Some(hit))["project"]["name"], "widgets", "{hit}");
        }
        for miss in [Some("twin"), Some("unknown"), None] {
            assert!(prepared(miss)["project"].is_null(), "{miss:?}");
        }
        let out = prepared(None);
        assert_eq!(out["initial_prompt"], "Do work");
        assert!(out["template_criterion"].is_null());
        let templated = prepare("default", &item("a", "assigned"), Some(""), None).unwrap();
        assert_eq!(templated["template_criterion"], "assigned");
        assert_eq!(out["source_ref"], "example/1");
        assert_eq!(out["extra_projects"], json!([]));
        assert_eq!(
            out["view"], "structured",
            "auto picks structured for an ACP tool"
        );
        crate::session::config::update_config(|c| c.acp.structured_view = StructuredView::Off)
            .unwrap();
        assert_eq!(prepared(None)["view"], "terminal");
    }

    #[test]
    #[serial_test::serial]
    fn mapping_beats_hint_and_a_stale_primary_never_promotes_an_extra() {
        let _home = crate::session::test_support::isolate_app_dir();
        let dir = tempfile::tempdir().unwrap();
        let mut paths = Vec::new();
        for name in ["api", "web", "hinted"] {
            let path = dir.path().join(name);
            std::fs::create_dir(&path).unwrap();
            let path = path.to_string_lossy().into_owned();
            let scope = projects::ProjectScope::Global;
            projects::add("default", scope, Project::new(name, &path, scope), false).unwrap();
            paths.push(path);
        }
        let gone = dir.path().join("gone").to_string_lossy().into_owned();
        let mapped = |list: &[&String]| list.iter().map(|p| p.to_string()).collect::<Vec<_>>();
        let run = |hint: Option<&str>, mapping: Option<Vec<String>>| {
            let mut i = item("a", "assigned");
            i.project = hint.map(String::from);
            prepare("default", &i, None, mapping.as_deref()).unwrap()
        };
        let names = |v: &Value| -> Vec<String> {
            v["extra_projects"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p["name"].as_str().unwrap().to_string())
                .collect()
        };

        let mapping = mapped(&[&paths[0], &gone, &paths[1]]);
        let out = run(Some("hinted"), Some(mapping));
        assert_eq!(out["project"]["name"], "api", "mapping beats the hint");
        assert_eq!(names(&out), ["web"], "stale extras drop, order kept");

        let stale_primary = vec![gone.clone(), paths[1].clone()];
        let out = run(Some("hinted"), Some(stale_primary.clone()));
        assert_eq!(out["project"]["name"], "hinted", "stale primary falls back");
        assert!(names(&out).is_empty(), "an extra is never promoted");
        assert!(run(None, Some(stale_primary))["project"].is_null());
        assert!(run(None, None)["project"].is_null());
        assert_eq!(run(Some("hinted"), None)["project"]["name"], "hinted");
    }

    #[test]
    fn expand_substitutes_only_known_names_once() {
        let vars: BTreeMap<String, String> = [
            ("pr_number".to_string(), "42".to_string()),
            ("repo".to_string(), "{pr_number}".to_string()),
            ("name".to_string(), "日本語".to_string()),
        ]
        .into();
        let cases = [
            ("/aoe-review #{pr_number}", "/aoe-review #42"),
            ("{pr_number}{pr_number}", "4242"),
            ("{repo}", "{pr_number}"),
            ("{bogus} {} { {pr_number", "{bogus} {} { {pr_number"),
            ("{a{pr_number}}", "{a42}"),
            ("no placeholders", "no placeholders"),
            ("", ""),
            ("é{pr_number}日 {é}", "é42日 {é}"),
            ("{name}é{", "日本語é{"),
        ];
        for (template, expected) in cases {
            assert_eq!(
                expand(template, &vars).as_deref(),
                Ok(expected),
                "{template:?}"
            );
        }
    }

    #[test]
    fn expand_stops_at_the_prompt_bound() {
        let vars: BTreeMap<String, String> = [("x".to_string(), "ab".to_string())].into();
        assert!(expand(&"a".repeat(MAX_INITIAL_PROMPT_BYTES), &vars).is_ok());
        assert_eq!(
            expand(&"a".repeat(MAX_INITIAL_PROMPT_BYTES + 1), &vars),
            Err(PromptTooLong)
        );
        // Exactly at the bound, then one byte over, through substitution.
        let at = format!("{}{{x}}", "a".repeat(MAX_INITIAL_PROMPT_BYTES - 2));
        assert_eq!(expand(&at, &vars).unwrap().len(), MAX_INITIAL_PROMPT_BYTES);
        let over = format!("{}{{x}}", "a".repeat(MAX_INITIAL_PROMPT_BYTES - 1));
        assert_eq!(expand(&over, &vars), Err(PromptTooLong));
        // Many braces stay linear and verbatim.
        let braces = "{".repeat(MAX_INITIAL_PROMPT_BYTES);
        assert_eq!(expand(&braces, &vars).unwrap(), braces);
    }

    #[test]
    #[serial_test::serial]
    fn prepare_applies_the_template_in_place_of_the_draft_prompt() {
        let _home = crate::session::test_support::isolate_app_dir();
        let i = item("a", "assigned");
        let prompt =
            |template| prepare("default", &i, template, None).map(|v| v["initial_prompt"].clone());
        assert_eq!(prompt(None), Ok(json!("Do work")));
        assert_eq!(
            prompt(Some("Fix {repo} {nope}")),
            Ok(json!("Fix acme/widgets {nope}"))
        );
        assert_eq!(prompt(Some("")), Ok(json!("")));
        let templated =
            |template| prepare("default", &i, template, None).unwrap()["templated"].clone();
        assert_eq!(templated(None), json!(false));
        assert_eq!(templated(Some("Fix {repo}")), json!(true));
        // 12-byte value, template within the stored bound: expansion is not.
        let bomb = "{repo}".repeat(MAX_INITIAL_PROMPT_BYTES / "{repo}".len());
        assert!(bomb.len() <= MAX_INITIAL_PROMPT_BYTES);
        assert_eq!(prompt(Some(&bomb)), Err(PrepareRefusal::PromptTooLong));
    }

    #[test]
    fn prepare_refuses_a_stale_item() {
        let mut stale = item("a", "assigned");
        stale.stale = true;
        assert_eq!(
            prepare("default", &stale, None, None),
            Err(PrepareRefusal::Stale)
        );
    }

    #[test]
    fn items_get_the_project_prepare_would_resolve() {
        let conn = store();
        let project = |name: &str, path: &str| {
            let scope = projects::ProjectScope::Global;
            Project::new(name, path, scope)
        };
        let registered = [
            project("api", "/tmp/aoe-api"),
            project("web", "/tmp/aoe-web"),
            project("twin", "/tmp/aoe-twin-a"),
            project("twin", "/tmp/aoe-twin-b"),
        ];
        put_mapping(
            &conn,
            "p",
            "acme.github",
            "o/r",
            &["/tmp/aoe-web".to_string()],
        )
        .unwrap();
        let mut snapshot = json!({ "items": [
            { "provider_id": "acme.github", "id": "hint", "project": "api" },
            { "provider_id": "acme.github", "id": "mapped", "project": "api", "mapping_key": "o/r" },
            { "provider_id": "acme.github", "id": "ambiguous", "project": "twin" },
            { "provider_id": "acme.github", "id": "none" },
            { "provider_id": "other", "id": "unmapped", "mapping_key": "o/r" },
        ]});
        attach_projects(
            &load_mappings(&conn, "p").unwrap(),
            &mut snapshot,
            &registered,
        );
        let resolved = |i: usize| snapshot["items"][i]["resolved_project"].clone();
        assert_eq!(resolved(0)["name"], "api");
        assert_eq!(
            resolved(1),
            json!({ "name": "web", "path": "/tmp/aoe-web" })
        );
        assert!(resolved(2).is_null(), "an ambiguous hint is never guessed");
        assert!(resolved(3).is_null());
        assert!(resolved(4).is_null(), "a mapping belongs to its provider");
    }
}
