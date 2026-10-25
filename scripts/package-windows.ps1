$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$prefix = Join-Path $root '.tools\msys64\ucrt64'
$dist = Join-Path $root 'dist'
$package = Join-Path $dist 'MTR-LAN-Copy-0.7.3-windows-x64-portable-v7'
if (Test-Path $package) { throw "Package directory already exists: $package" }
New-Item -ItemType Directory -Force $package | Out-Null
Copy-Item "$root\target\release\lan-mouse.exe" "$package\MTR-LAN-Copy.exe"

# Bundle the recursive native DLL dependency closure, not development tools.
$queue = [System.Collections.Generic.Queue[string]]::new()
$seen = @{}
$queue.Enqueue("$package\MTR-LAN-Copy.exe")
foreach ($relative in @('lib\gdk-pixbuf-2.0', 'lib\gio\modules')) {
    $source = Join-Path $prefix $relative
    if (Test-Path $source) {
        $destination = Join-Path $package $relative
        New-Item -ItemType Directory -Force (Split-Path $destination) | Out-Null
        Copy-Item $source $destination -Recurse
        Get-ChildItem $destination -Filter *.dll -Recurse | ForEach-Object { $queue.Enqueue($_.FullName) }
    }
}
while ($queue.Count -gt 0) {
    $file = $queue.Dequeue()
    foreach ($line in (& "$prefix\bin\objdump.exe" -p $file)) {
        if ($line -match 'DLL Name:\s*(\S+)') {
            $dll = $Matches[1]
            if (!$seen.ContainsKey($dll.ToLowerInvariant())) {
                $seen[$dll.ToLowerInvariant()] = $true
                $source = Join-Path "$prefix\bin" $dll
                if (Test-Path $source) {
                    Copy-Item $source $package
                    $queue.Enqueue((Join-Path $package $dll))
                } elseif (!(Test-Path "$env:WINDIR\System32\$dll") -and $dll -notmatch '^(api-ms-|ext-ms-)') {
                    throw "Unresolved runtime DLL: $dll (from $file)"
                }
            }
        }
    }
}
foreach ($relative in @('share\glib-2.0\schemas', 'share\icons\Adwaita', 'share\icons\hicolor', 'share\mime', 'share\licenses', 'etc\fonts', 'share\fontconfig')) {
    $source = Join-Path $prefix $relative
    if (Test-Path $source) {
        $destination = Join-Path $package $relative
        New-Item -ItemType Directory -Force (Split-Path $destination) | Out-Null
        Copy-Item $source $destination -Recurse
    }
}
Copy-Item "$root\LICENSE" "$package\LICENSE.txt"
Copy-Item "$root\packaging\portable-config.toml" "$package\config.toml"
Set-Content "$package\portable.flag" 'Portable mode: load config.toml beside MTR-LAN-Copy.exe.'
Copy-Item "$root\packaging\desktop-settings.toml" $package
Copy-Item "$root\packaging\PORTABLE-README.txt" "$package\README.txt"
Copy-Item "$root\packaging\TWO-PC-SETUP.txt" "$package\TWO-PC-SETUP.txt"
& "$root\.tools\msys64\usr\bin\bash.exe" -lc 'pacman -Q' | Set-Content "$package\BUILD-DEPENDENCIES.txt"
git -C $root rev-parse HEAD | Set-Content "$package\SOURCE-COMMIT.txt"
Compress-Archive -Path $package -DestinationPath "$package.zip" -CompressionLevel Optimal
$sourceStage = Join-Path $dist 'MTR-LAN-Copy-0.7.3-source-v7'
if (Test-Path $sourceStage) { throw "Source stage already exists: $sourceStage" }
New-Item -ItemType Directory -Force $sourceStage | Out-Null
$sourceFiles = @(& git -C $root ls-files --cached --others --exclude-standard)
foreach ($relative in ($sourceFiles | Sort-Object -Unique)) {
    $destination = Join-Path $sourceStage $relative
    New-Item -ItemType Directory -Force (Split-Path $destination) | Out-Null
    Copy-Item (Join-Path $root $relative) $destination
}
Compress-Archive -Path $sourceStage -DestinationPath "$sourceStage.zip" -CompressionLevel Optimal
$checksumLines = foreach ($archive in @("$package.zip", "$sourceStage.zip")) {
    $stream = [IO.File]::OpenRead($archive)
    $sha = [Security.Cryptography.SHA256]::Create()
    try {
        $hash = [BitConverter]::ToString($sha.ComputeHash($stream)).Replace('-', '').ToLowerInvariant()
        "$hash  $([IO.Path]::GetFileName($archive))"
    } finally { $stream.Dispose(); $sha.Dispose() }
}
$checksumLines | Set-Content "$dist\SHA256SUMS-v7.txt"
Write-Output "Portable executable: $package\MTR-LAN-Copy.exe"
Write-Output "Portable archive: $package.zip"
