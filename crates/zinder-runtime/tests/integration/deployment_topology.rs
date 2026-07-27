use serde::Deserialize;
use zinder_runtime::{ConfigError, DeploymentSection, DeploymentToml, DeploymentTopology};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeConfig {
    deployment: DeploymentSection,
}

#[test]
fn supported_deployment_topologies_resolve_from_exact_config_names()
-> Result<(), Box<dyn std::error::Error>> {
    for (configured, expected) in [
        ("rocksdb-single-host", DeploymentTopology::RocksDbSingleHost),
        (
            "postgres-horizontal",
            DeploymentTopology::PostgresHorizontal,
        ),
    ] {
        let parsed: RuntimeConfig =
            toml::from_str(&format!("[deployment]\ntopology = \"{configured}\"\n"))?;
        assert_eq!(parsed.deployment.resolve()?, expected);
        assert_eq!(
            DeploymentTopology::parse_config_name(configured),
            Some(expected)
        );
    }
    assert_eq!(
        DeploymentTopology::parse_config_name("postgres-scale-out"),
        None
    );

    Ok(())
}

#[test]
fn resolved_deployment_topology_renders_with_the_same_stable_name()
-> Result<(), Box<dyn std::error::Error>> {
    for (topology, expected) in [
        (
            DeploymentTopology::RocksDbSingleHost,
            "topology = \"rocksdb-single-host\"\n",
        ),
        (
            DeploymentTopology::PostgresHorizontal,
            "topology = \"postgres-horizontal\"\n",
        ),
    ] {
        let rendered = toml::to_string(&DeploymentToml::from_resolved(topology))?;
        assert_eq!(rendered, expected);
    }

    Ok(())
}

#[test]
fn unsupported_deployment_topology_is_rejected_with_supported_names()
-> Result<(), Box<dyn std::error::Error>> {
    let outcome =
        toml::from_str::<RuntimeConfig>("[deployment]\ntopology = \"rocksdb-horizontal\"\n");
    let Err(error) = outcome else {
        return Err("unsupported deployment topology was accepted".into());
    };
    let message = error.to_string();

    assert!(message.contains("unknown variant `rocksdb-horizontal`"));
    assert!(message.contains("`rocksdb-single-host`"));
    assert!(message.contains("`postgres-horizontal`"));

    Ok(())
}

#[test]
fn unknown_deployment_field_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
    let outcome = toml::from_str::<RuntimeConfig>(
        "[deployment]\ntopology = \"postgres-horizontal\"\nreplicas = 3\n",
    );
    let Err(error) = outcome else {
        return Err("unknown deployment field was accepted".into());
    };

    assert!(error.to_string().contains("unknown field `replicas`"));

    Ok(())
}

#[test]
fn missing_deployment_topology_fails_closed() -> Result<(), Box<dyn std::error::Error>> {
    let parsed: RuntimeConfig = toml::from_str("[deployment]\n")?;

    assert!(matches!(
        parsed.deployment.resolve(),
        Err(ConfigError::MissingField {
            field: "deployment.topology"
        })
    ));

    Ok(())
}
