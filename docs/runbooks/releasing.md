# Releasing Zinder

Zinder releases one lockstep product version across every first-party crate and
service. A release publishes 4 multi-architecture runtime images, the native
protocol descriptor, generated OpenAPI documents, and one GitHub Release. The
optional explorer and Cipherscan services, benchmark executable, and internal
Rust crates are not release artifacts.

## Release identity

The root `[workspace.package]` version is the product version. Every
first-party package inherits it, and every package also inherits
`publish = false`; Zinder does not publish crates to a registry. The release
tag must equal `v` followed by the workspace version. Stable versions use
`vMAJOR.MINOR.PATCH`, while prereleases may add a SemVer suffix such as
`v0.5.0-rc.1`. Build metadata is rejected because `+` cannot be represented in
an OCI image tag without changing the version.

Every user-invokable executable supports `--version`. Runtime health responses,
the `zinder_build_info` metric, native `ServerInfo`, and lightwalletd
`LightdInfo` report the same product version and embedded Git commit. Local
builds report `unknown` when no commit was injected; release images receive the
full tag-target commit from the release workflow.

Run the local release-policy checks before opening the version change:

```console
bash scripts/test-release-tag.sh
bash scripts/test-deployment-admission.sh
cargo fmt --all --check
cargo check --workspace --all-targets --all-features
```

The protocol workflow runs `buf lint`, `buf generate`, and `buf build` whenever
the native schemas or Buf configuration change. A release cannot begin if API
generation fails.

## Publish a release

Merge the version change through `main`, wait for required checks to pass, and
create an annotated tag at that commit:

```console
git switch main
git pull --ff-only
git tag -a v0.5.0 -m "v0.5.0"
git push origin v0.5.0
```

The tag must be reachable from `main`. The `release` workflow validates the tag
against Cargo metadata before it logs in to GitHub Container Registry (GHCR).
The `release` environment then requires approval before the first registry
write.

After approval, the workflow performs these operations in order:

1. Generate and validate the OpenAPI documents and descriptor set.
2. Build the 4 runtime images natively for Linux amd64 and arm64.
3. Publish exact `vX.Y.Z` and `sha-<commit>` multi-architecture manifests.
4. Create a draft GitHub Release and attach the API artifacts.
5. Publish the GitHub Release after every exact image manifest succeeds.
6. For a stable release, verify all 4 exact manifests and promote their
   `latest` tags in one final job.

Prereleases never move `latest`, and GitHub marks their Releases as
prereleases. The release workflow has no manual-dispatch path. Use the manual
`build-images` workflow for a build-only smoke test; it does not authenticate to
GHCR, publish an image, create a manifest, or create a GitHub Release.

## Verify publication

Confirm the Release and each exact image before changing a deployment:

```console
gh release view v0.5.0

for image in \
  zinder-ingest \
  zinder-projector \
  zinder-query \
  zinder-compat-lightwalletd
do
  docker buildx imagetools inspect \
    "ghcr.io/gustavovalverde/${image}:v0.5.0"
done
```

The GitHub Release must contain `zinder.v1.descriptor.bin` and the versioned
`zinder-api-docs` archive. Each image must contain amd64 and arm64 manifests,
and its OCI revision label must equal the tag-target commit. Deployments should
pin the verified image digests. The Compose file uses `:local` only for source
builds and does not treat `latest` as a deployment identity.

## Recover a failed release

The workflow creates the public GitHub Release only after all exact image tags
exist. A failure before publication may leave untagged platform digests or an
editable draft Release; rerunning the failed workflow jobs is safe because the
workflow replaces draft assets and recreates exact tags from the same validated
commit. A published Release causes the preparation job to fail closed rather
than modify public assets.

If only the final stable promotion fails, rerun the failed
`promote stable image tags` job. It verifies all 4 exact manifests before moving
any `latest` tag. Do not move a release tag or rebuild it from another commit.

Application rollback remains constrained by storage compatibility. Follow the
[VM deployment runbook](deploying-on-a-vm.md) and restore only a previously
certified binary and storage lane; do not start an older binary against a store
unless that physical layout is explicitly compatible.
