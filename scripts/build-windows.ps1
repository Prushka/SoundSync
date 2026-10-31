# Builds the release exe and stages dist\windows\SoundSync\ plus dist\SoundSync-windows-x64.zip
# (soundsync.exe, install-windows.ps1, uninstall-windows.ps1, README.md) for copying to other PCs.
[CmdletBinding()]
param([switch]$SkipTests)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$Root = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$Dist = Join-Path $Root 'dist\windows\SoundSync'
$Exe = Join-Path $Dist 'soundsync.exe'
$Zip = Join-Path $Root 'dist\SoundSync-windows-x64.zip'

$running = Get-Process -Name soundsync -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $Exe }
if ($running) { throw "Quit SoundSync ($Exe) before building: soundsync.exe --quit" }

Push-Location $Root
try {
    if (-not $SkipTests) {
        cargo test --workspace
        if ($LASTEXITCODE) { throw 'Tests failed.' }
    }
    cargo build --release -p soundsync
    if ($LASTEXITCODE) { throw 'Build failed.' }
} finally { Pop-Location }

New-Item -ItemType Directory -Force -Path $Dist | Out-Null
Copy-Item -LiteralPath (Join-Path $Root 'target\release\soundsync.exe') -Destination $Exe -Force
foreach ($script in 'install-windows.ps1', 'uninstall-windows.ps1') {
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot $script) -Destination $Dist -Force
}
$readme = Join-Path $Root 'README.md'
if (Test-Path -LiteralPath $readme) { Copy-Item -LiteralPath $readme -Destination $Dist -Force }
if (Test-Path -LiteralPath $Zip) { Remove-Item -LiteralPath $Zip -Force }
Compress-Archive -Path (Join-Path $Dist '*') -DestinationPath $Zip
Write-Host "Built $Zip"
