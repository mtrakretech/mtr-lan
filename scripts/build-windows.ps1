param([switch]$Test)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$prefix = Join-Path $root '.tools\msys64\ucrt64'
if (!(Test-Path "$prefix\bin\cargo.exe")) { throw 'Local MSYS2 UCRT64 Rust toolchain is missing.' }
$env:PATH = "$prefix\bin;$root\.tools\msys64\usr\bin;$env:PATH"
$env:PKG_CONFIG = "$prefix\bin\pkgconf.exe"
$env:PKG_CONFIG_PATH = "$prefix\lib\pkgconfig;$prefix\share\pkgconfig"
$env:CARGO_HOME = "$root\.tools\cargo"
Push-Location $root
try {
    & "$prefix\bin\cargo.exe" build --release --locked --no-default-features --features gtk
    if ($LASTEXITCODE -ne 0) { throw 'Cargo build failed.' }
    if ($Test) {
        & "$prefix\bin\cargo.exe" test --locked --no-default-features --features gtk
        if ($LASTEXITCODE -ne 0) { throw 'Cargo tests failed.' }
    }
} finally { Pop-Location }
