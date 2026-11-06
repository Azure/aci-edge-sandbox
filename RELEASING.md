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

From a clean checkout of the release commit, with network access to crates.io, prepare both
archives. The version must be the same in both `Cargo.toml` files. Keep output outside the checkout
so that generated files do not dirty it:

```powershell
pwsh scripts/release/Invoke-CratePackage.ps1 -OutDir ..\release-approval -Toolchain <toolchain>
```

The script checks the release metadata and exact version pins, runs one
`cargo publish --dry-run --registry crates-io` for both crates, packages them again, requires the
same bytes, and writes `SHA256SUMS` and `release-manifest.json` in the output directory. Review and
approve both files independently of the publishing run. If the release tag already exists, add
`-ExpectedTag aci_edge_sandboxes-v<version>` to require that it names HEAD. The dry run
resolves the unpublished model from a temporary local registry: it shows that the archives build,
not that crates.io resolves them.

## Validate before publication

```powershell
pwsh scripts/test-agent-bundling.ps1             # bundle staging with synthetic bundles
pwsh scripts/test-agent-bundling.ps1 -Pinned     # the pinned bundle, from the checkout and an export
pwsh scripts/release/Test-CrateConsumer.ps1 -ArchiveDir ..\release-approval   # an SDK-like consumer of the archives
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
on a self-hosted WHP runner when dispatched, and is reusable by other workflows.

[CI](.github/workflows/ci.yml) calls this workflow on pushes to `main` and manual runs, using the
bundle pinned in `agent-bundle.json`. Functional acceptance must pass before the package job can
run and upload the crate archives; a failed, cancelled, or skipped acceptance check blocks
packaging on those runs. Pull requests skip the WHP job and retain their existing CI checks and
packaging. An available self-hosted runner with all of the workflow's labels is required.

Push and pull-request CI runs only validate and package; they never publish crates. Manual CI
defaults to `dryRun: true`, which also only validates and packages. Use a successful main push
or manual CI run on the exact release commit as the pre-publication functional evidence,
alongside the other release gates. Keep its `summary.json` and logs with the release's
compatibility record.

## Publish

Publish `aci_edge_sandboxes_model` first, wait until the crates.io index lists it, then publish
`aci_edge_sandboxes`. What is uploaded must have the approved digests from `Prepare`; each route
checks this before uploading.

The owners of both crates decide the route.

### CI publication

Manually dispatch [CI](.github/workflows/ci.yml) on the approved release commit and uncheck
`dryRun` to request actual publication. The publish job runs only after the source checks,
functional acceptance and packaging all succeed. It calls
[`Invoke-CratePublish.ps1`](scripts/release/Invoke-CratePublish.ps1) without `-DryRun`, using the
current run's `crates` artifact as its approval input. Package preparation still performs its
dry run; the checkbox controls whether the separate publish job runs.

Before enabling publication, configure the repository:

1. Create the `crates-io` GitHub environment with required reviewers and deployment restrictions
   for `main` and approved release tags. The release commit must be reachable from `origin/main`.
2. Store a short-lived crates.io token in that environment's `CARGO_REGISTRY_TOKEN` secret. Its
   owner must be allowed to publish both crates; scope it to `publish-update`, and `publish-new`
   if this is their first publication. Cargo reads the token from the publishing step's
   environment; the workflow does not run login or write a Cargo credential file.
3. Ensure repository rules allow the publishing job's `GITHUB_TOKEN` to create a missing
   `aci_edge_sandboxes-v<version>` tag. Only the publish job receives `contents: write`; existing
   tags are never moved.

Before approving the `crates-io` deployment, independently review the `crates` artifact's
`release-manifest.json` and `SHA256SUMS`, and the current run's source and functional results.
Approval covers that exact version, commit, bundle pin and package digests. CI passes the manifest
and `SHA256SUMS` directly to the publisher, with `-ExpectedCommit` bound to the workflow commit.
The script centrally validates the manifest's clean-source status, exact Cargo version, full
bundle pin, publication order and archive digests, then runs the source, registry and tag gates.
Missing credentials or any failed gate stops publication.

Publishing jobs are serialized. The script publishes the model first, waits for its approved
checksum in crates.io's index, then publishes the client. A partial failure is not rolled back;
rerunning the same approved release skips only versions whose checksums already match. Revoke
the short-lived token after the release and replace the environment secret before another release.

After a successful publish job, CI automatically runs
[`Test-PublishedRelease.ps1`](scripts/release/Test-PublishedRelease.ps1) on a self-hosted Windows
x64 WHP runner. It takes the version and `nvx` requirement from the approved crate artifact and
the expected backend-library digest from the successful pre-publication acceptance summary.
The workflow requires both artifacts to name the same commit and bundle ZIP hash.

This final job validates the published crates.io packages and their GitHub bundle download, then
runs the published lifecycle example for cold startup and snapshot capture/clone startup. It uses
a fresh work directory and requires the runner's temporary directory to be on NTFS, not a ReFS
Dev Drive. No publishing credentials are supplied to this read-only job. It uploads logs and
`summary.json` as `crate-post-publish-validation`, including available logs after a failure.

Failed, cancelled or skipped publication skips this final job. A failed post-publication check
fails CI but cannot undo versions already published. Pushes, pull requests and manual dry runs
do not perform post-publication validation because their publish job is skipped.

### Manual publication by an owner

[`scripts/release/Invoke-CratePublish.ps1`](scripts/release/Invoke-CratePublish.ps1) is the
reusable version of the standalone script used for the manual publication. CI packages the
crates and invokes the same script for explicitly requested, approved publication. The invocation
below is the operator-run alternative to CI.

Prerequisites: PowerShell 7.3 or later, Git, Rust/Cargo with the exact toolchain used for
approval already installed, the platform's Rust linker/build tools, `tar`, network access to
GitHub and crates.io, and all release gates above. Use a clean checkout of the approved full
40-character commit, reachable from `origin/main`, with `origin` set to the canonical
`Azure/aci-edge-sandbox` HTTPS or SSH URL. A release must have a new version; never reuse an
already published version for different source or package bytes.

Use the reviewed `release-manifest.json` and `SHA256SUMS`, not newly generated values that bypass
approval. The manifest binds the version, commit, Cargo version, exact `nvx` requirement and
bundle pin; the sums file supplies both full package hashes in model-first order. The tag is
`aci_edge_sandboxes-v<version>`; `-Tag` may be supplied explicitly but must match that convention.
Both local and remote tags, if present, must already name the approved commit.

The publisher accepts two invocation styles: the existing individual parameters, or
`-ApprovedManifest` with `-ApprovedSha256Sums`. They cannot be mixed. Manifest mode reads the
version, commit, exact `nvx` requirement and bundle pin from the approved file and verifies its
recorded Cargo version and archive digests. An optional `-ExpectedCommit` additionally binds the
manifest to a caller-supplied commit; otherwise the approved commit must still match checkout HEAD.
`-Tag` remains optional and must follow the same release-tag convention.

Run the new tooling from its own checkout when publishing an older source checkout that does not
contain it. `-Repository` selects that clean source checkout; it defaults to the tooling checkout.
Every `-WorkDir` must be a **new directory outside the release checkout**. Previous runs and
approved files are never overwritten.

First run the dry run; no authentication is required. Replace the paths and toolchain below with
the approved inputs. The toolchain must match the `cargo` field in the reviewed manifest:

```powershell
$approvalDir = Resolve-Path ..\release-approval
$publish = @{
    ApprovedManifest = (Join-Path $approvalDir 'release-manifest.json')
    ApprovedSha256Sums = (Join-Path $approvalDir 'SHA256SUMS')
    Toolchain = '<approved-toolchain>'
    Repository = (Resolve-Path ..\release-source).Path
}
.\scripts\release\Invoke-CratePublish.ps1 @publish -WorkDir ..\release-dry-run -DryRun
```

Existing callers can continue supplying the individual approval values instead:

```powershell
$approval = Get-Content -Raw (Join-Path $approvalDir 'release-manifest.json') | ConvertFrom-Json
$publish = @{
    Version = $approval.version
    Commit = $approval.commit
    Tag = "aci_edge_sandboxes-v$($approval.version)"
    Toolchain = '<approved-toolchain>'
    ApprovedSha256Sums = (Join-Path $approvalDir 'SHA256SUMS')
    ExpectedNvxVersion = $approval.nvx.TrimStart('=')
    ExpectedBundleSha256 = $approval.agentBundle.sha256
    Repository = (Resolve-Path ..\release-source).Path
}
.\scripts\release\Invoke-CratePublish.ps1 @publish -WorkDir ..\release-dry-run -DryRun
```

The dry run verifies source, pins, local/remote tag targets and registry checksums, and reruns the
full packaging/digest gate even if either version is already published. It fetches `origin/main`
and writes only local working artifacts; it creates no tags, pushes nothing, and publishes nothing.

For actual publication, a crate owner must sign in to crates.io, verify their email address,
and create a short-lived token scoped to `publish-update` for the two crates (also `publish-new`
for a first publication). Authenticate **interactively outside the script**:

```powershell
cargo "+$($publish.Toolchain)" login --registry crates-io
# Paste the token only at Cargo's prompt, never into a command argument or this script.
.\scripts\release\Invoke-CratePublish.ps1 @publish -WorkDir ..\release-publish
```

Only the command without `-DryRun` can create/push the release tag or upload crates. After the
full approval gate passes, it creates and pushes a missing tag without force, publishes the
model first, waits for its approved checksum in the registry index, and then publishes the client.
`cargo publish` repackages rather than uploading a prepared archive, so each standalone package
is hash-checked again immediately before publication with the same toolchain and clean source.
Every failed command, wrong pin, tag conflict, yanked version or checksum mismatch stops the run.
Existing published versions are skipped only when their checksums match.

After publication, or if you abandon an attempt, **revoke the token on crates.io** and remove the
locally stored credential:

```powershell
cargo "+$($publish.Toolchain)" logout --registry crates-io
```

The script does not read credential files, print tokens or run login/logout. For a first
publication, the owner can subsequently add the long-term owners with `cargo owner --add`.

The offline control-flow checks substitute git, cargo and registry responses; they cannot
publish crates or push tags:

```powershell
pwsh -NoProfile -File scripts/release/Test-CratePublish.ps1
```

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
   - a publish job in the `crates-io` environment with `id-token: write` runs `cargo publish`
     with the package job's toolchain, in the order above, only if the package job passed.

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
archives. Rerun the manual script with the same approved inputs and a new `-WorkDir`; it verifies
the existing tag and skips the published model only after checking its checksum. The ESRP
pipeline also supports resumption on the same tag and digests. If the crate itself must change,
release both crates at the next patch version; yank a version only when it is harmful to use.

## Verify after publication

CI runs the full published-release check automatically after successful publication. To run the
same check manually on a Windows x64 WHP host, with its state directory on NTFS:

```powershell
pwsh scripts/release/Test-PublishedRelease.ps1 -Version <version> -NvxVersion <nvx-version> -ExpectedLibrarySha256 <approved-backend-library-sha256>
```

The backend-library digest must come from the approved bundle or its successful functional
acceptance summary; it is not the bundle ZIP digest.

For releases whose lifecycle example includes startup metrics, `lifecycle.log` and
`snapshots.log` also contain a `startup-metrics:` JSON record with the measured `client.start()`
duration in milliseconds and the actual boot mode. Compare the cold and clone records to
inspect restore latency. Snapshot preparation is outside this timing, and a single pair of
measurements does not establish a performance guarantee. These records are already included in
the uploaded logs; no CI input or extra artifact is required. Older published examples retain
their existing output and checks.

For a lighter consumer/package check without booting sandbox VMs:

On a Windows x64 machine (the only target with a published bundle), with no `[patch]`, path, or
Git source, and a new `CARGO_HOME`:

```powershell
pwsh scripts/release/Test-CrateConsumer.ps1 -Version <version>
```

Its lock file must name crates.io as the source of `aci_edge_sandboxes`,
`aci_edge_sandboxes_model`, and `nvx`. On a Windows x64 host with the Windows Hypervisor
Platform, add `-Open` to load the agent library through the relocated bundle, and run the
`agent_lifecycle` example from the published crate.
