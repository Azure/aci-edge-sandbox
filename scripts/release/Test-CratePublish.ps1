# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

#Requires -Version 7.3
<#
.SYNOPSIS
Exercises manual publishing control flow with fake git, cargo and registry responses.
.DESCRIPTION
Runs without credentials, network access, Git tag writes or real publication.
Real package preparation is separately checked with Invoke-CratePublish.ps1 -DryRun.
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$root = Join-Path ([IO.Path]::GetTempPath()) "crate-publish-tests-$([guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path (Join-Path $root 'scripts/release') -Force | Out-Null
New-Item -ItemType Directory -Path (Join-Path $root 'source/aci-edge-sandbox') -Force | Out-Null
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'Invoke-CratePublish.ps1') -Destination (Join-Path $root 'scripts/release')
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'Get-CratesIoVersion.ps1') -Destination (Join-Path $root 'scripts/release')

# Only the package helper is substituted; registry validation uses the real helper.
@'
param($Repository, $OutDir, $ApprovedSha256Sums, $Toolchain)
$global:CratePublishTest.Events.Add('approval-gate')
if ($global:CratePublishTest.PackageFailure) { throw 'simulated package command failure' }
$expected = foreach ($name in 'aci_edge_sandboxes_model', 'aci_edge_sandboxes')
{
    "$($global:CratePublishTest.Hashes[$name])  $name/$name-$($global:CratePublishTest.Version).crate"
}
if ((Get-Content -Raw $ApprovedSha256Sums).Trim() -cne ($expected -join "`n"))
{
    throw 'the archives differ from the approved digests'
}
'@ | Set-Content -LiteralPath (Join-Path $root 'scripts/release/Invoke-CratePackage.ps1') -Encoding utf8NoBOM

function git
{
    $a = @($args)
    if ($a[0] -eq '-C') { $a = $a[2..($a.Count - 1)] }
    $command = $a -join ' '
    $global:CratePublishTest.Events.Add("git $command")
    $global:LASTEXITCODE = 0
    if ($global:CratePublishTest.NativeFailure -and $command.StartsWith($global:CratePublishTest.NativeFailure))
    {
        $global:LASTEXITCODE = 17
        return
    }
    switch -Wildcard ($command)
    {
        'rev-parse HEAD' { $global:CratePublishTest.Head }
        'rev-parse --show-toplevel' { Join-Path $root 'source' }
        'status *' { if ($global:CratePublishTest.Dirty) { ' M tracked-file' } }
        'remote get-url origin' { $global:CratePublishTest.Origin }
        'tag --list *' { if ($global:CratePublishTest.LocalTag) { 'local-tag' } }
        'rev-parse --verify *' { $global:CratePublishTest.LocalTag }
        'ls-remote *' {
            if ($global:CratePublishTest.RemoteTag)
            {
                "$('a' * 40)`trefs/tags/$($global:CratePublishTest.Tag)"
                "$($global:CratePublishTest.RemoteTag)`trefs/tags/$($global:CratePublishTest.Tag)^{}"
            }
        }
        'fetch *' { }
        'merge-base *' { }
        'tag -a *' { }
        'push *' { }
        default { throw "unexpected fake git command: $command" }
    }
}

function cargo
{
    $a = @($args)
    $command = $a -join ' '
    $global:CratePublishTest.Events.Add("cargo $command")
    $global:LASTEXITCODE = 0
    if ($global:CratePublishTest.NativeFailure -and $a[1] -eq $global:CratePublishTest.NativeFailure)
    {
        $global:LASTEXITCODE = 17
        return
    }
    switch ($a[1])
    {
        '--version' { 'cargo 1.95.0 (test)' }
        'metadata' {
            $packages = foreach ($name in 'aci_edge_sandboxes_model', 'aci_edge_sandboxes')
            {
                @{ name = $name; version = $global:CratePublishTest.SourceVersion
                    dependencies = @(@{ name = 'nvx'; req = "=$($global:CratePublishTest.Nvx)" }) }
            }
            @{ packages = @($packages) } | ConvertTo-Json -Depth 6 -Compress
        }
        'package' {
            $name = $a[-1]
            $directory = Join-Path $env:CARGO_TARGET_DIR 'package'
            New-Item -ItemType Directory -Path $directory -Force | Out-Null
            $text = if ($global:CratePublishTest.StandaloneMismatch) { 'unexpected archive' } else { $name }
            [IO.File]::WriteAllText((Join-Path $directory "$name-$($global:CratePublishTest.Version).crate"), $text)
        }
        'publish' { $global:CratePublishTest.Published[$a[-1]] = $true }
        default { throw "unexpected fake cargo command: $command" }
    }
}

function Invoke-WebRequest
{
    param($Uri, [switch] $SkipHttpErrorCheck, $Headers)
    $name = ($Uri -split '/')[-1]
    $global:CratePublishTest.Events.Add("registry $name")
    if ($global:CratePublishTest.HttpFailure -or ($global:CratePublishTest.HttpFailureAfterUpload -and
        ($global:CratePublishTest.Events | Where-Object { $_ -like 'cargo * publish *' })))
    {
        return @{ StatusCode = 503; Content = '' }
    }
    $hash = if ($global:CratePublishTest.RegistryMismatch) { 'f' * 64 } else { $global:CratePublishTest.Hashes[$name] }
    $content = if ($global:CratePublishTest.Published[$name])
    {
        @{ vers = $global:CratePublishTest.Version; cksum = $hash; yanked = $global:CratePublishTest.Yanked } |
            ConvertTo-Json -Compress
    }
    else { '' }
    @{ StatusCode = 200; Content = $content }
}

$cases = @(
    @{ Name = 'unpublished dry run'; DryRun = $true },
    @{ Name = 'fully published dry run still packages'; DryRun = $true; Published = 'both' },
    @{ Name = 'partial dry run still packages'; DryRun = $true; Published = 'model' },
    @{ Name = 'model-first publication'; DryRun = $false },
    @{ Name = 'partial publication resumes client only'; DryRun = $false; Published = 'model' },
    @{ Name = 'fully published skips both uploads'; DryRun = $false; Published = 'both' },
    @{ Name = 'matching local and remote tags'; DryRun = $true; MatchingTags = $true },
    @{ Name = 'matching remote tag without local tag'; DryRun = $false; Change = { $global:CratePublishTest.RemoteTag = $global:CratePublishTest.Head } },
    @{ Name = 'wrong HEAD'; Error = 'HEAD is'; Change = { $global:CratePublishTest.Head = 'b' * 40 } },
    @{ Name = 'dirty checkout'; Error = 'not clean'; Change = { $global:CratePublishTest.Dirty = $true } },
    @{ Name = 'wrong origin'; Error = 'origin must'; Change = { $global:CratePublishTest.Origin = 'https://github.com/example/wrong.git' } },
    @{ Name = 'failed git command'; Error = 'exit 17'; Change = { $global:CratePublishTest.NativeFailure = 'fetch' } },
    @{ Name = 'commit outside main'; Error = 'exit 17'; Change = { $global:CratePublishTest.NativeFailure = 'merge-base' } },
    @{ Name = 'conflicting local tag with matching remote'; Error = 'local tag'; Change = {
        $global:CratePublishTest.LocalTag = 'b' * 40; $global:CratePublishTest.RemoteTag = $global:CratePublishTest.Head
    } },
    @{ Name = 'conflicting remote tag'; Error = 'remote tag'; Change = { $global:CratePublishTest.RemoteTag = 'b' * 40 } },
    @{ Name = 'wrong dependency pin'; Error = 'approved nvx'; Change = { $global:CratePublishTest.Nvx = 'wrong' } },
    @{ Name = 'wrong source version'; Error = 'not version'; Change = { $global:CratePublishTest.SourceVersion = '1.2.4' } },
    @{ Name = 'wrong bundle pin'; Error = 'bundle pin'; BundleMismatch = $true },
    @{ Name = 'wrong lock file pin'; Error = 'Cargo.lock'; LockMismatch = $true },
    @{ Name = 'inconsistent registry state'; Error = 'model is absent'; Published = 'client' },
    @{ Name = 'existing work directory'; Error = 'new directory'; ExistingWorkDir = $true },
    @{ Name = 'work directory inside checkout'; Error = 'outside Repository'; InsideWorkDir = $true },
    @{ Name = 'malformed approval'; Error = 'publication order'; MalformedApproval = $true },
    @{ Name = 'wrong approval hash'; Error = 'approved digests'; WrongHash = $true },
    @{ Name = 'failed packaging'; Error = 'package command failure'; Change = { $global:CratePublishTest.PackageFailure = $true } },
    @{ Name = 'failed cargo command'; Error = 'exit 17'; Change = { $global:CratePublishTest.NativeFailure = 'metadata' } },
    @{ Name = 'registry checksum mismatch'; Published = 'both'; Error = 'crates.io lists'; Change = { $global:CratePublishTest.RegistryMismatch = $true } },
    @{ Name = 'yanked release'; Published = 'both'; Error = 'as yanked'; Change = { $global:CratePublishTest.Yanked = $true } },
    @{ Name = 'registry HTTP failure'; Error = 'answered 503'; Change = { $global:CratePublishTest.HttpFailure = $true } },
    @{ Name = 'standalone repackaging mismatch'; DryRun = $false; Error = 'packages as'; Change = { $global:CratePublishTest.StandaloneMismatch = $true } },
    @{ Name = 'native upload failure stops client'; DryRun = $false; Error = 'exit 17'; Change = { $global:CratePublishTest.NativeFailure = 'publish' } },
    @{ Name = 'model registry verification failure stops client'; DryRun = $false; Error = 'answered 503'; Change = { $global:CratePublishTest.HttpFailureAfterUpload = $true } },
    @{ Name = 'tag push failure stops uploads'; DryRun = $false; Error = 'exit 17'; Change = { $global:CratePublishTest.NativeFailure = 'push' } }
)

$oldTarget = $env:CARGO_TARGET_DIR
$oldBundle = $env:ACI_EDGE_SANDBOXES_AGENT_BUNDLE
try
{
    $env:CARGO_TARGET_DIR = 'test-target-sentinel'
    $env:ACI_EDGE_SANDBOXES_AGENT_BUNDLE = 'test-bundle-sentinel'
    foreach ($case in $cases)
    {
        foreach ($key in 'Published', 'MatchingTags', 'Change', 'WrongHash', 'BundleMismatch', 'DryRun', 'Error',
            'LockMismatch', 'ExistingWorkDir', 'InsideWorkDir', 'MalformedApproval')
        {
            if (-not $case.ContainsKey($key)) { $case[$key] = $null }
        }
        $global:CratePublishTest = @{
            Version = '1.2.3'; SourceVersion = '1.2.3'; Tag = 'aci_edge_sandboxes-v1.2.3'; Head = '1' * 40; Nvx = '0.2.0'
            Origin = 'https://github.com/Azure/aci-edge-sandbox.git'
            LocalTag = ''; RemoteTag = ''; NativeFailure = ''; Dirty = $false; PackageFailure = $false
            RegistryMismatch = $false; HttpFailure = $false; HttpFailureAfterUpload = $false; Yanked = $false; StandaloneMismatch = $false
            Hashes = @{}; Published = @{ aci_edge_sandboxes_model = $false; aci_edge_sandboxes = $false }
            Events = [Collections.Generic.List[string]]::new()
        }
        foreach ($name in 'aci_edge_sandboxes_model', 'aci_edge_sandboxes')
        {
            $hash = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData([Text.Encoding]::UTF8.GetBytes($name))).ToLowerInvariant()
            $global:CratePublishTest.Hashes[$name] = $hash
            $global:CratePublishTest.Published[$name] = $case.Published -eq 'both' -or
                ($case.Published -eq 'model' -and $name -eq 'aci_edge_sandboxes_model') -or
                ($case.Published -eq 'client' -and $name -eq 'aci_edge_sandboxes')
        }
        if ($case.MatchingTags)
        {
            $global:CratePublishTest.LocalTag = $global:CratePublishTest.Head
            $global:CratePublishTest.RemoteTag = $global:CratePublishTest.Head
        }
        if ($case.Change) { & $case.Change }
        $approval = Join-Path $root 'SHA256SUMS'
        $sums = foreach ($name in 'aci_edge_sandboxes_model', 'aci_edge_sandboxes')
        {
            $hash = if ($case.WrongHash) { '0' * 64 } else { $global:CratePublishTest.Hashes[$name] }
            "$hash  $name/$name-1.2.3.crate"
        }
        if ($case.MalformedApproval) { $sums[0] = $sums[0] -replace '  ', ' ' }
        [IO.File]::WriteAllText($approval, ($sums -join "`n") + "`n")
        $lock = @'
[[package]]
name = "nvx"
version = "0.2.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
'@
        if ($case.LockMismatch) { $lock = $lock.Replace('0.2.0', '0.2.1') }
        Set-Content -LiteralPath (Join-Path $root 'source/aci-edge-sandbox/Cargo.lock') -Value $lock
        $bundleHash = '2' * 64
        @{ release = 'bundle-test'; asset = 'bundle.zip'; sha256 = $bundleHash } |
            ConvertTo-Json | Set-Content -LiteralPath (Join-Path $root 'source/aci-edge-sandbox/agent-bundle.json')
        $parameters = @{
            Version = '1.2.3'; Commit = '1' * 40; Toolchain = '1.95.0'
            Repository = (Join-Path $root 'source'); WorkDir = (Join-Path $root ([guid]::NewGuid().ToString('N')))
            ApprovedSha256Sums = $approval; ExpectedNvxVersion = '0.2.0'
            ExpectedBundleSha256 = $(if ($case.BundleMismatch) { '3' * 64 } else { $bundleHash })
            DryRun = $case.DryRun -ne $false
        }
        if ($case.ExistingWorkDir) { $parameters.WorkDir = $root }
        if ($case.InsideWorkDir) { $parameters.WorkDir = Join-Path $root 'source/output' }
        $caught = $null
        try { & (Join-Path $root 'scripts/release/Invoke-CratePublish.ps1') @parameters | Out-Null }
        catch { $caught = $_.Exception.Message }
        if ($case.Error)
        {
            if (-not $caught -or $caught -notlike "*$($case.Error)*") { throw "$($case.Name): expected '$($case.Error)', got '$caught'" }
        }
        elseif ($caught) { throw "$($case.Name): $caught" }
        $events = @($global:CratePublishTest.Events)
        $writes = @($events | Where-Object { $_ -like 'git tag -a *' -or $_ -like 'git push *' -or $_ -like 'cargo * publish *' })
        if ($parameters.DryRun -and $writes.Count) { throw "$($case.Name): dry run performed a write" }
        if ($env:CARGO_TARGET_DIR -ne 'test-target-sentinel' -or $env:ACI_EDGE_SANDBOXES_AGENT_BUNDLE -ne 'test-bundle-sentinel')
        {
            throw "$($case.Name): caller environment was not restored"
        }
        if (-not $case.Error)
        {
            if ($events -notcontains 'approval-gate') { throw "$($case.Name): approval gate skipped" }
            $uploads = @($events | Where-Object { $_ -like 'cargo * publish *' })
            $expectedCount = if ($parameters.DryRun -or $case.Published -eq 'both') { 0 } elseif ($case.Published -eq 'model') { 1 } else { 2 }
            if ($uploads.Count -ne $expectedCount) { throw "$($case.Name): wrong upload count" }
            if ($uploads.Count -eq 2 -and ($uploads[0] -notlike '*-p aci_edge_sandboxes_model' -or $uploads[1] -notlike '*-p aci_edge_sandboxes'))
            {
                throw "$($case.Name): publication order is wrong"
            }
            if ($uploads.Count -eq 2)
            {
                $first = [Array]::IndexOf($events, $uploads[0])
                $second = [Array]::IndexOf($events, $uploads[1])
                if ($events[($first + 1)..($second - 1)] -notcontains 'registry aci_edge_sandboxes_model')
                {
                    throw "$($case.Name): did not verify the model before publishing the client"
                }
            }
        }
        elseif ($case.Name -notin 'standalone repackaging mismatch', 'native upload failure stops client',
            'model registry verification failure stops client', 'tag push failure stops uploads' -and $writes.Count)
        {
            throw "$($case.Name): wrote a tag or uploaded before passing preflight"
        }
        if ($case.Name -in 'native upload failure stops client', 'model registry verification failure stops client')
        {
            $uploads = @($events | Where-Object { $_ -like 'cargo * publish *' })
            if ($uploads.Count -ne 1 -or $uploads[0] -notlike '*-p aci_edge_sandboxes_model')
            {
                throw "$($case.Name): did not stop after the failed model upload/verification"
            }
        }
        $firstWrite = $writes | Select-Object -First 1
        if ($firstWrite -and [Array]::IndexOf($events, 'approval-gate') -gt [Array]::IndexOf($events, $firstWrite))
        {
            throw "$($case.Name): wrote before the full approval gate"
        }
        Write-Host "PASS: $($case.Name)"
    }
    Write-Host "$($cases.Count) manual publishing cases passed; all external operations were simulated."
    $global:LASTEXITCODE = 0
}
finally
{
    $env:CARGO_TARGET_DIR = $oldTarget
    $env:ACI_EDGE_SANDBOXES_AGENT_BUNDLE = $oldBundle
    Remove-Variable -Name CratePublishTest -Scope Global -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $root -Recurse -Force
}
