//! Default ports and addresses used by every shared section helper.
//!
//! Centralizing defaults here keeps the per-service port table in one
//! place. Operators reviewing port assignments read this module; service
//! binaries look up their defaults through the shared
//! `ConfigLoader::with_*_section` helpers rather than re-declaring
//! constants.

use crate::sections::ServiceIdentifier;

/// Default operational endpoint listen address per service.
///
/// Binds to `127.0.0.1` so the safe default works on bare metal without
/// exposing the endpoint. Container deployments override via
/// `ZINDER_OPS__LISTEN_ADDR=0.0.0.0:<port>` baked in each service's
/// Dockerfile.
#[must_use]
pub const fn default_ops_listen_addr(service: ServiceIdentifier) -> &'static str {
    match service {
        ServiceIdentifier::Ingest => "127.0.0.1:9105",
        ServiceIdentifier::CompatLightwalletd => "127.0.0.1:9107",
        ServiceIdentifier::CompatCipherscan => "127.0.0.1:9108",
        ServiceIdentifier::Explorer => "127.0.0.1:9069",
    }
}

/// Default gRPC listen address per service, when the service binds one.
///
/// [`ServiceIdentifier::Ingest`] returns `None` because the ingest writer
/// does not expose a public gRPC endpoint; its private `IngestControl`
/// endpoint uses [`DEFAULT_INGEST_CONTROL_LISTEN_ADDR`].
#[must_use]
pub const fn default_grpc_listen_addr(service: ServiceIdentifier) -> Option<&'static str> {
    match service {
        ServiceIdentifier::CompatLightwalletd => Some("127.0.0.1:9067"),
        ServiceIdentifier::Explorer => Some("127.0.0.1:9068"),
        ServiceIdentifier::CompatCipherscan | ServiceIdentifier::Ingest => None,
    }
}

/// Default ingest-control writer listen address.
pub const DEFAULT_INGEST_CONTROL_LISTEN_ADDR: &str = "127.0.0.1:9100";

/// Default ingest-control reader URL.
pub const DEFAULT_INGEST_CONTROL_READER_URL: &str = "http://127.0.0.1:9100";

/// Default directory used by the canonical owner to stage checkpoint
/// candidates. Operators may override this with
/// `ingest_control.checkpoint_staging_root`.
pub const DEFAULT_INGEST_CONTROL_CHECKPOINT_STAGING_ROOT: &str = "/var/lib/zinder/checkpoints";

/// Default private projector-owner control listen address.
pub const DEFAULT_PROJECTOR_CONTROL_LISTEN_ADDR: &str = "127.0.0.1:9101";

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn ops_default_ports_are_distinct() {
        let ports: HashSet<&'static str> = [
            default_ops_listen_addr(ServiceIdentifier::Ingest),
            default_ops_listen_addr(ServiceIdentifier::CompatLightwalletd),
            default_ops_listen_addr(ServiceIdentifier::CompatCipherscan),
            default_ops_listen_addr(ServiceIdentifier::Explorer),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            ports.len(),
            4,
            "per-service ops ports must be distinct so a single host can run \
             all four binaries without port conflicts",
        );
    }
}
