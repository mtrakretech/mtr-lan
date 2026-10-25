param([switch]$KeepRunning, [string]$PackagePath)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
if (!$PackagePath) { $PackagePath = Join-Path $root 'dist\MTR-LAN-Copy-0.7.3-windows-x64-portable-v8' }
$PackagePath = (Resolve-Path $PackagePath).Path
if (Get-Process MTR-LAN-Copy -ErrorAction SilentlyContinue) { throw 'Close existing MTR-LAN-Copy instances before this smoke test.' }
$oldPath = $env:PATH
$oldData = $env:XDG_DATA_DIRS
$oldSchemas = $env:GSETTINGS_SCHEMA_DIR
$oldA11y = $env:GTK_A11Y
$process = $null
try {
    # Intentionally omit the local build environment to detect missing runtime files.
    $env:PATH = "$PackagePath;$env:SystemRoot\System32;$env:SystemRoot"
    $env:XDG_DATA_DIRS = "$PackagePath\share"
    $env:GSETTINGS_SCHEMA_DIR = "$PackagePath\share\glib-2.0\schemas"
    $env:GTK_A11Y = 'none'
    # GUI-subsystem executables are asynchronous when invoked directly by Windows
    # PowerShell. Wait explicitly rather than reading a stale $LASTEXITCODE.
    $version = Start-Process "$PackagePath\MTR-LAN-Copy.exe" -ArgumentList '--version' -Wait -PassThru -RedirectStandardOutput "$PackagePath\smoke-version.log"
    if ($version.ExitCode -ne 0) { throw "Portable --version failed: $($version.ExitCode)" }
    Get-Content "$PackagePath\smoke-version.log"
    $help = Start-Process "$PackagePath\MTR-LAN-Copy.exe" -ArgumentList '--help' -Wait -PassThru -RedirectStandardOutput "$PackagePath\smoke-help.log"
    if ($help.ExitCode -ne 0) { throw "Portable --help failed: $($help.ExitCode)" }
    Get-Content "$PackagePath\smoke-help.log"
    # Exercise double-click behavior: no launcher, no --config, unrelated working directory.
    $process = Start-Process "$PackagePath\MTR-LAN-Copy.exe" -WorkingDirectory $env:TEMP -RedirectStandardOutput "$PackagePath\smoke-stdout.log" -RedirectStandardError "$PackagePath\smoke-stderr.log" -PassThru
    Start-Sleep -Seconds 8
    $process.Refresh()
    if ($process.HasExited) { throw "Portable GUI exited with code $($process.ExitCode). See smoke-stderr.log." }
    $daemonIds = @(Get-CimInstance Win32_Process -Filter "Name='MTR-LAN-Copy.exe'" | Where-Object { $_.ParentProcessId -eq $process.Id } | Select-Object -ExpandProperty ProcessId)
    if ($daemonIds.Count -ne 1) { throw 'Expected one child daemon.' }
    $ipc = @(Get-NetTCPConnection -State Listen -LocalAddress 127.0.0.1 -LocalPort 5252 -ErrorAction SilentlyContinue | Where-Object { $_.OwningProcess -in $daemonIds })
    $udp = @(Get-NetUDPEndpoint -LocalPort 4242 -ErrorAction SilentlyContinue | Where-Object { $_.OwningProcess -in $daemonIds })
    if (!$ipc.Count -or !$udp.Count) { throw 'Expected loopback IPC and UDP listener were not found.' }
    Get-Process -Id $process.Id | Select-Object Id,ProcessName,MainWindowTitle,Responding
    Write-Output "PASS: clean-PATH startup; GUI PID $($process.Id); daemon $daemonIds; loopback TCP 5252; UDP 4242."
    Write-Output 'This is a startup smoke test, not an end-to-end keyboard/mouse sharing test.'
} finally {
    $env:PATH = $oldPath
    $env:XDG_DATA_DIRS = $oldData
    $env:GSETTINGS_SCHEMA_DIR = $oldSchemas
    $env:GTK_A11Y = $oldA11y
    if (!$KeepRunning -and $process) {
        $children = @(Get-CimInstance Win32_Process -Filter "Name='MTR-LAN-Copy.exe'" | Where-Object { $_.ParentProcessId -eq $process.Id } | Select-Object -ExpandProperty ProcessId)
        $children | ForEach-Object { Stop-Process -Id $_ -ErrorAction SilentlyContinue }
        Stop-Process -Id $process.Id -ErrorAction SilentlyContinue
        # Stop-Process is asynchronous. Release the named single-instance mutex
        # and mapped EXE before an immediate test relaunch or binary replacement.
        $process.WaitForExit(10000) | Out-Null
        foreach ($childId in $children) {
            $child = Get-Process -Id $childId -ErrorAction SilentlyContinue
            if ($child) { $child.WaitForExit(10000) | Out-Null }
        }
    }
}
