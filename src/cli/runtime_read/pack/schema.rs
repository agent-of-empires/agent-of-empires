//! The Draft 2020-12 subset the published Contract Pack schemas use, evaluated
//! against a recorded application frame.
//!
//! A validator rather than a second set of closed structs: a struct would
//! re-derive the contract in Rust and drift the same way, only further from the
//! published document.
//!
//! The supported keyword set is exactly what the shipped schemas use, and
//! [`Schema::compile`] refuses a document that reaches outside it, so a schema
//! cannot quietly start using a keyword nothing here evaluates. `pattern` is
//! anchored in the document itself (`^...$`), and the regex dialect is
//! `regex::Regex`, which has no backreferences or lookaround to disagree with
//! ECMA-262 about.
//!
//! The load-time walk is keyword-directed rather than a blind descent, and that
//! is the whole of its job: every position where a schema can appear is
//! reached, every schema is required to be an object, every `pattern` is
//! compiled under the position of the node that carries it, every keyword value
//! has the shape the evaluator reads, and every `$ref` is a resolvable
//! `#/$defs/<name>` that does not close a cycle. An unknown is a refusal, never
//! a pass: the evaluator answers `None` only for an assertion it actually
//! evaluated and found to hold.
//!
//! One boolean is accepted, and only where it is a value rather than a schema:
//! `additionalProperties: false`. A `not` or an `items` written as `false` is a
//! boolean *subschema*, which the evaluator has no branch for, so it is refused
//! at load with every other non-object node rather than half-evaluated.

use std::collections::{BTreeMap, BTreeSet};

use regex::Regex;
use serde_json::{Number, Value};

/// A compiled published schema document.
pub(super) struct Schema {
    root: Value,
    /// Every `pattern` in the document, compiled once at load. Keyed by the
    /// position of the schema node that carries it: the same pointer the walk
    /// built and the same one the evaluator arrives at, so the two spell a
    /// position one way. A miss is a refusal rather than an absent assertion:
    /// "no compiled pattern here" is a statement about the loader, and reading
    /// it as "this keyword constrains nothing" would let an unreached pattern
    /// pass every frame.
    patterns: BTreeMap<String, Regex>,
}

impl Schema {
    /// Compile a published document, refusing any keyword this subset does not
    /// implement. A schema that reaches outside the subset would validate
    /// nothing here and read as if it had, which is the drift this whole gate
    /// exists to end.
    pub(super) fn compile(name: &str, document: Value) -> Result<Self, String> {
        let mut patterns = BTreeMap::new();
        let mut references: References<'_> = Vec::new();
        compile_walk(name, "", &document, None, &mut patterns, &mut references)?;
        resolve_references(name, &document, &references)?;
        refuse_reference_cycles(name, &document, &references)?;
        Ok(Self {
            root: document,
            patterns,
        })
    }

    /// The first reason `instance` does not satisfy the document, or `Ok` when
    /// it does. The reason names a JSON Pointer, so a failure says which row of
    /// which frame the contract and the bytes disagree about.
    pub(super) fn validate(&self, instance: &Value) -> Result<(), String> {
        match self.check(&self.root, instance, "", "") {
            Some(reason) => Err(reason),
            None => Ok(()),
        }
    }

    /// Whether `instance` satisfies `schema`, and the first reason it does not.
    ///
    /// `pointer` is the position of `instance` in the frame, which is what a
    /// refusal names; `schema_at` is the position of `schema` in the
    /// document, which is how the compiled `pattern` map is keyed. The two
    /// move together through the recursion and are spelled the same way as
    /// the walk spells them.
    fn check(
        &self,
        schema: &Value,
        instance: &Value,
        pointer: &str,
        schema_at: &str,
    ) -> Option<String> {
        let at = at(pointer);
        let object = match schema.as_object() {
            Some(object) => object,
            // `compile` refused every position that is not an object, so this
            // cannot happen. It answers with a refusal rather than a pass
            // anyway: the one thing this gate must never do is read "I do not
            // know how to evaluate this" as "yes".
            None => return Some(format!("{at}: the schema here is not an object")),
        };
        if let Some(reason) = self.check_keyword(object, instance, pointer, schema_at, "type") {
            return Some(reason);
        }
        if let Some(reason) = self.check_keyword(object, instance, pointer, schema_at, "const") {
            return Some(reason);
        }
        if let Some(reason) = self.check_keyword(object, instance, pointer, schema_at, "enum") {
            return Some(reason);
        }
        if let Some(reason) = self.check_keyword(object, instance, pointer, schema_at, "pattern") {
            return Some(reason);
        }
        if let Some(reason) = self.check_keyword(object, instance, pointer, schema_at, "minimum") {
            return Some(reason);
        }
        if let Some(reason) = self.check_keyword(object, instance, pointer, schema_at, "minLength")
        {
            return Some(reason);
        }
        if let Some(reason) = self.check_keyword(object, instance, pointer, schema_at, "required") {
            return Some(reason);
        }
        if let Some(reason) = self.check_keyword(object, instance, pointer, schema_at, "properties")
        {
            return Some(reason);
        }
        if let Some(reason) =
            self.check_keyword(object, instance, pointer, schema_at, "additionalProperties")
        {
            return Some(reason);
        }
        if let Some(reason) = self.check_keyword(object, instance, pointer, schema_at, "items") {
            return Some(reason);
        }
        if let Some(reason) = self.check_keyword(object, instance, pointer, schema_at, "oneOf") {
            return Some(reason);
        }
        if let Some(reason) = self.check_not(object, instance, pointer, schema_at) {
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
        let unresolvable = format!("{at}: $ref {reference} is unresolvable");
        let Some(name) = reference
            .strip_prefix("#/$defs/")
            .filter(|name| !name.is_empty() && !name.contains('/'))
        else {
            return Some(unresolvable);
        };
        let Some(target) = self
            .root
            .get("$defs")
            .and_then(Value::as_object)
            .and_then(|defs| defs.get(name))
        else {
            return Some(unresolvable);
        };
        // A definition is evaluated at its own position in the document, which
        // is the position the walk compiled its `pattern` under: `/` plus the
        // `$defs` keyword plus the name, spelled here as the walk spells it.
        self.check(target, instance, pointer, &format!("/$defs/{name}"))
            .map(|reason| format!("{reason} (through $ref {reference})"))
    }

    fn check_not(
        &self,
        object: &serde_json::Map<String, Value>,
        instance: &Value,
        pointer: &str,
        schema_at: &str,
    ) -> Option<String> {
        let at = at(pointer);
        let negated = object.get("not")?;
        if self
            .check(negated, instance, pointer, &format!("{schema_at}/not"))
            .is_none()
        {
            return Some(format!("{at}: it satisfies a `not` subschema"));
        }
        None
    }

    fn check_keyword(
        &self,
        object: &serde_json::Map<String, Value>,
        instance: &Value,
        pointer: &str,
        schema_at: &str,
        keyword: &str,
    ) -> Option<String> {
        // The position of this schema node, for the one message that reports on
        // the document rather than on the frame. Spelled before `at` shadows
        // the function of the same name.
        let schema_here = at(schema_at);
        let at = at(pointer);
        let expected = object.get(keyword)?;
        match keyword {
            "type" => self.check_type(expected, instance, pointer),
            // `instance == expected` compares references in older serde_json
            // releases; `Number` is compared by value here so `2` and `2.0`
            // cannot both pass a `const` by accident. `enum` asks the same
            // identity question of each of its values: the two keywords are
            // defined by the same test, and a gate that answered them
            // differently would refuse a frame over the *spelling* of a
            // number rather than over its value.
            "const" => (!const_holds(instance, expected))
                .then_some(format!("{at}: it is not the constant {expected}")),
            "enum" => {
                let options = expected.as_array()?;
                (!options.iter().any(|option| const_holds(instance, option))).then_some(format!(
                    "{at}: {instance} is not one of the enumerated values"
                ))
            }
            "pattern" => {
                let pattern = expected.as_str()?;
                // The walk compiled every position it reached, and a position
                // it did not reach is one the evaluator cannot be at, so a
                // miss means the two disagree about where a subschema sits. It
                // is reported as a refusal rather than read as "this keyword
                // asserts nothing": that reading is the no-op which let a
                // pattern the walk never reached pass every frame behind a
                // green gate.
                let Some(compiled) = self.patterns.get(schema_at) else {
                    return Some(format!(
                        "{schema_here}: the pattern {pattern:?} here is not in the \
                         compiled set, so this gate cannot say whether it holds"
                    ));
                };
                let text = instance.as_str()?;
                (!compiled.is_match(text))
                    .then_some(format!("{at}: {text:?} does not match {pattern}"))
            }
            "minimum" => {
                let bound = expected.as_f64()?;
                let value = instance.as_f64()?;
                (value < bound).then_some(format!("{at}: {value} is below the minimum {bound}"))
            }
            "minLength" => {
                // Length in characters, as JSON Schema counts it, so a
                // multi-byte value is not held to its byte count.
                let bound = expected.as_u64()?;
                let text = instance.as_str()?;
                (text.chars().count() < bound as usize)
                    .then_some(format!("{at}: {text:?} is shorter than {bound} characters"))
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
                    self.check(
                        subschema,
                        member,
                        &child(pointer, name),
                        &format!("{schema_at}/properties/{name}"),
                    )
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
                    self.check(
                        expected,
                        member,
                        &child(pointer, name),
                        &format!("{schema_at}/additionalProperties"),
                    )
                    .map(|reason| format!("{reason} (at additional member {name:?})"))
                })
            }
            "items" => {
                let entries = instance.as_array()?;
                entries.iter().enumerate().find_map(|(index, entry)| {
                    self.check(
                        expected,
                        entry,
                        &format!("{pointer}/{index}"),
                        &format!("{schema_at}/items"),
                    )
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
                    .enumerate()
                    .map(|(index, branch)| {
                        self.check(
                            branch,
                            instance,
                            pointer,
                            &format!("{schema_at}/oneOf/{index}"),
                        )
                    })
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
/// structural, since `2` is a `PosInt` and `2.0` a `Float` and they compare
/// unequal, so the spellings are reconciled here. Two *integral* values are
/// compared exactly: `as_f64` would report `9007199254740993` and
/// `9007199254740992` as the same number, and a `const` that admits a value
/// it does not name is a hole rather than a strictness bug.
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
    "minLength",
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

/// Keywords whose value is a single subschema, which the evaluator reads by
/// recursing into it. It must therefore be an object, like every other node.
const SUBSCHEMAS: &[&str] = &["additionalProperties", "items", "not"];

/// The one keyword whose `false` form is a *value* rather than a schema.
/// `additionalProperties: false` says "no member beyond the declared ones", and
/// the evaluator reads it with an explicit `Bool(false)` branch of its own.
/// `not: false` and `items: false` are boolean subschemas, "always satisfied"
/// and "no items allowed", and the evaluator has no branch for them. It would
/// hand the `false` to `check`, be told the schema there is not an object, and
/// refuse every instance in the subtree for a reason that misdescribes the
/// document. So the boolean form is supported for `additionalProperties` alone,
/// and everywhere else a `false` is refused at load with every other non-object
/// node rather than half-evaluated.
const FALSE_IS_A_VALUE: &[&str] = &["additionalProperties"];

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
/// that is a string, a number or an array. The one exception is
/// `additionalProperties: false`, which is a value rather than a schema and is
/// enumerated as such by [`FALSE_IS_A_VALUE`].
///
/// `definition` is the `$defs` entry this node sits inside, if any, and is
/// what turns a `$ref` written here into an edge in that definition's own
/// reference graph.
fn compile_walk<'a>(
    name: &str,
    at: &str,
    node: &'a Value,
    definition: Option<&'a str>,
    patterns: &mut BTreeMap<String, Regex>,
    references: &mut References<'a>,
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
                // Only the root `$defs` names the reference graph. A `$defs`
                // nested inside another schema is still walked, but a `$ref`
                // written under it names a root entry, so the enclosing root
                // definition is unchanged.
                let inside = match (key.as_str(), at.is_empty()) {
                    ("$defs", true) => Some(member.as_str()),
                    _ => definition,
                };
                compile_walk(
                    name,
                    &format!("{at}/{key}/{member}"),
                    subschema,
                    inside,
                    patterns,
                    references,
                )?;
            }
            continue;
        }
        if SUBSCHEMAS.contains(&key.as_str()) {
            if value == &Value::Bool(false) && FALSE_IS_A_VALUE.contains(&key.as_str()) {
                continue;
            }
            compile_walk(
                name,
                &format!("{at}/{key}"),
                value,
                definition,
                patterns,
                references,
            )?;
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
                    definition,
                    patterns,
                    references,
                )?;
            }
            continue;
        }
        if key == "$ref" {
            collect_reference(name, at, value, definition, references)?;
            continue;
        }
        check_plain(name, at, key, value, patterns)?;
    }
    Ok(())
}

/// The `$ref`s a document uses: the definition each one was written inside, if
/// any, and the definition it names. An edge from a definition is a reference
/// cycle waiting to happen; an edge from nothing is a reference the document
/// body makes into a definition.
type References<'a> = Vec<(Option<&'a str>, &'a str)>;

/// Record one `$ref` and refuse the spellings this evaluator does not follow.
///
/// A reference into a `$defs` the document never declares is caught afterwards
/// by [`resolve_references`]; what is caught here is a reference whose *shape*
/// this evaluator cannot follow, which must never be a reference it silently
/// declines to resolve.
fn collect_reference<'a>(
    name: &str,
    at: &str,
    value: &'a Value,
    definition: Option<&'a str>,
    references: &mut References<'a>,
) -> Result<(), String> {
    let reference = value
        .as_str()
        .ok_or_else(|| format!("{name}{at}: \"$ref\" must be a string, not {value}"))?;
    let target = reference
        .strip_prefix("#/$defs/")
        .filter(|target| !target.is_empty() && !target.contains('/'));
    match target {
        Some(target) => {
            references.push((definition, target));
            Ok(())
        }
        None => Err(format!(
            "{name}{at}: $ref {reference:?} is not a local `#/$defs/<name>` \
             pointer, which is the only form this validator follows; \
             `#/definitions/...` and any remote or nested reference are \
             refused by name rather than skipped"
        )),
    }
}

/// Check a keyword whose value is plain data rather than a schema, and compile
/// the one that carries a side effect: a `pattern`, filed under the position of
/// the node that holds it, which is where the evaluator will look for it.
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
) -> Result<(), String> {
    let bad = |detail: &str| format!("{name}{at}: {key:?} {detail}, not {value}");
    match key {
        "pattern" => {
            let text = value.as_str().ok_or_else(|| bad("must be a string"))?;
            let compiled = Regex::new(text).map_err(|error| {
                format!("{name}{at}: pattern {text:?} does not compile: {error}")
            })?;
            patterns.insert(at.to_string(), compiled);
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
        "minimum" if value.as_f64().is_none() => {
            return Err(bad("must be a number"));
        }
        // `const`, `default`, `examples` and the annotation keywords carry data
        // the evaluator does not assert on, so any JSON value is correct.
        _ => {}
    }
    Ok(())
}

/// Every `$ref` the document uses must name a definition the document carries.
/// Resolved after the walk so a reference into a `$defs` the document never
/// declares is a load failure, which is what stops a typo, or a 2019-09
/// `#/definitions/uuid`, from turning the whole document into a no-op behind a
/// green gate.
fn resolve_references(name: &str, root: &Value, references: &References<'_>) -> Result<(), String> {
    let defs = root.get("$defs").and_then(Value::as_object);
    for (_, target) in references {
        if !defs.is_some_and(|defs| defs.contains_key(*target)) {
            return Err(format!(
                "{name}: $ref #/$defs/{target} names no definition this document declares"
            ));
        }
    }
    Ok(())
}

/// A definition that reaches itself names no schema this evaluator can read.
/// `check_ref` would follow it until the stack gave out, so a one-character
/// authoring mistake would abort `pack::verify` instead of producing the load
/// refusal every other malformation produces. This is the guard, and there is
/// deliberately no depth limit behind it: an acyclic graph bounds the
/// recursion by the depth of the document, whereas a limit would turn a schema
/// error into a size limit and read as if a deep document were a defect too.
///
/// Every declared definition is checked, not only the ones a `$ref` happens to
/// reach today. A cycle is a latent abort, and whether anything points at it is
/// a question the next edit answers; refusing it now keeps the answer from
/// mattering.
fn refuse_reference_cycles<'a>(
    name: &str,
    root: &'a Value,
    references: &References<'a>,
) -> Result<(), String> {
    let Some(defs) = root.get("$defs").and_then(Value::as_object) else {
        return Ok(());
    };
    let mut open: Vec<&str> = Vec::new();
    let mut settled: BTreeSet<&str> = BTreeSet::new();
    for definition in defs.keys() {
        descend_definitions(name, definition, references, &mut open, &mut settled)?;
    }
    Ok(())
}

/// Follow one definition's references, refusing a return to a definition
/// already on the path. `open` is that path, in order, so the refusal can print
/// the cycle rather than merely announce one; `settled` is the set already
/// proved acyclic, so a definition shared by two others is walked once.
fn descend_definitions<'a>(
    name: &str,
    definition: &'a str,
    references: &References<'a>,
    open: &mut Vec<&'a str>,
    settled: &mut BTreeSet<&'a str>,
) -> Result<(), String> {
    if settled.contains(definition) {
        return Ok(());
    }
    if let Some(start) = open.iter().position(|visited| *visited == definition) {
        let mut cycle: Vec<String> = open[start..]
            .iter()
            .map(|name| format!("#/$defs/{name}"))
            .collect();
        cycle.push(format!("#/$defs/{definition}"));
        return Err(format!(
            "{name}: the reference cycle {} names no schema this evaluator can \
             read, and is refused at load rather than followed until the stack \
             gives out",
            cycle.join(" -> ")
        ));
    }
    open.push(definition);
    for (from, target) in references {
        if *from == Some(definition) {
            descend_definitions(name, target, references, open, settled)?;
        }
    }
    open.pop();
    settled.insert(definition);
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

    /// A `health.profiles` member that is not a profile health object. The
    /// subschema of a schema-valued `additionalProperties` reaches every
    /// member it does not name, which is the shape `snapshot.schema.json`
    /// ships.
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

    /// A `pattern` written inside a `oneOf` branch, which the walk compiles
    /// under its own node like any other.
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

    /// The likeliest author error, a draft-2019-09 spelling. The loader
    /// refuses it by name, so every reference it leaves behind fails rather
    /// than resolving to nothing.
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

    /// A node that is not a schema at all, so there is nothing to evaluate.
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

    /// A keyword present, in the supported set, and unreadable, which must not
    /// become an assertion the evaluator silently skips.
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
    /// constant. Two large integers that share an `f64` rounding are not, and
    /// widening them would let a value through a `const` that never named it.
    /// `enum` is the same identity question asked of each of its values, so the
    /// two keywords answer it the same way in both directions.
    #[test]
    fn a_constant_compares_integral_values_exactly() {
        let big = "9007199254740992";
        let bigger = "9007199254740993";
        assert!(
            accept(&format!(r#"{{"const": {big}}}"#), bigger).is_err(),
            "{bigger} is not the constant {big}"
        );
        for keyword in ["const", "enum"] {
            let exact = match keyword {
                "const" => format!(r#"{{"const": {big}}}"#),
                _ => format!(r#"{{"enum": [{big}]}}"#),
            };
            assert!(
                accept(&exact, bigger).is_err(),
                "{keyword} refuses {bigger} for {big}"
            );
            assert!(
                accept(&exact, big).is_ok(),
                "{keyword} admits the constant it names"
            );
            for (named, spelled) in [("2", "2.0"), ("2.0", "2")] {
                let document = match keyword {
                    "const" => format!(r#"{{"const": {named}}}"#),
                    _ => format!(r#"{{"enum": [{named}]}}"#),
                };
                assert!(
                    accept(&document, spelled).is_ok(),
                    "{keyword} admits {spelled} for the {named} it names"
                );
            }
        }
    }

    /// The value identity a `const` states is the value, and `enum` states the
    /// same identity, so the `2` and `2.0` spellings are one value in both
    /// keywords and in both directions.
    #[test]
    fn a_constant_and_an_enumerated_value_are_one_number() {
        assert!(accept(r#"{"const": 2}"#, "2.0").is_ok());
        assert!(accept(r#"{"const": 2.0}"#, "2").is_ok());
        assert!(accept(r#"{"const": 0}"#, "0.0").is_ok());
        assert!(accept(r#"{"const": 2}"#, "3").is_err());
        assert!(accept(r#"{"enum": [2]}"#, "2.0").is_ok());
        assert!(accept(r#"{"enum": [2.0]}"#, "2").is_ok());
        assert!(accept(r#"{"enum": ["main", 2]}"#, "2.0").is_ok());
        assert!(accept(r#"{"enum": [2]}"#, "2.5").is_err());
        assert!(accept(r#"{"enum": [2]}"#, "3").is_err());
    }

    /// A definition that reaches itself. The name *is* declared, so the walk
    /// has nothing to complain about, and the evaluator then follows the
    /// reference until the stack gives out: a one-character authoring mistake
    /// that aborted the whole pack instead of producing the readable load
    /// refusal every other malformation produces. A cycle is in the set of
    /// things this evaluator cannot read, so it is refused at load.
    #[test]
    fn a_reference_cycle_is_refused_at_load() {
        for (document, fragment) in [
            (
                r##"{"$defs": {"a": {"$ref": "#/$defs/a"}},
                    "properties": {"id": {"$ref": "#/$defs/a"}}}"##,
                "#/$defs/a -> #/$defs/a",
            ),
            (
                r##"{"$defs": {"a": {"$ref": "#/$defs/b"}, "b": {"$ref": "#/$defs/a"}}}"##,
                "#/$defs/a -> #/$defs/b -> #/$defs/a",
            ),
            // A cycle two definitions deep, reached through a `properties`
            // rather than a `$defs` body.
            (
                r##"{"$defs": {"a": {"properties": {"next": {"$ref": "#/$defs/b"}}},
                    "b": {"$ref": "#/$defs/a"}}}"##,
                "#/$defs/a -> #/$defs/b -> #/$defs/a",
            ),
        ] {
            let error = refused(document);
            assert!(
                error.contains(fragment),
                "{document} is refused as the cycle {fragment}: {error}"
            );
        }
    }

    /// A cycle nothing points at yet is refused too. Whether a definition is
    /// reachable today is a question the next edit answers, and a cycle is an
    /// abort waiting for the answer.
    #[test]
    fn an_unreferenced_definition_that_reaches_itself_is_refused() {
        let error = refused(r##"{"$defs": {"orphan": {"$ref": "#/$defs/orphan"}}}"##);
        assert!(
            error.contains("#/$defs/orphan -> #/$defs/orphan") && error.contains("refused at load"),
            "the refusal names the cycle: {error}"
        );
    }

    /// Acyclic documents are not refused, and a definition that several others
    /// share is walked once rather than mistaken for a cycle.
    #[test]
    fn an_acyclic_reference_graph_is_accepted() {
        let document = r##"{
            "properties": {"id": {"$ref": "#/$defs/leaf"}},
            "$defs": {
                "leaf": {"type": "string"},
                "a": {"$ref": "#/$defs/leaf"},
                "b": {"$ref": "#/$defs/leaf"}
            }
        }"##;
        assert!(
            accept(document, r#"{"id": "x"}"#).is_ok(),
            "a definition two references share is not a cycle"
        );
    }

    /// The `false` form of a single-subschema keyword.
    /// `additionalProperties: false` is a *value* the evaluator reads with a
    /// branch of its own, and every published document uses it. `not: false`
    /// and `items: false` are boolean subschemas, and the evaluator has no
    /// branch for them. It would hand the `false` to `check`, be told the
    /// schema there was not an object, and so refuse every instance in the
    /// subtree, which for `not: false`, always satisfied by the draft, refuses
    /// everything, and for `items: false`, which forbids items, refuses every
    /// non-empty array for a reason that misdescribes it. Both are refused at
    /// load with every other non-object node.
    #[test]
    fn a_boolean_subschema_is_refused_except_as_the_additional_properties_value() {
        for document in [
            r#"{"not": false}"#,
            r#"{"items": false}"#,
            r#"{"oneOf": [{"not": false}]}"#,
            r#"{"properties": {"tags": {"items": false}}}"#,
        ] {
            let error = refused(document);
            assert!(
                error.contains("a subschema must be an object"),
                "{document} is refused at load: {error}"
            );
        }

        let document =
            r#"{"properties": {"id": {"type": "integer"}}, "additionalProperties": false}"#;
        assert!(
            accept(document, r#"{"id": 1}"#).is_ok(),
            "the one boolean the subset accepts still loads"
        );
        let error = accept(document, r#"{"id": 1, "other": 2}"#)
            .expect_err("an undeclared member is refused");
        assert!(
            error.contains("undeclared member"),
            "and it still means what it says: {error}"
        );
    }

    /// Every position the evaluator recurses through, carrying a pattern that
    /// must have been compiled by the walk. The compiled set is keyed by the
    /// position of the node that holds the pattern, so a position the walk
    /// skipped arrives here as a *miss*, a refusal naming the position rather
    /// than a silent pass, and a position spelled two different ways by the
    /// two sides is the same miss. This test is what says the two agree today.
    #[test]
    fn a_pattern_is_compiled_at_every_position_the_evaluator_reaches() {
        for (document, instance, pattern) in [
            (
                r#"{"properties": {"id": {"pattern": "^p$"}}}"#,
                r#"{"id": "x"}"#,
                "^p$",
            ),
            (
                r#"{"additionalProperties": {"pattern": "^a$"}}"#,
                r#"{"id": "x"}"#,
                "^a$",
            ),
            (r#"{"items": {"pattern": "^i$"}}"#, r#"["x"]"#, "^i$"),
            (
                r#"{"oneOf": [{"pattern": "^o$"}, {"type": "null"}]}"#,
                r#""x""#,
                "^o$",
            ),
            (
                r##"{"properties": {"id": {"$ref": "#/$defs/d"}},
                    "$defs": {"d": {"pattern": "^d$"}}}"##,
                r#"{"id": "x"}"#,
                "^d$",
            ),
            (
                r#"{"properties": {"id": {"pattern": "^r$"}}, "pattern": "^s$"}"#,
                r#"{"id": "x"}"#,
                "^r$",
            ),
        ] {
            let error = accept(document, instance)
                .expect_err("a pattern the walk never reached would be a silent pass");
            assert!(
                error.contains(&format!("does not match {pattern}")),
                "{document} enforces the pattern at that position: {error}"
            );
        }
    }

    /// `not` is the one position where a compiled pattern is proved by an
    /// acceptance, because a `not` fires when its subschema *accepts*: a
    /// pattern the walk never reached would satisfy every instance and make
    /// the `not` refuse them all.
    #[test]
    fn a_pattern_inside_a_not_is_compiled() {
        assert!(
            accept(r#"{"not": {"pattern": "^n$"}}"#, r#""m""#).is_ok(),
            "a value the inner pattern refuses satisfies the `not`"
        );
        let error = accept(r#"{"not": {"pattern": "^n$"}}"#, r#""n""#)
            .expect_err("a value the inner pattern admits satisfies the `not`");
        assert!(
            error.contains("it satisfies a `not` subschema"),
            "the `not` is the assertion: {error}"
        );
    }

    /// The two documents the pack verifies every frame against, checked
    /// against a descent that shares nothing with the keyword-directed walk:
    /// this one reads every object and every array in the file, so a position
    /// the walk does not dispatch over cannot hide a pattern from it.
    #[test]
    fn every_pattern_in_a_published_document_is_compiled() {
        for name in [super::super::HELLO_SCHEMA, super::super::SNAPSHOT_SCHEMA] {
            let path = super::super::pack_root().join(name);
            let document: Value =
                serde_json::from_slice(&std::fs::read(&path).expect("read the published document"))
                    .expect("parse the published document");
            let schema =
                Schema::compile(name, document.clone()).expect("the published document compiles");
            let mut positions = Vec::new();
            every_pattern(&document, "", &mut positions);
            assert!(!positions.is_empty(), "{name} carries patterns to check");
            for position in positions {
                assert!(
                    schema.patterns.contains_key(&position),
                    "{name} compiles the pattern at {position}"
                );
            }
        }
    }

    /// Every `pattern` in `node`, as the position of the object that carries
    /// it: the same position the evaluator arrives at, and deliberately not
    /// the same traversal.
    fn every_pattern(node: &Value, at: &str, positions: &mut Vec<String>) {
        match node {
            Value::Object(members) => {
                if members.get("pattern").and_then(Value::as_str).is_some() {
                    positions.push(at.to_string());
                }
                for (key, value) in members {
                    every_pattern(value, &format!("{at}/{key}"), positions);
                }
            }
            Value::Array(entries) => {
                for (index, entry) in entries.iter().enumerate() {
                    every_pattern(entry, &format!("{at}/{index}"), positions);
                }
            }
            _ => {}
        }
    }
}
