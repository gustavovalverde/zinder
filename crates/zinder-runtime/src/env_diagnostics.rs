//! Operator-facing diagnostics for environment-variable mistakes.
//!
//! `config-rs` treats `__` as a TOML nesting separator and leaves every
//! other character (including single `_`) inside the key name. Anyone who
//! types `ZINDER_OPS_LISTEN_ADDR` (single underscore) for what should be
//! `ZINDER_OPS__LISTEN_ADDR` ends up with a top-level `ops_listen_addr`
//! key. Strict schemas reject that with a serde "unknown field" error
//! whose text mentions the produced config key but not the env var that
//! caused it.
//!
//! This module owns the heuristic that maps such errors back to the
//! offending `ZINDER_…` name and proposes the most plausible corrected
//! form.

use std::collections::HashMap;

use ::config::ConfigError as InnerConfigError;

/// Information about an environment variable that was rejected during
/// configuration deserialization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RejectedEnvVar {
    /// Original env var name the operator set, e.g. `ZINDER_OPS_LISTEN_ADDR`.
    pub original_name: String,
    /// Config-key path the env var produced, e.g. `ops_listen_addr`.
    pub rejected_key: String,
    /// Suggested corrected env var name, when the heuristic matches.
    pub suggested_name: Option<String>,
}

/// Translates a `config-rs` error into a rejected-env-var record when the
/// failure can be attributed to a specific env var the operator set.
///
/// Returns `None` when the error has no extractable field name or when no
/// env var in the reverse index produced that field. Callers fall back to
/// the generic `ConfigError::Load` rendering in that case.
pub(crate) fn translate_env_error(
    inner: &InnerConfigError,
    reverse_index: &HashMap<String, String>,
) -> Option<RejectedEnvVar> {
    let message = inner_error_message(inner)?;
    let rejected_key = extract_unknown_field_name(message)?;
    let original_name = reverse_index.get(&rejected_key)?.clone();
    let suggested_name = suggest_double_underscore_form(&original_name);
    Some(RejectedEnvVar {
        original_name,
        rejected_key,
        suggested_name,
    })
}

/// Suggests an env var name with `__` after the first section, on the
/// assumption that the operator typed `_` where they should have typed
/// `__` to separate a TOML section from a field.
///
/// For `ZINDER_OPS_LISTEN_ADDR` returns `ZINDER_OPS__LISTEN_ADDR`. For env
/// vars without an underscore after the `ZINDER_` prefix returns `None`.
/// The suggestion is heuristic and only fixes the first section boundary;
/// when an operator nested three levels deep with single underscores the
/// hint still gets them to the next iteration faster than the raw serde
/// message.
fn suggest_double_underscore_form(original_name: &str) -> Option<String> {
    let suffix = original_name.strip_prefix("ZINDER_")?;
    let first_underscore = suffix.find('_')?;
    let (head, tail) = suffix.split_at(first_underscore);
    let rest = tail.strip_prefix('_')?;
    Some(format!("ZINDER_{head}__{rest}"))
}

fn inner_error_message(inner: &InnerConfigError) -> Option<&str> {
    if let InnerConfigError::Message(message) = inner {
        Some(message.as_str())
    } else {
        None
    }
}

/// Extracts the field name from a serde "unknown field" error message.
///
/// serde renders the field name between single backticks in stable
/// releases and between ASCII double quotes in newer ones. Both forms are
/// accepted so the heuristic survives serde upgrades.
fn extract_unknown_field_name(message: &str) -> Option<String> {
    for (marker, closing) in [("unknown field `", '`'), ("unknown field \"", '"')] {
        if let Some(start) = message.find(marker) {
            let after = &message[start + marker.len()..];
            if let Some(end) = after.find(closing) {
                return Some(after[..end].to_owned());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggests_double_underscore_after_first_section() {
        assert_eq!(
            suggest_double_underscore_form("ZINDER_OPS_LISTEN_ADDR"),
            Some("ZINDER_OPS__LISTEN_ADDR".to_owned()),
        );
    }

    #[test]
    fn suggests_nothing_when_suffix_has_no_underscore() {
        assert_eq!(suggest_double_underscore_form("ZINDER_NETWORK"), None);
    }

    #[test]
    fn suggests_nothing_when_zinder_prefix_missing() {
        assert_eq!(suggest_double_underscore_form("FOO_BAR"), None);
    }

    #[test]
    fn extracts_field_name_from_backtick_message() {
        let message = "unknown field `ops_listen_addr`, expected one of `network`, `ops`";
        assert_eq!(
            extract_unknown_field_name(message),
            Some("ops_listen_addr".to_owned()),
        );
    }

    #[test]
    fn extracts_field_name_from_quoted_message() {
        let message = "unknown field \"ops_listen_addr\", expected one of \"network\"";
        assert_eq!(
            extract_unknown_field_name(message),
            Some("ops_listen_addr".to_owned()),
        );
    }

    #[test]
    fn extracts_nothing_without_unknown_field_marker() {
        assert_eq!(extract_unknown_field_name("some other error"), None);
    }
}
