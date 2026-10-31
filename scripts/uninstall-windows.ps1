# Uninstalls SoundSync installed by install-windows.ps1 (per user).
#
#   uninstall-windows.ps1               quit SoundSync, restore the default output devices if it
#                                       left them switched, remove its Start Menu and Startup
#                                       shortcuts, the installed program and the firewall rule (UAC)
#   uninstall-windows.ps1 -Purge        also delete settings, logs and state
#                                       (%APPDATA%\SoundSync, %LOCALAPPDATA%\SoundSync)
#   uninstall-windows.ps1 -NoFirewall   leave the firewall rule (no administrator prompt)
#
# Shortcuts that point at another program are never removed. A copy of this script is placed in
# the install folder, so it also works after the downloaded release folder is gone.
[CmdletBinding()]
param(
    [switch]$Purge,
    [switch]$NoFirewall,
    # Testing: the install, both shortcut folders and the data folders live under this directory.
    [string]$InstallRoot
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if ($InstallRoot) {
    $InstallDir = Join-Path $InstallRoot 'Programs\SoundSync'
    $StartupDir = Join-Path $InstallRoot 'Startup'
    $ProgramsDir = Join-Path $InstallRoot 'Start Menu'
    $DataDirs = @((Join-Path $InstallRoot 'Roaming\SoundSync'), (Join-Path $InstallRoot 'Local\SoundSync'))
} else {
    $InstallDir = Join-Path $env:LOCALAPPDATA 'Programs\SoundSync'
    $StartupDir = [Environment]::GetFolderPath('Startup')
    $ProgramsDir = [Environment]::GetFolderPath('Programs')
    $DataDirs = @((Join-Path $env:APPDATA 'SoundSync'), (Join-Path $env:LOCALAPPDATA 'SoundSync'))
}
$AppPath = Join-Path $InstallDir 'soundsync.exe'
$RuleName = 'SoundSync (UDP in)'
$shell = New-Object -ComObject WScript.Shell

function Test-Owned([string]$Path) {
    $target = $shell.CreateShortcut($Path).TargetPath
    return ($target -eq $AppPath) -or -not (Test-Path -LiteralPath $target)
}

try {
    # 1. Quit the running app: on exit it puts back default output devices it switched.
    $running = @(Get-Process -Name soundsync -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $AppPath })
    if ($running) {
        & $AppPath --quit | Out-Null
        $running | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
        $left = @($running | Where-Object { -not $_.HasExited })
        if ($left) {
            $left | Stop-Process -Force
            Write-Warning 'SoundSync did not quit within 10 s and was stopped.'
        }
        Write-Host 'Quit SoundSync'
    }

    # 2. After a crash the switched defaults are still saved; restore them before the app goes.
    if ((Test-Path -LiteralPath $AppPath) -and -not $InstallRoot) {
        $restored = & $AppPath restore-defaults 2>&1 | Out-String
        if ($restored.Trim() -and $restored.Trim() -ne 'nothing to restore') { Write-Host $restored.Trim() }
    }

    # 3. Shortcuts (only ours).
    foreach ($lnk in @((Join-Path $StartupDir 'SoundSync.lnk'), (Join-Path $ProgramsDir 'SoundSync.lnk'))) {
        if (Test-Path -LiteralPath $lnk) {
            if (Test-Owned $lnk) {
                Remove-Item -LiteralPath $lnk -Force
                Write-Host "Removed $lnk"
            } else {
                Write-Warning "Kept $lnk (it belongs to another program)."
            }
        }
    }

    # 4. The program (and this script's copy) from the install folder.
    foreach ($file in @($AppPath, (Join-Path $InstallDir 'uninstall-windows.ps1'))) {
        if (Test-Path -LiteralPath $file) {
            Remove-Item -LiteralPath $file -Force
            Write-Host "Removed $file"
        }
    }
    if ((Test-Path -LiteralPath $InstallDir) -and -not (Get-ChildItem -LiteralPath $InstallDir -Force)) {
        Remove-Item -LiteralPath $InstallDir
    }

    # 5. The firewall rule, only if there is one (removing it needs administrator rights).
    if (-not $NoFirewall -and -not $InstallRoot) {
        if (Get-NetFirewallRule -DisplayName $RuleName -ErrorAction SilentlyContinue) {
            $script = "Remove-NetFirewallRule -DisplayName '$RuleName' -ErrorAction SilentlyContinue; exit 0"
            $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($script))
            $p = Start-Process powershell -Verb RunAs -Wait -PassThru -ArgumentList @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-EncodedCommand', $encoded)
            if ($p.ExitCode -ne 0) { throw "Removing the firewall rule failed (exit $($p.ExitCode))." }
            Write-Host "Removed firewall rule '$RuleName'"
        }
    }

    # 6. Settings, logs and state.
    if ($Purge) {
        foreach ($dir in $DataDirs) {
            if (Test-Path -LiteralPath $dir) {
                Remove-Item -LiteralPath $dir -Recurse -Force
                Write-Host "Removed $dir"
            }
        }
    } else {
        Write-Host "Settings and logs were kept ($($DataDirs -join ', ')); use -Purge to delete them."
    }
    Write-Host 'SoundSync uninstalled.'
} finally {
    [void][Runtime.InteropServices.Marshal]::FinalReleaseComObject($shell)
}
