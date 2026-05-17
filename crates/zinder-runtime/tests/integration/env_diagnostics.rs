//! End-to-end coverage of the env-var diagnostic loader.
//!
//! Drives the full layered loader (defaults + env + deserialization)
//! against a toy schema mirroring the shape of a service `RawConfig`, so
//! the heuristic is exercised through the same code path the four
//! binaries take at startup.

#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use eyre::{Result, eyre};
use serde::Deserialize;
use zinder_runtime::{ConfigError, ConfigLoader};

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ToyConfig {
    ops: OpsSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct OpsSection {
    listen_addr: Option<String>,
}

fn load_toy(env_pair: (&str, &str)) -> Result<ToyConfig, ConfigError> {
    ConfigLoader::new()
        .with_zinder_env_from_map([(env_pair.0.to_owned(), env_pair.1.to_owned())])?
        .load::<ToyConfig>()
}

#[test]
fn single_underscore_env_var_is_attributed_to_its_zinder_name() -> Result<()> {
    let outcome = load_toy(("ZINDER_OPS_LISTEN_ADDR", "0.0.0.0:9069"));
    let Err(error) = outcome else {
        return Err(eyre!(
            "expected single-underscore env var to be rejected, got Ok"
        ));
    };
    let ConfigError::RejectedEnvVar {
        original_name,
        rejected_key,
        suggested_name,
        ..
    } = error
    else {
        return Err(eyre!("expected ConfigError::RejectedEnvVar, got {error:?}"));
    };
    assert_eq!(original_name, "ZINDER_OPS_LISTEN_ADDR");
    assert_eq!(rejected_key, "ops_listen_addr");
    assert_eq!(suggested_name.as_deref(), Some("ZINDER_OPS__LISTEN_ADDR"));
    Ok(())
}

#[test]
fn rejected_env_var_display_carries_the_suggestion() -> Result<()> {
    let Err(error) = load_toy(("ZINDER_OPS_LISTEN_ADDR", "0.0.0.0:9069")) else {
        return Err(eyre!("expected loader to reject single-underscore env var"));
    };
    let rendered = error.to_string();
    assert!(
        rendered.contains("ZINDER_OPS_LISTEN_ADDR"),
        "rendered error must name the original env var:\n{rendered}",
    );
    assert!(
        rendered.contains("Try `ZINDER_OPS__LISTEN_ADDR` instead."),
        "rendered error must suggest the corrected env var name:\n{rendered}",
    );
    Ok(())
}

#[test]
fn double_underscore_env_var_loads_successfully() -> Result<()> {
    let config = load_toy(("ZINDER_OPS__LISTEN_ADDR", "0.0.0.0:9069"))?;
    assert_eq!(config.ops.listen_addr.as_deref(), Some("0.0.0.0:9069"));
    Ok(())
}

#[test]
fn env_var_outside_zinder_prefix_is_ignored() -> Result<()> {
    let config = load_toy(("UNRELATED_OPS_LISTEN_ADDR", "0.0.0.0:9069"))?;
    assert!(config.ops.listen_addr.is_none());
    Ok(())
}

#[test]
fn test_only_env_var_is_stripped_before_deserialization() -> Result<()> {
    let config = load_toy(("ZINDER_TEST_LIVE", "1"))?;
    assert!(config.ops.listen_addr.is_none());
    Ok(())
}
