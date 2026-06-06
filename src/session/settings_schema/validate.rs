//! Server-authoritative value validation.
//!
//! The web min/max in [`super::WidgetKind`] is advisory UI metadata; this is
//! the gate the server applies before merging an incoming value. Invalid input
//! is rejected (no silent clamping), so a client always knows whether its value
//! was accepted.

use serde_json::Value;

use super::ValidationKind;

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
    }
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
    fn port_mapping_list_grammar() {
        assert!(validate_value(&ValidationKind::PortMappingList, &json!(["3000:3000"])).is_ok());
        assert!(validate_value(&ValidationKind::PortMappingList, &json!(["8080:80"])).is_ok());
        assert!(validate_value(&ValidationKind::PortMappingList, &json!(["3000"])).is_err());
        assert!(validate_value(&ValidationKind::PortMappingList, &json!(["a:b"])).is_err());
    }
}
