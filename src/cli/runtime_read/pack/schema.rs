//! The Draft 2020-12 subset the published Contract Pack schemas use, evaluated
//! against a recorded application frame.
//!
//! The pack's own claim is that the JSON Schema documents beside the fixtures
//! describe the wire. Nothing checked that claim, so a producer that grew a
//! field and a schema that did not follow it both kept passing: the two
//! documents agreed with each other and nobody compared either to a frame. This
//! module is the missing comparison, and it is deliberately a validator rather
//! than a second set of closed structs — a struct would re-derive the contract
//! in Rust and drift the same way, only further from the published document.
//!
//! The supported keyword set is exactly what the five shipped schemas use, and
//! [`Schema::compile`] refuses a document that reaches outside it, so a schema
//! cannot quietly start using a keyword nothing here evaluates. `pattern` is
//! anchored in the document itself (`^...$`), and the regex dialect is
//! `regex::Regex`, which has no backreferences or lookaround to disagree with
//! ECMA-262 about.
//!
//! The load-time walk is keyword-directed rather than a blind descent, and that
//! is the whole of its job: every position where a schema can appear is
//! reached, every schema is required to be an object, every `pattern` is
//! compiled, every keyword value has the shape the evaluator reads, and every
//! `$ref` is a resolvable `#/$defs/<name>`. Anything the evaluator cannot read
//! is a load failure. A blind descent cannot do that — it stops at the first
//! array, so a `pattern` inside a `oneOf` branch was never compiled and never
//! enforced, and it reads a `const`'s data as though it were a subschema. The
//! unifying rule is that an unknown must be a refusal, never a pass: the
//! evaluator answers `None` only for an assertion it actually evaluated and
//! found to hold, and everything it could not evaluate never reaches it.

use std::collections::BTreeMap;

use regex::Regex;
use serde_json::{Number, Value};

/// A compiled published schema document.
pub(super) struct Schema {
    root: Value,
    /// Every `pattern` string in the document, compiled once at load. Keyed by
    /// the pattern text, which is what the walk looks up, so a pattern is
    /// compiled once per document rather than once per instance.
    patterns: BTreeMap<String, Regex>,
}

impl Schema {
    /// Compile a published document, refusing any keyword this subset does not
    /// implement. A schema that reaches outside the subset would validate
    /// nothing here and read as if it had, which is the drift this whole gate
    /// exists to end.
    pub(super) fn compile(name: &str, document: Value) -> Result<Self, String> {
        let mut patterns = BTreeMap::new();
        let mut references = Vec::new();
        compile_walk(name, "", &document, &mut patterns, &mut references)?;
        resolve_references(name, &document, &references)?;
        Ok(Self {
            root: document,
            patterns,
        })
    }

    /// The first reason `instance` does not satisfy the document, or `Ok` when
    /// it does. The reason names a JSON Pointer, so a failure says which row of
    /// which frame the contract and the bytes disagree about.
    pub(super) fn validate(&self, instance: &Value) -> Result<(), String> {
        match self.check(&self.root, instance, "") {
            Some(reason) => Err(reason),
            None => Ok(()),
        }
    }

    fn check(&self, schema: &Value, instance: &Value, pointer: &str) -> Option<String> {
        let at = at(pointer);
        let object = match schema.as_object() {
            Some(object) => object,
            // `compile` refused every position that is not an object, so this
            // cannot happen. It answers with a refusal rather than a pass
            // anyway: the one thing this gate must never do is read "I do not
            // know how to evaluate this" as "yes".
            None => return Some(format!("{at}: the schema here is not an object")),
        };
        if let Some(reason) = self.check_keyword(object, instance, pointer, "type") {
            return Some(reason);
        }
        if let Some(reason) = self.check_keyword(object, instance, pointer, "const") {
            return Some(reason);
        }
        if let Some(reason) = self.check_keyword(object, instance, pointer, "enum") {
            return Some(reason);
        }
        if let Some(reason) = self.check_keyword(object, instance, pointer, "pattern") {
            return Some(reason);
        }
        if let Some(reason) = self.check_keyword(object, instance, pointer, "minimum") {
            return Some(reason);
        }
        if let Some(reason) = self.check_keyword(object, instance, pointer, "required") {
            return Some(reason);
        }
        if let Some(reason) = self.check_keyword(object, instance, pointer, "properties") {
            return Some(reason);
        }
        if let Some(reason) = self.check_keyword(object, instance, pointer, "additionalProperties")
        {
            return Some(reason);
        }
        if let Some(reason) = self.check_keyword(object, instance, pointer, "items") {
            return Some(reason);
        }
        if let Some(reason) = self.check_keyword(object, instance, pointer, "oneOf") {
            return Some(reason);
        }
        if let Some(reason) = self.check_not(object, instance, pointer) {
            return Some(reason);
        }
        if let Some(reason) = self.check_ref(object, instance, pointer) {
            return Some(reason);
        }
        None
    }

    fn check_ref(
        &self,
        object: &serde_json::Map<String, Value>,
        instance: &Value,
        pointer: &str,
    ) -> Option<String> {
        // The one `?` that is right: a missing `$ref` member is not an
        // unevaluated `$ref`, and the load-time walk has already refused a
        // `$ref` whose value is not a string.
        let reference = object.get("$ref").and_then(Value::as_str)?;
        let at = at(pointer);
        // The only form the shipped documents use, and the only form the
        // load-time walk accepts: a local `$defs` pointer whose name resolves.
        // A remote reference, a nested one, a 2019-09 `#/definitions/...`, or
        // one that names nothing is a refusal. Skipping it would turn every
        // reference in the document into a no-op behind a green gate.
        let target = reference
            .strip_prefix("#/$defs/")
            .filter(|name| !name.is_empty() && !name.contains('/'))
            .and_then(|name| self.root.get("$defs")?.as_object()?.get(name));
        let Some(target) = target else {
            return Some(format!("{at}: $ref {reference} is unresolvable"));
        };
        self.check(target, instance, pointer)
            .map(|reason| format!("{reason} (through $ref {reference})"))
    }

    fn check_not(
        &self,
        object: &serde_json::Map<String, Value>,
        instance: &Value,
        pointer: &str,
    ) -> Option<String> {
        let at = at(pointer);
        let negated = object.get("not")?;
        if self.check(negated, instance, pointer).is_none() {
            return Some(format!("{at}: it satisfies a `not` subschema"));
        }
        None
    }

    fn check_keyword(
        &self,
        object: &serde_json::Map<String, Value>,
        instance: &Value,
        pointer: &str,
        keyword: &str,
    ) -> Option<String> {
        let at = at(pointer);
        let expected = object.get(keyword)?;
        match keyword {
            "type" => self.check_type(expected, instance, pointer),
            // `instance == expected` compares references in older serde_json
            // releases; `Number` is compared by value here so `2` and `2.0`
            // cannot both pass a `const` by accident.
            "const" => (!const_holds(instance, expected))
                .then_some(format!("{at}: it is not the constant {expected}")),
            "enum" => {
                let options = expected.as_array()?;
                (!options.contains(instance)).then_some(format!(
                    "{at}: {instance} is not one of the enumerated values"
                ))
            }
            "pattern" => {
                let pattern = expected.as_str()?;
                let compiled = self.patterns.get(pattern)?;
                let text = instance.as_str()?;
                (!compiled.is_match(text))
                    .then_some(format!("{at}: {text:?} does not match {pattern}"))
            }
            "minimum" => {
                let bound = expected.as_f64()?;
                let value = instance.as_f64()?;
                (value < bound).then_some(format!("{at}: {value} is below the minimum {bound}"))
            }
            "required" => {
                let members = instance.as_object()?;
                expected.as_array()?.iter().find_map(|name| {
                    let name = name.as_str()?;
                    (!members.contains_key(name))
                        .then_some(format!("{at}: it has no member {name:?}"))
                })
            }
            "properties" => {
                let members = instance.as_object()?;
                expected.as_object()?.iter().find_map(|(name, subschema)| {
                    let member = members.get(name)?;
                    self.check(subschema, member, &child(pointer, name))
                        .map(|reason| format!("{reason} (at member {name:?})"))
                })
            }
            "additionalProperties" => {
                let members = instance.as_object()?;
                // `properties` names the declared members; every other member
                // is additional, and an absent `properties` leaves all of them
                // additional.
                let declared = object.get("properties").and_then(Value::as_object);
                let additional =
                    |name: &String| !declared.is_some_and(|declared| declared.contains_key(name));
                if expected == &Value::Bool(false) {
                    return members
                        .keys()
                        .find(|name| additional(name))
                        .map(|name| format!("{at}: it has the undeclared member {name:?}"));
                }
                // The schema-valued form, which `snapshot.schema.json` uses
                // for `health.profiles`. The subschema is evaluated against
                // every additional member; compiling it and then throwing it
                // away would leave the whole of `health.profiles`
                // unconstrained while the keyword still read as supported.
                members.iter().find_map(|(name, member)| {
                    if !additional(name) {
                        return None;
                    }
                    self.check(expected, member, &child(pointer, name))
                        .map(|reason| format!("{reason} (at additional member {name:?})"))
                })
            }
            "items" => {
                let entries = instance.as_array()?;
                entries.iter().enumerate().find_map(|(index, entry)| {
                    self.check(expected, entry, &format!("{pointer}/{index}"))
                        .map(|reason| format!("{reason} (at entry {index})"))
                })
            }
            "oneOf" => {
                let branches = expected.as_array()?;
                // The reasons are collected rather than counted, because a bare
                // "0 of 2 branches" says nothing about *why*: a `oneOf` over
                // tagged variants rejects a value that misses every branch, and
                // the branch it missed is the only useful thing to report.
                let reasons: Vec<Option<String>> = branches
                    .iter()
                    .map(|branch| self.check(branch, instance, pointer))
                    .collect();
                let satisfied = reasons.iter().filter(|reason| reason.is_none()).count();
                (satisfied != 1).then(|| {
                    let first = reasons
                        .iter()
                        .flatten()
                        .next()
                        .map(String::as_str)
                        .unwrap_or("no branch names a reason");
                    format!(
                        "{at}: {satisfied} of the {} `oneOf` branches accept it, \
                         which is not exactly one; the first refusal was {first}",
                        branches.len()
                    )
                })
            }
            _ => None,
        }
    }

    fn check_type(&self, expected: &Value, instance: &Value, pointer: &str) -> Option<String> {
        let at = at(pointer);
        let names: Vec<&str> = match expected {
            Value::String(name) => vec![name.as_str()],
            Value::Array(names) => names.iter().filter_map(Value::as_str).collect(),
            _ => return None,
        };
        let matched = names.iter().any(|name| type_matches(name, instance));
        (!matched).then_some(format!(
            "{at}: it is {} where the schema wants {names:?}",
            describe(instance)
        ))
    }
}

/// The pointer a failure names, with the document root spelled `/` rather
/// than as an empty prefix.
fn at(pointer: &str) -> &str {
    if pointer.is_empty() {
        "/"
    } else {
        pointer
    }
}

/// Whether `instance` is the constant `expected`. A JSON Schema `const` is an
/// identity test, and the identity is the *value*: two numbers are equal when
/// they are mathematically equal, so `2` and `2.0` are one constant.
fn const_holds(instance: &Value, expected: &Value) -> bool {
    match (instance, expected) {
        (Value::Number(left), Value::Number(right)) => numbers_equal(left, right),
        _ => instance == expected,
    }
}

/// JSON Schema equality on numbers. `serde_json`'s own `Number` equality is
/// structural — `2` is a `PosInt` and `2.0` a `Float`, so they compare unequal —
/// so the spellings are reconciled here. Two *integral* values are compared
/// exactly: `as_f64` would report `9007199254740993` and `9007199254740992` as
/// the same number, and a `const` that admits a value it does not name is a
/// hole rather than a strictness bug.
fn numbers_equal(left: &Number, right: &Number) -> bool {
    match (integral(left), integral(right)) {
        (Some(left), Some(right)) => left == right,
        (Some(exact), None) => float_is(right, exact),
        (None, Some(exact)) => float_is(left, exact),
        (None, None) => left.as_f64() == right.as_f64(),
    }
}

/// The exact value of a number that is an integer in the JSON data model, or
/// `None` for a spelling that is genuinely fractional.
fn integral(number: &Number) -> Option<i128> {
    number
        .as_i64()
        .map(i128::from)
        .or_else(|| number.as_u64().map(i128::from))
}

/// Whether a non-integral spelling of `number` is exactly `exact`. The
/// round-trip back to an integer is what refuses an `f64` that merely rounds
/// to the integer, which is the only way an `f64` could stand in for an
/// integer that `f64` cannot hold.
fn float_is(number: &Number, exact: i128) -> bool {
    match number.as_f64() {
        Some(float) => float as i128 == exact && float == exact as f64,
        None => false,
    }
}

fn child(pointer: &str, name: &str) -> String {
    format!("{pointer}/{name}")
}

fn type_matches(name: &str, instance: &Value) -> bool {
    match name {
        "null" => instance.is_null(),
        "boolean" => instance.is_boolean(),
        "string" => instance.is_string(),
        "array" => instance.is_array(),
        "object" => instance.is_object(),
        "number" => instance.is_number(),
        // JSON has one number type, so Draft 2020-12 calls a number an integer
        // when it has no fractional part. `is_i64`/`is_u64` would refuse the
        // `2.0` and `1e2` spellings, which that definition admits.
        "integer" => instance
            .as_f64()
            .is_some_and(|value| value.is_finite() && value.fract() == 0.0),
        _ => false,
    }
}

fn describe(instance: &Value) -> &'static str {
    match instance {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Every keyword this subset evaluates, plus the annotation keywords that carry
/// no assertion. Anything else is a schema reaching past the subset.
const SUPPORTED: &[&str] = &[
    "$comment",
    "$defs",
    "$id",
    "$ref",
    "$schema",
    "additionalProperties",
    "const",
    "default",
    "deprecated",
    "description",
    "enum",
    "examples",
    "items",
    "minimum",
    "not",
    "oneOf",
    "pattern",
    "properties",
    "readOnly",
    "required",
    "title",
    "type",
    "writeOnly",
];

/// Keywords whose value is a map from an arbitrary name to a subschema. The
/// distinction is why this walks rather than trusting `serde_json`: a member
/// name is not a keyword, and treating it as one would refuse every schema that
/// has properties at all.
const SUBSCHEMA_MAPS: &[&str] = &["$defs", "properties"];

/// Keywords whose value is a single subschema. `additionalProperties` is one
/// only in its schema-valued form; the `false` form is a value, not a schema,
/// and is the one boolean the subset accepts.
const SUBSCHEMAS: &[&str] = &["additionalProperties", "items", "not"];

/// Keywords whose value is an array of subschemas. Every published document
/// leans on `oneOf`, and a walk that stops at an array never reaches a branch:
/// a `pattern` written inside one is then neither compiled nor enforced, while
/// the document still reads as if it were.
const SUBSCHEMA_ARRAYS: &[&str] = &["oneOf"];

/// Walk one schema node, refusing everything the evaluator cannot read.
///
/// Every position a subschema can occupy is reached, and every node must be an
/// object: Draft 2020-12 permits a boolean subschema, where `true` accepts
/// everything and `false` refuses everything, and reading either one as "an
/// object with no members" makes `true` a way to write a schema that
/// constrains nothing. Both are refused at load instead, along with a node
/// that is a string, a number or an array.
fn compile_walk(
    name: &str,
    at: &str,
    node: &Value,
    patterns: &mut BTreeMap<String, Regex>,
    references: &mut Vec<String>,
) -> Result<(), String> {
    let Some(members) = node.as_object() else {
        return Err(format!(
            "{name}{at}: a subschema must be an object this evaluator can read, \
             not {node}; a boolean or a bare value is refused rather than \
             silently constraining nothing"
        ));
    };
    for (key, value) in members {
        if !SUPPORTED.contains(&key.as_str()) {
            return Err(format!(
                "{name}{at}: the schema uses {key:?}, which this validator does not evaluate"
            ));
        }
        if SUBSCHEMA_MAPS.contains(&key.as_str()) {
            let Some(subschemas) = value.as_object() else {
                return Err(format!(
                    "{name}{at}: {key:?} must be an object of subschemas"
                ));
            };
            for (member, subschema) in subschemas {
                compile_walk(
                    name,
                    &format!("{at}/{key}/{member}"),
                    subschema,
                    patterns,
                    references,
                )?;
            }
            continue;
        }
        if SUBSCHEMAS.contains(&key.as_str()) && value != &Value::Bool(false) {
            compile_walk(name, &format!("{at}/{key}"), value, patterns, references)?;
            continue;
        }
        if SUBSCHEMA_ARRAYS.contains(&key.as_str()) {
            let Some(branches) = value.as_array() else {
                return Err(format!(
                    "{name}{at}: {key:?} must be an array of subschemas"
                ));
            };
            for (index, branch) in branches.iter().enumerate() {
                compile_walk(
                    name,
                    &format!("{at}/{key}/{index}"),
                    branch,
                    patterns,
                    references,
                )?;
            }
            continue;
        }
        check_plain(name, at, key, value, patterns, references)?;
    }
    Ok(())
}

/// Check a keyword whose value is plain data rather than a schema, and collect
/// the side effects the walk owes the evaluator: a compiled `pattern`, and a
/// `$ref` to resolve once the whole document is in hand.
///
/// The shape of each value is checked here because the evaluator reads it
/// through `as_str`, `as_array` and `as_f64` and treats a value of the wrong
/// shape as no assertion at all. A `type` of `5` would otherwise be a keyword
/// that is in the supported set, is present, and constrains nothing.
fn check_plain(
    name: &str,
    at: &str,
    key: &str,
    value: &Value,
    patterns: &mut BTreeMap<String, Regex>,
    references: &mut Vec<String>,
) -> Result<(), String> {
    let bad = |detail: &str| format!("{name}{at}: {key:?} {detail}, not {value}");
    match key {
        "pattern" => {
            let text = value.as_str().ok_or_else(|| bad("must be a string"))?;
            let compiled = Regex::new(text).map_err(|error| {
                format!("{name}{at}: pattern {text:?} does not compile: {error}")
            })?;
            patterns.insert(text.to_string(), compiled);
        }
        "type" => match value {
            Value::String(_) => {}
            Value::Array(names) => {
                for entry in names {
                    if entry.as_str().is_none() {
                        return Err(bad("must be a string or an array of strings"));
                    }
                }
            }
            _ => return Err(bad("must be a string or an array of strings")),
        },
        "enum" => {
            if value.as_array().is_none() {
                return Err(bad("must be an array of values"));
            }
        }
        "required" => {
            let names = value
                .as_array()
                .ok_or_else(|| bad("must be an array of names"))?;
            for name in names {
                if name.as_str().is_none() {
                    return Err(bad("must be an array of names"));
                }
            }
        }
        "minimum" => {
            if value.as_f64().is_none() {
                return Err(bad("must be a number"));
            }
        }
        "$ref" => {
            let reference = value.as_str().ok_or_else(|| bad("must be a string"))?;
            let target = reference
                .strip_prefix("#/$defs/")
                .filter(|target| !target.is_empty() && !target.contains('/'));
            match target {
                Some(target) => references.push(target.to_string()),
                None => {
                    return Err(format!(
                        "{name}{at}: $ref {reference:?} is not a local `#/$defs/<name>` \
                         pointer, which is the only form this validator follows; \
                         `#/definitions/...` and any remote or nested reference are \
                         refused by name rather than skipped"
                    ))
                }
            }
        }
        // `const`, `default`, `examples` and the annotation keywords carry data
        // the evaluator does not assert on, so any JSON value is correct.
        _ => {}
    }
    Ok(())
}

/// Every `$ref` the document uses must name a definition the document carries.
/// Resolved after the walk so a reference into a `$defs` the document never
/// declares is a load failure, which is what stops a typo — or a 2019-09
/// `#/definitions/uuid` — from turning the whole document into a no-op behind a
/// green gate.
fn resolve_references(name: &str, root: &Value, references: &[String]) -> Result<(), String> {
    let defs = root.get("$defs").and_then(Value::as_object);
    for reference in references {
        if !defs.is_some_and(|defs| defs.contains_key(reference)) {
            return Err(format!(
                "{name}: $ref #/$defs/{reference} names no definition this document declares"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile a document written inline, so a test states the whole contract
    /// it is about rather than depending on which fixture happens to carry it.
    fn compile(text: &str) -> Result<Schema, String> {
        Schema::compile(
            "inline.schema.json",
            serde_json::from_str(text).expect("test document"),
        )
    }

    fn accept(document: &str, instance: &str) -> Result<(), String> {
        compile(document)?.validate(&serde_json::from_str(instance).expect("test instance"))
    }

    /// The load-time refusal for a document, as the message the pack reports.
    /// The compiled schema is not `Debug`, so the failure is taken by value.
    fn refused(document: &str) -> String {
        match compile(document) {
            Ok(_) => panic!("{document} was accepted"),
            Err(reason) => reason,
        }
    }

    /// A `health.profiles` member that is not a profile health object. This is
    /// the shape `snapshot.schema.json` ships, and before the schema-valued
    /// `additionalProperties` was evaluated the whole subtree was unchecked
    /// while the keyword still read as supported.
    #[test]
    fn a_schema_valued_additional_properties_constrains_every_undeclared_member() {
        let document = r##"{
            "type": "object",
            "properties": { "kind": { "const": "healthy" } },
            "additionalProperties": { "$ref": "#/$defs/profile_health" },
            "$defs": { "profile_health": { "type": "object", "required": ["metadata"] } }
        }"##;
        assert!(
            accept(
                document,
                r#"{"kind":"healthy","main":{"metadata":{"kind":"healthy"}}}"#
            )
            .is_ok(),
            "a conforming additional member is accepted"
        );
        let error = accept(document, r#"{"kind":"healthy","main":123}"#)
            .expect_err("an additional member that is not a profile health is refused");
        assert!(
            error.contains("at additional member \"main\""),
            "the refusal names the member: {error}"
        );
    }

    /// A `pattern` written inside a `oneOf` branch. The walk used to stop at
    /// the array, so the pattern was never compiled and the check that reads it
    /// was a no-op.
    #[test]
    fn a_pattern_inside_a_one_of_branch_is_compiled_and_enforced() {
        let document = r#"{
            "oneOf": [
                { "pattern": "^main$" },
                { "type": "null" }
            ]
        }"#;
        assert!(
            accept(document, r#""main""#).is_ok(),
            "a value the branch's pattern admits is accepted"
        );
        let error = accept(document, r#""other""#)
            .expect_err("a value the branch's pattern refuses is refused");
        assert!(
            error.contains(r#"does not match ^main$"#),
            "the refusal quotes the pattern: {error}"
        );
    }

    /// A keyword inside a branch that reaches past the subset is a load
    /// failure, for the same reason a keyword at the top of the document is.
    #[test]
    fn an_unsupported_keyword_inside_a_one_of_branch_is_refused_at_load() {
        let error = refused(r#"{"oneOf": [{"allOf": []}]}"#);
        assert!(
            error.contains("/oneOf/0") && error.contains("\"allOf\""),
            "the refusal names the branch and the keyword: {error}"
        );
    }

    /// The likeliest author error: a draft-2019-09 spelling. It used to be
    /// skipped, which turned every reference in the document into a no-op
    /// behind a green gate.
    #[test]
    fn a_definitions_reference_is_refused_by_name() {
        let error = refused(
            r##"{"$defs": {"uuid": {}}, "properties": {"id": {"$ref": "#/definitions/uuid"}}}"##,
        );
        assert!(
            error.contains("#/definitions/uuid"),
            "the refusal quotes the reference: {error}"
        );
    }

    /// The other half of the same hole: a reference to a definition the
    /// document does not carry.
    #[test]
    fn a_reference_to_an_undeclared_definition_is_refused() {
        let error = refused(r##"{"properties": {"id": {"$ref": "#/$defs/uuid"}}}"##);
        assert!(
            error.contains("#/$defs/uuid") && error.contains("names no definition"),
            "the refusal quotes the reference: {error}"
        );
    }

    /// A boolean subschema, which Draft 2020-12 permits. Read as "an object
    /// with no members", `true` is a way to write a constraint that constrains
    /// nothing and `false` is a way to delete a published one.
    #[test]
    fn a_boolean_subschema_is_refused_at_load() {
        for document in [
            r#"{"properties": {"id": false}}"#,
            r#"{"properties": {"id": true}}"#,
            r#"{"oneOf": [true]}"#,
        ] {
            let error = refused(document);
            assert!(
                error.contains("a subschema must be an object"),
                "the refusal explains why: {error}"
            );
        }
    }

    /// A node that is not a schema at all. `properties: "x"` used to load clean
    /// and check nothing.
    #[test]
    fn a_node_that_is_not_a_schema_is_refused_at_load() {
        for (document, fragment) in [
            // `properties` must be a map of subschemas, not a bare string.
            (r#"{"properties": "x"}"#, "must be an object of subschemas"),
            // Nor may a member of it be a bare value.
            (
                r#"{"properties": {"id": 5}}"#,
                "a subschema must be an object",
            ),
            (
                r#"{"$defs": {"a": ["b"]}}"#,
                "a subschema must be an object",
            ),
            // A `$ref` target is a schema too.
            (r#"{"$defs": {"a": "b"}}"#, "a subschema must be an object"),
            // And the document root itself.
            ("true", "a subschema must be an object"),
        ] {
            let error = refused(document);
            assert!(
                error.contains(fragment),
                "{document} is refused as {fragment}: {error}"
            );
        }
    }

    /// A keyword present, in the supported set, and unreadable. Each used to be
    /// an assertion the evaluator silently skipped.
    #[test]
    fn a_keyword_of_the_wrong_shape_is_refused_at_load() {
        for (document, fragment) in [
            (r#"{"type": 5}"#, r#""type" must be a string"#),
            (r#"{"type": ["string", 5]}"#, r#""type" must be a string"#),
            (r#"{"enum": "healthy"}"#, r#""enum" must be an array"#),
            (r#"{"required": "kind"}"#, r#""required" must be an array"#),
            (r#"{"minimum": "0"}"#, r#""minimum" must be a number"#),
            (r#"{"pattern": 7}"#, r#""pattern" must be a string"#),
            (r#"{"$ref": 5}"#, r#""$ref" must be a string"#),
            (
                r#"{"oneOf": {"type": "null"}}"#,
                r#""oneOf" must be an array"#,
            ),
        ] {
            let error = refused(document);
            assert!(
                error.contains(fragment),
                "{document} is refused as {fragment}: {error}"
            );
        }
    }

    /// Draft 2020-12: `2.0` and `1e2` are integers, because JSON has one number
    /// type and an integer is a number with no fractional part.
    #[test]
    fn an_integer_type_admits_every_spelling_with_no_fractional_part() {
        let document = r#"{"type": "integer"}"#;
        for instance in ["2", "2.0", "1e2", "-3", "0"] {
            assert!(
                accept(document, instance).is_ok(),
                "{instance} is an integer"
            );
        }
        assert!(accept(document, "2.5").is_err(), "2.5 is not an integer");
    }

    /// `const` is an identity test on the value, so `2` and `2.0` are one
    /// constant — but two large integers that share an `f64` rounding are not,
    /// and widening them would let a value through a `const` that never named
    /// it. `enum` compares structurally and must agree.
    #[test]
    fn a_constant_compares_integral_values_exactly() {
        let big = "9007199254740992";
        let bigger = "9007199254740993";
        assert!(
            accept(&format!(r#"{{"const": {big}}}"#), bigger).is_err(),
            "{bigger} is not the constant {big}"
        );
        for keyword in ["const", "enum"] {
            let document = match keyword {
                "const" => format!(r#"{{"const": {big}}}"#),
                _ => format!(r#"{{"enum": [{big}]}}"#),
            };
            assert!(
                accept(&document, bigger).is_err(),
                "{keyword} refuses {bigger} for {big}"
            );
            assert!(
                accept(&document, big).is_ok(),
                "{keyword} admits the constant it names"
            );
        }
    }

    /// The value identity a `const` states is the value, so the `2` and `2.0`
    /// spellings are one constant in both directions.
    #[test]
    fn a_constant_reconciles_the_two_spellings_of_one_number() {
        assert!(accept(r#"{"const": 2}"#, "2.0").is_ok());
        assert!(accept(r#"{"const": 2.0}"#, "2").is_ok());
        assert!(accept(r#"{"const": 0}"#, "0.0").is_ok());
        assert!(accept(r#"{"const": 2}"#, "3").is_err());
    }
}
