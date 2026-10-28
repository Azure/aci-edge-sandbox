# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

<#
.SYNOPSIS
Reports whether crates.io's index lists a version of a crate, optionally waiting for it.

.DESCRIPTION
Reads the crate's entry in the crates.io sparse index. Prints `absent` when the version is not
listed, and `published` when it is listed with ExpectedSha256; fails when it is listed with
another checksum or is yanked. With -Wait, polls until the version is listed or TimeoutMinutes
pass, for the index to catch up after an upload.

.EXAMPLE
pwsh scripts/release/Get-CratesIoVersion.ps1 -Crate aci_edge_sandboxes_model -Version 0.1.0 -ExpectedSha256 <sha256>
#>
[CmdletBinding()]
param
(
    [Parameter(Mandatory)] [string] $Crate,
    [Parameter(Mandatory)] [string] $Version,
    [Parameter(Mandatory)] [string] $ExpectedSha256,
    [switch] $Wait,
    [int] $TimeoutMinutes = 15
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$name = $Crate.ToLowerInvariant()
$path = switch ($name.Length)
{
    1 { "1/$name" }
    2 { "2/$name" }
    3 { "3/$($name.Substring(0, 1))/$name" }
    default { "$($name.Substring(0, 2))/$($name.Substring(2, 2))/$name" }
}
$url = "https://index.crates.io/$path"
$deadline = (Get-Date).AddMinutes($TimeoutMinutes)

while ($true)
{
    $response = Invoke-WebRequest -Uri $url -SkipHttpErrorCheck -Headers @{ 'User-Agent' = 'aci-edge-sandbox-release (https://github.com/Azure/aci-edge-sandbox)'; 'Cache-Control' = 'no-cache' }
    if ($response.StatusCode -notin 200, 404) { throw "$url answered $($response.StatusCode)" }
    $entry = $null
    if ($response.StatusCode -eq 200)
    {
        $entry = $response.Content -split "`n" | Where-Object { $_ } | ForEach-Object { $_ | ConvertFrom-Json } |
            Where-Object vers -EQ $Version | Select-Object -First 1
    }
    if ($entry)
    {
        if ($entry.cksum -ne $ExpectedSha256.ToLowerInvariant())
        {
            throw "crates.io lists $Crate $Version with checksum $($entry.cksum), not $ExpectedSha256"
        }
        if ($entry.yanked) { throw "crates.io lists $Crate $Version as yanked" }
        Write-Output 'published'
        return
    }
    if (-not $Wait)
    {
        Write-Output 'absent'
        return
    }
    if ((Get-Date) -gt $deadline) { throw "crates.io's index did not list $Crate $Version within $TimeoutMinutes minutes" }
    Write-Host "waiting for crates.io's index to list $Crate $Version"
    Start-Sleep -Seconds 20
}
