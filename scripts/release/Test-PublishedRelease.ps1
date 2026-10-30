# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

<#
.SYNOPSIS
Checks a published release from crates.io and the GitHub release alone, as a new consumer.

.DESCRIPTION
In a new work directory with its own CARGO_HOME and target directory, and with every
ACI_EDGE_SANDBOXES_* variable cleared, it builds a consumer that depends on exactly the published
aci_edge_sandboxes version with `agent-bundled`. The consumer's program is the crate's own
`agent_lifecycle` example, taken from the published crate archive. It then checks that:

- Cargo.lock takes aci_edge_sandboxes, aci_edge_sandboxes_model, and nvx from crates.io, at the
  expected versions, with no path, git, or patched source;
- the build downloaded the bundle that the published agent-bundle.json pins, from the GitHub
  release, and the staged bundle's library digest is the expected one;
- on a Windows x64 host with the Windows Hypervisor Platform, the program pulls and converts a
  public image, provisions, starts, runs a command, stops, and deprovisions a sandbox; and, with
  platform snapshots, captures the templates and starts the sandbox as a clone of one (restore),
  runs the command in it, and deprovisions it;
- no OpenVMM process outlives the run.

State lives on NTFS (the default temporary directory): snapshot capture fails on ReFS Dev Drives.

-RehearsalArchiveDir, -RehearsalBundleZip and -RehearsalBundleSha256 rehearse the check before
publication: the crates come from Invoke-CratePackage.ps1's archives through [patch.crates-io], and
the bundle from a local ZIP through ACI_EDGE_SANDBOXES_AGENT_BUNDLE_URL and _SHA256. The checks of
crates.io sources and of the GitHub download then expect those local sources instead; every other
check is the same. A rehearsal is not evidence of the published release.

.EXAMPLE
pwsh scripts/release/Test-PublishedRelease.ps1 -Version 0.1.0 -NvxVersion 0.1.0-alpha.6 -ExpectedLibrarySha256 <sha256>
#>
[CmdletBinding()]
param
(
    [Parameter(Mandatory)] [string] $Version,
    [Parameter(Mandatory)] [string] $NvxVersion,
    # The backendLibrarySha256 of the released bundle's bundle-manifest.json.
    [Parameter(Mandatory)] [string] $ExpectedLibrarySha256,
    [string] $ImageRef = 'mcr.microsoft.com/azurelinux/base/python:3',
    [string] $Toolchain,
    [string] $WorkDir,
    # Sandbox state; on NTFS (a Dev Drive fails snapshot capture). Under WorkDir when omitted.
    [string] $StateDir,
    [string] $RehearsalArchiveDir,
    [string] $RehearsalBundleZip,
    [string] $RehearsalBundleSha256
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if (-not $WorkDir)
{
    $WorkDir = Join-Path ([System.IO.Path]::GetTempPath()) ('aci-published-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
}
New-Item -ItemType Directory -Force -Path $WorkDir | Out-Null
$WorkDir = (Resolve-Path $WorkDir).Path
$results = [System.Collections.Generic.List[object]]::new()
function Add-Result([string] $Check, [bool] $Pass, [string] $Detail = '')
{
    $results.Add([pscustomobject]@{ Check = $Check; Result = $(if ($Pass) { 'PASS' } else { 'FAIL' }); Detail = $Detail })
    Write-Host "[$($results[-1].Result)] $Check $Detail"
}

# Only the published packages: no bundle overrides, no inherited Cargo configuration or caches.
Get-ChildItem Env: | Where-Object { $_.Name -like 'ACI_EDGE_SANDBOXES_*' -or $_.Name -like 'CARGO_*' -or $_.Name -like 'EDGE_AGENT_TEST_*' } |
    ForEach-Object { Remove-Item -LiteralPath "Env:$($_.Name)" }
$env:CARGO_HOME = Join-Path $WorkDir 'cargo-home'
$env:CARGO_TARGET_DIR = Join-Path $WorkDir 'target'
$cargo = @('cargo')
if ($Toolchain) { $cargo += "+$Toolchain" }
function Invoke-Cargo([string] $Log, [string[]] $Arguments)
{
    & $cargo[0] @($cargo | Select-Object -Skip 1) @Arguments *> (Join-Path $WorkDir $Log)
    return $LASTEXITCODE -eq 0
}

$rehearsal = [bool]$RehearsalArchiveDir
if ($rehearsal -and -not ($RehearsalBundleZip -and $RehearsalBundleSha256 -match '^[0-9a-f]{64}$')) { throw 'A rehearsal needs -RehearsalBundleZip and -RehearsalBundleSha256.' }
if ($rehearsal) { Write-Host 'REHEARSAL: local archives and bundle ZIP; not evidence of the published release.' }

# The published crate's own example, from its crates.io archive (or the rehearsal's archives).
$archive = Join-Path $WorkDir "aci_edge_sandboxes-$Version.crate"
if ($rehearsal)
{
    Copy-Item -LiteralPath (Join-Path $RehearsalArchiveDir "aci_edge_sandboxes/aci_edge_sandboxes-$Version.crate") -Destination $archive
    tar -xzf (Join-Path $RehearsalArchiveDir "aci_edge_sandboxes_model/aci_edge_sandboxes_model-$Version.crate") -C $WorkDir
}
else
{
    Invoke-WebRequest -UseBasicParsing -Uri "https://static.crates.io/crates/aci_edge_sandboxes/aci_edge_sandboxes-$Version.crate" -OutFile $archive
}
tar -xzf $archive -C $WorkDir
$published = Join-Path $WorkDir "aci_edge_sandboxes-$Version"
$pin = Get-Content -Raw (Join-Path $published 'agent-bundle.json') | ConvertFrom-Json
Add-Result 'published crate pins a bundle' ($pin.release -and $pin.asset -and $pin.sha256 -match '^[0-9a-f]{64}$') "$($pin.release)/$($pin.asset) $($pin.sha256)"
if ($rehearsal)
{
    $env:ACI_EDGE_SANDBOXES_AGENT_BUNDLE_URL = 'file:///' + ((Resolve-Path $RehearsalBundleZip).Path -replace '\\', '/')
    $env:ACI_EDGE_SANDBOXES_AGENT_BUNDLE_SHA256 = $RehearsalBundleSha256.ToLowerInvariant()
}

$app = Join-Path $WorkDir 'consumer'
New-Item -ItemType Directory -Force -Path (Join-Path $app 'src') | Out-Null
Set-Content -LiteralPath (Join-Path $app 'Cargo.toml') -Value @"
[package]
name = "aci_published_consumer"
version = "0.1.0"
edition = "2024"
publish = false

[dependencies]
aci_edge_sandboxes = { version = "=$Version", features = ["agent-bundled"] }
# The example reads JSON output; a consumer names its own direct dependencies.
serde_json = "1"

# The example's cfg(feature = ...) checks name the compiling crate's features: enable the bundled path.
[features]
default = ["agent", "agent-bundled"]
agent = []
agent-bundled = []

[workspace]
"@
if ($rehearsal)
{
    $model = (Join-Path $WorkDir "aci_edge_sandboxes_model-$Version") -replace '\\', '/'
    Add-Content -LiteralPath (Join-Path $app 'Cargo.toml') -Value "`n[patch.crates-io]`naci_edge_sandboxes = { path = `"$($published -replace '\\', '/')`" }`naci_edge_sandboxes_model = { path = `"$model`" }"
}
Copy-Item -LiteralPath (Join-Path $published 'examples/agent_lifecycle.rs') -Destination (Join-Path $app 'src/main.rs')

Push-Location $app
try
{
    $built = Invoke-Cargo 'build.log' @('build', '--release', '-vv')
    Add-Result 'build from crates.io' $built (Join-Path $WorkDir 'build.log')
    $lock = Get-Content -Raw 'Cargo.lock'
    foreach ($pair in @(@('aci_edge_sandboxes', $Version), @('aci_edge_sandboxes_model', $Version), @('nvx', $NvxVersion)))
    {
        $match = [regex]::Match($lock, "(?ms)^name = `"$($pair[0])`"\r?\nversion = `"([^`"]+)`"\r?\nsource = `"([^`"]+)`"")
        if ($rehearsal -and $pair[0] -ne 'nvx')
        {
            # A patched path dependency has no source line.
            $local = [regex]::Match($lock, "(?ms)^name = `"$($pair[0])`"\r?\nversion = `"([^`"]+)`"\r?\n(?!source)")
            Add-Result "lock: $($pair[0]) (rehearsal: local archive)" ($local.Success -and $local.Groups[1].Value -ceq $pair[1]) $local.Groups[1].Value
            continue
        }
        Add-Result "lock: $($pair[0])" ($match.Success -and $match.Groups[1].Value -ceq $pair[1] -and
            $match.Groups[2].Value -ceq 'registry+https://github.com/rust-lang/crates.io-index') "$($match.Groups[1].Value) $($match.Groups[2].Value)"
    }
    Add-Result 'lock: no git sources' (-not ($lock -match 'source = "git\+')) ''
    if (-not $rehearsal) { Add-Result 'lock: no path or patched sources' (-not ($lock -match 'source = "path\+') -and -not (Get-Content -Raw 'Cargo.toml').Contains('[patch')) '' }
    $url = if ($rehearsal) { $env:ACI_EDGE_SANDBOXES_AGENT_BUNDLE_URL } else { "https://github.com/Azure/aci-edge-sandbox/releases/download/$($pin.release)/$($pin.asset)" }
    $log = Get-Content -Raw (Join-Path $WorkDir 'build.log')
    # A file:// URL is read in place, without a download message; the staged library digest below checks it.
    if (-not $rehearsal) { Add-Result 'bundle downloaded from the GitHub release' ($log.Contains("agent: downloaded $url")) $url }
    $staged = Join-Path $env:CARGO_TARGET_DIR 'release\aci-edge-sandboxes'
    $library = Join-Path $staged 'bin\aci_edge_agent.dll'
    $actual = if (Test-Path $library) { (Get-FileHash $library).Hash.ToLowerInvariant() } else { '' }
    Add-Result 'staged library is the released one' ($actual -ceq $ExpectedLibrarySha256.ToLowerInvariant()) $actual

    $exe = Join-Path $env:CARGO_TARGET_DIR 'release\aci_published_consumer.exe'
    $state = if ($StateDir) { $StateDir } else { Join-Path $WorkDir 'state' }
    & $exe --image-ref $ImageRef --state-root (Join-Path $state 'cold') *> (Join-Path $WorkDir 'lifecycle.log')
    $out = Get-Content -Raw (Join-Path $WorkDir 'lifecycle.log')
    Add-Result 'lifecycle with image pull and conversion' ($LASTEXITCODE -eq 0 -and $out.Contains('READY')) (Join-Path $WorkDir 'lifecycle.log')
    & $exe --image-ref $ImageRef --state-root (Join-Path $state 'snapshots') --platform-snapshots 2,512 *> (Join-Path $WorkDir 'snapshots.log')
    $out = Get-Content -Raw (Join-Path $WorkDir 'snapshots.log')
    Add-Result 'snapshot capture and clone (restore) start, command in the clone' ($LASTEXITCODE -eq 0 -and $out.Contains('captured generation') -and
        $out.Contains('bootMode: clone') -and $out.Contains('READY')) (Join-Path $WorkDir 'snapshots.log')
}
finally
{
    Pop-Location
}
Add-Result 'no OpenVMM left running' (@(Get-Process -Name openvmm -ErrorAction SilentlyContinue).Count -eq 0) ''
# Each run deprovisioned its sandbox: a state root keeps only the host's shared state (image cache,
# image seals, platform-snapshot templates), never a sandbox's record, disks, or channels.
$shared = 'aci', 'images', 'seals', 'templates', 'templates-placeholder'
$left = @(foreach ($root in 'cold', 'snapshots') { Get-ChildItem -Force -ErrorAction SilentlyContinue (Join-Path $state $root) | Where-Object Name -cnotin $shared | ForEach-Object { "$root/$($_.Name)" } })
Add-Result 'no sandbox state left after deprovision' ($left.Count -eq 0) ($left -join ', ')
$results | ConvertTo-Json | Set-Content (Join-Path $WorkDir 'summary.json')
$results | Format-Table -AutoSize | Out-String | Write-Host
if (@($results | Where-Object Result -ne 'PASS').Count -gt 0) { throw "Published release check failed; logs are in $WorkDir." }
