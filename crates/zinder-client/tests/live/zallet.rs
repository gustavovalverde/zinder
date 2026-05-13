#![allow(
    missing_docs,
    reason = "Live test names describe the behavior under test."
)]

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use eyre::{WrapErr, eyre};
use zinder_core::Network;
use zinder_core::wire::encode_zinder_native_chain_name;
use zinder_testkit::live::{init, require_env, require_live_for};

const ENABLE_ZALLET_GATE: &str = "ZINDER_TEST_ZALLET";
const ZALLET_BINARY_ENV: &str = "ZINDER_TEST_ZALLET_BIN";
const ZALLET_CONFIG_ENV: &str = "ZINDER_TEST_ZALLET_CONFIG";
const ZALLET_CONFIG_MARKER_ENV: &str = "ZINDER_TEST_ZALLET_CONFIG_MUST_CONTAIN";
const ZALLET_ARGS_ENV: &str = "ZINDER_TEST_ZALLET_ARGS";
const ZALLET_OUTPUT_MARKER_ENV: &str = "ZINDER_TEST_ZALLET_OUTPUT_MUST_CONTAIN";
const EMBEDDED_ZAINO_CONFIG_FIELDS: &[&str] = &[
    "validator_address",
    "validator_cookie_path",
    "validator_user",
    "validator_password",
];

#[test]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
fn zallet_binary_runs_against_zinder_native_contract() -> eyre::Result<()> {
    let _guard = init();
    if !zallet_gate_enabled() {
        return Ok(());
    }
    let Some(live_env) = require_live_for(&[
        Network::ZcashRegtest,
        Network::ZcashTestnet,
        Network::ZcashMainnet,
    ])?
    else {
        return Ok(());
    };

    let config_path = PathBuf::from(require_env(ZALLET_CONFIG_ENV)?);
    let config_source = fs::read_to_string(&config_path)
        .wrap_err_with(|| format!("could not read {}", config_path.display()))?;
    let parsed_config: toml::Value = toml::from_str(&config_source).wrap_err_with(|| {
        format!(
            "Zallet config at {} is not valid TOML",
            config_path.display()
        )
    })?;
    reject_embedded_zaino_config(&parsed_config, &config_path)?;
    require_config_marker(&parsed_config)?;

    let zallet_binary = env::var(ZALLET_BINARY_ENV).unwrap_or_else(|_| "zallet".to_owned());
    let zallet_args = require_args()?;
    let output = Command::new(&zallet_binary)
        .args(&zallet_args)
        .env(
            "ZINDER_NETWORK",
            encode_zinder_native_chain_name(live_env.network()),
        )
        .output()
        .wrap_err_with(|| format!("failed to execute {zallet_binary}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(eyre!(
            "Zallet command failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            stdout,
            stderr
        ));
    }

    let output_marker = require_env(ZALLET_OUTPUT_MARKER_ENV)?;
    if !stdout.contains(&output_marker) && !stderr.contains(&output_marker) {
        return Err(eyre!(
            "Zallet command output did not contain required marker {output_marker:?}\nstdout:\n{}\nstderr:\n{}",
            stdout,
            stderr
        ));
    }

    Ok(())
}

fn zallet_gate_enabled() -> bool {
    env::var(ENABLE_ZALLET_GATE).as_deref() == Ok("1")
}

fn require_config_marker(parsed_config: &toml::Value) -> eyre::Result<()> {
    let marker_text = require_env(ZALLET_CONFIG_MARKER_ENV)?;
    let marker_table: toml::Table = toml::from_str(&marker_text).wrap_err_with(|| {
        format!(
            "{ZALLET_CONFIG_MARKER_ENV} must be a `key = value` TOML fragment, got {marker_text:?}"
        )
    })?;
    if marker_table.is_empty() {
        return Err(eyre!(
            "{ZALLET_CONFIG_MARKER_ENV} must contain at least one key = value pair"
        ));
    }
    for (marker_key, marker_value) in &marker_table {
        if !config_contains_key_value(parsed_config, marker_key, marker_value) {
            return Err(eyre!(
                "Zallet config does not contain required active entry `{marker_key} = {marker_value}`; \
                 set {ZALLET_CONFIG_MARKER_ENV} to a TOML fragment that proves the Zallet build is \
                 pointed at Zinder's native contract"
            ));
        }
    }
    Ok(())
}

fn reject_embedded_zaino_config(
    parsed_config: &toml::Value,
    config_path: &Path,
) -> eyre::Result<()> {
    for field in EMBEDDED_ZAINO_CONFIG_FIELDS {
        if config_contains_key(parsed_config, field) {
            return Err(eyre!(
                "{} contains active `{field}`; that is Zallet's embedded-Zaino \
                 validator path, not the Zinder native contract",
                config_path.display()
            ));
        }
    }
    Ok(())
}

fn require_args() -> eyre::Result<Vec<String>> {
    let args_source = require_env(ZALLET_ARGS_ENV)?;
    let args: Vec<String> = args_source
        .split_whitespace()
        .map(ToOwned::to_owned)
        .collect();
    if args.is_empty() {
        Err(eyre!(
            "{ZALLET_ARGS_ENV} must contain at least one argument"
        ))
    } else {
        Ok(args)
    }
}

fn config_contains_key(parsed_config: &toml::Value, target_key: &str) -> bool {
    match parsed_config {
        toml::Value::Table(table) => table.iter().any(|(entry_key, entry_value)| {
            entry_key == target_key || config_contains_key(entry_value, target_key)
        }),
        toml::Value::Array(entries) => entries
            .iter()
            .any(|entry| config_contains_key(entry, target_key)),
        toml::Value::String(_)
        | toml::Value::Integer(_)
        | toml::Value::Float(_)
        | toml::Value::Boolean(_)
        | toml::Value::Datetime(_) => false,
    }
}

fn config_contains_key_value(
    parsed_config: &toml::Value,
    target_key: &str,
    target_value: &toml::Value,
) -> bool {
    match parsed_config {
        toml::Value::Table(table) => table.iter().any(|(entry_key, entry_value)| {
            (entry_key == target_key && entry_value == target_value)
                || config_contains_key_value(entry_value, target_key, target_value)
        }),
        toml::Value::Array(entries) => entries
            .iter()
            .any(|entry| config_contains_key_value(entry, target_key, target_value)),
        toml::Value::String(_)
        | toml::Value::Integer(_)
        | toml::Value::Float(_)
        | toml::Value::Boolean(_)
        | toml::Value::Datetime(_) => false,
    }
}
