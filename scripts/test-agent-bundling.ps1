# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

<#
.SYNOPSIS
Checks how the agent-bundled build stages the edge agent bundle.

.DESCRIPTION
Builds synthetic bundles and checks that the build script of aci_edge_sandboxes stages good
ones through ACI_EDGE_SANDBOXES_AGENT_BUNDLE_DIR and ACI_EDGE_SANDBOXES_AGENT_BUNDLE_URL, that
the bundled-agent tests then find them, and that it rejects broken ones. The synthetic bundles
need neither a hypervisor nor an agent library that runs, and no network.

With -Pinned, it instead builds without any bundle variable, so that the build downloads the
release asset that agent-bundle.json pins, from this checkout and from a copy of the crate
exported without Git metadata. That needs network access, and succeeds only when the pinned
bundle speaks this crate's sandbox ABI.

.EXAMPLE
pwsh scripts/test-agent-bundling.ps1

.EXAMPLE
pwsh scripts/test-agent-bundling.ps1 -Pinned
#>
[CmdletBinding()]
param
(
    # Rust toolchain, such as 1.95.0; the active one when omitted.
    [string] $Toolchain,
    # Work directory; a new temporary one when omitted. Kept after the run.
    [string] $WorkDir,
    # Build from the pinned release asset instead of synthetic bundles.
    [switch] $Pinned
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$crate = Join-Path (Split-Path $PSScriptRoot -Parent) 'aci-edge-sandbox'
if (-not $WorkDir)
{
    $WorkDir = Join-Path ([System.IO.Path]::GetTempPath()) ('aci-agent-bundling-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
}
New-Item -ItemType Directory -Force -Path $WorkDir | Out-Null
$WorkDir = (Resolve-Path $WorkDir).Path
$targetDir = Join-Path $WorkDir 'target'
$isWindowsHost = [System.Environment]::OSVersion.Platform -eq [System.PlatformID]::Win32NT
$toolchainArgs = @()
if ($Toolchain) { $toolchainArgs += "+$Toolchain" }
$variables = 'ACI_EDGE_SANDBOXES_AGENT_BUNDLE', 'ACI_EDGE_SANDBOXES_AGENT_BUNDLE_DIR',
    'ACI_EDGE_SANDBOXES_AGENT_BUNDLE_URL', 'ACI_EDGE_SANDBOXES_AGENT_BUNDLE_SHA256',
    'ACI_EDGE_SANDBOXES_AGENT_ARTIFACTS_DIR'
$results = [System.Collections.Generic.List[object]]::new()

function Get-Sha256([string] $Path)
{
    (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

# The sandbox ABI that this crate's model speaks; synthetic bundles speak it unless a scenario
# names another.
$wire = Get-Content -Raw -LiteralPath (Join-Path $crate 'model/src/wire.rs')
$match = [regex]::Match($wire, 'pub const SANDBOX_ABI_VERSION: u32 = (\d+);')
if (-not $match.Success) { throw 'model/src/wire.rs declares no SANDBOX_ABI_VERSION.' }
$crateAbi = [int]$match.Groups[1].Value

# A library header that names `Machine`: a PE file on Windows and a little-endian ELF file
# elsewhere. The build script reads only the header.
function New-LibraryBytes([bool] $Pe, [int] $Machine)
{
    if ($Pe)
    {
        $bytes = [byte[]]::new(0x200)
        $bytes[0] = 0x4d; $bytes[1] = 0x5a
        [BitConverter]::GetBytes([uint32]0x80).CopyTo($bytes, 0x3c)
        [System.Text.Encoding]::ASCII.GetBytes("PE`0`0").CopyTo($bytes, 0x80)
        [BitConverter]::GetBytes([uint16]$Machine).CopyTo($bytes, 0x84)
        return , $bytes
    }
    $bytes = [byte[]]::new(0x100)
    ([byte[]](0x7f, 0x45, 0x4c, 0x46, 2, 1, 1)).CopyTo($bytes, 0)
    [BitConverter]::GetBytes([uint16]$Machine).CopyTo($bytes, 18)
    return , $bytes
}

# Writes a format-2 edge bundle into a new `Directory` whose manifests agree with its files.
function New-SyntheticBundle
{
    param
    (
        [string] $Directory,
        [int] $SandboxAbi = $crateAbi,
        [int] $Machine = $(if ($isWindowsHost) { 0x8664 } else { 62 })
    )
    $exe = if ($isWindowsHost) { '.exe' } else { '' }
    $library = if ($isWindowsHost) { 'bin/aci_edge_agent.dll' } else { 'bin/libaci_edge_agent.so' }
    Remove-Item -LiteralPath $Directory -Recurse -Force -ErrorAction SilentlyContinue
    $files = [ordered]@{
        $library = $null
        "bin/openvmm$exe" = 'synthetic openvmm'
        "bin/direct-images$exe" = 'synthetic direct-images'
        'guest/vmlinux' = 'synthetic kernel'
        'guest/initramfs-edge.cpio.gz' = 'synthetic initramfs'
    }
    foreach ($name in $files.Keys)
    {
        $path = Join-Path $Directory $name
        New-Item -ItemType Directory -Force -Path (Split-Path $path -Parent) | Out-Null
        if ($name -eq $library)
        {
            [System.IO.File]::WriteAllBytes($path, (New-LibraryBytes $isWindowsHost $Machine))
        }
        else
        {
            [System.IO.File]::WriteAllText($path, $files[$name])
        }
    }
    $digest = @{}
    foreach ($name in $files.Keys) { $digest[$name] = Get-Sha256 (Join-Path $Directory $name) }
    $source = [ordered]@{
        format = 2
        profile = 'edge'
        openvmm = @{ executable_sha256 = $digest["bin/openvmm$exe"] }
        linux = @{ kernel_sha256 = $digest['guest/vmlinux'] }
        edge = @{ initramfs_sha256 = $digest['guest/initramfs-edge.cpio.gz'] }
        direct_images = @{ executable_sha256 = $digest["bin/direct-images$exe"] }
        agent = @{ library_sha256 = $digest[$library] }
    }
    $sourcePath = Join-Path $Directory 'SOURCE-MANIFEST.json'
    [System.IO.File]::WriteAllText($sourcePath, ($source | ConvertTo-Json -Depth 4))
    $manifest = [ordered]@{
        status = @{ complete = $true }
        consumer = [ordered]@{
            runtimeBundleManifest = 'SOURCE-MANIFEST.json'
            runtimeBundleManifestSha256 = Get-Sha256 $sourcePath
            backendLibrary = $library
            backendLibrarySha256 = $digest[$library]
            sandboxAbiVersion = $SandboxAbi
        }
    }
    [System.IO.File]::WriteAllText((Join-Path $Directory 'bundle-manifest.json'), ($manifest | ConvertTo-Json -Depth 4))
    $root = (Resolve-Path $Directory).Path
    $lines = Get-ChildItem -LiteralPath $root -Recurse -File |
        ForEach-Object { $_.FullName.Substring($root.Length + 1).Replace('\', '/') } |
        Sort-Object |
        ForEach-Object { "$(Get-Sha256 (Join-Path $root $_))  $_" }
    [System.IO.File]::WriteAllText((Join-Path $root 'SHA256SUMS'), (($lines -join "`n") + "`n"))
}

# Packs the bundle in `Directory` into the ZIP `Archive`.
function New-BundleArchive([string] $Directory, [string] $Archive)
{
    Remove-Item -LiteralPath $Archive -Force -ErrorAction SilentlyContinue
    if ($isWindowsHost)
    {
        & (Join-Path $env:SystemRoot 'System32\tar.exe') -a -c -f $Archive -C $Directory .
    }
    else
    {
        Push-Location $Directory
        try { zip -qr $Archive . }
        finally { Pop-Location }
    }
    if ($LASTEXITCODE -ne 0) { throw "cannot pack $Directory" }
}

function ConvertTo-FileUrl([string] $Path)
{
    # An explicitly absolute URI: PowerShell's [System.Uri] cast parses a Unix path such as /tmp/a.zip
    # as relative, whose AbsoluteUri is then silently empty.
    [System.Uri]::new((Resolve-Path $Path).Path, [System.UriKind]::Absolute).AbsoluteUri
}

# Runs cargo in `Directory` with `Environment` set and every other bundle variable unset, and
# returns its exit code and output.
function Invoke-Cargo([string] $Directory, [hashtable] $Environment, [string[]] $Arguments)
{
    $saved = @{}
    foreach ($name in $variables)
    {
        $saved[$name] = [Environment]::GetEnvironmentVariable($name)
        [Environment]::SetEnvironmentVariable($name, $null)
    }
    foreach ($name in $Environment.Keys) { [Environment]::SetEnvironmentVariable($name, $Environment[$name]) }
    Push-Location $Directory
    try
    {
        $output = & cargo @toolchainArgs @Arguments --target-dir $targetDir 2>&1 | Out-String
        [pscustomobject]@{ ExitCode = $LASTEXITCODE; Output = $output }
    }
    finally
    {
        Pop-Location
        foreach ($name in $variables) { [Environment]::SetEnvironmentVariable($name, $saved[$name]) }
    }
}

# Runs cargo for one scenario and records whether it succeeded, or failed with `ExpectedError`.
# By default it builds and runs the bundled-agent tests, which require the staged bundle.
function Test-Scenario
{
    param
    (
        [string] $Name,
        [hashtable] $Environment = @{},
        [string] $ExpectedError,
        [string] $ExpectedOutput,
        [string] $Directory = $crate,
        [string[]] $Arguments = @('test', '--locked', '--no-default-features', '--features', 'agent-bundled', '--lib', 'agent::bundled')
    )
    $run = Invoke-Cargo $Directory $Environment $Arguments
    $passed = if ($ExpectedError) { $run.ExitCode -ne 0 -and $run.Output.Contains($ExpectedError) } else { $run.ExitCode -eq 0 }
    if ($ExpectedOutput -and -not $run.Output.Contains($ExpectedOutput)) { $passed = $false }
    $log = Join-Path $WorkDir (($Name -replace '[^A-Za-z0-9]+', '-').Trim('-') + '.log')
    [System.IO.File]::WriteAllText($log, $run.Output)
    $result = if ($passed) { 'PASS' } else { 'FAIL' }
    $results.Add([pscustomobject]@{ Scenario = $Name; Result = $result; Log = $log })
    Write-Host "[$result] $Name"
    if (-not $passed)
    {
        Write-Host ((($run.Output -split "`n") | Select-Object -Last 15) -join "`n")
    }
}

if ($Pinned)
{
    Test-Scenario -Name 'pinned bundle from the checkout'
    # A copy of the crate without Git metadata or `.cargo_vcs_info.json`, with the working
    # tree's changes.
    $export = Join-Path $WorkDir 'export'
    Remove-Item -LiteralPath $export -Recurse -Force -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Path $export | Out-Null
    Get-ChildItem -LiteralPath $crate -Force | Where-Object Name -NotIn 'target', '.git' |
        Copy-Item -Destination $export -Recurse -Force
    Test-Scenario -Name 'pinned bundle from exported source' -Directory $export
}
else
{
    $good = Join-Path $WorkDir 'good'
    New-SyntheticBundle -Directory $good
    $goodZip = Join-Path $WorkDir 'good.zip'
    New-BundleArchive $good $goodZip
    $goodUrl = ConvertTo-FileUrl $goodZip

    Test-Scenario -Name 'directory override stages the bundle' -Environment @{ ACI_EDGE_SANDBOXES_AGENT_BUNDLE_DIR = $good }
    Test-Scenario -Name 'file URL override with digest stages the bundle' -Environment @{
        ACI_EDGE_SANDBOXES_AGENT_BUNDLE_URL = $goodUrl; ACI_EDGE_SANDBOXES_AGENT_BUNDLE_SHA256 = (Get-Sha256 $goodZip) }
    Test-Scenario -Name 'skip stages nothing' -Environment @{ ACI_EDGE_SANDBOXES_AGENT_BUNDLE = 'skip' }
    Test-Scenario -Name 'URL with another digest is refused' -ExpectedError 'SHA-256 mismatch' -Environment @{
        ACI_EDGE_SANDBOXES_AGENT_BUNDLE_URL = $goodUrl; ACI_EDGE_SANDBOXES_AGENT_BUNDLE_SHA256 = ('0' * 64) }
    Test-Scenario -Name 'directory and URL together are refused' -ExpectedError 'not both' -Environment @{
        ACI_EDGE_SANDBOXES_AGENT_BUNDLE_DIR = $good; ACI_EDGE_SANDBOXES_AGENT_BUNDLE_URL = $goodUrl }

    $tampered = Join-Path $WorkDir 'tampered'
    New-SyntheticBundle -Directory $tampered
    [System.IO.File]::AppendAllText((Join-Path $tampered 'guest/vmlinux'), 'changed')
    Test-Scenario -Name 'tampered file is refused' -ExpectedError 'SHA-256 mismatch' -Environment @{ ACI_EDGE_SANDBOXES_AGENT_BUNDLE_DIR = $tampered }

    $oldAbi = Join-Path $WorkDir 'abi1'
    New-SyntheticBundle -Directory $oldAbi -SandboxAbi 1
    Test-Scenario -Name 'another sandbox ABI is refused' -ExpectedError 'speaks sandbox ABI 1' -Environment @{ ACI_EDGE_SANDBOXES_AGENT_BUNDLE_DIR = $oldAbi }

    $arm = Join-Path $WorkDir 'arm64'
    New-SyntheticBundle -Directory $arm -Machine $(if ($isWindowsHost) { 0xaa64 } else { 183 })
    Test-Scenario -Name 'library for another architecture is refused' -ExpectedError 'is built for machine' -Environment @{ ACI_EDGE_SANDBOXES_AGENT_BUNDLE_DIR = $arm }

    # A Windows target other than x86_64 stages no x86_64 bundle as usable.
    $armTarget = 'aarch64-pc-windows-msvc'
    $listArgs = @('target', 'list', '--installed')
    if ($Toolchain) { $listArgs += '--toolchain', $Toolchain }
    if ($isWindowsHost -and ((& rustup @listArgs) -contains $armTarget))
    {
        $check = @('check', '--locked', '--no-default-features', '--features', 'agent-bundled', '--lib', '--target', $armTarget)
        Test-Scenario -Name "$armTarget without variables stages nothing" -Arguments $check -ExpectedOutput 'staging no bundle'
        Test-Scenario -Name "$armTarget refuses an x86_64 bundle" -Arguments $check -ExpectedError 'is built for machine' -Environment @{ ACI_EDGE_SANDBOXES_AGENT_BUNDLE_DIR = $good }
    }
    else
    {
        $results.Add([pscustomobject]@{ Scenario = "$armTarget scenarios"; Result = 'SKIPPED'; Log = "rustup target add $armTarget" })
        Write-Host "[SKIPPED] $armTarget scenarios: the target is not installed"
    }
}

$results | Format-Table -AutoSize | Out-String -Width 220 | Write-Host
Write-Host "Logs: $WorkDir"
if ($results.Result -contains 'FAIL') { exit 1 }
# Scenarios that expect cargo to fail leave its exit code in $LASTEXITCODE.
exit 0
