# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

<#
.SYNOPSIS
Builds a consumer of aci_edge_sandboxes outside this repository, as an SDK would.

.DESCRIPTION
Creates a workspace with an SDK-like library, `sdk_wrapper`, that depends on aci_edge_sandboxes
optionally, on x86_64 Windows only, with default features disabled and `agent-bundled` enabled,
behind its `microvm` feature, and an application that uses it. It checks that:

1. without `microvm`, aci_edge_sandboxes is not built and the application reports the backend
   disabled;
2. with `microvm`, the wrapper's build script receives the staged bundle through
   DEP_ACI_EDGE_SANDBOXES_AGENT_ARTIFACTS_DIR;
3. the application finds the bundle after it and the bundle are copied together to another
   directory, and reports the backend unavailable without the bundle.

With -ArchiveDir, the dependency is satisfied by the archives that Invoke-CratePackage.ps1
prepared, through [patch.crates-io]: LOCAL STAGING evidence that the archives build, not that
crates.io resolves them. Without it, the dependency comes from crates.io only, with a new
CARGO_HOME, and the lock file must name crates.io as the source of aci_edge_sandboxes,
aci_edge_sandboxes_model, and nvx: the post-publication check.

The bundle variables of the build script (ACI_EDGE_SANDBOXES_AGENT_BUNDLE_DIR and so on) pass
through; without them the build uses the release asset that agent-bundle.json pins.

.EXAMPLE
pwsh scripts/release/Test-CrateConsumer.ps1 -ArchiveDir out/crates

.EXAMPLE
pwsh scripts/release/Test-CrateConsumer.ps1 -Version 0.1.0 -Open
#>
[CmdletBinding()]
param
(
    # Version of aci_edge_sandboxes; read from -ArchiveDir's release-manifest.json when omitted.
    [string] $Version,
    # Archives from Invoke-CratePackage.ps1, for a local staging check before publication.
    [string] $ArchiveDir,
    # Work directory; a new temporary one when omitted. Kept after the run.
    [string] $WorkDir,
    # Also opens the agent host through the relocated bundle, which loads the agent library and
    # needs a real bundle on a Windows x64 host.
    [switch] $Open,
    # Rust toolchain, such as 1.95.0; the active one when omitted.
    [string] $Toolchain
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if (-not $WorkDir)
{
    $WorkDir = Join-Path ([System.IO.Path]::GetTempPath()) ('aci-consumer-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
}
New-Item -ItemType Directory -Force -Path $WorkDir | Out-Null
$WorkDir = (Resolve-Path $WorkDir).Path
$toolchainArgs = @()
if ($Toolchain) { $toolchainArgs += "+$Toolchain" }
$local = [bool]$ArchiveDir
$mode = if ($local) { 'LOCAL STAGING (archives through [patch.crates-io]; not crates.io evidence)' } else { 'REGISTRY (crates.io only, new CARGO_HOME)' }
$results = [System.Collections.Generic.List[object]]::new()

function Add-Result([string] $Check, [bool] $Passed, [string] $Detail)
{
    $result = if ($Passed) { 'PASS' } else { 'FAIL' }
    $results.Add([pscustomobject]@{ Check = $Check; Result = $result; Detail = $Detail })
    Write-Host "[$result] $Check $(if ($Detail) { "- $Detail" })"
}

function Invoke-Cargo([string] $Log, [string[]] $Arguments)
{
    Push-Location (Join-Path $WorkDir 'consumer')
    try
    {
        $output = & cargo @toolchainArgs @Arguments 2>&1 | Out-String
        [System.IO.File]::WriteAllText((Join-Path $WorkDir $Log), $output)
        [pscustomobject]@{ ExitCode = $LASTEXITCODE; Output = $output }
    }
    finally { Pop-Location }
}

# The dependency's source: unpacked archives for a local check, crates.io otherwise.
$patch = ''
if ($local)
{
    $ArchiveDir = (Resolve-Path $ArchiveDir).Path
    $release = Get-Content -Raw (Join-Path $ArchiveDir 'release-manifest.json') | ConvertFrom-Json
    if (-not $Version) { $Version = $release.version }
    $unpacked = Join-Path $WorkDir 'unpacked'
    Remove-Item -LiteralPath $unpacked -Recurse -Force -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Path $unpacked | Out-Null
    $patch = "`n[patch.crates-io]`n"
    foreach ($archive in $release.archives)
    {
        tar -xzf (Join-Path $ArchiveDir $archive.file) -C $unpacked
        if ($LASTEXITCODE -ne 0) { throw "cannot unpack $($archive.file)" }
        $path = (Join-Path $unpacked "$($archive.crate)-$Version").Replace('\', '/')
        $patch += "$($archive.crate) = { path = `"$path`" }`n"
    }
}
elseif (-not $Version)
{
    throw 'set -Version, or -ArchiveDir for a local check'
}
else
{
    $env:CARGO_HOME = Join-Path $WorkDir 'cargo-home'
    Remove-Item -LiteralPath $env:CARGO_HOME -Recurse -Force -ErrorAction SilentlyContinue
}

$consumer = Join-Path $WorkDir 'consumer'
Remove-Item -LiteralPath $consumer -Recurse -Force -ErrorAction SilentlyContinue
foreach ($directory in 'wrapper/src', 'app/src') { New-Item -ItemType Directory -Force -Path (Join-Path $consumer $directory) | Out-Null }
@"
[workspace]
members = ["wrapper", "app"]
resolver = "3"
$patch
"@ | Set-Content -Encoding utf8NoBOM (Join-Path $consumer 'Cargo.toml')
@"
[package]
name = "sdk_wrapper"
version = "0.0.0"
edition = "2024"
publish = false

[features]
microvm = ["dep:aci_edge_sandboxes"]

[target.'cfg(all(windows, target_arch = "x86_64"))'.dependencies]
aci_edge_sandboxes = { version = "=$Version", optional = true, default-features = false, features = ["agent-bundled"] }
"@ | Set-Content -Encoding utf8NoBOM (Join-Path $consumer 'wrapper/Cargo.toml')
@'
fn main() {
    // Cargo gives the metadata of a `links` crate only to its direct dependents.
    println!("cargo::rerun-if-env-changed=DEP_ACI_EDGE_SANDBOXES_AGENT_ARTIFACTS_DIR");
    if let Some(directory) = std::env::var_os("DEP_ACI_EDGE_SANDBOXES_AGENT_ARTIFACTS_DIR") {
        println!(
            "cargo::rustc-env=SDK_WRAPPER_BUNDLE_SOURCE={}",
            std::path::Path::new(&directory).display()
        );
    }
}
'@ | Set-Content -Encoding utf8NoBOM (Join-Path $consumer 'wrapper/build.rs')
@'
/// Where the build staged the agent bundle, which an SDK copies beside its application.
pub fn bundle_source() -> Option<&'static str> {
    option_env!("SDK_WRAPPER_BUNDLE_SOURCE")
}

#[cfg(all(feature = "microvm", windows, target_arch = "x86_64"))]
pub fn microvm(open_state_root: Option<&str>) -> Result<String, String> {
    use aci_edge_sandboxes::AciEdgeSandbox;
    use aci_edge_sandboxes::agent::{AgentConfig, BundledAgent};
    let bundle = BundledAgent::discover().map_err(|error| error.to_string())?;
    if let Some(state_root) = open_state_root {
        let client = AciEdgeSandbox::agent(AgentConfig::bundled(state_root).map_err(|e| e.to_string())?)
            .map_err(|error| error.to_string())?;
        client.probe().map_err(|error| error.to_string())?;
    }
    Ok(bundle.directory().display().to_string())
}

#[cfg(not(all(feature = "microvm", windows, target_arch = "x86_64")))]
pub fn microvm(_open_state_root: Option<&str>) -> Result<String, String> {
    Err("the microvm backend is disabled".to_owned())
}
'@ | Set-Content -Encoding utf8NoBOM (Join-Path $consumer 'wrapper/src/lib.rs')
@'
[package]
name = "consumer_app"
version = "0.0.0"
edition = "2024"
publish = false

[features]
microvm = ["sdk_wrapper/microvm"]

[dependencies]
sdk_wrapper = { path = "../wrapper" }
'@ | Set-Content -Encoding utf8NoBOM (Join-Path $consumer 'app/Cargo.toml')
@'
fn main() {
    let open = std::env::args().nth(1);
    println!("bundle_source={}", sdk_wrapper::bundle_source().unwrap_or(""));
    match sdk_wrapper::microvm(open.as_deref()) {
        Ok(directory) => println!("microvm=available {directory}"),
        Err(error) => println!("microvm=unavailable {error}"),
    }
}
'@ | Set-Content -Encoding utf8NoBOM (Join-Path $consumer 'app/src/main.rs')

Write-Host "Mode: $mode"
$exe = if ([System.Environment]::OSVersion.Platform -eq [System.PlatformID]::Win32NT) { '.exe' } else { '' }
$application = Join-Path $consumer "target/debug/consumer_app$exe"

# 1. The optional dependency disabled.
$off = Invoke-Cargo 'build-microvm-off.log' @('build', '-p', 'consumer_app')
Add-Result 'build without microvm' ($off.ExitCode -eq 0) 'build-microvm-off.log'
Add-Result 'aci_edge_sandboxes not built without microvm' (-not ($off.Output -match 'Compiling aci_edge_sandboxes')) ''
$run = (& $application | Out-String)
Add-Result 'application reports the backend disabled' ($run -match 'microvm=unavailable the microvm backend is disabled') $run.Trim()

# 2. The optional dependency enabled.
$on = Invoke-Cargo 'build-microvm-on.log' @('build', '-p', 'consumer_app', '--features', 'microvm')
Add-Result 'build with microvm' ($on.ExitCode -eq 0) 'build-microvm-on.log'
if ($on.ExitCode -ne 0)
{
    Write-Host ((($on.Output -split "`n") | Where-Object { $_ -match 'panicked|agent bundled artifacts|error' } | Select-Object -First 6) -join "`n")
}
else
{
    Add-Result 'aci_edge_sandboxes built with microvm' ($on.Output -match 'Compiling aci_edge_sandboxes ') ''
    $run = (& $application | Out-String)
    $source = ([regex]::Match($run, 'bundle_source=(.*)')).Groups[1].Value.Trim()
    Add-Result 'direct dependent received DEP_ACI_EDGE_SANDBOXES_AGENT_ARTIFACTS_DIR' ($source -and (Test-Path (Join-Path $source 'SOURCE-MANIFEST.json'))) $source

    # 3. The application and its bundle, copied together elsewhere.
    $relocated = Join-Path $WorkDir 'relocated/bin'
    Remove-Item -LiteralPath (Split-Path $relocated -Parent) -Recurse -Force -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force -Path $relocated | Out-Null
    Copy-Item -LiteralPath $application -Destination $relocated
    $relocatedApp = Join-Path $relocated (Split-Path $application -Leaf)
    $bundle = Join-Path $relocated 'aci-edge-sandboxes'
    Copy-Item -LiteralPath $source -Destination $bundle -Recurse
    $arguments = @()
    if ($Open) { $arguments += (Join-Path $WorkDir 'state') }
    $run = (& $relocatedApp @arguments | Out-String)
    Add-Result 'relocated application finds its bundle' ($run -match [regex]::Escape("microvm=available $bundle")) $run.Trim()
    Remove-Item -LiteralPath $bundle -Recurse -Force
    $run = (& $relocatedApp | Out-String)
    Add-Result 'relocated application without its bundle reports it unavailable' ($run -match 'microvm=unavailable .*backend_unavailable|microvm=unavailable .*no agent bundle') $run.Trim()
}

# The resolved sources.
$lock = Get-Content -Raw (Join-Path $consumer 'Cargo.lock')
foreach ($crate in 'aci_edge_sandboxes', 'aci_edge_sandboxes_model', 'nvx')
{
    $entry = [regex]::Match($lock, "(?s)\[\[package\]\]\s*name = `"$crate`"\s*version = `"([^`"]+)`"\s*(source = `"([^`"]+)`")?")
    $resolved = if ($entry.Success) { "$($entry.Groups[1].Value) from $(if ($entry.Groups[3].Success) { $entry.Groups[3].Value } else { 'a path (local staging)' })" } else { 'absent' }
    $fromRegistry = $entry.Success -and $entry.Groups[3].Value -eq 'registry+https://github.com/rust-lang/crates.io-index'
    if ($local)
    {
        $results.Add([pscustomobject]@{ Check = "lock file: $crate"; Result = 'INFO'; Detail = $resolved })
        Write-Host "[INFO] lock file: $crate $resolved"
    }
    else
    {
        Add-Result "lock file: $crate from crates.io" $fromRegistry $resolved
    }
}
if (-not $local) { Add-Result 'no [patch], path, or git source' (-not ($lock -match 'source = "(git|path)\+')) '' }

$results | Format-Table -AutoSize -Wrap | Out-String -Width 220 | Write-Host
Write-Host "Mode: $mode"
Write-Host "Logs: $WorkDir"
if ($results.Result -contains 'FAIL') { exit 1 }
