//! Server-authoritative value validation.
//!
//! The web min/max in [`super::WidgetKind`] is advisory UI metadata; this is
//! the gate the server applies before merging an incoming value. Invalid input
//! is rejected (no silent clamping), so a client always knows whether its value
//! was accepted.

use serde_json::Value;

use super::{ObjectFieldDescriptor, ValidationKind};

/// A value failed validation for a field. Carries a human-readable reason the
/// server surfaces to the client (HTTP 400).
#[derive(Debug, Clone, PartialEq)]
pub struct ValidationError {
    pub reason: String,
}

impl ValidationError {
    fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

/// Validate `value` against `kind`. The value is the raw JSON the client sent
/// for one field (already typed by serde_json, e.g. a number or string).
pub fn validate_value(kind: &ValidationKind, value: &Value) -> Result<(), ValidationError> {
    match kind {
        ValidationKind::None => Ok(()),
        ValidationKind::RangeU64 { min, max } => {
            let n = value
                .as_u64()
                .ok_or_else(|| ValidationError::new("expected a non-negative integer"))?;
            if n < *min {
                return Err(ValidationError::new(format!("must be at least {min}")));
            }
            if let Some(max) = max {
                if n > *max {
                    return Err(ValidationError::new(format!("must be at most {max}")));
                }
            }
            Ok(())
        }
        ValidationKind::NonEmptyString => {
            let s = value
                .as_str()
                .ok_or_else(|| ValidationError::new("expected a string"))?;
            if s.trim().is_empty() {
                return Err(ValidationError::new("must not be empty"));
            }
            Ok(())
        }
        ValidationKind::StringValue => {
            value
                .as_str()
                .ok_or_else(|| ValidationError::new("expected a string"))?;
            Ok(())
        }
        ValidationKind::BoolValue => {
            value
                .as_bool()
                .ok_or_else(|| ValidationError::new("expected a boolean"))?;
            Ok(())
        }
        ValidationKind::RangeI64 { min, max } => {
            let n = value
                .as_i64()
                .ok_or_else(|| ValidationError::new("expected an integer"))?;
            if let Some(min) = min {
                if n < *min {
                    return Err(ValidationError::new(format!("must be at least {min}")));
                }
            }
            if let Some(max) = max {
                if n > *max {
                    return Err(ValidationError::new(format!("must be at most {max}")));
                }
            }
            Ok(())
        }
        ValidationKind::MemoryLimit => {
            let s = value
                .as_str()
                .ok_or_else(|| ValidationError::new("expected a string"))?;
            crate::session::validate_memory_limit(s).map_err(ValidationError::new)
        }
        ValidationKind::VolumeList => {
            validate_string_list(value, crate::session::validate_volume_format)
        }
        ValidationKind::EnvList => validate_string_list(value, crate::session::validate_env_format),
        ValidationKind::PortMappingList => {
            validate_string_list(value, crate::session::validate_port_mapping_format)
        }
        ValidationKind::Network => {
            let s = value
                .as_str()
                .ok_or_else(|| ValidationError::new("expected a string"))?;
            crate::session::validate_network_format(s).map_err(ValidationError::new)
        }
        ValidationKind::OneOf { options } => {
            let s = value
                .as_str()
                .ok_or_else(|| ValidationError::new("expected a string"))?;
            if options.iter().any(|o| o == s) {
                Ok(())
            } else {
                Err(ValidationError::new(format!(
                    "must be one of: {}",
                    options.join(", ")
                )))
            }
        }
        ValidationKind::Cron => {
            let s = value
                .as_str()
                .ok_or_else(|| ValidationError::new("expected a string"))?;
            validate_cron(s).map_err(ValidationError::new)
        }
        ValidationKind::ObjectList {
            id_field,
            fields,
            min_items,
            max_items,
        } => validate_object_list(value, id_field, fields, *min_items, *max_items),
    }
}

/// Validate an object-list value: an array of objects, item-count bounds, a
/// unique non-empty stable id per item, only declared fields plus the id, and
/// each present field against its own descriptor (a required field must be
/// present). Dynamic-select membership is NOT checked here: catalogs change,
/// and a saved id is authoritatively revalidated at `sessions.create`.
fn validate_object_list(
    value: &Value,
    id_field: &str,
    fields: &[ObjectFieldDescriptor],
    min_items: Option<u32>,
    max_items: Option<u32>,
) -> Result<(), ValidationError> {
    let arr = value
        .as_array()
        .ok_or_else(|| ValidationError::new("expected a list of items"))?;
    if let Some(min) = min_items {
        if (arr.len() as u32) < min {
            return Err(ValidationError::new(format!(
                "needs at least {min} item(s)"
            )));
        }
    }
    if let Some(max) = max_items {
        if (arr.len() as u32) > max {
            return Err(ValidationError::new(format!(
                "allows at most {max} item(s)"
            )));
        }
    }
    let mut seen_ids = std::collections::HashSet::new();
    for (i, item) in arr.iter().enumerate() {
        let obj = item
            .as_object()
            .ok_or_else(|| ValidationError::new(format!("item {i} must be an object")))?;
        let id = obj
            .get(id_field)
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                ValidationError::new(format!("item {i} is missing a non-empty {id_field:?}"))
            })?;
        if !seen_ids.insert(id.to_string()) {
            return Err(ValidationError::new(format!("duplicate item id {id:?}")));
        }
        for key in obj.keys() {
            if key != id_field && !fields.iter().any(|f| &f.field == key) {
                return Err(ValidationError::new(format!(
                    "item {i} has an undeclared field {key:?}"
                )));
            }
        }
        for field in fields {
            match obj.get(&field.field) {
                Some(v) => validate_value(&field.validation, v).map_err(|e| {
                    ValidationError::new(format!("item {i} field {:?}: {}", field.field, e.reason))
                })?,
                None if field.required => {
                    return Err(ValidationError::new(format!(
                        "item {i} is missing required field {:?}",
                        field.field
                    )));
                }
                None => {}
            }
        }
    }
    Ok(())
}

/// Validate a 5-field cron expression (minute hour day-of-month month
/// day-of-week). Each field is `*`, or a comma list of items where an item is
/// a number, an `a-b` range, or either followed by `/step`, all within the
/// field's inclusive bounds. Mirrors the plugin scheduler's `croner` dialect
/// closely enough to reject garbage at settings-write time; the scheduler is
/// the authoritative parser at run time.
fn validate_cron(expr: &str) -> Result<(), String> {
    // day-of-week is 0-7 (both 0 and 7 are Sunday), matching croner and the
    // web-side cronValidation.ts.
    const BOUNDS: [(u32, u32); 5] = [(0, 59), (0, 23), (1, 31), (1, 12), (0, 7)];
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(format!(
            "cron must have 5 fields (got {}); e.g. \"0 9 * * 1-5\"",
            fields.len()
        ));
    }
    for (field, (lo, hi)) in fields.iter().zip(BOUNDS.iter()) {
        for item in field.split(',') {
            validate_cron_item(item, *lo, *hi)?;
        }
    }
    Ok(())
}

fn validate_cron_item(item: &str, lo: u32, hi: u32) -> Result<(), String> {
    if item.is_empty() {
        return Err("empty cron field item".to_string());
    }
    let (range, step) = match item.split_once('/') {
        Some((r, s)) => {
            let step: u32 = s.parse().map_err(|_| format!("invalid cron step {s:?}"))?;
            if step == 0 {
                return Err("cron step must be positive".to_string());
            }
            (r, Some(step))
        }
        None => (item, None),
    };
    // `*` (optionally with a step) covers the whole range.
    if range == "*" {
        return Ok(());
    }
    let in_bounds = |n: u32| n >= lo && n <= hi;
    match range.split_once('-') {
        Some((a, b)) => {
            let a: u32 = a.parse().map_err(|_| format!("invalid cron value {a:?}"))?;
            let b: u32 = b.parse().map_err(|_| format!("invalid cron value {b:?}"))?;
            if !in_bounds(a) || !in_bounds(b) {
                return Err(format!("cron value out of range {lo}-{hi}"));
            }
            if a > b {
                return Err(format!("cron range {a}-{b} is reversed"));
            }
        }
        None => {
            let n: u32 = range
                .parse()
                .map_err(|_| format!("invalid cron value {range:?}"))?;
            if !in_bounds(n) {
                return Err(format!("cron value {n} out of range {lo}-{hi}"));
            }
            // A bare number with a step (e.g. `5/10`) is meaningless.
            if step.is_some() {
                return Err("cron step requires a range or *".to_string());
            }
        }
    }
    Ok(())
}

/// Validate that `value` is a JSON array of strings and each passes `check`.
fn validate_string_list(
    value: &Value,
    check: impl Fn(&str) -> Result<(), String>,
) -> Result<(), ValidationError> {
    let arr = value
        .as_array()
        .ok_or_else(|| ValidationError::new("expected a list"))?;
    for entry in arr {
        let s = entry
            .as_str()
            .ok_or_else(|| ValidationError::new("list entries must be strings"))?;
        check(s).map_err(ValidationError::new)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn range_rejects_below_min() {
        let kind = ValidationKind::RangeU64 {
            min: 1,
            max: Some(128),
        };
        assert!(validate_value(&kind, &json!(0)).is_err());
        assert!(validate_value(&kind, &json!(1)).is_ok());
        assert!(validate_value(&kind, &json!(128)).is_ok());
        assert!(validate_value(&kind, &json!(129)).is_err());
    }

    #[test]
    fn range_rejects_non_integer() {
        let kind = ValidationKind::RangeU64 { min: 0, max: None };
        assert!(validate_value(&kind, &json!("nope")).is_err());
        assert!(validate_value(&kind, &json!(-1)).is_err());
    }

    #[test]
    fn non_empty_string_trims() {
        assert!(validate_value(&ValidationKind::NonEmptyString, &json!("  ")).is_err());
        assert!(validate_value(&ValidationKind::NonEmptyString, &json!("x")).is_ok());
    }

    #[test]
    fn typed_values_reject_mismatched_json() {
        // str: any string incl. empty, but never a number/object.
        assert!(validate_value(&ValidationKind::StringValue, &json!("")).is_ok());
        assert!(validate_value(&ValidationKind::StringValue, &json!("x")).is_ok());
        assert!(validate_value(&ValidationKind::StringValue, &json!(1)).is_err());
        assert!(validate_value(&ValidationKind::StringValue, &json!({})).is_err());
        // bool: only true/false.
        assert!(validate_value(&ValidationKind::BoolValue, &json!(true)).is_ok());
        assert!(validate_value(&ValidationKind::BoolValue, &json!("true")).is_err());
        // signed range, single- and double-sided.
        let signed = ValidationKind::RangeI64 {
            min: Some(-5),
            max: Some(5),
        };
        assert!(validate_value(&signed, &json!(-5)).is_ok());
        assert!(validate_value(&signed, &json!(6)).is_err());
        assert!(validate_value(&signed, &json!("3")).is_err());
        let lower_only = ValidationKind::RangeI64 {
            min: Some(-1),
            max: None,
        };
        assert!(validate_value(&lower_only, &json!(1_000)).is_ok());
        assert!(validate_value(&lower_only, &json!(-2)).is_err());
    }

    #[test]
    fn memory_limit_grammar() {
        assert!(validate_value(&ValidationKind::MemoryLimit, &json!("512m")).is_ok());
        assert!(validate_value(&ValidationKind::MemoryLimit, &json!("")).is_ok());
        assert!(validate_value(&ValidationKind::MemoryLimit, &json!("512mb")).is_err());
    }

    #[test]
    fn volume_list_grammar() {
        assert!(validate_value(&ValidationKind::VolumeList, &json!(["/h:/c"])).is_ok());
        assert!(validate_value(&ValidationKind::VolumeList, &json!(["bad"])).is_err());
    }

    #[test]
    fn env_list_grammar() {
        assert!(validate_value(&ValidationKind::EnvList, &json!(["KEY"])).is_ok());
        assert!(validate_value(&ValidationKind::EnvList, &json!(["KEY=value"])).is_ok());
        assert!(validate_value(&ValidationKind::EnvList, &json!(["_K=v", "A1=b"])).is_ok());
        assert!(validate_value(&ValidationKind::EnvList, &json!(["1BAD=v"])).is_err());
        assert!(validate_value(&ValidationKind::EnvList, &json!(["has space"])).is_err());
        assert!(validate_value(&ValidationKind::EnvList, &json!("notalist")).is_err());
    }

    #[test]
    fn one_of_membership() {
        let kind = ValidationKind::OneOf {
            options: vec!["fast".into(), "slow".into()],
        };
        assert!(validate_value(&kind, &json!("fast")).is_ok());
        assert!(validate_value(&kind, &json!("turbo")).is_err());
        assert!(validate_value(&kind, &json!(3)).is_err());
    }

    #[test]
    fn port_mapping_list_grammar() {
        assert!(validate_value(&ValidationKind::PortMappingList, &json!(["3000:3000"])).is_ok());
        assert!(validate_value(&ValidationKind::PortMappingList, &json!(["8080:80"])).is_ok());
        assert!(validate_value(&ValidationKind::PortMappingList, &json!(["3000"])).is_err());
        assert!(validate_value(&ValidationKind::PortMappingList, &json!(["a:b"])).is_err());
    }

    #[test]
    fn network_grammar() {
        assert!(validate_value(&ValidationKind::Network, &json!("")).is_ok());
        assert!(validate_value(&ValidationKind::Network, &json!("none")).is_ok());
        assert!(validate_value(&ValidationKind::Network, &json!("egress-proxy")).is_ok());
        assert!(validate_value(&ValidationKind::Network, &json!("host")).is_err());
        assert!(validate_value(&ValidationKind::Network, &json!(42)).is_err());
    }

    #[test]
    fn cron_grammar() {
        let ok = ["0 9 * * 1-5", "*/15 * * * *", "0 0,12 1 */2 *", "* * * * *"];
        for e in ok {
            assert!(
                validate_value(&ValidationKind::Cron, &json!(e)).is_ok(),
                "{e}"
            );
        }
        let bad = [
            "0 9 * *",     // too few fields
            "0 9 * * * *", // too many fields
            "60 * * * *",  // minute out of range
            "* 24 * * *",  // hour out of range
            "* * 0 * *",   // dom below 1
            "* * * 13 *",  // month out of range
            "* * * * 8",   // dow out of range (0-7 valid, 8 not)
            "5-1 * * * *", // reversed range
            "*/0 * * * *", // zero step
            "abc * * * *", // non-numeric
        ];
        for e in bad {
            assert!(
                validate_value(&ValidationKind::Cron, &json!(e)).is_err(),
                "{e}"
            );
        }
        assert!(validate_value(&ValidationKind::Cron, &json!(5)).is_err());
    }

    fn jobs_validation() -> ValidationKind {
        ValidationKind::ObjectList {
            id_field: "id".into(),
            fields: vec![
                ObjectFieldDescriptor {
                    field: "agent".into(),
                    label: "Agent".into(),
                    description: String::new(),
                    required: true,
                    widget: super::super::ObjectFieldWidget::Text {
                        multiline: false,
                        mono: false,
                    },
                    validation: ValidationKind::NonEmptyString,
                    default: None,
                },
                ObjectFieldDescriptor {
                    field: "schedule".into(),
                    label: "Schedule".into(),
                    description: String::new(),
                    required: true,
                    widget: super::super::ObjectFieldWidget::Cron,
                    validation: ValidationKind::Cron,
                    default: None,
                },
            ],
            min_items: Some(0),
            max_items: Some(2),
        }
    }

    #[test]
    fn object_list_structural_rules() {
        let kind = jobs_validation();
        // Happy path.
        assert!(validate_value(
            &kind,
            &json!([{"id": "a", "agent": "claude", "schedule": "0 9 * * 1-5"}])
        )
        .is_ok());
        // Missing stable id.
        assert!(validate_value(
            &kind,
            &json!([{"agent": "claude", "schedule": "* * * * *"}])
        )
        .is_err());
        // Duplicate id.
        assert!(validate_value(
            &kind,
            &json!([
                {"id": "x", "agent": "a", "schedule": "* * * * *"},
                {"id": "x", "agent": "b", "schedule": "* * * * *"}
            ])
        )
        .is_err());
        // Missing required field.
        assert!(validate_value(&kind, &json!([{"id": "a", "agent": "claude"}])).is_err());
        // Undeclared field.
        assert!(validate_value(
            &kind,
            &json!([{"id": "a", "agent": "c", "schedule": "* * * * *", "bogus": 1}])
        )
        .is_err());
        // Nested field validation runs (bad cron).
        assert!(validate_value(
            &kind,
            &json!([{"id": "a", "agent": "c", "schedule": "bad"}])
        )
        .is_err());
        // max_items.
        assert!(validate_value(
            &kind,
            &json!([
                {"id": "1", "agent": "a", "schedule": "* * * * *"},
                {"id": "2", "agent": "b", "schedule": "* * * * *"},
                {"id": "3", "agent": "c", "schedule": "* * * * *"}
            ])
        )
        .is_err());
        // Not an array.
        assert!(validate_value(&kind, &json!({"id": "a"})).is_err());
    }
}
