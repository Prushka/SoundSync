# Per-user SoundSync install (no admin, except the optional firewall rule).
#
#   install-windows.ps1                 copy soundsync.exe (next to this script) and the
#                                       uninstaller to %LOCALAPPDATA%\Programs\SoundSync, create
#                                       Start Menu + Startup shortcuts, and allow inbound UDP in
#                                       Windows Firewall (UAC)
#   install-windows.ps1 -NoFirewall     skip the elevated firewall step
#   install-windows.ps1 -Start          open SoundSync when done
#   install-windows.ps1 -Remove         same as uninstall-windows.ps1 (settings and logs are kept)
#
# Running it again updates an existing install: a running SoundSync is quit, replaced and
# started again. Shortcuts that point at another program are never replaced or removed.
[CmdletBinding()]
param(
    [switch]$Remove,
    [switch]$NoStartup,
    [switch]$NoStartMenu,
    [switch]$NoFirewall,
    [switch]$Start,
    [string]$SourceExe,
    [int]$Port = 47810,
    # Testing: put the install and both shortcut folders under this directory instead.
    [string]$InstallRoot
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if ($InstallRoot) {
    $InstallDir = Join-Path $InstallRoot 'Programs\SoundSync'
    $StartupDir = Join-Path $InstallRoot 'Startup'
    $ProgramsDir = Join-Path $InstallRoot 'Start Menu'
} else {
    $InstallDir = Join-Path $env:LOCALAPPDATA 'Programs\SoundSync'
    $StartupDir = [Environment]::GetFolderPath('Startup')
    $ProgramsDir = [Environment]::GetFolderPath('Programs')
}
$AppPath = Join-Path $InstallDir 'soundsync.exe'
if (-not $SourceExe) { $SourceExe = Join-Path $PSScriptRoot 'soundsync.exe' }
$StartupLnk = Join-Path $StartupDir 'SoundSync.lnk'
$MenuLnk = Join-Path $ProgramsDir 'SoundSync.lnk'
$RuleName = 'SoundSync (UDP in)'
$shell = New-Object -ComObject WScript.Shell

function Test-Owned([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) { return $true }
    $target = $shell.CreateShortcut($Path).TargetPath
    return ($target -eq $AppPath) -or -not (Test-Path -LiteralPath $target)
}

function Assert-OwnedShortcut([string]$Path) {
    if (-not (Test-Owned $Path)) { throw "Another application owns '$Path'; refusing to replace it." }
}

function Write-Shortcut([string]$Path, [string]$Arguments) {
    Assert-OwnedShortcut $Path
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $Path) | Out-Null
    $s = $shell.CreateShortcut($Path)
    $s.TargetPath = $AppPath
    $s.Arguments = $Arguments
    $s.WorkingDirectory = $InstallDir
    $s.Description = 'SoundSync: synchronized multi-room audio'
    $s.IconLocation = "$AppPath,0"
    $s.Save()
    Write-Host "Installed $Path"
}

function Invoke-Elevated([string]$Script) {
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($Script))
    $p = Start-Process powershell -Verb RunAs -Wait -PassThru -ArgumentList @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-EncodedCommand', $encoded)
    if ($p.ExitCode -ne 0) { throw "Elevated firewall step failed (exit $($p.ExitCode))." }
}

# Quits the installed SoundSync if it runs; returns whether it was running.
function Stop-Installed {
    $running = @(Get-Process -Name soundsync -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $AppPath })
    if (-not $running) { return $false }
    & $AppPath --quit | Out-Null
    $running | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
    if (@($running | Where-Object { -not $_.HasExited })) { throw 'SoundSync did not quit; quit it from the tray and run this again.' }
    return $true
}

try {
    if ($Remove) {
        $uninstall = Join-Path $PSScriptRoot 'uninstall-windows.ps1'
        if (-not (Test-Path -LiteralPath $uninstall)) { throw "uninstall-windows.ps1 not found next to this script." }
        $options = @{ NoFirewall = $NoFirewall }
        if ($InstallRoot) { $options.InstallRoot = $InstallRoot }
        & $uninstall @options
        return
    }

    if (-not (Test-Path -LiteralPath $SourceExe)) { throw "soundsync.exe not found at '$SourceExe' (use -SourceExe)." }
    Assert-OwnedShortcut $StartupLnk
    Assert-OwnedShortcut $MenuLnk
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    $wasRunning = $false
    if ((Resolve-Path -LiteralPath $SourceExe).Path -ne $AppPath) {
        $wasRunning = Stop-Installed
        Copy-Item -LiteralPath $SourceExe -Destination $AppPath -Force
        Write-Host "Installed $AppPath"
    }
    $uninstaller = Join-Path $PSScriptRoot 'uninstall-windows.ps1'
    if ((Test-Path -LiteralPath $uninstaller) -and $PSScriptRoot -ne $InstallDir) {
        Copy-Item -LiteralPath $uninstaller -Destination $InstallDir -Force
    }
    if (-not $NoStartMenu) { Write-Shortcut $MenuLnk '--show' }
    if (-not $NoStartup) { Write-Shortcut $StartupLnk '' }
    if (-not $NoFirewall) {
        Write-Host 'Allowing SoundSync through Windows Firewall (UDP; private and domain networks)...'
        Invoke-Elevated ("Remove-NetFirewallRule -DisplayName '$RuleName' -ErrorAction SilentlyContinue; " +
            "New-NetFirewallRule -DisplayName '$RuleName' -Direction Inbound -Program '$AppPath' -Protocol UDP " +
            "-LocalPort $Port,5353 -Profile Private,Domain -Action Allow | Out-Null")
    }
    if ($Start) {
        Start-Process -FilePath $AppPath -ArgumentList '--show' -WorkingDirectory $InstallDir
    } elseif ($wasRunning) {
        Start-Process -FilePath $AppPath -WorkingDirectory $InstallDir # back in the tray after an update
        Write-Host 'Restarted SoundSync'
    }
    Write-Host "SoundSync installed to $InstallDir"
} finally {
    [void][Runtime.InteropServices.Marshal]::FinalReleaseComObject($shell)
}
