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
| The bundle runs a sandbox | The lifecycle smoke below passes on a Windows x64 host with the Windows Hypervisor Platform |
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

Lifecycle smoke, on a Windows x64 host with the Windows Hypervisor Platform enabled:

```powershell
cd aci-edge-sandbox
cargo run --release --locked --features agent-bundled --example agent_lifecycle
```

It must print `READY` and the guest's `/etc/os-release`, then stop and deprovision the sandbox.

## Publish

[`.azure-pipelines/release-crates.yml`](.azure-pipelines/release-crates.yml) is a manual pipeline.
With `dryRun` (the default), it only prepares and publishes the archives as a pipeline artifact.
To publish, queue it on the release tag with `dryRun` cleared and `approvedModelSha256` and
`approvedCrateSha256` set to the approved digests from `SHA256SUMS`. It then:

1. fails unless the run's source is the tag `aci_edge_sandboxes-v<version>`, the tag names the
   commit being built, and the manifests have that version;
2. fails unless the archives have exactly the approved digests;
3. submits `aci_edge_sandboxes_model` through ESRP Release, and waits until crates.io's index lists
   it;
4. submits `aci_edge_sandboxes` the same way.

Before submitting a crate, it asks crates.io whether that version exists: a version with the
approved checksum is skipped, and one with another checksum fails the run.

## Recovering from a partial release

If the model is published and the crate is not, do not yank the model. Fix the cause outside the
crates (credentials, approvals, the index), and queue the pipeline again on the same tag with the
same approved digests: it skips the published model and submits the crate. If the crate itself
must change, release both crates at the next patch version; yank a version only when it is
harmful to use.

## Verify after publication

From any machine, with no `[patch]`, path, or Git source, and a new `CARGO_HOME`:

```powershell
pwsh scripts/release/Test-CrateConsumer.ps1 -Version <version>
```

Its lock file must name crates.io as the source of `aci_edge_sandboxes`,
`aci_edge_sandboxes_model`, and `nvx`. On a Windows x64 host with the Windows Hypervisor
Platform, add `-Open` to load the agent library through the relocated bundle, and run the
lifecycle smoke from the published crate.
