# zinder-proto

`zinder-proto` is Zinder's generated gRPC protocol package. It contains the
native Zinder v1 services, the vendored lightwalletd compatibility protocol,
the vendored Zebra indexer protocol, and the native and compatibility file
descriptor sets.

Generated Rust is checked into the crate, so compiling a registry package does
not invoke `protoc`. The owned `.proto` source closure is included for
non-Rust generators and protocol review. Repository contributors regenerate
and verify these files with `scripts/regenerate-zinder-proto.sh` from the
Zinder repository root.

Wallet and application authors should normally use `zinder-client` instead.
Use this crate directly when implementing a gRPC server, a non-SDK transport
adapter, reflection, or protocol compatibility tooling.
