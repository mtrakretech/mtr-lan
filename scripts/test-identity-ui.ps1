param(
    [Parameter(Mandatory = $true)]
    [string]$PackagePath
)
$ErrorActionPreference = 'Stop'
$exe = Join-Path $PackagePath 'MTR-LAN-Copy.exe'
if (!(Test-Path $exe)) { throw "Missing executable: $exe" }
if (Get-Process MTR-LAN-Copy -ErrorAction SilentlyContinue) {
    throw 'Close existing MTR-LAN-Copy instances before the isolated UI test.'
}

$root = Join-Path $env:LOCALAPPDATA 'Temp\opencode'
$testDir = Join-Path $root ("MTR-LAN-Copy Identity UI Test " + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $testDir | Out-Null
$config = Join-Path $testDir 'config.toml'
Set-Content -Path $config -Value "port = 4242`nfrontend = 'gtk'`n" -Encoding Ascii
$process = $null
$testPassed = $false
try {
    $quotedConfig = '"' + $config + '"'
    $process = Start-Process -FilePath $exe -ArgumentList @('--config', $quotedConfig) -WorkingDirectory $PackagePath -PassThru
    $deadline = (Get-Date).AddSeconds(30)
    $window = $null
    while ((Get-Date) -lt $deadline -and $null -eq $window) {
        if ($process.HasExited) {
            $log = Join-Path $testDir 'logs\app.log'
            if (Test-Path $log) { Get-Content $log | Write-Output }
            throw "Application exited during startup (code $($process.ExitCode)); test files kept at $testDir."
        }
        $current = Get-Process -Id $process.Id -ErrorAction SilentlyContinue
        if ($current -and $current.MainWindowTitle -eq 'MTR-LAN-Copy' -and $current.MainWindowHandle -ne [IntPtr]::Zero) {
            $window = $current
        }
        Start-Sleep -Milliseconds 250
    }
    if ($null -eq $window) { throw 'Could not find the visible MTR-LAN-Copy main window.' }

    # GTK on Windows exposes the top-level window but not its child text boxes
    # through native UI Automation on this build. Send the same SetIdentity
    # event the GUI sends, while the actual GUI process is running, and verify
    # both persistence and that the frontend remains alive.
    $payload = [System.Text.Encoding]::UTF8.GetBytes(
        '{"SetIdentity":["UI Rename Regression","client"]}'
    )
    [byte[]]$length = [BitConverter]::GetBytes([UInt64]$payload.Length)
    [Array]::Reverse($length)
    $client = New-Object System.Net.Sockets.TcpClient
    try {
        $client.Connect('127.0.0.1', 5252)
        $stream = $client.GetStream()
        $stream.Write($length, 0, $length.Length)
        $stream.Write($payload, 0, $payload.Length)
        $stream.Flush()
    }
    finally {
        $client.Dispose()
    }

    $deadline = (Get-Date).AddSeconds(10)
    do {
        if ($process.HasExited) {
            $log = Join-Path $testDir 'logs\app.log'
            if (Test-Path $log) { Get-Content $log | Write-Output }
            throw "Application closed after editing Display name (code $($process.ExitCode)); test files kept at $testDir."
        }
        Start-Sleep -Milliseconds 250
        $saved = Get-Content $config -Raw
    } until ($saved -match '(?m)^device_name\s*=\s*"UI Rename Regression"' -or (Get-Date) -gt $deadline)
    if ($saved -notmatch '(?m)^device_name\s*=\s*"UI Rename Regression"') {
        throw "Display name did not persist to config.toml. Contents:`n$saved"
    }
    if ($process.HasExited) { throw 'Application closed immediately after the rename was saved.' }
    $testPassed = $true
    Write-Output 'PASS: visible GTK frontend stays open while a rename is applied and persists to config.toml.'
}
finally {
    if ($process -and !$process.HasExited) {
        Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
        $process.WaitForExit(10000)
    }
    if (!$testPassed) {
        # Preserve config and app/service logs when a test fails; they are
        # necessary to diagnose an immediate exit. The path is printed above.
    } elseif (Test-Path $testDir) {
        Remove-Item -LiteralPath $testDir -Recurse -Force
    }
}
