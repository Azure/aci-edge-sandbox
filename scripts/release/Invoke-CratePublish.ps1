# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

#Requires -Version 7.3
<#
.SYNOPSIS
Publishes the approved model, then client crate, to crates.io.

.DESCRIPTION
Authenticate outside this script through cargo login or CARGO_REGISTRY_TOKEN.
No credentials are read or printed here.
Requires a clean release checkout on origin/main and a new work directory outside that checkout.
Reuses Invoke-CratePackage.ps1 and Get-CratesIoVersion.ps1 for the approval and registry gates.
Existing local and remote tags must name Commit; they are never moved. Already published
versions are skipped only when their checksums match. With -DryRun, all preparation checks run
but no tag is created or pushed and neither crate is published.

Supply either the individual approval values or ApprovedManifest with ApprovedSha256Sums.
Manifest mode also validates the recorded Cargo version, clean-source status, full bundle pin
and archive digests. ExpectedCommit can bind that manifest to a caller's workflow commit.

See RELEASING.md for the approval inputs and invocation examples.
#>
[CmdletBinding(DefaultParameterSetName = 'Explicit')]
param
(
    [Parameter(Mandatory, ParameterSetName = 'Explicit')]
    [ValidatePattern('^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$')]
    [string] $Version,
    [Parameter(Mandatory, ParameterSetName = 'Explicit')]
    [ValidatePattern('^[0-9a-f]{40}$')]
    [string] $Commit,
    [string] $Tag,
    [Parameter(Mandatory, ParameterSetName = 'Manifest')]
    [ValidateNotNullOrEmpty()]
    [string] $ApprovedManifest,
    [Parameter(ParameterSetName = 'Manifest')]
    [ValidatePattern('^[0-9a-f]{40}$')]
    [string] $ExpectedCommit,
    [Parameter(Mandatory)]
    [ValidatePattern('^\d+\.\d+\.\d+$')]
    [string] $Toolchain,
    [Parameter(Mandatory)]
    [string] $ApprovedSha256Sums,
    [Parameter(Mandatory, ParameterSetName = 'Explicit')]
    [ValidateNotNullOrEmpty()]
    [string] $ExpectedNvxVersion,
    [Parameter(Mandatory, ParameterSetName = 'Explicit')]
    [ValidatePattern('^[0-9a-fA-F]{64}$')]
    [string] $ExpectedBundleSha256,
    [string] $Repository = (Split-Path (Split-Path $PSScriptRoot -Parent) -Parent),
    [Parameter(Mandatory)]
    [string] $WorkDir,
    [switch] $DryRun
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true
Set-StrictMode -Version Latest

function Invoke-Native([string] $Description, [scriptblock] $Command)
{
    $output = & $Command
    if ($LASTEXITCODE -ne 0) { throw "$Description failed with exit $LASTEXITCODE" }
    $output
}

$crates = 'aci_edge_sandboxes_model', 'aci_edge_sandboxes'
$approval = $null
if ($PSCmdlet.ParameterSetName -eq 'Manifest')
{
    $ApprovedManifest = (Resolve-Path -LiteralPath $ApprovedManifest).Path
    $approval = Get-Content -Raw -LiteralPath $ApprovedManifest | ConvertFrom-Json -AsHashtable -NoEnumerate
    if ($approval -isnot [System.Collections.IDictionary]) { throw 'ApprovedManifest must contain a JSON object' }
    foreach ($field in 'version', 'commit', 'cargo', 'nvx')
    {
        if (-not $approval.Contains($field) -or $approval[$field] -isnot [string] -or
            [string]::IsNullOrWhiteSpace($approval[$field]))
        {
            throw "ApprovedManifest must contain a nonempty string '$field'"
        }
    }
    if (-not $approval.Contains('dirty') -or $approval.dirty -isnot [bool] -or $approval.dirty)
    {
        throw 'ApprovedManifest must record a clean checkout with dirty: false'
    }
    if ($approval.nvx -cnotmatch '^=[^=\s]+$') { throw 'ApprovedManifest must contain one exact nvx requirement' }
    if (-not $approval.Contains('agentBundle') -or $approval.agentBundle -isnot [System.Collections.IDictionary])
    {
        throw 'ApprovedManifest must contain an agentBundle object'
    }
    foreach ($field in 'release', 'asset', 'sha256')
    {
        if (-not $approval.agentBundle.Contains($field) -or $approval.agentBundle[$field] -isnot [string] -or
            [string]::IsNullOrWhiteSpace($approval.agentBundle[$field]))
        {
            throw "ApprovedManifest must contain a nonempty agentBundle.$field"
        }
    }
    if (-not $approval.Contains('publicationOrder') -or $approval.publicationOrder -isnot [array] -or
        $approval.publicationOrder.Count -ne $crates.Count -or
        ($approval.publicationOrder -join ',') -cne ($crates -join ','))
    {
        throw 'ApprovedManifest must list both crates in publication order'
    }
    if (-not $approval.Contains('archives') -or $approval.archives -isnot [array] -or
        $approval.archives.Count -ne $crates.Count)
    {
        throw 'ApprovedManifest must contain exactly two archives'
    }
    $Version = $approval.version
    $Commit = $approval.commit
    $ExpectedNvxVersion = $approval.nvx.Substring(1)
    $ExpectedBundleSha256 = $approval.agentBundle.sha256
    if ($ExpectedCommit -and $Commit -cne $ExpectedCommit)
    {
        throw "ApprovedManifest commit $Commit does not match expected commit $ExpectedCommit"
    }
}
if (-not $PSBoundParameters.ContainsKey('Tag')) { $Tag = "aci_edge_sandboxes-v$Version" }
if ($null -ne $approval -and $approval.Contains('tag') -and $null -ne $approval.tag)
{
    if ($approval.tag -isnot [string] -or ($approval.tag -and $approval.tag -cne $Tag))
    {
        throw 'ApprovedManifest tag does not match the release tag'
    }
}
$Repository = (Resolve-Path -LiteralPath $Repository).Path
$WorkDir = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($WorkDir)
$ApprovedSha256Sums = (Resolve-Path -LiteralPath $ApprovedSha256Sums).Path
if ($Tag -cne "aci_edge_sandboxes-v$Version") { throw "tag must be aci_edge_sandboxes-v$Version" }
if (Test-Path -LiteralPath $WorkDir) { throw 'WorkDir must be a new directory; previous runs are never overwritten' }
$relativeWorkDir = [IO.Path]::GetRelativePath($Repository, $WorkDir)
if ($relativeWorkDir -eq '.' -or (-not [IO.Path]::IsPathRooted($relativeWorkDir) -and
    $relativeWorkDir -ne '..' -and -not $relativeWorkDir.StartsWith("..$([IO.Path]::DirectorySeparatorChar)")))
{
    throw 'WorkDir must be outside Repository'
}

$approved = [ordered]@{}
$lines = @(Get-Content -LiteralPath $ApprovedSha256Sums | Where-Object { $_.Trim() })
if ($lines.Count -ne $crates.Count) { throw 'ApprovedSha256Sums must contain exactly the two approved crate digests' }
for ($i = 0; $i -lt $crates.Count; $i++)
{
    $name = $crates[$i]
    $file = "$name/$name-$Version.crate"
    if ($lines[$i] -cnotmatch "^([0-9a-fA-F]{64})  $([regex]::Escape($file))$")
    {
        throw "ApprovedSha256Sums must list a SHA-256 and $file in publication order"
    }
    $approved[$name] = $Matches[1].ToLowerInvariant()
    if ($null -ne $approval)
    {
        $archive = $approval.archives[$i]
        foreach ($field in 'crate', 'file', 'sha256')
        {
            if ($archive -isnot [System.Collections.IDictionary] -or -not $archive.Contains($field) -or
                $archive[$field] -isnot [string])
            {
                throw "ApprovedManifest archive must contain a string '$field'"
            }
        }
        if ($archive.crate -cne $name -or $archive.file -cne $file)
        {
            throw 'ApprovedManifest archives must match the version and publication order'
        }
        if ($archive.sha256.ToLowerInvariant() -cne $approved[$name])
        {
            throw 'ApprovedManifest archive digests do not match ApprovedSha256Sums'
        }
    }
}

$head = (Invoke-Native 'git rev-parse HEAD' { git -C $Repository rev-parse HEAD }).Trim()
if ($head -cne $Commit) { throw "HEAD is $head, not approved commit $Commit" }
if (Invoke-Native 'git status' { git -C $Repository status --porcelain --untracked-files=all })
{
    throw 'the release checkout is not clean'
}
$root = (Invoke-Native 'git rev-parse --show-toplevel' { git -C $Repository rev-parse --show-toplevel }).Trim()
if ((Resolve-Path -LiteralPath $root).Path -ne $Repository) { throw 'Repository must be the checkout root' }
$origin = (Invoke-Native 'git remote get-url origin' { git -C $Repository remote get-url origin }).Trim()
if ($origin -cnotmatch '^(https://github\.com/Azure/aci-edge-sandbox(\.git)?|git@github\.com:Azure/aci-edge-sandbox\.git)$')
{
    throw 'origin must be the Azure/aci-edge-sandbox GitHub repository'
}
Invoke-Native 'git fetch origin main' { git -C $Repository fetch --quiet --no-tags origin 'refs/heads/main:refs/remotes/origin/main' }
Invoke-Native 'approved commit on origin/main' { git -C $Repository merge-base --is-ancestor $Commit refs/remotes/origin/main }

$localExists = [bool](Invoke-Native 'git tag --list' { git -C $Repository tag --list $Tag })
if ($localExists)
{
    $localTarget = (Invoke-Native 'resolve local tag' { git -C $Repository rev-parse --verify "refs/tags/$Tag^{commit}" }).Trim()
    if ($localTarget -cne $Commit) { throw "local tag $Tag does not name approved commit $Commit" }
}
$remote = @(Invoke-Native 'git ls-remote' { git -C $Repository ls-remote origin "refs/tags/$Tag" "refs/tags/$Tag^{}" })
$peeled = $remote | Where-Object { $_ -match '\^\{\}$' } | Select-Object -First 1
$remoteTarget = if ($peeled) { ($peeled -split '\s+')[0] } elseif ($remote.Count) { ($remote[0] -split '\s+')[0] } else { $null }
if ($remoteTarget -and $remoteTarget -cne $Commit) { throw "remote tag $Tag does not name approved commit $Commit" }

$toolchainArgs = @("+$Toolchain")
$cargoVersion = (Invoke-Native 'cargo --version' { cargo @toolchainArgs --version }).Trim()
if (-not $cargoVersion.StartsWith("cargo $Toolchain ")) { throw 'the selected Cargo does not match Toolchain' }
if ($null -ne $approval -and $approval.cargo -cne $cargoVersion)
{
    throw "ApprovedManifest used $($approval.cargo), not $cargoVersion"
}
$manifest = Join-Path $Repository 'aci-edge-sandbox/Cargo.toml'
$packageScript = Join-Path $PSScriptRoot 'Invoke-CratePackage.ps1'
$versionScript = Join-Path $PSScriptRoot 'Get-CratesIoVersion.ps1'
$environment = @{}
foreach ($item in Get-ChildItem Env: | Where-Object { $_.Name -like 'ACI_EDGE_SANDBOXES_*' -or $_.Name -eq 'CARGO_TARGET_DIR' })
{
    $environment[$item.Name] = $item.Value
}

Push-Location $Repository
try
{
    foreach ($name in $environment.Keys) { Remove-Item -LiteralPath "Env:$name" }
    New-Item -ItemType Directory -Path $WorkDir | Out-Null
    $env:CARGO_TARGET_DIR = Join-Path $WorkDir 'target'
    $metadata = Invoke-Native 'cargo metadata' {
        cargo @toolchainArgs metadata --locked --no-deps --format-version 1 --manifest-path $manifest
    } | Out-String | ConvertFrom-Json
    foreach ($name in $crates)
    {
        $packages = @($metadata.packages | Where-Object name -EQ $name)
        if ($packages.Count -ne 1 -or $packages[0].version -cne $Version) { throw "$name is not version $Version" }
        $nvx = @($packages[0].dependencies | Where-Object name -EQ 'nvx')
        if ($nvx.Count -ne 1 -or $nvx[0].req -cne "=$ExpectedNvxVersion") { throw "$name does not pin the approved nvx version" }
    }
    $lock = Get-Content -Raw -LiteralPath (Join-Path $Repository 'aci-edge-sandbox/Cargo.lock')
    if ($lock -notmatch "name = `"nvx`"\s+version = `"$([regex]::Escape($ExpectedNvxVersion))`"\s+source = `"registry\+https://github\.com/rust-lang/crates\.io-index`"")
    {
        throw 'Cargo.lock does not resolve the approved nvx version from crates.io'
    }
    $pin = Get-Content -Raw -LiteralPath (Join-Path $Repository 'aci-edge-sandbox/agent-bundle.json') | ConvertFrom-Json
    if (-not $pin.release -or -not $pin.asset -or $pin.sha256 -ne $ExpectedBundleSha256) { throw 'the bundle pin does not match the approval' }
    if ($null -ne $approval -and
        ($pin.release -cne $approval.agentBundle.release -or $pin.asset -cne $approval.agentBundle.asset))
    {
        throw 'the full bundle pin does not match ApprovedManifest'
    }

    $state = [ordered]@{}
    foreach ($name in $crates)
    {
        $state[$name] = & $versionScript -Crate $name -Version $Version -ExpectedSha256 $approved[$name]
        if ($state[$name] -notin 'absent', 'published') { throw "unexpected registry state for $name" }
        Write-Host "$name $Version on crates.io: $($state[$name])"
    }
    if ($state.aci_edge_sandboxes -eq 'published' -and $state.aci_edge_sandboxes_model -ne 'published')
    {
        throw 'the client is published but its model is absent'
    }

    # Always run the full gate, including dry runs and fully or partially published releases.
    & $packageScript -Repository $Repository -OutDir (Join-Path $WorkDir 'crates') `
        -ApprovedSha256Sums $ApprovedSha256Sums -Toolchain $Toolchain
    if ($DryRun)
    {
        Write-Host "Dry run passed. No tag was created or pushed and no crate was published. Packages: $WorkDir"
        return
    }

    if (-not $remoteTarget)
    {
        if (-not $localExists)
        {
            Invoke-Native 'git tag' { git -C $Repository tag -a $Tag $Commit -m "aci_edge_sandboxes and aci_edge_sandboxes_model $Version" }
        }
        Invoke-Native 'git push tag' { git -C $Repository push --no-follow-tags origin "refs/tags/${Tag}:refs/tags/$Tag" }
    }
    foreach ($name in $crates)
    {
        # Recheck on resumption and immediately before each upload; a mismatch is never skipped.
        $current = & $versionScript -Crate $name -Version $Version -ExpectedSha256 $approved[$name]
        if ($current -eq 'published')
        {
            Write-Host "$name $Version already has the approved checksum; skipped."
            continue
        }
        if ($current -ne 'absent') { throw "unexpected registry state for $name" }

        # Cargo publish repackages. Check the standalone archive after the model is indexed.
        Invoke-Native "cargo package $name" {
            cargo @toolchainArgs package --locked --no-verify --registry crates-io --manifest-path $manifest -p $name
        }
        $archive = Join-Path $env:CARGO_TARGET_DIR "package/$name-$Version.crate"
        $actual = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actual -cne $approved[$name]) { throw "$name packages as $actual, not approved checksum $($approved[$name])" }
        Invoke-Native "cargo publish $name" {
            cargo @toolchainArgs publish --locked --registry crates-io --manifest-path $manifest -p $name
        }
        & $versionScript -Crate $name -Version $Version -ExpectedSha256 $approved[$name] -Wait | Out-Null
    }
    Write-Host 'Both crates have the approved checksums on crates.io. Revoke the token and run cargo logout.'
}
finally
{
    if (Test-Path -LiteralPath Env:CARGO_TARGET_DIR) { Remove-Item -LiteralPath Env:CARGO_TARGET_DIR }
    foreach ($name in $environment.Keys) { Set-Item -LiteralPath "Env:$name" -Value $environment[$name] }
    Pop-Location
}
