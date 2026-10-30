# Releasing the crates

This repository publishes two crates to [crates.io](https://crates.io), together and at the same
version:

1. `aci_edge_sandboxes_model`, the data model;
2. `aci_edge_sandboxes`, the lifecycle client, which depends on exactly the same version of the
   model.

Both depend on one exact version of [`nvx`](https://crates.io/crates/nvx). The runtime bundle
(the agent library, OpenVMM, and the guest) is not part of either crate: the `agent-bundled`
feature downloads the release asset that
[`aci-edge-sandbox/agent-bundle.json`](aci-edge-sandbox/agent-bundle.json) pins and refuses it
unless its SHA-256 matches.

A published version can never be replaced, only yanked. Every check below runs before the first
upload.

## Release gates

Publish only when every gate holds:

| Gate | Evidence |
| --- | --- |
| The pinned bundle is the approved bundle for this release | `agent-bundle.json` names it by release tag, asset, and SHA-256 |
| The pinned bundle speaks this crate's sandbox ABI | `pwsh scripts/test-agent-bundling.ps1 -Pinned` passes |
| The bundle works with this client | `pwsh scripts/test-agent-guest.ps1` passes for the pinned asset on a Windows x64 host with the Windows Hypervisor Platform: the lifecycle example and every ignored `tests/agent_guest.rs` test |
| The `nvx` version that the manifests pin is on crates.io | `cargo publish --dry-run` resolves it |
| The source builds and its tests pass | The CI workflow passes on the release commit |
| The archives are the reviewed ones | The release run's `SHA256SUMS` equals the approved digests |
| Publication is authorized | An explicit approval of the version, commit, and digests |

## Prepare

Tag the release commit `aci_edge_sandboxes-v<version>`; the version is the one in both
`Cargo.toml` files. Then, from a clean checkout of the tag, with network access to crates.io:

```powershell
pwsh scripts/release/Invoke-CratePackage.ps1 -OutDir out/crates -ExpectedTag aci_edge_sandboxes-v<version>
```

The script checks the release metadata and exact version pins, runs one
`cargo publish --dry-run --registry crates-io` for both crates, packages them again, requires the
same bytes, and writes `out/crates/SHA256SUMS` and `out/crates/release-manifest.json`. The dry run
resolves the unpublished model from a temporary local registry: it shows that the archives build,
not that crates.io resolves them.

## Validate before publication

```powershell
pwsh scripts/test-agent-bundling.ps1             # bundle staging with synthetic bundles
pwsh scripts/test-agent-bundling.ps1 -Pinned     # the pinned bundle, from the checkout and an export
pwsh scripts/release/Test-CrateConsumer.ps1 -ArchiveDir out/crates   # an SDK-like consumer of the archives
```

`Test-CrateConsumer.ps1` builds a consumer outside the repository that depends on
`aci_edge_sandboxes` optionally, on x86_64 Windows, with `agent-bundled`. It checks that the crate
is not built without the consumer's feature, that the consumer's build script receives
`DEP_ACI_EDGE_SANDBOXES_AGENT_ARTIFACTS_DIR`, and that the application finds its bundle after
both are copied elsewhere. With `-ArchiveDir` it is local staging evidence only.

Functional acceptance, on a Windows x64 host with the Windows Hypervisor Platform enabled. A
matching sandbox ABI shows that the bundle and the crate speak the same interface, not that they
work together, so every bundle that `agent-bundle.json` pins passes these checks first:

```powershell
pwsh scripts/test-agent-guest.ps1 -BundleZip <asset> -BundleZipSha256 <sha256>
```

It runs the `agent_lifecycle` example, which pulls an image through the bundle's `direct-images`
and provisions, starts, runs a command in, stops, and deprovisions a sandbox, then every ignored
test of `tests/agent_guest.rs` against the bundle's library, OpenVMM, kernel, and initramfs. The
[Edge functional acceptance](.github/workflows/edge-functional.yml) workflow runs the same script
on a self-hosted WHP runner when dispatched. Keep its `summary.json` and logs with the release's
compatibility record.

## Publish

Publish `aci_edge_sandboxes_model` first, wait until the crates.io index lists it, then publish
`aci_edge_sandboxes`. Upload only the approved archives from `Prepare`, never a fresh build.

The owners of both crates decide the route. Each route uploads the same approved archives.

### First release by an owner

crates.io creates a crate when its first version is published, so the first upload of each crate
uses an owner's API token:

1. The initial owner signs in to crates.io with GitHub, verifies an email address, and creates a
   token scoped to `publish-new` and `publish-update` for `aci_edge_sandboxes*`, with a short
   expiry.
2. On a clean checkout of the release tag, run
   `cargo publish --locked --registry crates-io --package <crate>` for each crate in order.
   Packaging is reproducible (`Prepare` packaged twice and compared), and the uploaded archive
   must have the approved checksum: compare the `cksum` from
   `https://crates.io/api/v1/crates/<crate>/<version>` with `SHA256SUMS`.
3. The owner revokes the token and adds the long-term owners (`cargo owner --add`), such as a
   Microsoft GitHub team.

### Later releases: trusted publishing

For later versions, crates.io trusted publishing lets a GitHub Actions job publish without a
stored token. The job requests a short-lived token with `rust-lang/crates-io-auth-action`. NVX
publishes `nvx` this way (`.github/workflows/ci.yml`, jobs `crate-package` and `crate-release`).

Setup, done by owners and repository admins:

1. On crates.io, add a trusted publisher to each crate: repository `Azure/aci-edge-sandbox`, the
   release workflow file, and the environment `crates-io`.
2. In this repository, create the `crates-io` environment, restricted to release tags and
   requiring a reviewer.
3. Add the release workflow:
   - a package job with read-only permissions runs `Invoke-CratePackage.ps1 -ExpectedTag` and
     `-ApprovedSha256Sums`;
   - a publish job in the `crates-io` environment with `id-token: write` uploads those archives
     in the order above.

### Alternative: ESRP Release

[`.azure-pipelines/release-crates.yml`](.azure-pipelines/release-crates.yml) is a manual
Azure Pipelines route through ESRP Release, as used for crates owned by `microsoft-oss-releases`.
It needs an ESRP onboarding and service connection for crates.io, which this repository does not
have yet. With `dryRun` (the default), it only prepares and publishes the archives as a pipeline
artifact. To publish, queue it on the release tag with `dryRun` cleared and `approvedModelSha256`
and `approvedCrateSha256` set to the approved digests from `SHA256SUMS`. It then:

1. fails unless the run's source is the tag `aci_edge_sandboxes-v<version>`, the tag names the
   commit being built, and the manifests have that version;
2. fails unless the archives have exactly the approved digests;
3. submits `aci_edge_sandboxes_model` through ESRP Release, and waits until crates.io's index lists
   it;
4. submits `aci_edge_sandboxes` the same way.

Before submitting a crate, it asks crates.io whether that version exists: a version with the
approved checksum is skipped, and one with another checksum fails the run.

## Compatibility record

The crate checks a bundle's sandbox ABI and digests, not that its components work together. Each
release's notes therefore record the combination that passed functional acceptance
(`scripts/test-agent-guest.ps1`): the crate version and its `nvx` version; the bundle's release
tag, asset, and SHA-256; the producer revisions that its `bundle-manifest.json` names (the
Compute-ACI-Agent commit of the agent library and edge guest, and the NVX and OpenVMM revisions of
the kernel and `openvmm.exe`); and the host, image, and results of the run. A new bundle or `nvx`
version needs functional acceptance again before `agent-bundle.json` or the manifests pin it.

## Recovering from a partial release

If the model is published and the crate is not, do not yank the model. Fix the cause outside the
crates (credentials, approvals, the index), then publish the crate from the same approved
archives. The ESRP pipeline, queued again on the same tag with the same digests, skips the
published model. If the crate itself must change, release both crates at the next patch
version; yank a version only when it is harmful to use.

## Verify after publication

From any machine, with no `[patch]`, path, or Git source, and a new `CARGO_HOME`:

```powershell
pwsh scripts/release/Test-CrateConsumer.ps1 -Version <version>
```

Its lock file must name crates.io as the source of `aci_edge_sandboxes`,
`aci_edge_sandboxes_model`, and `nvx`. On a Windows x64 host with the Windows Hypervisor
Platform, add `-Open` to load the agent library through the relocated bundle, and run the
`agent_lifecycle` example from the published crate.
