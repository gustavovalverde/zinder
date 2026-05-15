//! Operator-facing environment-variable contract for Zinder service binaries.
//!
//! The `ZINDER_*` env-var schema is part of Zinder's public interface (see
//! [Public interfaces §Environment variable mapping]). This module is the
//! single source of truth for the operator-facing surface: the entries in
//! [`ENVIRONMENT_VARIABLES`] feed both the rendered Markdown table embedded
//! in `docs/architecture/public-interfaces.md` and the CI drift test that
//! catches doc/source skew.
//!
//! [`render_environment_variable_table`] returns the Markdown the doc block
//! must contain. Tests assert exact equality; the doc-mirror integration
//! test prints the expected rendering on failure so the operator can
//! copy-paste the canonical output back into the doc.
//!
//! Adding a new env-var to a service config is a two-line change here plus
//! the corresponding field on the relevant config struct. The mirror test
//! makes drift in either direction a build failure.
//!
//! [Public interfaces §Environment variable mapping]:
//!     ../../docs/architecture/public-interfaces.md#environment-variable-mapping

use std::fmt::Write as _;

/// One operator-facing environment variable.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct EnvVarDoc {
    /// Variable name in the canonical `ZINDER_<SECTION>__<FIELD>` form.
    pub name: &'static str,
    /// The TOML config field the variable resolves to. Mirrors the inverse
    /// mapping from the env-var name back to its config struct's field path.
    pub toml_path: &'static str,
    /// Which Zinder binaries consume this variable. Used for the "Used by"
    /// column so operators can scan the table for one runtime at a time.
    pub used_by: &'static [&'static str],
    /// Whether the field is required, optional, or required-when-conditioned
    /// (string descriptors keep the column compact).
    pub requirement: Requirement,
    /// Whether the value is sensitive (redacted in `--print-config`, structured
    /// logs, and `Debug` impls).
    pub sensitive: bool,
    /// One-line operator-facing description.
    pub description: &'static str,
}

/// Whether an env var is required, optional, or conditional.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Requirement {
    /// Binary fails to start without this variable (or its config-file
    /// equivalent).
    Required,
    /// Variable is optional; a default applies when unset.
    Optional,
    /// Required only when the named precondition is met (e.g. another env
    /// var has a specific value).
    ConditionalOn(&'static str),
}

impl Requirement {
    fn label(self) -> String {
        match self {
            Self::Required => "Required".to_owned(),
            Self::Optional => "Optional".to_owned(),
            Self::ConditionalOn(condition) => format!("When `{condition}`"),
        }
    }
}

/// Operator-facing `ZINDER_*` environment variables advertised by Zinder
/// service binaries.
///
/// Mirrored into `docs/architecture/public-interfaces.md` between the
/// `<!-- env-var-table:public-interfaces:start -->` and corresponding `:end`
/// markers; the doc-mirror test in
/// `crates/zinder-runtime/tests/integration/env_var_docs.rs` fails when the
/// rendered table and the doc block diverge.
///
/// `ZINDER_TEST_*` variables are intentionally absent: they are stripped at
/// the runtime config loader so production binaries never observe them.
/// Live-test environment knobs are documented next to the affected tests in
/// `CLAUDE.md` and `docs/runbooks/testing.md`.
pub const ENVIRONMENT_VARIABLES: &[EnvVarDoc] = &[
    EnvVarDoc {
        name: "ZINDER_NETWORK__NAME",
        toml_path: "network.name",
        used_by: &[
            "zinder-ingest",
            "zinder-query",
            "zinder-compat-lightwalletd",
            "zinder-derive",
        ],
        requirement: Requirement::Required,
        sensitive: false,
        description: "Network identifier: `zcash-mainnet`, `zcash-testnet`, or `zcash-regtest`. \
                      Note: live-test gating reads the bare `ZINDER_NETWORK` env var directly \
                      and never reaches the config loader, so test runbooks still quote that form.",
    },
    EnvVarDoc {
        name: "ZINDER_NODE__JSON_RPC_ADDR",
        toml_path: "node.json_rpc_addr",
        used_by: &[
            "zinder-ingest",
            "zinder-query",
            "zinder-compat-lightwalletd",
        ],
        requirement: Requirement::Required,
        sensitive: false,
        description: "Upstream Zebra JSON-RPC URL the service connects to.",
    },
    EnvVarDoc {
        name: "ZINDER_NODE__INDEXER_GRPC_ADDR",
        toml_path: "node.indexer_grpc_addr",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Optional Zebra indexer gRPC endpoint enabling the streaming mempool source \
                      and chain-tip wakeups. Falls back to JSON-RPC polling when unset.",
    },
    EnvVarDoc {
        name: "ZINDER_NODE__AUTH__METHOD",
        toml_path: "node.auth.method",
        used_by: &[
            "zinder-ingest",
            "zinder-query",
            "zinder-compat-lightwalletd",
        ],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Upstream-node auth shape: `basic`, `cookie`, or unset for no auth.",
    },
    EnvVarDoc {
        name: "ZINDER_NODE__AUTH__USERNAME",
        toml_path: "node.auth.username",
        used_by: &[
            "zinder-ingest",
            "zinder-query",
            "zinder-compat-lightwalletd",
        ],
        requirement: Requirement::ConditionalOn("ZINDER_NODE__AUTH__METHOD=basic"),
        sensitive: false,
        description: "Basic-auth username. Paired with `ZINDER_NODE__AUTH__PASSWORD`.",
    },
    EnvVarDoc {
        name: "ZINDER_NODE__AUTH__PASSWORD",
        toml_path: "node.auth.password",
        used_by: &[
            "zinder-ingest",
            "zinder-query",
            "zinder-compat-lightwalletd",
        ],
        requirement: Requirement::ConditionalOn("ZINDER_NODE__AUTH__METHOD=basic"),
        sensitive: true,
        description: "Basic-auth password. Redacted in `--print-config` and structured logs.",
    },
    EnvVarDoc {
        name: "ZINDER_NODE__AUTH__PATH",
        toml_path: "node.auth.path",
        used_by: &[
            "zinder-ingest",
            "zinder-query",
            "zinder-compat-lightwalletd",
        ],
        requirement: Requirement::ConditionalOn("ZINDER_NODE__AUTH__METHOD=cookie"),
        sensitive: false,
        description: "Path to a cookie file. Mutually exclusive with `ZINDER_NODE__AUTH__COOKIE`.",
    },
    EnvVarDoc {
        name: "ZINDER_NODE__AUTH__COOKIE",
        toml_path: "node.auth.cookie",
        used_by: &[
            "zinder-ingest",
            "zinder-query",
            "zinder-compat-lightwalletd",
        ],
        requirement: Requirement::ConditionalOn("ZINDER_NODE__AUTH__METHOD=cookie"),
        sensitive: true,
        description: "Inline cookie credentials (`username:password`). Mutually exclusive with \
                      `ZINDER_NODE__AUTH__PATH`. Accepted for PaaS environments without \
                      persistent disks.",
    },
    EnvVarDoc {
        name: "ZINDER_NODE__REQUEST_TIMEOUT_SECS",
        toml_path: "node.request_timeout_secs",
        used_by: &[
            "zinder-ingest",
            "zinder-query",
            "zinder-compat-lightwalletd",
        ],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Upstream-node JSON-RPC request timeout in seconds. Defaults to 30.",
    },
    EnvVarDoc {
        name: "ZINDER_NODE__MAX_RESPONSE_BYTES",
        toml_path: "node.max_response_bytes",
        used_by: &[
            "zinder-ingest",
            "zinder-query",
            "zinder-compat-lightwalletd",
        ],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Maximum JSON-RPC response body size (bytes) accepted from the node.",
    },
    EnvVarDoc {
        name: "ZINDER_INGEST__CONTROL__LISTEN_ADDR",
        toml_path: "ingest.control.listen_addr",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Listen address of the private IngestControl gRPC endpoint. Localhost-only \
                      by default; cross-host deployments must add bearer-token auth per ADR-0006.",
    },
    EnvVarDoc {
        name: "ZINDER_INGEST__CONTROL__BEARER_TOKEN_PATH",
        toml_path: "ingest.control.bearer_token_path",
        used_by: &["zinder-ingest"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Path to the shared-secret bearer token the IngestControl endpoint enforces \
                      on every request (ADR-0006). File-only by policy; inline secrets are \
                      rejected at config load.",
    },
    EnvVarDoc {
        name: "ZINDER_STORAGE__INGEST_CONTROL_ADDR",
        toml_path: "storage.ingest_control_addr",
        used_by: &["zinder-query", "zinder-compat-lightwalletd"],
        requirement: Requirement::Required,
        sensitive: false,
        description: "URL of the colocated IngestControl writer (`http://host:port`). Readers \
                      use it for tip-change subscriptions, mempool reads, and writer-status \
                      lookups.",
    },
    EnvVarDoc {
        name: "ZINDER_STORAGE__INGEST_CONTROL_BEARER_TOKEN_PATH",
        toml_path: "storage.ingest_control_bearer_token_path",
        used_by: &["zinder-query", "zinder-compat-lightwalletd"],
        requirement: Requirement::ConditionalOn("ingest enforces auth"),
        sensitive: false,
        description: "Path to the bearer token file presented to the IngestControl writer when \
                      the writer enforces auth (ADR-0006).",
    },
    EnvVarDoc {
        name: "ZINDER_DERIVE__BEARER_TOKEN_PATH",
        toml_path: "derive.bearer_token_path",
        used_by: &["zinder-derive"],
        requirement: Requirement::Optional,
        sensitive: false,
        description: "Path to the shared-secret bearer token the ExplorerQuery endpoint enforces \
                      on cross-service derive-plane reads.",
    },
];

/// Renders [`ENVIRONMENT_VARIABLES`] as a GitHub-flavored Markdown table.
///
/// The rendered block is the canonical content of the
/// `env-var-table:public-interfaces` doc block; the doc-mirror integration
/// test fails when the doc and renderer diverge so adding a field to one
/// without the other is caught at build time.
#[must_use]
pub fn render_environment_variable_table() -> String {
    let mut output = String::new();
    output.push_str("| Variable | Used by | Requirement | TOML field | Description |\n");
    output.push_str("| -------- | ------- | ----------- | ---------- | ----------- |\n");
    for env_var in ENVIRONMENT_VARIABLES {
        let used_by = env_var.used_by.join(", ");
        let toml_field = format!("`{}`", env_var.toml_path);
        let sensitivity_marker = if env_var.sensitive {
            " (sensitive; redacted)"
        } else {
            ""
        };
        let _ = writeln!(
            output,
            "| `{}` | {} | {} | {} | {}{} |",
            env_var.name,
            used_by,
            env_var.requirement.label(),
            toml_field,
            env_var.description,
            sensitivity_marker,
        );
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_table_includes_required_network_row() {
        let rendered = render_environment_variable_table();
        assert!(rendered.contains("| `ZINDER_NETWORK__NAME` |"));
        assert!(rendered.contains("Required"));
        assert!(rendered.contains("zcash-mainnet"));
    }

    #[test]
    fn sensitive_variables_carry_redaction_marker_in_table() {
        let rendered = render_environment_variable_table();
        assert!(
            rendered
                .lines()
                .any(|line| line.starts_with("| `ZINDER_NODE__AUTH__PASSWORD`")
                    && line.contains("(sensitive; redacted)")),
            "sensitive rows must surface the redaction marker so operators do not \
             expect to see plaintext in --print-config:\n{rendered}",
        );
    }

    #[test]
    fn conditional_requirement_renders_when_clause() {
        let rendered = render_environment_variable_table();
        assert!(
            rendered
                .lines()
                .any(|line| line.starts_with("| `ZINDER_NODE__AUTH__USERNAME`")
                    && line.contains("When `ZINDER_NODE__AUTH__METHOD=basic`")),
            "conditional rows must render their precondition:\n{rendered}",
        );
    }

    #[test]
    fn every_variable_starts_with_zinder_prefix() {
        for env_var in ENVIRONMENT_VARIABLES {
            assert!(
                env_var.name.starts_with("ZINDER_"),
                "documented variables must follow the ZINDER_* convention; got {}",
                env_var.name,
            );
        }
    }
}
