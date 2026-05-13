# ADR-0018: Environment Variable Secret Policy

## Context

[PRD-0002 REQ-3](../prd-0002-self-hosting-and-integration-experience.md#capability-requirements) needs Zinder to accept upstream-node credentials directly through the `ZINDER_NODE__AUTH__*` environment variable family. Modern PaaS targets (Railway, Fly.io, Render) inject secrets as environment variables, not as files on disk. Operators without a way to feed cookie content through the environment have to write a per-deployment entrypoint shim that materializes the cookie file before launching Zinder.

The pre-existing loader (`crates/zinder-runtime/src/config.rs`) rejected any `ZINDER_*` env var whose leaf segment contained `password`, `secret`, `token`, `cookie`, or `private_key`. The rejection was meant to prevent process listings (`ps`, `/proc/<pid>/environ`, debugger snapshots) from leaking secrets. In practice it blocked the legitimate PaaS path and pushed secret hygiene to the wrong layer: an operator who set `ZINDER_NODE__AUTH__PASSWORD` got a hard load failure, but the same operator could put the same secret in a world-readable `config.toml` and load fine.

`NodeAuth::Cookie` was a single-shape variant: `Cookie { path: PathBuf }`. There was no way to express "the credential lives in this env-injected secret value" without first writing it to disk.

## Decision

**Secret hygiene moves from rejection-at-load to redaction-at-emit.** Secrets pass through the env-var loader unchanged. Every observable surface (`--print-config`, structured logs, `Debug` impls) redacts them. Per-surface file-only constraints that remain load-bearing for security reasons (the ingest-control bearer token, per [ADR-0009](0009-ingest-control-transport-security.md)) stay enforced at their respective config types.

**`NodeAuth::Cookie` becomes `NodeAuth::Cookie(CookieSource)`.** The new `CookieSource` enum names the credential source:

```rust
pub enum CookieSource {
    File(PathBuf),       // node-rotated cookie file
    Inline(SecretString), // injected through configuration
}
```

`CookieSource::File` preserves the canonical Zebra/zcashd shape. `CookieSource::Inline` accepts the credentials directly. Both resolve to the same `Authorization: Basic ...` header at HTTP-client construction time through `CookieSource::read_credentials()`; the inline form does not need filesystem materialization because the only consumer reads the credentials once and stores a header value.

**Env-var schema additions:**

| Env var | Resolves to |
| ------- | ----------- |
| `ZINDER_NODE__AUTH__METHOD=cookie` | `NodeAuth::Cookie(_)` |
| `ZINDER_NODE__AUTH__PATH=/var/run/auth/.cookie` | `CookieSource::File` |
| `ZINDER_NODE__AUTH__COOKIE=<credentials>` | `CookieSource::Inline` |
| `ZINDER_NODE__AUTH__USERNAME` + `ZINDER_NODE__AUTH__PASSWORD` | `NodeAuth::Basic` |

`__PATH` and `__COOKIE` are mutually exclusive; supplying both is a configuration error.

## Consequences

**Operators get a one-step PaaS path.** Setting `ZINDER_NODE__AUTH__METHOD=cookie` and `ZINDER_NODE__AUTH__COOKIE=<cookie-content>` is sufficient. No entrypoint shim, no init container, no filesystem juggling.

**Redaction is the security boundary, not rejection.** `--print-config` continues to emit `password = "[REDACTED]"` and `path = "[REDACTED]"` regardless of how the secret was supplied. The `Debug` impls on `NodeAuth` and `CookieSource` redact at the field level so structured logs cannot accidentally include credentials. The `BearerToken` redaction (from [ADR-0009](0009-ingest-control-transport-security.md)) is unchanged.

**The ingest-control bearer token stays file-only.** [ADR-0009](0009-ingest-control-transport-security.md) gates the bearer token to `from_file` only, and the `ingest.control.token_path` config field accepts a path, not a secret. This ADR does not change that contract.

**Cookie rotation requires a restart.** Zinder reads the cookie credentials once at HTTP-client construction. When the upstream node rotates the cookie, the credentials Zinder caches go stale. Operators restart Zinder to pick up the new cookie. A reload-on-401 path is a future enhancement, not part of this ADR.

**Process listings can expose secrets.** On Linux, `cat /proc/<pid>/environ` reveals every env var. Operators who consider this a threat continue to use `CookieSource::File`. Operators on PaaS targets where the env var is the only secret-injection mechanism accept the tradeoff explicitly. The runbook documents both modes.

**Removed code:**

- `SENSITIVE_ENV_LEAF_MARKERS` constant.
- `ConfigError::SensitiveEnvironmentOverride` variant.
- `env_leaf_is_sensitive` predicate.
- Three integration tests asserting rejection (`sensitive_password_environment_override_is_rejected` and friends in `services/zinder-{ingest,query,compat-lightwalletd}/tests/integration/cli.rs`).

**New code:**

- `CookieSource` enum and `CookieSourceError` in `crates/zinder-source/src/node_auth.rs`.
- `NodeAuthSection.cookie: Option<String>` field for the inline path.
- `CookieSource::read_credentials()` resolving both branches to a single `SecretString`.
- Positive tests confirming `--print-config` redacts both basic-auth password and inline cookie content supplied through the environment.

## Alternatives Considered

**Materialize inline cookies into a 0600 tempfile.** Rejected as YAGNI. The only consumer of the cookie is `cookie_authorization_header`, which reads the credentials once at client construction. Tempfile machinery added a filesystem indirection no consumer needed.

**Keep the rejection list and add a per-leaf opt-out.** Rejected. The right shape is "secrets are handled at the emit boundary"; adding per-leaf opt-outs would require operators to memorize which env vars are allowed and which are not, which is exactly the kind of accidental complexity the redaction-at-emit policy eliminates.

**Use `secrecy::SecretString` in `NodeAuthSection`.** Considered but deferred. `config-rs` deserializes through `serde`, and `SecretString` does not implement `Deserialize`. The inline content is a plain `String` in the section and wraps into `SecretString` at `resolve_node_auth` time. The plain `String` window is bounded by the deserialization step.

## References

- [PRD-0002 §Capability Requirements REQ-3](../prd-0002-self-hosting-and-integration-experience.md#capability-requirements)
- [ADR-0009: IngestControl Transport Security](0009-ingest-control-transport-security.md)
- [`crates/zinder-source/src/node_auth.rs`](../../crates/zinder-source/src/node_auth.rs)
- [`crates/zinder-source/src/node_target.rs`](../../crates/zinder-source/src/node_target.rs)
- [`crates/zinder-runtime/src/config.rs`](../../crates/zinder-runtime/src/config.rs)
