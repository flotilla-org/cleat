param(
    [string]$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
)

$ErrorActionPreference = 'Stop'

$ToolchainFile = Join-Path $RepoRoot 'tools\ghostty-toolchain.toml'
$SourceDir = Join-Path $RepoRoot '.tools\ghostty-src'
$InstallDir = Join-Path $RepoRoot '.tools\ghostty-install'

function Get-TomlValue {
    param(
        [string]$Section,
        [string]$Key,
        [string]$Path
    )

    $current = ''
    foreach ($line in Get-Content $Path) {
        $trimmed = ($line -replace '#.*$', '').Trim()
        if ($trimmed -match '^\[(.+)\]$') {
            $current = $Matches[1]
            continue
        }
        if ($current -eq $Section -and $trimmed -match "^$([regex]::Escape($Key))\s*=\s*`"(.+)`"\s*$") {
            return $Matches[1]
        }
    }

    throw "Missing [$Section].$Key in $Path"
}

$requiredZigVersion = Get-TomlValue -Section 'zig' -Key 'version' -Path $ToolchainFile
$zig = (Get-Command zig -ErrorAction SilentlyContinue).Source
if ($null -eq $zig -or ((& $zig version).Trim()) -ne $requiredZigVersion) {
    $zig = (& (Join-Path $RepoRoot 'tools\install-zig.ps1') -Version $requiredZigVersion -RepoRoot $RepoRoot | Select-Object -Last 1).Trim()
}

$zigVersion = (& $zig version).Trim()
if ($zigVersion -ne $requiredZigVersion) {
    throw "Expected zig version $requiredZigVersion, found $zigVersion"
}

$ghosttyRepo = Get-TomlValue -Section 'ghostty' -Key 'repo' -Path $ToolchainFile
$ghosttyRef = Get-TomlValue -Section 'ghostty' -Key 'ref' -Path $ToolchainFile
$buildStep = Get-TomlValue -Section 'ghostty' -Key 'build_step' -Path $ToolchainFile
$buildArgs = @()
if ($buildStep.Trim().Length -gt 0) {
    $buildArgs = $buildStep -split '\s+'
}

New-Item -ItemType Directory -Force -Path (Join-Path $RepoRoot '.tools') | Out-Null

if (!(Test-Path (Join-Path $SourceDir '.git'))) {
    git init $SourceDir
    if ($LASTEXITCODE -ne 0) { throw 'Ghostty git init failed' }
    git -C $SourceDir remote add origin $ghosttyRepo
} else {
    git -C $SourceDir remote set-url origin $ghosttyRepo
}
if ($LASTEXITCODE -ne 0) { throw 'Ghostty remote setup failed' }
git -C $SourceDir fetch --depth=1 origin $ghosttyRef
if ($LASTEXITCODE -ne 0) { throw 'Ghostty fetch failed' }
git -C $SourceDir checkout --detach --force $ghosttyRef
if ($LASTEXITCODE -ne 0) { throw 'Ghostty checkout failed' }
git -C $SourceDir reset --hard $ghosttyRef
if ($LASTEXITCODE -ne 0) { throw 'Ghostty reset failed' }

if (Test-Path $InstallDir) {
    Remove-Item -Recurse -Force $InstallDir
}
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null

Push-Location $SourceDir
try {
    & $zig build @buildArgs --prefix $InstallDir
    if ($LASTEXITCODE -ne 0) { throw "Ghostty build failed with exit code $LASTEXITCODE" }
} finally {
    Pop-Location
}

$header = Join-Path $InstallDir 'include\ghostty\vt.h'
if (!(Test-Path $header)) {
    throw "Missing Ghostty VT header at $header"
}

$staticLib = Join-Path $InstallDir 'lib\ghostty-vt-static.lib'
$sharedLib = Join-Path $InstallDir 'bin\ghostty-vt.dll'
$sharedLibAlt = Join-Path $InstallDir 'lib\ghostty-vt.dll'
$importLib = Join-Path $InstallDir 'lib\ghostty-vt.lib'

if (!(Test-Path $staticLib) -and !((Test-Path $importLib) -and ((Test-Path $sharedLib) -or (Test-Path $sharedLibAlt)))) {
    throw "Missing Ghostty VT library; expected $staticLib or $importLib plus ghostty-vt.dll"
}
