//! One-shot `PostgreSQL` schema migration command.

use std::{path::PathBuf, process::ExitCode};

use clap::Parser;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zinder_postgres::{DatabaseConfig, MigrationError, MigrationOutcome, migrate_database};
use zinder_runtime::{
    ConfigError, ConfigLoader, DeploymentSection, DeploymentToml, DeploymentTopology,
    NetworkSection, NetworkToml, PostgresStorageSection, PostgresStorageToml,
    install_tracing_subscriber, require_field,
};

#[derive(Parser)]
#[command(name = "zinder-migrate")]
#[command(about = "Apply the exact PostgreSQL schema required by Zinder")]
#[command(version)]
struct Cli {
    /// TOML configuration file loaded before environment variables.
    #[arg(long = "config")]
    config_path: Option<PathBuf>,
    /// Print redacted resolved migration configuration without connecting.
    #[arg(long = "print-config")]
    print_config: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct MigrateFileConfig {
    deployment: DeploymentSection,
    network: NetworkSection,
    storage: StorageSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct StorageSection {
    postgres: Option<PostgresStorageSection>,
}

#[derive(Debug)]
struct ResolvedMigrateConfig {
    migration: DatabaseConfig,
    rendered: MigrateConfigToml,
}

#[derive(Debug, Serialize)]
struct MigrateConfigToml {
    deployment: DeploymentToml,
    network: NetworkToml,
    storage: StorageToml,
}

#[derive(Debug, Serialize)]
struct StorageToml {
    postgres: PostgresStorageToml,
}

#[derive(Debug, Error)]
enum MigrateCommandError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Migration(#[from] MigrationError),
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    install_tracing_subscriber();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(
                target: "zinder::migrate",
                event = "migration_failed",
                %error,
                "PostgreSQL migration failed"
            );
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), MigrateCommandError> {
    let config = load_config(cli.config_path)?;
    if cli.print_config {
        print_config(&config.rendered)?;
        return Ok(());
    }
    let outcome = migrate_database(&config.migration).await?;
    tracing::info!(
        target: "zinder::migrate",
        event = "migration_completed",
        outcome = outcome_label(outcome),
        topology = DeploymentTopology::PostgresHorizontal.as_str(),
        "PostgreSQL schema is current"
    );
    Ok(())
}

fn load_config(config_path: Option<PathBuf>) -> Result<ResolvedMigrateConfig, ConfigError> {
    let raw: MigrateFileConfig = ConfigLoader::new()
        .with_file(config_path)
        .with_zinder_env()?
        .load()?;
    let topology = raw.deployment.resolve()?;
    if topology != DeploymentTopology::PostgresHorizontal {
        return Err(ConfigError::invalid(format!(
            "zinder-migrate requires deployment.topology = {}, found {}",
            DeploymentTopology::PostgresHorizontal,
            topology
        )));
    }
    let network = raw.network.resolve()?;
    let postgres = require_field(raw.storage.postgres, "storage.postgres")?.resolve()?;
    let rendered_postgres = PostgresStorageToml::redacted(&postgres);
    let (database_url_path, tls) = postgres.into_parts();
    Ok(ResolvedMigrateConfig {
        migration: DatabaseConfig::new(database_url_path, tls, network, "zinder-migrate"),
        rendered: MigrateConfigToml {
            deployment: DeploymentToml::from_resolved(topology),
            network: NetworkToml::from_network(network),
            storage: StorageToml {
                postgres: rendered_postgres,
            },
        },
    })
}

#[allow(
    clippy::print_stdout,
    reason = "--print-config is an operator-requested diagnostic with secret paths redacted"
)]
fn print_config(config: &MigrateConfigToml) -> Result<(), ConfigError> {
    let rendered =
        toml::to_string_pretty(config).map_err(|source| ConfigError::Render { source })?;
    println!("{rendered}");
    Ok(())
}

const fn outcome_label(outcome: MigrationOutcome) -> &'static str {
    match outcome {
        MigrationOutcome::Applied => "applied",
        MigrationOutcome::AlreadyCurrent => "already-current",
    }
}

#[cfg(test)]
mod tests {
    use super::MigrateFileConfig;

    #[test]
    fn unknown_top_level_config_sections_fail_closed() {
        let outcome = toml::from_str::<MigrateFileConfig>(
            r#"
[deployment]
topology = "postgres-horizontal"

[node]
json_rpc_addr = "http://127.0.0.1:18232"
"#,
        );

        assert!(outcome.is_err());
    }
}
