# Zinder Cipherscan adapter

`zinder-compat-cipherscan` translates Cipherscan REST and WebSocket contracts
onto Zinder `ExplorerQuery` and `WalletQuery` endpoints. It is a stateless
protocol-edge service. It does not open Zinder storage, index blocks, or own
Cipherscan product data.

```text
Cipherscan UI
    -> zinder-compat-cipherscan
        -> ExplorerQuery
        -> WalletQuery
        -> external market-price providers
```

The [adapter architecture](../../docs/architecture/cipherscan-adapter.md)
records ownership and route coverage. The
[verification runbook](../../docs/runbooks/cipherscan-adapter-verification.md)
defines the acceptance probes.

## Deployment status

This service compiles in the workspace but is not a release image. The checked
single-host composition publishes `zinder-ingest`, `zinder-projector`, and
`zinder-compat-lightwalletd`; it does not publish standalone `WalletQuery` or
`ExplorerQuery` endpoints. Running the Cipherscan adapter therefore requires a
custom composition that supplies both native gRPC dependencies on the same
network. Do not treat the checked wallet-serving Compose file as an end-to-end
Cipherscan deployment.

## Configuration

Configuration precedence is defaults, TOML, `ZINDER_*` environment variables,
then CLI overrides. Use `--print-config` to inspect the resolved, redacted
configuration.

```toml
[network]
name = "zcash-testnet"

[security]
allow_public_bind = false

[ops]
listen_addr = "127.0.0.1:9108"

[cipherscan]
listen_addr = "127.0.0.1:9070"
explorer_query_endpoint = "http://127.0.0.1:9068"
wallet_query_endpoint = "http://127.0.0.1:9102"
current_price_endpoint = "https://api.coingecko.com/api/v3/simple/price?ids=zcash&vs_currencies=usd&include_24hr_change=true"
historical_price_endpoint_template = "https://api.coingecko.com/api/v3/coins/zcash/history?localization=false&date={date}"
# bearer_token_path = "/run/secrets/zinder-reader-token"
```

Start the adapter only after both native gRPC endpoints are healthy:

```bash
cargo run -p zinder-compat-cipherscan -- \
  --config /etc/zinder/cipherscan.toml
```

The adapter refuses public listener binds unless
`security.allow_public_bind = true`. Put public traffic behind an
operator-controlled TLS proxy with authentication, rate limits, and quotas.
Use a private network and the optional shared bearer token for both Zinder gRPC
dependencies.

## Readiness

`/healthz` proves the process is alive. `/readyz` proves startup dependencies
were connected. Neither proves that every materialized view covers an
arbitrary requested range. Deployment probes must include at least one
data-bearing route and must preserve explicit unavailable or degraded results.

```bash
curl --fail http://127.0.0.1:9108/healthz
curl --fail http://127.0.0.1:9108/readyz
curl --fail http://127.0.0.1:9070/api/info
curl --fail 'http://127.0.0.1:9070/api/mining/pool-distribution?period=7d'
```

The adapter keeps only bounded in-memory caches and can restart without a
backfill. Missing labels, market data, graph analytics, identity data, or other
Cipherscan-owned enrichments remain sidecar responsibilities. The adapter must
not replace missing facts with zeroes, empty successful results, or invented
finality.
