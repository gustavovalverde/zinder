# ADR-0018: Environment Variable Secret Policy

## Context

Zinder accepts upstream-node credentials directly through the `ZINDER_NODE__AUTH__*` environment variable family. Modern PaaS targets (Railway, Fly.io, Render) inject secrets as environment variables, not as files on disk. Operators on those targets cannot write a cookie file before launching Zinder without a per-deployment entrypoint shim.

Secret hygiene cannot live at the env-var loader: rejecting `ZINDER_*` variables that look secret blocks the PaaS path while doing nothing about a world-readable `config.toml` carrying the same value. The right layer is the emit boundary (`--print-config`, structured logs, `Debug` impls).

The credential shape needs both forms. A `NodeAuth::Cookie` variant has to carry either a filesystem path (Zebra/zcashd's canonical cookie shape) or inline credentials (the PaaS shape) without forcing one through the other.

## Decision

**Secret hygiene lives at the emit boundary, not the loader.** Secrets pass through the env-var loader unchanged. Every observable surface (`--print-config`, structured logs, `Debug` impls) redacts them. Per-surface file-only constraints that remain load-bearing for security reasons (the ingest-control bearer token, per [ADR-0009](0009-ingest-control-transport-security.md)) stay enforced at their respective config types.

**`NodeAuth::Cookie` wraps a `CookieSource`.** The enum names the credential source:

```rust
pub enum CookieSource {
    File(PathBuf),       // node-rotated cookie file
    Inline(SecretString), // injected through configuration
}
```

`CookieSource::File` carries the canonical Zebra/zcashd shape. `CookieSource::Inline` accepts the credentials directly. Both resolve to the same `Authorization: Basic ...` header at HTTP-client construction time through `CookieSource::read_credentials()`; the inline form does not need filesystem materialization because the only consumer reads the credentials once and stores a header value.

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

**Process listings can expose secrets.** On Linux, `cat /proc/<pid>/environ` reveals every env var. Operators who consider this a threat use `CookieSource::File`. Operators on PaaS targets where the env var is the only secret-injection mechanism accept the tradeoff explicitly. The runbook documents both modes.

## Alternatives Considered

**Materialize inline cookies into a 0600 tempfile.** Rejected as YAGNI. The only consumer of the cookie is `cookie_authorization_header`, which reads the credentials once at client construction. Tempfile machinery adds a filesystem indirection no consumer needs.

**Reject env-var secrets and require file paths.** Rejected. The right shape is "secrets are handled at the emit boundary"; per-leaf opt-outs would require operators to memorize which env vars are allowed and which are not, exactly the accidental complexity the redaction-at-emit policy avoids.

**Use `secrecy::SecretString` in `NodeAuthSection`.** Considered but deferred. `config-rs` deserializes through `serde`, and `SecretString` does not implement `Deserialize`. The inline content is a plain `String` in the section and wraps into `SecretString` at `resolve_node_auth` time. The plain `String` window is bounded by the deserialization step.

## References

- [ADR-0009: IngestControl Transport Security](0009-ingest-control-transport-security.md)
- [`crates/zinder-source/src/node_auth.rs`](../../crates/zinder-source/src/node_auth.rs)
- [`crates/zinder-source/src/node_target.rs`](../../crates/zinder-source/src/node_target.rs)
- [`crates/zinder-runtime/src/config.rs`](../../crates/zinder-runtime/src/config.rs)
