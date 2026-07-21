# Lightwalletd Compatibility Certification

Zinder can replace the server role of lightwalletd only for clients that speak
the vendored `CompactTxStreamer` protocol. It remains an adapter over
`WalletQueryApi`; it must not introduce a second chain-data model, wallet
state, or key-management surface.

## Claim boundary

The replacement claim applies to the network-facing server only. It excludes
wallet-local scanning, key custody, transaction construction, and user-facing
wallet UX.

Use a named level in release notes and operator docs:

| Level | Evidence |
| --- | --- |
| `protocol-compatible` | Pinned vendored proto, drift check, and generated-client integration coverage. |
| `reference-parity-compatible` | Live comparison against the pinned reference lightwalletd for the RPCs claimed. |
| `client-compatible` | A separately versioned lightwalletd client completes bootstrap, restore, transparent discovery, shielded-note discovery, send, and pending-transaction observation. |
| `public-operator-compatible` | The deployed endpoint has the required TLS/proxy, access, readiness, rate-limit, and redaction controls. |

Do not call the adapter a full or drop-in replacement without the protocol pin,
the claimed RPC set, and the highest evidence-backed level.

## Architecture

- `zinder-proto` owns the vendored protocol and its provenance.
- `zinder-query` owns the native `WalletQueryApi`, `WalletServingQuery`, and the
  shared exact-fence serving-pair publisher.
- `zinder-compat-lightwalletd` translates protocol messages, query values, and
  errors over an admitted exact-fence canonical/wallet-projection reader pair.
- `zinder-ingest` and `zinder-store` own compact blocks, tree states, subtree
  roots, and transaction bytes; `zinder-projector` owns the wallet projection
  that provides transparent UTXOs and transaction history.

In particular, `GetLightdInfo.taddrSupport` is an explicit serving-process
claim. Enable it only after the wallet projection has admitted both
transparent-output and transparent-history reads at the serving fence.

## Certification work

1. Keep the protocol pin and generated-client coverage green. Updating the
   vendored proto is a protocol change, not an adapter-only change.
2. Run the live parity suite against pinned Zebra and reference-lightwalletd
   images. It compares observable values and status codes; intentional
   operator metadata differences remain allow-listed.
3. Keep the wallet-serving live tests focused on retained compact blocks, tree
   states, subtree roots, transaction bytes, transparent history and UTXOs, and
   strict below-floor failures.
4. Run the ignored Zingolib live test when its known binary is available. It
   is the independent-client proof, not a substitute for protocol parity.
5. Before a public claim, validate the actual TLS/proxy deployment and record
   the exact commands, image digests, client version, network, and store floor
   with the release. No repository-side manifest schema is required.

## Reference inputs

The vendored protocol pin and parity workflow image digests are:

- `lightwallet-protocol` `v0.5.0`
  (`ac7cee052a1bf5d430985a478d39e8b513fc4bd4`);
- `lightwalletd` `v0.4.19`
  (`028401c4c4a7c8c386c81212324cc8083eed7510`) as the source reference;
- `electriccoinco/lightwalletd:v0.4.19@sha256:a3dfb04b4054b78ae3107dcc804c3a15a6e38d1f0dfcadeac48da482dd1d3448`;
- `zfnd/zebra:6.0.0-rc.0@sha256:998178a61a67b4776ea7104d05c481d86f069a688595e99fcff7f090ae4b7e2b`.

The pin is reproducibility, not evidence by itself. It makes a failing parity
run explainable and a passing one repeatable.
