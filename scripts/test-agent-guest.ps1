# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

<#
.SYNOPSIS
Runs the functional acceptance checks of a real edge agent bundle through this crate's client.

.DESCRIPTION
A matching sandbox ABI shows that the bundle and the crate speak the same interface, not that
they work together. These checks run the crate's client against the bundle's agent library,
OpenVMM, kernel, and edge initramfs on a Windows x64 host with the Windows Hypervisor Platform:

1. The agent_lifecycle example, built with agent-bundled from the bundle: it pulls a public
   image reference (-TestImageRef, which has python3) through the bundle's direct-images,
   provisions, starts, runs a command, stops, and deprovisions a sandbox.
2. The ignored tests of tests/agent_guest.rs, one at a time and optimized: lifecycle and restart,
   executions (exit codes, timeouts, streaming, cancellation, concurrency), crash and reattach,
   registered images and references, host path mappings, network and loopback policies, and
   execution environments. They use a writable copy of the image that step 1 pulled unless
   -Image names one; the host-path, network, proxy, port-forwarding, and exec-environment tests
   need python3 in it.

The network tests listen on the host for the guest, so Windows Firewall asks once per test
executable. Set CARGO_TARGET_DIR to reuse an executable that the firewall already allows.

It fails if any step fails or an OpenVMM process outlives the run, and writes a summary with the
bundle's digests to WorkDir.

.EXAMPLE
pwsh scripts/test-agent-guest.ps1 -BundleDir C:\bundles\aci-edge-sandboxes

.EXAMPLE
pwsh scripts/test-agent-guest.ps1 -BundleZip aci-edge-sandboxes.zip -BundleZipSha256 <sha256>
#>
[CmdletBinding(DefaultParameterSetName = 'Directory')]
param
(
    # An extracted bundle (bundle-manifest.json at its root).
    [Parameter(Mandatory, ParameterSetName = 'Directory')]
    [string] $BundleDir,
    # A bundle ZIP, which is extracted after its SHA-256 is checked.
    [Parameter(Mandatory, ParameterSetName = 'Zip')]
    [string] $BundleZip,
    [Parameter(Mandatory, ParameterSetName = 'Zip')]
    [string] $BundleZipSha256,
    # A GPT disk image with python3 for the tests; a copy of the image that step 1 pulls when omitted.
    [string] $Image,
    # The image reference that the lifecycle example pulls for the tests.
    [string] $TestImageRef = 'mcr.microsoft.com/azurelinux/base/python:3',
    # The public image reference that the reference tests pull.
    [string] $ImageRef = 'mcr.microsoft.com/azurelinux/base/core:3.0',
    # Runs only the tests whose names contain this filter.
    [string] $Filter,
    # Rust toolchain, such as 1.95.0; the active one when omitted.
    [string] $Toolchain,
    # Work directory; a new temporary one when omitted. Kept after the run.
    [string] $WorkDir
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if (-not ($IsWindows -and [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -eq 'X64'))
{
    throw 'The bundle runs on Windows x64 with the Windows Hypervisor Platform.'
}
$crate = Join-Path (Split-Path $PSScriptRoot -Parent) 'aci-edge-sandbox'
if (-not $WorkDir)
{
    $WorkDir = Join-Path ([System.IO.Path]::GetTempPath()) ('aci-agent-guest-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
}
New-Item -ItemType Directory -Force -Path $WorkDir | Out-Null
$WorkDir = (Resolve-Path $WorkDir).Path

if ($PSCmdlet.ParameterSetName -eq 'Zip')
{
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $BundleZip).Hash.ToLowerInvariant()
    if ($actual -ne $BundleZipSha256.ToLowerInvariant())
    {
        throw "$BundleZip has SHA-256 $actual, not $BundleZipSha256."
    }
    $BundleDir = Join-Path $WorkDir 'bundle'
    if (Test-Path $BundleDir) { throw "$BundleDir already exists." }
    Expand-Archive -LiteralPath $BundleZip -DestinationPath $BundleDir
}
$BundleDir = (Resolve-Path $BundleDir).Path
$manifest = Get-Content -Raw (Join-Path $BundleDir 'bundle-manifest.json') | ConvertFrom-Json
$consumer = $manifest.consumer
$library = Join-Path $BundleDir $consumer.backendLibrary
if ((Get-FileHash -Algorithm SHA256 -LiteralPath $library).Hash.ToLowerInvariant() -ne $consumer.backendLibrarySha256)
{
    throw "$library does not match backendLibrarySha256 in bundle-manifest.json."
}

$cargo = @('cargo')
if ($Toolchain) { $cargo += "+$Toolchain" }
$target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $WorkDir 'target' }
$state = Join-Path $WorkDir 'state'
$results = [System.Collections.Generic.List[object]]::new()
function Invoke-Step([string] $Name, [string[]] $Arguments)
{
    $log = Join-Path $WorkDir "$Name.log"
    & $cargo[0] @($cargo | Select-Object -Skip 1) @Arguments *> $log
    $code = $LASTEXITCODE
    $results.Add([pscustomobject]@{ Check = $Name; Result = $(if ($code -eq 0) { 'PASS' } else { 'FAIL' }); Log = $log })
    Write-Host "[$($results[-1].Result)] $Name - $log"
    return $code -eq 0
}
function Get-OpenVmm { @(Get-Process -Name openvmm -ErrorAction SilentlyContinue | ForEach-Object Id) }

$before = Get-OpenVmm
$saved = @{}
$environment = [ordered]@{
    CARGO_TARGET_DIR = $target
    ACI_EDGE_SANDBOXES_AGENT_BUNDLE_DIR = $BundleDir
    EDGE_AGENT_TEST_LIBRARY = $library
    EDGE_AGENT_TEST_SHA256 = $consumer.backendLibrarySha256
    EDGE_AGENT_TEST_OPENVMM = Join-Path $BundleDir 'bin\openvmm.exe'
    EDGE_AGENT_TEST_KERNEL = Join-Path $BundleDir 'guest\vmlinux'
    EDGE_AGENT_TEST_INITRD = Join-Path $BundleDir 'guest\initramfs-edge.cpio.gz'
    EDGE_AGENT_TEST_BUNDLE = $BundleDir
    EDGE_AGENT_TEST_BUNDLE_SHA256 = $consumer.runtimeBundleManifestSha256
    EDGE_AGENT_TEST_IMAGE_REF = $ImageRef
}
foreach ($name in $environment.Keys)
{
    $saved[$name] = [Environment]::GetEnvironmentVariable($name)
    [Environment]::SetEnvironmentVariable($name, $environment[$name])
}
Push-Location $crate
try
{
    $null = Invoke-Step 'agent-lifecycle' @('run', '--release', '--locked', '--features', 'agent-bundled', '--example', 'agent_lifecycle',
        '--', '--image-ref', $TestImageRef, '--state-root', $state)
    if (-not $Image)
    {
        $pulled = Get-ChildItem (Join-Path $state 'images') -Filter '*.img' -ErrorAction SilentlyContinue |
            Sort-Object LastWriteTime -Descending | Select-Object -First 1
        if ($pulled)
        {
            # The tests change the registered file, and the agent stages images read-only.
            $Image = Join-Path $WorkDir 'test-image.img'
            Copy-Item -LiteralPath $pulled.FullName -Destination $Image -Force
            (Get-Item -LiteralPath $Image).IsReadOnly = $false
        }
    }
    if ($Image)
    {
        $env:EDGE_AGENT_TEST_IMAGE = (Resolve-Path $Image).Path
        $arguments = @('test', '--release', '--locked', '--features', 'agent-bundled', '--test', 'agent_guest', '--')
        if ($Filter) { $arguments += $Filter }
        $null = Invoke-Step 'agent-guest' ($arguments + @('--ignored', '--test-threads=1'))
    }
    else
    {
        $results.Add([pscustomobject]@{ Check = 'agent-guest'; Result = 'FAIL'; Log = 'no image: the lifecycle example pulled none and -Image is not set' })
    }
}
finally
{
    Pop-Location
    foreach ($name in $saved.Keys) { [Environment]::SetEnvironmentVariable($name, $saved[$name]) }
    Remove-Item Env:EDGE_AGENT_TEST_IMAGE -ErrorAction SilentlyContinue
}
$left = @(Get-OpenVmm | Where-Object { $_ -notin $before })
$results.Add([pscustomobject]@{ Check = 'no OpenVMM left running'; Result = $(if ($left.Count -eq 0) { 'PASS' } else { 'FAIL' }); Log = ($left -join ', ') })

$summary = [ordered]@{
    bundle = [ordered]@{
        directory = $BundleDir
        sandboxAbiVersion = $consumer.sandboxAbiVersion
        runtimeAbi = $consumer.runtimeAbi
        backendLibrarySha256 = $consumer.backendLibrarySha256
        runtimeBundleManifestSha256 = $consumer.runtimeBundleManifestSha256
        zipSha256 = $(if ($PSCmdlet.ParameterSetName -eq 'Zip') { $BundleZipSha256.ToLowerInvariant() } else { $null })
    }
    client = (git -C $crate rev-parse HEAD 2>$null)
    image = $Image
    imageSha256 = $(if ($Image) { (Get-FileHash -Algorithm SHA256 -LiteralPath $Image).Hash.ToLowerInvariant() } else { $null })
    testImageRef = $TestImageRef
    imageRef = $ImageRef
    results = $results
}
$summary | ConvertTo-Json -Depth 5 | Set-Content (Join-Path $WorkDir 'summary.json')
$results | Format-Table -AutoSize | Out-String | Write-Host
if (@($results | Where-Object Result -ne 'PASS').Count -gt 0)
{
    throw "Functional acceptance failed; logs are in $WorkDir."
}
