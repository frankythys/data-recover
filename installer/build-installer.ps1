# Builds the release exe and the DiskTools installer in target\installer.
# Needs Inno Setup 6: winget install JRSoftware.InnoSetup

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
Set-Location $root

cargo build --release
if ($LASTEXITCODE -ne 0) { throw 'cargo build --release mislukt' }

$version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
$iscc = @(
    "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe",
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) { throw 'Inno Setup 6 niet gevonden. Installeer het met: winget install JRSoftware.InnoSetup' }

& $iscc "/DAppVersion=$version" (Join-Path $PSScriptRoot 'disktools.iss')
if ($LASTEXITCODE -ne 0) { throw 'Inno Setup gaf een fout' }
Write-Host "Klaar: target\installer\DiskTools-Setup-$version.exe"
