# Releasing Zinder

Zinder releases one lockstep product version across every first-party crate and
service. A release publishes 4 multi-architecture runtime images, the native
protocol descriptor, generated OpenAPI documents, and one GitHub Release. The
optional explorer and Cipherscan services, benchmark executable, and internal
Rust crates are not release artifacts.

`deploy/release-images.json` is the authoritative published image set. The PR
smoke build and tagged release both consume the exact catalog value validated
by their admission job.

## Release identity

The root `[workspace.package]` version is the product version. Every
first-party package inherits it, and every package also inherits
`publish = false`; Zinder does not publish crates to a registry. The release
tag must equal `v` followed by the workspace version. Stable versions use
`vMAJOR.MINOR.PATCH`, while prereleases may add a SemVer suffix such as
`v0.5.0-rc.1`. Build metadata is rejected because `+` cannot be represented in
an OCI image tag without changing the version.

Every user-visible pull request records its release note as an independent
Changie fragment under `.changes/unreleased/`. Each fragment separates its
changelog category from its SemVer impact, so fixes, features, and breaking
changes can describe their behavior accurately while still driving the next
product version. Pull requests without a user-visible effect must check the
exact `No release note required` declaration in the pull request template.
Contributor instructions live in [`.changes/README.md`](../../.changes/README.md).

Every user-invokable executable supports `--version`. Runtime health responses,
the `zinder_build_info` metric, native `ServerInfo`, and lightwalletd
`LightdInfo` report the same product version and embedded Git commit. Local
builds report `unknown` when no commit was injected; release images receive the
full tag-target commit from the release workflow.

Run the local release-policy checks before opening the version change:

```console
bash scripts/test-release-tag.sh
bash scripts/test-deployment-admission.sh
scripts/test-changelog.sh
scripts/validate-changelog.sh fragments
cargo fmt --all --check
cargo check --workspace --all-targets --all-features
```

The protocol workflow runs `buf lint`, `buf generate`, and `buf build` whenever
the native schemas or Buf configuration change. A release cannot begin if API
generation fails.

## Prepare a release

After all intended changes and their fragments have merged, ask Changie for
the version implied by their SemVer impacts:

```console
next_version="$(changie next auto)"
printf '%s\n' "$next_version"
```

Set `[workspace.package].version` in `Cargo.toml` to that version without its
`v` prefix, then prepare the changelog:

```console
scripts/prepare-changelog-release.sh "${next_version#v}"
cargo check --workspace --all-targets --all-features
git diff -- Cargo.toml Cargo.lock CHANGELOG.md .changes
```

The preparation command fails unless the requested version matches both the
fragment-derived version and the version inherited by every first-party Cargo
package. It batches the pending fragments into one dated `CHANGELOG.md`
section, preserves the fragments in the version archive, and is idempotent
when rerun after a successful batch.

Merge the release-preparation change through `main`. Because this pull request
only consumes notes recorded by earlier changes, check `No release note
required` in its pull request template.

## Publish a release

After the release-preparation change merges, wait for required checks to pass,
then create an annotated tag at that commit:

```console
git switch main
git pull --ff-only
git tag -a v0.5.0 -m "v0.5.0"
git push origin v0.5.0
```

The tag must be reachable from `main`. The `release` workflow validates the tag
against Cargo metadata, requires a matching non-empty changelog section, and
rejects pending fragments before it logs in to GitHub Container Registry
(GHCR). The `release` environment then requires approval before the first
registry write.

After approval, the workflow performs these operations in order:

1. Generate and validate the OpenAPI documents and descriptor set.
2. Build the 4 runtime images natively for Linux amd64 and arm64.
3. Publish exact `vX.Y.Z` and `sha-<commit>` multi-architecture manifests.
4. Create a draft GitHub Release from the exact versioned changelog section and
   attach the API artifacts.
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
