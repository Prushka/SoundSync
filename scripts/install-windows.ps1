# Per-user SoundSync install (no admin, except the optional firewall rule).
#
#   install-windows.ps1                 install next to-script soundsync.exe, Start Menu + Startup
#                                       shortcuts, and allow inbound UDP in Windows Firewall (UAC)
#   install-windows.ps1 -NoFirewall     skip the elevated firewall step
#   install-windows.ps1 -Remove         remove shortcuts, the installed exe and the firewall rule
#                                       (settings and logs are kept)
#
# Shortcuts that point at another program are never replaced or removed.
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

function Stop-Installed {
    if (Test-Path -LiteralPath $AppPath) {
        $running = Get-Process -Name soundsync -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $AppPath }
        if ($running) {
            & $AppPath --quit
            $running | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
        }
    }
}

try {
    if ($Remove) {
        foreach ($lnk in @($StartupLnk, $MenuLnk)) {
            if ((Test-Path -LiteralPath $lnk) -and (Test-Owned $lnk)) {
                Remove-Item -LiteralPath $lnk -Force
                Write-Host "Removed $lnk"
            }
        }
        Stop-Installed
        if (Test-Path -LiteralPath $AppPath) {
            Remove-Item -LiteralPath $AppPath -Force
            Write-Host "Removed $AppPath"
        }
        if ((Test-Path -LiteralPath $InstallDir) -and -not (Get-ChildItem -LiteralPath $InstallDir)) {
            Remove-Item -LiteralPath $InstallDir
        }
        if (-not $NoFirewall) {
            Invoke-Elevated "Remove-NetFirewallRule -DisplayName '$RuleName' -ErrorAction SilentlyContinue; exit 0"
            Write-Host "Removed firewall rule '$RuleName'"
        }
        Write-Host 'Settings and logs were kept (%APPDATA%\SoundSync, %LOCALAPPDATA%\SoundSync).'
        return
    }

    if (-not (Test-Path -LiteralPath $SourceExe)) { throw "soundsync.exe not found at '$SourceExe' (use -SourceExe)." }
    Assert-OwnedShortcut $StartupLnk
    Assert-OwnedShortcut $MenuLnk
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    if ((Resolve-Path -LiteralPath $SourceExe).Path -ne $AppPath) {
        Stop-Installed
        Copy-Item -LiteralPath $SourceExe -Destination $AppPath -Force
        Write-Host "Installed $AppPath"
    }
    if (-not $NoStartMenu) { Write-Shortcut $MenuLnk '--show' }
    if (-not $NoStartup) { Write-Shortcut $StartupLnk '' }
    if (-not $NoFirewall) {
        Write-Host 'Allowing SoundSync through Windows Firewall (UDP; private and domain networks)...'
        Invoke-Elevated ("Remove-NetFirewallRule -DisplayName '$RuleName' -ErrorAction SilentlyContinue; " +
            "New-NetFirewallRule -DisplayName '$RuleName' -Direction Inbound -Program '$AppPath' -Protocol UDP " +
            "-LocalPort $Port,5353 -Profile Private,Domain -Action Allow | Out-Null")
    }
    if ($Start) { Start-Process -FilePath $AppPath -ArgumentList '--show' -WorkingDirectory $InstallDir }
    Write-Host "SoundSync installed to $InstallDir"
} finally {
    [void][Runtime.InteropServices.Marshal]::FinalReleaseComObject($shell)
}
