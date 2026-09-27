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

use std::collections::BTreeMap;

use regex::Regex;
use serde_json::Value;

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
        collect_patterns(name, &document, &mut patterns)?;
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
        let object = schema.as_object()?;
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
        let at = at(pointer);
        let reference = object.get("$ref")?.as_str()?;
        // The only form the shipped documents use: a local `$defs` pointer. A
        // remote or nested reference is refused rather than followed.
        let name = reference
            .strip_prefix("#/$defs/")
            .filter(|name| !name.is_empty() && !name.contains('/'))?;
        let defs = self.root.get("$defs")?.as_object()?;
        let target = match defs.get(name) {
            Some(target) => target,
            None => return Some(format!("{at}: $ref {reference} is unresolvable")),
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
                if expected != &Value::Bool(false) {
                    return None;
                }
                let members = instance.as_object()?;
                let declared = object.get("properties").and_then(Value::as_object);
                members.keys().find_map(|name| {
                    let declared_here =
                        declared.is_some_and(|declared| declared.contains_key(name));
                    (!declared_here)
                        .then_some(format!("{at}: it has the undeclared member {name:?}"))
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
/// identity test, and the identity is the *value*: `serde_json`'s own `PartialEq`
/// compares the two `Number` variants structurally, which is what a constant
/// means here, so this only exists to make that explicit at the one call site.
fn const_holds(instance: &Value, expected: &Value) -> bool {
    match (instance, expected) {
        (Value::Number(left), Value::Number(right)) => {
            left == right || left.as_f64() == right.as_f64()
        }
        _ => instance == expected,
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
        // JSON has one number type, so an integer is a number with no fractional
        // part — which is what `as_f64` cannot tell and `is_i64`/`is_u64` can.
        "integer" => instance.is_i64() || instance.is_u64(),
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

/// Keywords whose value is a map from an arbitrary name to a subschema, rather
/// than a subschema itself. The distinction is why this walks rather than
/// trusting `serde_json`: a member name is not a keyword, and treating it as
/// one would refuse every schema that has properties at all.
const SUBSCHEMA_MAPS: &[&str] = &["$defs", "properties"];

fn collect_patterns(
    name: &str,
    node: &Value,
    patterns: &mut BTreeMap<String, Regex>,
) -> Result<(), String> {
    let Some(members) = node.as_object() else {
        return Ok(());
    };
    for (key, value) in members {
        if !SUPPORTED.contains(&key.as_str()) {
            return Err(format!(
                "{name}: the schema uses {key:?}, which this validator does not evaluate"
            ));
        }
        if key == "pattern" {
            let text = value
                .as_str()
                .ok_or_else(|| format!("{name}: a `pattern` must be a string, not {value}"))?;
            let compiled = Regex::new(text)
                .map_err(|error| format!("{name}: pattern {text:?} does not compile: {error}"))?;
            patterns.insert(text.to_string(), compiled);
            continue;
        }
        if SUBSCHEMA_MAPS.contains(&key.as_str()) {
            let Some(subschemas) = value.as_object() else {
                return Err(format!("{name}: {key:?} must be an object of subschemas"));
            };
            for subschema in subschemas.values() {
                collect_patterns(name, subschema, patterns)?;
            }
            continue;
        }
        // `oneOf` and `not` hold schemas, and `enum`/`required` hold plain
        // values. Descending into all three shapes is safe: a plain value has
        // no object members to mistake for keywords.
        collect_patterns(name, value, patterns)?;
    }
    Ok(())
}
