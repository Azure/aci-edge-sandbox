# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

<#
.SYNOPSIS
Prepares the aci_edge_sandboxes_model and aci_edge_sandboxes crates for crates.io.

.DESCRIPTION
Checks the release metadata of both crates, runs one joint `cargo publish --dry-run` for
crates.io that packages and verifies them together, packages them again, requires the archives
to be the bytes that the dry run verified, and collects them into OutDir in publication order
with a SHA256SUMS file and release-manifest.json.

The dry run resolves aci_edge_sandboxes_model from a temporary local registry, because it is not
on crates.io before its first release. It is preparation evidence, not proof that crates.io
resolves the crates; see RELEASING.md.

With -ApprovedSha256Sums, it fails unless the archives have exactly the approved digests, which
binds a release to the archives that were reviewed.

.EXAMPLE
pwsh scripts/release/Invoke-CratePackage.ps1 -OutDir out/crates

.EXAMPLE
pwsh scripts/release/Invoke-CratePackage.ps1 -OutDir out/crates -ExpectedTag aci_edge_sandboxes-v0.1.0 -ApprovedSha256Sums approved/SHA256SUMS
#>
[CmdletBinding()]
param
(
    [Parameter(Mandatory)]
    [string] $OutDir,
    # The release tag, `aci_edge_sandboxes-v<version>`, which must name HEAD.
    [string] $ExpectedTag,
    # A SHA256SUMS file whose digests the archives must have.
    [string] $ApprovedSha256Sums,
    # Packages a working tree with uncommitted changes; never for a release.
    [switch] $AllowDirty,
    # Rust toolchain, such as 1.95.0; the active one when omitted.
    [string] $Toolchain
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# Publication order: the model first, because the crate depends on it.
$crates = 'aci_edge_sandboxes_model', 'aci_edge_sandboxes'
$requiredFiles = @{
    aci_edge_sandboxes_model = 'Cargo.toml', 'Cargo.toml.orig', 'Cargo.lock', '.cargo_vcs_info.json', 'LICENSE', 'src/lib.rs'
    aci_edge_sandboxes = 'Cargo.toml', 'Cargo.toml.orig', 'Cargo.lock', '.cargo_vcs_info.json', 'LICENSE', 'README.md',
        'build.rs', 'build/agent_bundle.rs', 'src/agent/bundle_contract.rs', 'agent-bundle.json', 'src/lib.rs'
}

$repository = Split-Path (Split-Path $PSScriptRoot -Parent) -Parent
$manifest = Join-Path $repository 'aci-edge-sandbox/Cargo.toml'
$toolchainArgs = @()
if ($Toolchain) { $toolchainArgs += "+$Toolchain" }

function Invoke-Native([string] $Description, [scriptblock] $Command)
{
    $output = & $Command
    if ($LASTEXITCODE -ne 0) { throw "$Description failed with exit $LASTEXITCODE" }
    $output
}

# Release metadata.
$metadata = Invoke-Native 'cargo metadata' { cargo @toolchainArgs metadata --locked --no-deps --format-version 1 --manifest-path $manifest } |
    Out-String | ConvertFrom-Json
$packages = @{}
foreach ($package in $metadata.packages) { $packages[$package.name] = $package }
if (@($metadata.packages).Count -ne $crates.Count -or ($crates | Where-Object { -not $packages.ContainsKey($_) }))
{
    throw "the workspace must hold exactly $($crates -join ' and ')"
}
$version = $packages['aci_edge_sandboxes'].version
foreach ($name in $crates)
{
    $package = $packages[$name]
    if ($package.version -ne $version) { throw "$name is at $($package.version), not $version; release both at one version" }
    if ($null -eq $package.publish -or @($package.publish) -notcontains 'crates-io') { throw "$name may not be published to crates.io" }
    foreach ($field in 'description', 'license', 'repository')
    {
        if ([string]::IsNullOrWhiteSpace($package.$field)) { throw "$name has no $field" }
    }
    $nvx = @($package.dependencies | Where-Object name -EQ 'nvx')
    if ($nvx.Count -ne 1 -or -not $nvx[0].req.StartsWith('=') -or $nvx[0].source -notmatch 'crates\.io') { throw "$name must depend on one exact nvx version from crates.io" }
    if ($nvx[0].uses_default_features) { throw "$name must depend on nvx without its default features" }
}
$modelDependencies = @($packages['aci_edge_sandboxes'].dependencies | Where-Object name -EQ 'aci_edge_sandboxes_model')
if ($modelDependencies.Count -ne 2 -or ($modelDependencies | Where-Object { $_.req -ne "=$version" }))
{
    throw "aci_edge_sandboxes must depend on aci_edge_sandboxes_model =$version as a normal and a build dependency"
}
$nvxRequirement = (@($packages['aci_edge_sandboxes'].dependencies | Where-Object name -EQ 'nvx'))[0].req
if ((@($packages['aci_edge_sandboxes_model'].dependencies | Where-Object name -EQ 'nvx'))[0].req -ne $nvxRequirement)
{
    throw 'both crates must depend on the same nvx version'
}

# Source revision.
$commit = (Invoke-Native 'git rev-parse' { git -C $repository rev-parse HEAD }).Trim()
$dirty = [bool](Invoke-Native 'git status' { git -C $repository status --porcelain })
if ($dirty -and -not $AllowDirty) { throw 'the working tree has uncommitted changes' }
if ($ExpectedTag)
{
    if ($ExpectedTag -cne "aci_edge_sandboxes-v$version") { throw "tag $ExpectedTag does not name version $version; expected aci_edge_sandboxes-v$version" }
    $tagged = (Invoke-Native "git rev-parse $ExpectedTag" { git -C $repository rev-parse --verify --quiet "refs/tags/$ExpectedTag^{commit}" }).Trim()
    if ($tagged -ne $commit) { throw "tag $ExpectedTag names $tagged, not HEAD $commit" }
}

# One dry run for both crates: Cargo packages them, verifies each extracted archive with the
# other from a temporary registry, and stops before uploading.
$publish = @('publish', '--dry-run', '--locked', '--registry', 'crates-io', '--manifest-path', $manifest)
foreach ($name in $crates) { $publish += '-p', $name }
if ($AllowDirty) { $publish += '--allow-dirty' }
Write-Host "cargo $($publish -join ' ')"
cargo @toolchainArgs @publish
if ($LASTEXITCODE -ne 0) { throw "cargo publish --dry-run failed with exit $LASTEXITCODE" }
$verifiedDirectory = Join-Path $metadata.target_directory 'package/tmp-crate'
$verified = @{}
foreach ($name in $crates)
{
    $path = Join-Path $verifiedDirectory "$name-$version.crate"
    if (-not (Test-Path -LiteralPath $path)) { throw "the dry run left no $path" }
    $verified[$name] = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash
}

# The release archives, which must be the bytes that the dry run verified.
$package = @('package', '--no-verify', '--locked', '--registry', 'crates-io', '--manifest-path', $manifest)
foreach ($name in $crates) { $package += '-p', $name }
if ($AllowDirty) { $package += '--allow-dirty' }
Write-Host "cargo $($package -join ' ')"
cargo @toolchainArgs @package
if ($LASTEXITCODE -ne 0) { throw "cargo package failed with exit $LASTEXITCODE" }

# Archives, in publication order.
if (Test-Path -LiteralPath $OutDir) { Remove-Item -LiteralPath $OutDir -Recurse -Force }
New-Item -ItemType Directory -Path $OutDir | Out-Null
$OutDir = (Resolve-Path $OutDir).Path
$sums = [System.Collections.Generic.List[string]]::new()
$archives = [System.Collections.Generic.List[object]]::new()
foreach ($name in $crates)
{
    $file = "$name-$version.crate"
    $archive = Join-Path $metadata.target_directory "package/$file"
    if (-not (Test-Path -LiteralPath $archive)) { throw "cargo did not produce $archive" }
    if ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash -ne $verified[$name]) { throw "$file differs from the archive that the dry run verified" }
    $listing = Invoke-Native "tar -tzf $file" { tar -tzf $archive } | ForEach-Object { $_ -replace "^$name-$version/", '' }
    $missing = @($requiredFiles[$name] | Where-Object { $listing -notcontains $_ })
    if ($missing) { throw "$file lacks $($missing -join ', ')" }
    $vcs = Invoke-Native "tar -xOzf $file" { tar -xOzf $archive "$name-$version/.cargo_vcs_info.json" } | Out-String | ConvertFrom-Json
    if ($vcs.git.sha1 -ne $commit) { throw "$file records commit $($vcs.git.sha1), not $commit" }
    if (-not $AllowDirty -and ($vcs.git.PSObject.Properties.Name -contains 'dirty') -and $vcs.git.dirty) { throw "$file was packaged from a dirty tree" }
    $directory = Join-Path $OutDir $name
    New-Item -ItemType Directory -Path $directory | Out-Null
    Copy-Item -LiteralPath $archive -Destination $directory
    $sha256 = (Get-FileHash -LiteralPath (Join-Path $directory $file) -Algorithm SHA256).Hash.ToLowerInvariant()
    $sums.Add("$sha256  $name/$file")
    $archives.Add([ordered]@{ order = $archives.Count + 1; crate = $name; file = "$name/$file"; sha256 = $sha256; files = @($listing | Where-Object { $_ }).Count })
}
[System.IO.File]::WriteAllText((Join-Path $OutDir 'SHA256SUMS'), (($sums -join "`n") + "`n"))
$pin = Get-Content -Raw (Join-Path $repository 'aci-edge-sandbox/agent-bundle.json') | ConvertFrom-Json
$cargoVersion = (& cargo @toolchainArgs --version).Trim()
$record = [ordered]@{
    version = $version
    commit = $commit
    tag = $ExpectedTag
    dirty = $dirty
    cargo = $cargoVersion
    nvx = $nvxRequirement
    agentBundle = $pin
    publicationOrder = $crates
    archives = $archives
    evidence = 'cargo publish --dry-run against a temporary local registry; not proof of crates.io resolution'
}
[System.IO.File]::WriteAllText((Join-Path $OutDir 'release-manifest.json'), ($record | ConvertTo-Json -Depth 5))

if ($ApprovedSha256Sums)
{
    $normalize = { param($text) @($text -split "`n" | ForEach-Object { $_.Trim().ToLowerInvariant() } | Where-Object { $_ }) -join "`n" }
    $approved = & $normalize (Get-Content -Raw -LiteralPath $ApprovedSha256Sums)
    $actual = & $normalize ($sums -join "`n")
    if ($approved -cne $actual)
    {
        throw "the archives differ from the approved digests:`napproved:`n$approved`nactual:`n$actual"
    }
    Write-Host 'The archives have the approved digests.'
}

Get-Content -Raw (Join-Path $OutDir 'SHA256SUMS') | Write-Host
Write-Host "Prepared $($crates -join ', then ') $version from $commit in $OutDir"
