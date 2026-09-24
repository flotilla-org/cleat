param(
    [string]$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
)

# Fetch the pinned Microsoft.Windows.Console.ConPTY package from nuget.org,
# verify its SHA-256 and extract it to .tools/conpty/<version>, where build.rs
# stages conpty.dll and OpenConsole.exe beside the Windows executables.
# Idempotent: a verified extraction of the pinned version is left alone.

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$PinFile = Join-Path $RepoRoot 'tools\conpty.toml'

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

$package = Get-TomlValue -Section 'conpty' -Key 'package' -Path $PinFile
$version = Get-TomlValue -Section 'conpty' -Key 'version' -Path $PinFile
$expected = (Get-TomlValue -Section 'conpty' -Key 'sha256' -Path $PinFile).ToLowerInvariant()

$toolsDir = Join-Path $RepoRoot '.tools'
$conptyDir = Join-Path $toolsDir 'conpty'
$installDir = Join-Path $conptyDir $version
$marker = Join-Path $installDir '.sha256'

if ((Test-Path $marker) -and ((Get-Content $marker -Raw).Trim() -eq $expected)) {
    Write-Output $installDir
    return
}

New-Item -ItemType Directory -Force -Path $conptyDir | Out-Null
$id = $package.ToLowerInvariant()
$url = "https://api.nuget.org/v3-flatcontainer/$id/$version/$id.$version.nupkg"
# Expand-Archive in Windows PowerShell only accepts a .zip extension.
$archive = Join-Path $conptyDir "$id.$version.zip"
Write-Host "Downloading $url"
Invoke-WebRequest -Uri $url -OutFile $archive -UseBasicParsing

$actual = (Get-FileHash -Algorithm SHA256 $archive).Hash.ToLowerInvariant()
if ($actual -ne $expected) {
    Remove-Item -Force $archive
    throw "$package $version checksum mismatch: expected $expected, got $actual"
}

if (Test-Path $installDir) {
    Remove-Item -Recurse -Force $installDir
}
Expand-Archive -Path $archive -DestinationPath $installDir -Force
Remove-Item -Force $archive

foreach ($arch in @('x64', 'arm64', 'x86')) {
    foreach ($file in @("runtimes\win-$arch\native\conpty.dll", "build\native\runtimes\$arch\OpenConsole.exe")) {
        if (!(Test-Path (Join-Path $installDir $file))) {
            throw "$package $version is missing $file"
        }
    }
}

Set-Content -Path $marker -Value $expected -Encoding ascii
Write-Output $installDir
