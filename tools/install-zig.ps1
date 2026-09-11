param(
    [string]$Version = '0.16.0',
    [string]$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
)

$ErrorActionPreference = 'Stop'

$arch = switch ($env:PROCESSOR_ARCHITECTURE) {
    'ARM64' { 'aarch64' }
    default { 'x86_64' }
}

$toolsDir = Join-Path $RepoRoot '.tools'
$installDir = Join-Path $toolsDir "zig-$Version"
$zipName = "zig-$arch-windows-$Version.zip"
$zipPath = Join-Path $toolsDir $zipName
$url = "https://ziglang.org/download/$Version/$zipName"

New-Item -ItemType Directory -Force -Path $toolsDir | Out-Null

if (!(Test-Path (Join-Path $installDir 'zig.exe'))) {
    if (!(Test-Path $zipPath)) {
        Write-Host "Downloading $url"
        Invoke-WebRequest -Uri $url -OutFile $zipPath
    }

    $checksumKey = "$arch-windows"
    $checksumLine = Get-Content (Join-Path $RepoRoot 'tools\ghostty-toolchain.toml') |
        Where-Object { $_ -match "^$checksumKey\s*=" } | Select-Object -First 1
    if (!$checksumLine -or $checksumLine -notmatch '"([a-f0-9]{64})"') {
        throw "Missing Zig checksum for $checksumKey"
    }
    $expectedChecksum = $Matches[1]
    if ((Get-FileHash -Algorithm SHA256 $zipPath).Hash.ToLowerInvariant() -ne $expectedChecksum) {
        throw "Zig checksum mismatch for $zipPath"
    }

    $extractDir = Join-Path $toolsDir "zig-extract-$Version"
    if (Test-Path $extractDir) {
        Remove-Item -Recurse -Force $extractDir
    }
    New-Item -ItemType Directory -Force -Path $extractDir | Out-Null
    Expand-Archive -Path $zipPath -DestinationPath $extractDir -Force

    $expanded = Get-ChildItem -Path $extractDir -Directory | Select-Object -First 1
    if ($null -eq $expanded) {
        throw "Unable to find expanded Zig directory in $extractDir"
    }

    if (Test-Path $installDir) {
        Remove-Item -Recurse -Force $installDir
    }
    Move-Item -Path $expanded.FullName -Destination $installDir
    Remove-Item -Recurse -Force $extractDir
}

$zig = Join-Path $installDir 'zig.exe'
$actualVersion = (& $zig version).Trim()
if ($actualVersion -ne $Version) {
    throw "Expected Zig $Version at $zig, found $actualVersion"
}

Write-Output $zig
